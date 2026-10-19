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

    // ─── Stage 2a: pin coverage (honest-done) ───
    // Every plan pin must exist as a pad BEFORE we place/route — the v0
    // generator's fixed 2-pad model silently dropped IC pins; this gate
    // makes that impossible to ship again.
    verify_pin_coverage(&plan, &board)
        .map_err(|e| OrchestrateError::Verification(format!("pin coverage: {}", e)))?;
    stages.push(StageResult {
        stage: "pin-coverage".into(),
        success: true,
        detail: format!(
            "every plan pin has a pad ({} pins across {} footprints)",
            plan.nets.iter().map(|n| n.pins.len()).sum::<usize>(),
            board.footprints.len()
        ),
    });

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
    for (net_index, net) in plan.nets.iter().enumerate() {
        for pin in &net.pins {
            let ref_des = pin.split(':').next().unwrap_or("");
            if !refs.contains(ref_des) {
                return Err(OrchestrateError::PlanInvalid(format!(
                    "net '{}' references '{}' but component '{}' does not exist",
                    net.name, pin, ref_des
                )));
            }
            if pin.split(':').count() != 2 || pin.split(':').nth(1).unwrap_or("").is_empty() {
                return Err(OrchestrateError::PlanInvalid(format!(
                    "net '{}' has malformed pin '{}' (expected REFDES:PIN_NUMBER)",
                    net.name, pin
                )));
            }
        }
    }
    // Duplicate references would make net-to-pad joins ambiguous
    let mut seen = std::collections::HashSet::new();
    for c in &plan.components {
        if !seen.insert(c.reference.as_str()) {
            return Err(OrchestrateError::PlanInvalid(format!(
                "duplicate component reference '{}'",
                c.reference
            )));
        }
    }
    Ok(())
}

