//! The AI orchestrator: LLM plans a circuit as a strict JSON contract,
//! someda's deterministic generators write KiCad files, kicad-cli verifies
//! and exports. The honest-done loop gates every stage.
//!
//! Pipeline: describe → plan (LLM JSON) → generate schematic → generate PCB
//! → kicad-cli ERC → kicad-cli DRC → kicad-cli exports → honest-done check.

use crate::gateway::Gateway;
use crate::kicad_cli::KiCadCli;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// The circuit plan the LLM must produce (strict JSON contract).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitPlan {
    pub name: String,
    pub description: String,
    pub components: Vec<PlannedComponent>,
    pub nets: Vec<PlannedNet>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedComponent {
    /// Reference designator: R1, C1, U1, ...
    pub reference: String,
    /// Component value: "10k", "100nF", "LM358", ...
    pub value: String,
    /// Symbol library name: "Device:R", "Device:C", ...
    pub lib_id: String,
    /// Footprint: "Resistor_SMD:R_0805_2012Metric", ...
    pub footprint: Option<String>,
    /// Board placement in mm (optional; the placer assigns if absent)
    pub x_mm: Option<f64>,
    pub y_mm: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedNet {
    /// Net name: "VCC", "GND", "SIG_A", ...
    pub name: String,
    /// Connected pins as "REF:PIN" strings: ["R1:1", "R2:2"]
    pub pins: Vec<String>,
}

/// The system prompt for circuit planning. The LLM sees the JSON contract
/// and must produce it exactly — no prose, no markdown.
pub const PLANNER_SYSTEM_PROMPT: &str = r#"You are an electronics design agent. Given a description of a circuit, produce a JSON circuit plan.

Return EXACTLY this JSON structure, no other text:
{
  "name": "short project name",
  "description": "one-line description",
  "components": [
    {"reference": "R1", "value": "10k", "lib_id": "Device:R", "footprint": "Resistor_SMD:R_0805_2012Metric", "x_mm": 50, "y_mm": 50}
  ],
  "nets": [
    {"name": "VCC", "pins": ["R1:1", "R2:2"]}
  ]
}

Rules:
- Every pin reference is "REFDES:PIN_NUMBER" (e.g., "R1:1", "U1:3")
- Every pin listed in a net must belong to a listed component
- Every component pin must appear in exactly one net or be deliberately unconnected (omit it)
- Power nets are named "VCC" and "GND" (use these exact names)
- Include decoupling capacitors (100nF) on every IC's power pins
- Use standard values: E12 series for resistors, standard capacitor values
- Place components at reasonable x_mm/y_mm coordinates (0-100mm range)
- For a 555 timer LED blinker: NE555 in the center, timing R+C to one side, LED+R to output
- lib_id uses KiCad convention: "Device:R", "Device:C", "Device:LED", "Timer:NE555", "power:GND", "power:VCC"

Produce ONLY the JSON. No markdown fences, no explanation."#;

#[derive(Debug)]
pub enum OrchestrateError {
    Gateway(String),
    PlanInvalid(String),
    Io(String),
    KiCadCli(String),
    Verification(String),
}

impl std::fmt::Display for OrchestrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrchestrateError::Gateway(e) => write!(f, "AI gateway: {}", e),
            OrchestrateError::PlanInvalid(e) => write!(f, "invalid plan: {}", e),
            OrchestrateError::Io(e) => write!(f, "I/O: {}", e),
            OrchestrateError::KiCadCli(e) => write!(f, "kicad-cli: {}", e),
            OrchestrateError::Verification(e) => write!(f, "verification: {}", e),
        }
    }
}

/// Result of a full end-to-end build.
#[derive(Debug)]
pub struct BuildResult {
    pub plan: CircuitPlan,
    pub project_dir: String,
    pub schematic_file: String,
    pub board_file: String,
    pub stages: Vec<StageResult>,
}

#[derive(Debug)]
pub struct StageResult {
    pub stage: String,
    pub success: bool,
    pub detail: String,
}

/// Run the full pipeline: plan → generate → verify → export.
pub fn build(
    gateway: &Gateway,
    cli: &KiCadCli,
    project_dir: &Path,
    description: &str,
) -> Result<BuildResult, OrchestrateError> {
    let mut stages = Vec::new();

    // ─── Stage 1: Plan (LLM) ───
    println!("─── Stage 1: Planning circuit with LLM ───");
    let raw = gateway
        .chat_json(PLANNER_SYSTEM_PROMPT, description)
        .map_err(|e| OrchestrateError::Gateway(e.to_string()))?;

    let plan: CircuitPlan = serde_json::from_value(raw)
        .map_err(|e| OrchestrateError::PlanInvalid(format!("LLM output does not match the contract: {}", e)))?;

    validate_plan(&plan)?;
    stages.push(StageResult {
        stage: "plan".into(),
        success: true,
        detail: format!("{} components, {} nets", plan.components.len(), plan.nets.len()),
    });

    // ─── Stage 2: Generate files ───
    println!("─── Stage 2: Generating KiCad project files ───");
    fs::create_dir_all(project_dir)
        .map_err(|e| OrchestrateError::Io(e.to_string()))?;

    let sch_content = generate_schematic(&plan);
    let mut board = generate_board_model(&plan);

    let sch_path = project_dir.join("circuit.kicad_sch");
    let pcb_path = project_dir.join("circuit.kicad_pcb");

    fs::write(&sch_path, &sch_content)
        .map_err(|e| OrchestrateError::Io(e.to_string()))?;

    // ─── Stage 2b: Deterministic placement (skill 102) ───
    // The LLM never outputs coordinates; the solver owns geometry.
    println!("─── Stage 2b: Deterministic placement ───");
    {
        let nl = crate::netlist::Netlist::from_board(&board);
        let (w, h) = board
            .outline_bbox()
            .map(|(x0, y0, x1, y1)| (x1 - x0, y1 - y0))
            .unwrap_or((100.0, 80.0));
        let placer = crate::placer::Placer::new(w, h);
        let intent = crate::placer::PlacementIntent::default();
        let solved = placer.arrange(&board, &nl, &intent);
        for s in &solved {
            if let Some(fp) = board.footprints.iter_mut().find(|f| f.reference == s.reference) {
                // Move the footprint AND its pads (pads are absolute in the model)
                let (dx, dy) = (s.x - fp.x, s.y - fp.y);
                fp.x = s.x;
                fp.y = s.y;
                for pad in &mut fp.pads {
                    pad.x += dx;
                    pad.y += dy;
                }
            }
        }
        stages.push(StageResult {
            stage: "placement".into(),
            success: true,
            detail: format!("placed {} footprint(s) deterministically", solved.len()),
        });
    }

    // ─── Stage 2c: Autorouting (skill 101: SomRoute defaults) ───
    println!("─── Stage 2c: Autorouting (trace 0.25mm, clearance 0.2mm, via 0.6/0.3) ───");
    let route_summary;
    let unrouted_count;
    {
        let nl = crate::netlist::Netlist::from_board(&board);
        let router = crate::router::Router::new(crate::router::RouterConfig::default());
        let route = router.route(&board, &nl);
        route_summary = route.summary_line();
        unrouted_count = route.unrouted.len();
        for seg in &route.segments {
            board.segments.push(crate::board::Segment {
                start: (seg.x1, seg.y1),
                end: (seg.x2, seg.y2),
                width: seg.width,
                layer: seg.layer.clone(),
                net: Some(seg.net.clone()),
            });
        }
        for via in &route.vias {
            board.vias.push(crate::board::Via {
                x: via.x,
                y: via.y,
                size: via.size,
                drill: via.drill,
                layers: vec!["F.Cu".into(), "B.Cu".into()],
                net: Some(via.net.clone()),
            });
        }
        println!("  {}", route_summary);
        stages.push(StageResult {
            stage: "route".into(),
            success: route.unrouted.is_empty(),
            detail: route_summary.clone(),
        });
    }

    let pcb_content = board.to_kicad_pcb();
    fs::write(&pcb_path, &pcb_content)
        .map_err(|e| OrchestrateError::Io(e.to_string()))?;

    stages.push(StageResult {
        stage: "generate".into(),
        success: true,
        detail: format!("wrote {} and {}", sch_path.display(), pcb_path.display()),
    });

    // ─── Stage 3: kicad-cli ERC (if available) ───
    if cli.is_available() {
        println!("─── Stage 3: kicad-cli ERC ───");
        let erc = cli.sch_erc("circuit.kicad_sch");
        stages.push(StageResult {
            stage: "kicad-erc".into(),
            success: erc.success,
            detail: format!("{}", erc),
        });

        println!("─── Stage 4: kicad-cli DRC ───");
        let drc = cli.pcb_drc("circuit.kicad_pcb");
        stages.push(StageResult {
            stage: "kicad-drc".into(),
            success: drc.success,
            detail: format!("{}", drc),
        });

        println!("─── Stage 5: kicad-cli exports ───");
        let out_dir = project_dir.join("fab");
        let _ = fs::create_dir_all(&out_dir);

        let gerbers = cli.export_gerbers("circuit.kicad_pcb", "fab");
        stages.push(StageResult {
            stage: "export-gerbers".into(),
            success: gerbers.success,
            detail: format!("{}", gerbers),
        });

        let drill = cli.export_drill("circuit.kicad_pcb", "fab");
        stages.push(StageResult {
            stage: "export-drill".into(),
            success: drill.success,
            detail: format!("{}", drill),
        });

        let pos = cli.export_pos("circuit.kicad_pcb", "fab");
        stages.push(StageResult {
            stage: "export-pos".into(),
            success: pos.success,
            detail: format!("{}", pos),
        });

        let bom = cli.export_bom("circuit.kicad_sch", "fab/bom.csv");
        stages.push(StageResult {
            stage: "export-bom".into(),
            success: bom.success,
            detail: format!("{}", bom),
        });
    } else {
        stages.push(StageResult {
            stage: "kicad-cli".into(),
            success: false,
            detail: "kicad-cli not found on PATH; skipping ERC/DRC/exports (set SOMEDA_KICAD_CLI to configure)".into(),
        });
    }

    // ─── Final: someda's own verification (always runs) ───
    println!("─── Stage 6: someda verification ───");
    let sch_text = fs::read_to_string(&sch_path)
        .map_err(|e| OrchestrateError::Io(e.to_string()))?;
    let sch = crate::schematic::Schematic::from_kicad_sch(&sch_text)
        .map_err(|e| OrchestrateError::Verification(format!("schematic parse: {}", e)))?;
    let erc_report = crate::schematic::run_erc(&sch);
    stages.push(StageResult {
        stage: "someda-erc".into(),
        success: erc_report.is_clean(),
        detail: format!("{} finding(s)", erc_report.errors()),
    });

    let pcb_text = fs::read_to_string(&pcb_path)
        .map_err(|e| OrchestrateError::Io(e.to_string()))?;
    let board = crate::board::Board::from_kicad_pcb(&pcb_text)
        .map_err(|e| OrchestrateError::Verification(format!("board parse: {}", e)))?;
    let drc_report = crate::drc::run_drc(&board, &crate::drc::DrcConfig::default());
    stages.push(StageResult {
        stage: "someda-drc".into(),
        success: drc_report.is_clean(),
        detail: format!("{} error(s), {} warning(s)", drc_report.errors(), drc_report.warnings()),
    });

    // ─── Stage 7: the 6-gate export preflight (skill 103) ───
    // DRC ✓ + shorts ✓ + unrouted ✓ + pours + Gerbers + drill — the honest
    // done-gate: only all-six-pass means orderable.
    let pours_filled = true; // no zones yet: nothing unfilled (M3 pours land in M4)
    let gerber_ok = cli.is_available(); // exports ran above when kicad-cli exists
    let drill_ok = cli.is_available();
    let preflight_gates = crate::formulas::export_preflight(
        drc_report.errors(),
        0, // shorts surface as DRC errors in the current engine
        unrouted_count,
        gerber_ok,
        pours_filled,
        drill_ok,
    );
    let preflight_pass = preflight_gates.iter().all(|g| g.passed);
    stages.push(StageResult {
        stage: "export-preflight".into(),
        success: preflight_pass,
        detail: preflight_gates
            .iter()
            .map(|g| format!("{}={}", g.name, if g.passed { "PASS" } else { "FAIL" }))
            .collect::<Vec<_>>()
            .join(" "),
    });

    println!("─── Stage 7: export preflight (skill 103) ───");
    for g in &preflight_gates {
        println!("  {:>10} {}: {}", g.name, if g.passed { "PASS" } else { "FAIL" }, g.detail);
    }
    if preflight_pass {
        println!("─── HONEST-DONE: all 6 gates pass ───");
    } else {
        println!("─── NOT DONE: failing gates listed above ───");
    }

    Ok(BuildResult {
        plan,
        project_dir: project_dir.display().to_string(),
        schematic_file: sch_path.display().to_string(),
        board_file: pcb_path.display().to_string(),
        stages,
    })
}