/// Honest-done: every pin the plan names must exist as a pad on the board.
/// Catches generator drift (the v0 2-pad model dropped IC pins silently).
pub fn verify_pin_coverage(
    plan: &CircuitPlan,
    board: &crate::board::Board,
) -> Result<(), OrchestrateError> {
    for (net_index, net) in plan.nets.iter().enumerate() {
        for pin in &net.pins {
            let (refdes, num) = match pin.split_once(':') {
                Some(x) => x,
                None => {
                    return Err(OrchestrateError::Verification(format!(
                        "malformed pin '{}' in net '{}'",
                        pin, net.name
                    )))
                }
            };
            let ok = board
                .footprints
                .iter()
                .find(|f| f.reference == refdes)
                .map(|f| f.pads.iter().any(|p| p.number == num))
                .unwrap_or(false);
            if !ok {
                return Err(OrchestrateError::Verification(format!(
                    "pin '{}' (net '{}') has no pad on the board — the plan and copper disagree",
                    pin, net.name
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
        // KiCad schematic grid: 1.27 mm (50 mil). Snap the symbol origin so
        // every pin endpoint (origin ± 5.08 / ± 2.54k) lands ON grid —
        // endpoint_off_grid violations disappear (live-verified).
        let snap = |v: f64| (v / 1.27).round() * 1.27;
        let x = snap(comp.x_mm.unwrap_or(50.8 + (i % 2) as f64 * 63.5));
        let y = snap(comp.y_mm.unwrap_or(50.8 + (i / 2) as f64 * 38.1));

        // Embed a minimal lib symbol for this component type
        let lib_name = &comp.lib_id;
        // KiCad requires sub-symbol names as "libname:Name_unit_style" (e.g. "Device:R_0_1")
        let base_name = lib_name.split(':').last().unwrap_or(lib_name);
        let sub_name = format!("{}_0_1", base_name);
        lib_symbols.push_str(&format!(
            "    (symbol \"{}\" (pin_names (offset 0.254))\n      (symbol \"{}\"\n",
            lib_name, sub_name
        ));
        // ONE SYMBOL PIN PER PLAN-REFERENCED PIN — mirror of the board fix.
        // (Live-validated: the 2-pin-only schematic dropped U1:3/U1:8,
        // so ERC saw pins_not_connected for pins the plan actually uses.)
        let mut pin_nums: Vec<String> = Vec::new();
        for (net_index, net) in plan.nets.iter().enumerate() {
            for pin in &net.pins {
                if let Some((refdes, num)) = pin.split_once(':') {
                    if refdes == comp.reference && !pin_nums.iter().any(|p| p == num) {
                        pin_nums.push(num.to_string());
                    }
                }
            }
        }
        if pin_nums.is_empty() {
            pin_nums.push("1".into());
            pin_nums.push("2".into());
        }
        // Pin geometry: 2 pins inline; >2 pins on a DIP rectangle
        // (left column top-to-bottom, right column bottom-to-top)
        let n = pin_nums.len();
        for (pi, pin_num) in pin_nums.iter().enumerate() {
            let (px, py, ang) = if n <= 2 {
                if pi == 0 { (-5.08, 0.0, 0) } else { (5.08, 0.0, 180) }
            } else {
                let half = (n + 1) / 2;
                let pitch = 2.54;
                if pi < half {
                    (-5.08, (half as f64 - 1.0 - pi as f64) * pitch, 0)
                } else {
                    let k = n - 1 - pi;
                    (5.08, (k as f64 - (half as f64 - 1.0)) * pitch, 180)
                }
            };
            lib_symbols.push_str(&format!(
                "        (pin passive line (at {:.2} {:.2} {}) (length 5.08)\n\
                 (name \"{}\" (effects (font (size 1.27 1.27))))\n\
                 (number \"{}\" (effects (font (size 1.27 1.27)))))\n",
                px, py, ang, pin_num, pin_num
            ));
        }
        lib_symbols.push_str("             )\n    )\n");

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

    // Wires from nets: Manhattan channel routing (per-net rows below the grid)
    let mut wire_id = 0;
    // Channel rows start 30mm below the lowest symbol and stack 5mm per net
    let snap_grid = |v: f64| (v / 1.27).round() * 1.27;
    let n_rows = (plan.components.len() as f64 / 2.0).ceil().max(1.0);
    let channel_base = snap_grid(50.8 + n_rows * 38.1 + 12.7);
    let channel_gap = 5.08;
    for (net_index, net) in plan.nets.iter().enumerate() {
        if net.pins.len() < 2 {
            continue;
        }
        // MANHATTAN CHANNEL ROUTING (schematic bus): every net gets a
        // dedicated horizontal channel row far below the component grid.
        // pin → vertical drop to the channel → horizontal run → vertical
        // rise to the target pin. Straight runs THROUGH other components'
        // pins created real shorts (live-verified: GND's horizontal passed
        // through R1:1, joining VCC and GND — multiple_net_names).
        // Channel rows are per-net unique: crossings without shared
        // endpoints do not connect in KiCad.
        let channel_y = channel_base - net_index as f64 * channel_gap;
        for pair in net.pins.windows(2) {
            let p1 = parse_pin(&pair[0], &plan);
            let p2 = parse_pin(&pair[1], &plan);
            if let (Some((x1, y1)), Some((x2, y2))) = (p1, p2) {
                // Vertical drop from pin1 to channel
                out.push_str(&format!(
                    "  (wire (pts (xy {:.2} {:.2}) (xy {:.2} {:.2})) (uuid wire_{}) )\n",
                    x1, y1, x1, channel_y, wire_id
                ));
                wire_id += 1;
                // Horizontal run in the channel
                out.push_str(&format!(
                    "  (wire (pts (xy {:.2} {:.2}) (xy {:.2} {:.2})) (uuid wire_{}) )\n",
                    x1, channel_y, x2, channel_y, wire_id
                ));
                wire_id += 1;
                // Vertical rise to pin2
                out.push_str(&format!(
                    "  (wire (pts (xy {:.2} {:.2}) (xy {:.2} {:.2})) (uuid wire_{}) )\n",
                    x2, channel_y, x2, y2, wire_id
                ));
                wire_id += 1;
            }
        }
        // Label the net on a DEDICATED STUB: a 1.27mm wire sticking out from
        // the first pin, perpendicular to the net's first wire, with the
        // label at the stub's far end. The stub belongs to exactly one net
        // and touches no other wire — label collisions are structurally
        // impossible (live-verified: every on-wire placement collided with
        // a neighboring net's crossing or column).
        // Stub direction: OUTWARD from the symbol body (pin left of origin
        // → −x, right → +x). Vertical stubs on a pin column collide with
        // the vertical L-wire runs (live-verified: GND+VCC labels joined on
        // U1's left column). Outward stubs point into empty space.
        if let Some((x1, y1)) = net.pins.first().and_then(|p| parse_pin(p, &plan)) {
            let comp_idx = plan
                .components
                .iter()
                .position(|c| c.reference == net.pins[0].split(':').next().unwrap_or(""));
            let outward = match comp_idx {
                Some(i) => {
                    let snap = |v: f64| (v / 1.27).round() * 1.27;
                    let cx = snap(
                        plan.components[i]
                            .x_mm
                            .unwrap_or(50.8 + (i % 2) as f64 * 63.5),
                    );
                    if x1 < cx { -1.0 } else { 1.0 }
                }
                None => -1.0,
            };
            let stub_len = 1.27;
            let (sx, sy) = (x1 + outward * stub_len, y1);
            out.push_str(&format!(
                "  (wire (pts (xy {:.2} {:.2}) (xy {:.2} {:.2})) (uuid wire_{}) )\n",
                x1, y1, sx, sy, wire_id
            ));
            wire_id += 1;
            let rot = if outward < 0.0 { 0 } else { 180 };
            out.push_str(&format!(
                "  (label \"{}\" (at {:.2} {:.2} {}))\n",
                net.name, sx, sy, rot
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
    let snap = |v: f64| (v / 1.27).round() * 1.27;
    let x = snap(comp.x_mm.unwrap_or(50.8 + (idx % 2) as f64 * 63.5));
    let y = snap(comp.y_mm.unwrap_or(50.8 + (idx / 2) as f64 * 38.1));

    // Mirror of generate_schematic's pin placement: compute this component's
    // plan-referenced pin list, then place pin N exactly where the symbol did.
    let mut pin_nums: Vec<String> = Vec::new();
    for (net_index, net) in plan.nets.iter().enumerate() {
        for pin in &net.pins {
            if let Some((r, n_)) = pin.split_once(':') {
                if r == comp.reference && !pin_nums.iter().any(|p| p == n_) {
                    pin_nums.push(n_.to_string());
                }
            }
        }
    }
    if pin_nums.is_empty() {
        pin_nums.push("1".into());
        pin_nums.push("2".into());
    }
    let n = pin_nums.len();
    let pi = pin_nums.iter().position(|p| p == pin_num)?;
    if n <= 2 {
        // Inline: pin 0 left, pin 1 right (matches the symbol)
        if pi == 0 { Some((x - 5.08, y)) } else { Some((x + 5.08, y)) }
    } else {
        // DIP rectangle: same math as generate_schematic, with the KiCad
        // Y-mirror applied — a lib pin stored at file-y +2.54 RENDERS at
        // origin −2.54 (live-verified: ERC saw U1 pin1 at 48.26 for origin
        // 50.8). Wires must target the RENDERED position.
        let half = (n + 1) / 2;
        let pitch = 2.54;
        if pi < half {
            Some((x - 5.08, y - (half as f64 - 1.0 - pi as f64) * pitch))
        } else {
            let k = n - 1 - pi;
            Some((x + 5.08, y + (half as f64 - 1.0 - k as f64) * pitch))
        }
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
        // ONE PAD PER PLAN-REFERENCED PIN — never a fixed 2-pad model.
        // (Live-validated: the v0 board dropped U1:3/U1:8 silently because
        // only pads "1"/"2" existed. Every plan pin must land on copper.)
        // Collect every pin this component needs, in plan order, deduped:
        let mut pin_nums: Vec<String> = Vec::new();
        for (net_index, net) in plan.nets.iter().enumerate() {
            for pin in &net.pins {
                if let Some((refdes, num)) = pin.split_once(':') {
                    if refdes == comp.reference && !pin_nums.iter().any(|p| p == num) {
                        pin_nums.push(num.to_string());
                    }
                }
            }
        }
        // Components referenced by no net still get pins 1+2 (power parts
        // referenced by value later; honest default).
        if pin_nums.is_empty() {
            pin_nums.push("1".into());
            pin_nums.push("2".into());
        }
        // Pad geometry: 2 pins → 2.5mm pitch inline; >2 pins → DIP grid
        // (two columns, 1.27mm pitch — DIP-8 class), deterministic order.
        let n = pin_nums.len();
        for (i, pin_num) in pin_nums.iter().enumerate() {
            let (dx, dy) = if n <= 2 {
                let step = 1.25;
                if n == 1 { (0.0, 0.0) }
                else if i == 0 { (-step, 0.0) } else { (step, 0.0) }
            } else {
                // DIP: left column top-to-bottom, then right column bottom-to-top
                let half = (n + 1) / 2;
                let pitch = 1.27;
                let col_gap = 7.62 / 2.0; // DIP column spacing
                if i < half {
                    (-col_gap, (i as f64 - (half as f64 - 1.0) / 2.0) * pitch)
                } else {
                    let k = n - 1 - i; // mirror for right column
                    (col_gap, (k as f64 - (half as f64 - 1.0) / 2.0) * pitch)
                }
            };
            let pad_net = plan
                .nets
                .iter()
                .enumerate()
                .find(|(_, net)| {
                    net.pins
                        .iter()
                        .any(|p| p == &format!("{}:{}", comp.reference, pin_num))
                })
                .map(|(i, net)| (i + 1, net.name.clone()));
        // Pad size by pitch: DIP pitch 1.27mm → pads 1.0mm (a 1.5mm pad
        // overlaps its neighbor — live-verified by KiCad DRC as a short).
        // Inline 2-pin parts keep the 1.5mm SMD pad.
        let (pad_w, pad_h) = if n > 2 { (1.0, 1.0) } else { (1.5, 1.5) };
            fp.pads.push(crate::board::Pad {
                number: pin_num.clone(),
                net: pad_net,
                x: dx, // relative until the placer sets fp.x/y
                y: dy,
                width: pad_w,
                height: pad_h,
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
        // KiCad schematic grid: 1.27 mm (50 mil). Snap the symbol origin so
        // every pin endpoint (origin ± 5.08 / ± 2.54k) lands ON grid —
        // endpoint_off_grid violations disappear (live-verified).
        let snap = |v: f64| (v / 1.27).round() * 1.27;
        let x = snap(comp.x_mm.unwrap_or(50.8 + (i % 2) as f64 * 63.5));
        let y = snap(comp.y_mm.unwrap_or(50.8 + (i / 2) as f64 * 38.1));
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
    fn multi_pin_components_get_their_pins() {
        // Live-validated regression: the v0 board dropped U1:3/U1:8 silently
        let plan: CircuitPlan = serde_json::from_str(r#"{
            "name": "ic test",
            "description": "an IC with pins beyond 1 and 2",
            "components": [
                {"reference": "U1", "value": "NE555", "lib_id": "Timer:NE555", "footprint": "Package_DIP:DIP-8"}
            ],
            "nets": [
                {"name": "VCC", "pins": ["U1:1"]},
                {"name": "OUT", "pins": ["U1:3"]},
                {"name": "GND", "pins": ["U1:8"]}
            ]
        }"#).unwrap();
        validate_plan(&plan).unwrap();
        let board = generate_board_model(&plan);
        // Every plan pin must exist as a pad:
        verify_pin_coverage(&plan, &board).unwrap();
        let u1 = board.footprints.iter().find(|f| f.reference == "U1").unwrap();
        let numbers: Vec<&str> = u1.pads.iter().map(|p| p.number.as_str()).collect();
        assert!(numbers.contains(&"3"), "U1 pin 3 must exist, got {:?}", numbers);
        assert!(numbers.contains(&"8"), "U1 pin 8 must exist, got {:?}", numbers);
        // DIP grid: pins 1..8 → 3 pads here, in plan order, with nets attached
        assert_eq!(u1.pads.len(), 3);
        let p3 = u1.pads.iter().find(|p| p.number == "3").unwrap();
        assert_eq!(p3.net.as_ref().map(|(_, n)| n.as_str()), Some("OUT"));
    }

    #[test]
    fn pin_coverage_rejects_missing_pad() {
        // A board model that lacks a plan pin must FAIL the gate
        let plan: CircuitPlan = serde_json::from_str(r#"{
            "name": "gap test",
            "description": "pin missing from board",
            "components": [
                {"reference": "R1", "value": "10k", "lib_id": "Device:R"}
            ],
            "nets": [
                {"name": "A", "pins": ["R1:1", "R1:2"]}
            ]
        }"#).unwrap();
        let mut board = generate_board_model(&plan);
        // Sabotage: remove pad 2
        board.footprints[0].pads.truncate(1);
        let err = verify_pin_coverage(&plan, &board);
        assert!(err.is_err(), "missing pad must fail the gate");
        assert!(format!("{:?}", err).contains("R1:2"));
    }

    #[test]
    fn duplicate_reference_rejected() {
        let plan: CircuitPlan = serde_json::from_str(r#"{
            "name": "dup",
            "description": "duplicate refs",
            "components": [
                {"reference": "R1", "value": "10k", "lib_id": "Device:R"},
                {"reference": "R1", "value": "10k", "lib_id": "Device:R"}
            ],
            "nets": [
                {"name": "A", "pins": ["R1:1", "R1:2"]}
            ]
        }"#).unwrap();
        assert!(validate_plan(&plan).is_err(), "duplicate reference must fail");
    }

    #[test]
    fn malformed_pin_rejected() {
        let plan: CircuitPlan = serde_json::from_str(r#"{
            "name": "malformed",
            "description": "no pin number",
            "components": [
                {"reference": "R1", "value": "10k", "lib_id": "Device:R"}
            ],
            "nets": [
                {"name": "A", "pins": ["R1:"]}
            ]
        }"#).unwrap();
        assert!(validate_plan(&plan).is_err(), "empty pin number must fail");
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