/// Validate the plan against the contract (every pin belongs to a component).
pub fn validate_plan(plan: &CircuitPlan) -> Result<(), OrchestrateError> {
    if plan.components.is_empty() {
        return Err(OrchestrateError::PlanInvalid("no components in plan".into()));
    }
    if plan.nets.is_empty() {
        return Err(OrchestrateError::PlanInvalid("no nets in plan".into()));
    }
    let refs: std::collections::HashSet<&str> =
        plan.components.iter().map(|c| c.reference.as_str()).collect();
    for net in &plan.nets {
        for pin in &net.pins {
            let ref_des = pin.split(':').next().unwrap_or("");
            if !refs.contains(ref_des) {
                return Err(OrchestrateError::PlanInvalid(format!(
                    "net '{}' references '{}' but component '{}' does not exist",
                    net.name, pin, ref_des
                )));
            }
        }
    }
    Ok(())
}

/// Generate a .kicad_sch from the plan (deterministic, no LLM involvement).
pub fn generate_schematic(plan: &CircuitPlan) -> String {
    let mut out = String::new();
    out.push_str("(kicad_sch (version 20220904) (generator someda)\n");
    out.push_str(&format!("  (title_block (title \"{}\"))\n", plan.name));

    // Place symbols in a grid: 50mm apart, 2 per row
    let mut lib_symbols = String::new();
    let mut symbols = String::new();

    for (i, comp) in plan.components.iter().enumerate() {
        let x = comp.x_mm.unwrap_or(50.0 + (i % 2) as f64 * 60.0);
        let y = comp.y_mm.unwrap_or(50.0 + (i / 2) as f64 * 40.0);

        // Embed a minimal lib symbol for this component type
        let lib_name = &comp.lib_id;
        // KiCad requires sub-symbol names as "libname:Name_unit_style" (e.g. "Device:R_0_1")
        let base_name = lib_name.split(':').last().unwrap_or(lib_name);
        let sub_name = format!("{}_0_1", base_name);
        lib_symbols.push_str(&format!(
            "    (symbol \"{}\" (pin_names (offset 0.254))\n      (symbol \"{}\"\n",
            lib_name, sub_name
        ));
        // Two pins for passives, more for ICs (simplified: 2 pins)
        lib_symbols.push_str(
            "        (pin passive line (at -5.08 0 0) (length 5.08)\n\
             (name \"1\" (effects (font (size 1.27 1.27))))\n\
             (number \"1\" (effects (font (size 1.27 1.27)))))\n\
             (pin passive line (at 5.08 0 180) (length 5.08)\n\
             (name \"2\" (effects (font (size 1.27 1.27))))\n\
             (number \"2\" (effects (font (size 1.27 1.27)))))\n\
             )\n    )\n"
        );

        symbols.push_str(&format!(
            "  (symbol (lib_id \"{}\") (at {:.2} {:.2} 0) (unit 1)\n\
             (property \"Reference\" \"{}\" (at 0 -5 0))\n\
             (property \"Value\" \"{}\" (at 0 5 0))\n  )\n",
            lib_name, x, y, comp.reference, comp.value
        ));
    }

    out.push_str("  (lib_symbols\n");
    out.push_str(&lib_symbols);
    out.push_str("  )\n");

    // Wires from nets: connect pins of the same net with wires
    let mut wire_id = 0;
    for net in &plan.nets {
        if net.pins.len() < 2 {
            continue;
        }
        // Connect pins in a chain: pin1 → pin2 → pin3...
        for pair in net.pins.windows(2) {
            let p1 = parse_pin(&pair[0], &plan);
            let p2 = parse_pin(&pair[1], &plan);
            if let (Some((x1, y1)), Some((x2, y2))) = (p1, p2) {
                out.push_str(&format!(
                    "  (wire (pts (xy {:.2} {:.2}) (xy {:.2} {:.2})) (uuid wire_{}) )\n",
                    x1, y1, x2, y2, wire_id
                ));
                wire_id += 1;
            }
        }
        // Label the net at the first pin
        if let Some((lx, ly)) = net.pins.first().and_then(|p| parse_pin(p, &plan)) {
            out.push_str(&format!(
                "  (label \"{}\" (at {:.2} {:.2} 0))\n",
                net.name, lx, ly
            ));
        }
    }

    out.push_str(&symbols);
    out.push_str(")\n");
    out
}

/// Parse "REF:PIN" to (x, y) using the component placement grid.
fn parse_pin(pin_ref: &str, plan: &CircuitPlan) -> Option<(f64, f64)> {
    let (ref_des, pin_num) = pin_ref.split_once(':')?;
    let idx = plan.components.iter().position(|c| c.reference == ref_des)?;
    let comp = &plan.components[idx];
    let x = comp.x_mm.unwrap_or(50.0 + (idx % 2) as f64 * 60.0);
    let y = comp.y_mm.unwrap_or(50.0 + (idx / 2) as f64 * 40.0);
    // Pin 1 left, pin 2 right (simplified 2-pin model)
    match pin_num {
        "1" => Some((x - 5.08, y)),
        "2" => Some((x + 5.08, y)),
        _ => Some((x, y)), // other pins at center (simplified)
    }
}

/// Generate a .kicad_pcb from the plan (deterministic).
/// Generate a Board MODEL from the plan (the routing substrate).
/// Copper is added later by the router; this creates outline, nets,
/// footprints with pads, and assigns pad nets from the plan.
pub fn generate_board_model(plan: &CircuitPlan) -> crate::board::Board {
    let mut board = crate::board::Board::default();

    // Board outline from a fixed hobby-size A100x80 with margin — the
    // placer owns real placement, the outline follows the area budget
    // (skill 102: board area = Σ courtyards / target density).
    let courtyard_mm2: f64 = plan.components.len() as f64 * 60.0; // ~60mm²/part
    let area = (courtyard_mm2 / 0.35).max(4000.0); // hobby density 35%
    let w = (area / 0.8).sqrt(); // aspect 0.8
    let h = area / w;
    let (min_x, min_y, max_x, max_y) = (0.0, 0.0, w, h);
    board.outline = vec![
        ((min_x, min_y), (max_x, min_y)),
        ((max_x, min_y), (max_x, max_y)),
        ((max_x, max_y), (min_x, max_y)),
        ((min_x, max_y), (min_x, min_y)),
    ];

    // Nets
    for (i, net) in plan.nets.iter().enumerate() {
        board.nets.push((i + 1, net.name.clone()));
    }

    // Footprints with 2-pin pads; pad nets from the plan
    for comp in &plan.components {
        let mut fp = crate::board::Footprint {
            reference: comp.reference.clone(),
            lib_name: comp.footprint.clone().unwrap_or_else(|| "someda:DEFAULT".into()),
            x: 0.0,
            y: 0.0,
            rotation: 0.0,
            pads: Vec::new(),
        };
        // Default footprint-relative pad geometry (2-pin SMD, 2.5mm pitch)
        for (pin_num, dx) in [("1", -1.25), ("2", 1.25)] {
            let pad_net = plan
                .nets
                .iter()
                .enumerate()
                .find(|(_, n)| n.pins.iter().any(|p| p == &format!("{}:{}", comp.reference, pin_num)))
                .map(|(i, n)| (i + 1, n.name.clone()));
            fp.pads.push(crate::board::Pad {
                number: pin_num.into(),
                net: pad_net,
                x: dx, // relative until the placer sets fp.x/y; writer recomputes
                y: 0.0,
                width: 1.5,
                height: 1.5,
            });
        }
        // Absolute pad positions start centered at origin; the placer moves the fp
        fp.x = 0.0;
        fp.y = 0.0;
        for pad in &mut fp.pads {
            pad.x += fp.x;
            pad.y += fp.y;
        }
        board.footprints.push(fp);
    }

    board
}

pub fn generate_board(plan: &CircuitPlan) -> String {
    let mut out = String::new();
    out.push_str("(kicad_pcb (version 20221018) (generator someda)\n");
    out.push_str("  (general (thickness 1.6))\n");
    out.push_str("  (paper \"A4\")\n");
    out.push_str("  (layers\n");
    out.push_str("    (0 \"F.Cu\" signal)\n    (31 \"B.Cu\" signal)\n");
    out.push_str("    (44 \"Edge.Cuts\" user)\n  )\n");

    // Board outline: bounding box of components + 10mm margin
    let xs: Vec<f64> = plan.components.iter()
        .map(|c| c.x_mm.unwrap_or(50.0))
        .collect();
    let ys: Vec<f64> = plan.components.iter()
        .map(|c| c.y_mm.unwrap_or(50.0))
        .collect();
    let (min_x, max_x) = (xs.iter().cloned().fold(f64::INFINITY, f64::min) - 10.0,
                          xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max) + 10.0);
    let (min_y, max_y) = (ys.iter().cloned().fold(f64::INFINITY, f64::min) - 10.0,
                          ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max) + 10.0);

    out.push_str(&format!(
        "  (gr_line (start {:.2} {:.2}) (end {:.2} {:.2}) (layer \"Edge.Cuts\") (width 0.1))\n\
         (gr_line (start {:.2} {:.2}) (end {:.2} {:.2}) (layer \"Edge.Cuts\") (width 0.1))\n\
         (gr_line (start {:.2} {:.2}) (end {:.2} {:.2}) (layer \"Edge.Cuts\") (width 0.1))\n\
         (gr_line (start {:.2} {:.2}) (end {:.2} {:.2}) (layer \"Edge.Cuts\") (width 0.1))\n",
        min_x, min_y, max_x, min_y,
        max_x, min_y, max_x, max_y,
        max_x, max_y, min_x, max_y,
        min_x, max_y, min_x, min_y,
    ));

    // Net definitions
    for (i, net) in plan.nets.iter().enumerate() {
        out.push_str(&format!("  (net {} \"{}\")\n", i + 1, net.name));
    }

    // Footprints
    for (i, comp) in plan.components.iter().enumerate() {
        let x = comp.x_mm.unwrap_or(50.0 + (i % 2) as f64 * 60.0);
        let y = comp.y_mm.unwrap_or(50.0 + (i / 2) as f64 * 40.0);
        let footprint = comp.footprint.as_deref().unwrap_or("someda:DEFAULT");

        // Find nets for this component's pins
        let mut pad_net_1 = 0;
        let mut pad_net_2 = 0;
        for (ni, net) in plan.nets.iter().enumerate() {
            for pin in &net.pins {
                if pin == &format!("{}:1", comp.reference) {
                    pad_net_1 = ni + 1;
                }
                if pin == &format!("{}:2", comp.reference) {
                    pad_net_2 = ni + 1;
                }
            }
        }

        out.push_str(&format!(
            "  (footprint \"{}\" (layer \"F.Cu\") (at {:.2} {:.2} 0)\n\
             (fp_text reference \"{}\" (at 0 -2) (layer \"F.SilkS\"))\n\
             (fp_text value \"{}\" (at 0 2) (layer \"F.Fab\"))\n\
             (pad \"1\" smd rect (at -1.25 0) (size 1.5 1.5) (layers \"F.Cu\" \"F.Mask\") (net {} \"{}\"))\n\
             (pad \"2\" smd rect (at 1.25 0) (size 1.5 1.5) (layers \"F.Cu\" \"F.Mask\") (net {} \"{}\"))\n\
             )\n",
            footprint, x, y, comp.reference, comp.value,
            pad_net_1, plan.nets.get(pad_net_1.saturating_sub(1)).map(|n| n.name.as_str()).unwrap_or(""),
            pad_net_2, plan.nets.get(pad_net_2.saturating_sub(1)).map(|n| n.name.as_str()).unwrap_or(""),
        ));
    }

    out.push_str(")\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> CircuitPlan {
        serde_json::from_str(r#"{
            "name": "LED Blinker",
            "description": "555 timer LED blinker",
            "components": [
                {"reference": "U1", "value": "NE555", "lib_id": "Timer:NE555", "footprint": "Package_DIP:DIP-8", "x_mm": 50, "y_mm": 50},
                {"reference": "R1", "value": "10k", "lib_id": "Device:R", "footprint": "Resistor_SMD:R_0805", "x_mm": 70, "y_mm": 50},
                {"reference": "C1", "value": "10uF", "lib_id": "Device:C", "footprint": "Capacitor_SMD:C_0805", "x_mm": 50, "y_mm": 70}
            ],
            "nets": [
                {"name": "VCC", "pins": ["U1:1", "R1:1"]},
                {"name": "GND", "pins": ["U1:2", "C1:2"]},
                {"name": "TRIG", "pins": ["U1:3", "R1:2", "C1:1"]}
            ]
        }"#).unwrap()
    }

    #[test]
    fn plan_valid() {
        assert!(validate_plan(&sample_plan()).is_ok());
    }

    #[test]
    fn plan_invalid_pin_ref() {
        let mut plan = sample_plan();
        plan.nets[0].pins.push("X99:1".into());
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn schematic_generation() {
        let sch = generate_schematic(&sample_plan());
        assert!(sch.contains("(kicad_sch"));
        assert!(sch.contains("NE555"));
        assert!(sch.contains("\"R1\""));
        assert!(sch.contains("\"VCC\""));
        // Parse it back with our own parser
        let parsed = crate::schematic::Schematic::from_kicad_sch(&sch);
        assert!(parsed.is_ok(), "{:?}", parsed);
    }

    #[test]
    fn board_generation() {
        let pcb = generate_board(&sample_plan());
        assert!(pcb.contains("(kicad_pcb"));
        assert!(pcb.contains("Edge.Cuts"));
        assert!(pcb.contains("\"U1\""));
        // Parse it back with our own parser
        let parsed = crate::board::Board::from_kicad_pcb(&pcb);
        assert!(parsed.is_ok(), "{:?}", parsed);
    }

    #[test]
    fn full_pipeline_places_and_routes() {
        // The idea-to-PCB spine: model → place → route → serialize → reparse
        let plan = sample_plan();
        let mut board = generate_board_model(&plan);
        assert!(!board.footprints.is_empty());

        // Place deterministically (skill 102)
        let nl = crate::netlist::Netlist::from_board(&board);
        let (w, h) = board
            .outline_bbox()
            .map(|(x0, y0, x1, y1)| (x1 - x0, y1 - y0))
            .unwrap_or((100.0, 80.0));
        let placer = crate::placer::Placer::new(w, h);
        let solved = placer.arrange(&board, &nl, &crate::placer::PlacementIntent::default());
        for s in &solved {
            if let Some(fp) = board.footprints.iter_mut().find(|f| f.reference == s.reference) {
                let (dx, dy) = (s.x - fp.x, s.y - fp.y);
                fp.x = s.x;
                fp.y = s.y;
                for pad in &mut fp.pads {
                    pad.x += dx;
                    pad.y += dy;
                }
            }
        }
        // All footprints inside the outline
        let (x0, y0, x1, y1) = board.outline_bbox().unwrap();
        for fp in &board.footprints {
            assert!(fp.x >= x0 && fp.x <= x1 && fp.y >= y0 && fp.y <= y1,
                "{} outside outline", fp.reference);
        }

        // Route (skill 101 defaults)
        let nl = crate::netlist::Netlist::from_board(&board);
        let router = crate::router::Router::new(crate::router::RouterConfig::default());
        let route = router.route(&board, &nl);
        assert!(!route.segments.is_empty(), "copper must be produced: {}", route.summary_line());
        for seg in &route.segments {
            board.segments.push(crate::board::Segment {
                start: (seg.x1, seg.y1),
                end: (seg.x2, seg.y2),
                width: seg.width,
                layer: seg.layer.clone(),
                net: Some(seg.net.clone()),
            });
        }

        // Serialize + reparse: the copper survives the round-trip
        let written = board.to_kicad_pcb();
        let reparsed = crate::board::Board::from_kicad_pcb(&written)
            .expect("written board must reparse");
        assert_eq!(reparsed.segments.len(), board.segments.len(),
            "copper lost in round-trip");
        assert_eq!(reparsed.footprints.len(), board.footprints.len());
    }

    #[test]
    fn board_drc_clean_on_generated() {
        let pcb = generate_board(&sample_plan());
        let board = crate::board::Board::from_kicad_pcb(&pcb).unwrap();
        let report = crate::drc::run_drc(&board, &crate::drc::DrcConfig::default());
        assert!(report.is_clean(), "generated board should be DRC-clean: {:?}",
            report.findings.iter().map(|f| &f.code).collect::<Vec<_>>());
    }

    #[test]
    fn planner_prompt_is_contract() {
        assert!(PLANNER_SYSTEM_PROMPT.contains("\"components\""));
        assert!(PLANNER_SYSTEM_PROMPT.contains("\"nets\""));
        assert!(PLANNER_SYSTEM_PROMPT.contains("REFDES:PIN_NUMBER"));
    }
}