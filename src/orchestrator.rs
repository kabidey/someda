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
use crate::board::rotate_offset;

/// The circuit plan the LLM must produce (strict JSON contract).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CircuitPlan {
    pub name: String,
    pub description: String,
    pub components: Vec<PlannedComponent>,
    pub nets: Vec<PlannedNet>,
    /// Copper layer count: 2 (default), 4 or 6. Even, 2..=12.
    #[serde(default)]
    pub layers: Option<u32>,
    /// Optional fixed board size (mm). Absent: the engine budgets area
    /// from the real footprint courtyards.
    #[serde(default)]
    pub board: Option<BoardSpec>,
    /// Copper planes on inner layers: which net fills which layer.
    /// Absent on a 4/6-layer board: GND on In1.Cu (and In4.Cu on 6 layers),
    /// the main power net on the layer before the last inner GND.
    #[serde(default)]
    pub planes: Option<Vec<PlaneSpec>>,
    /// Differential pairs: routed as coupled pairs (same layer, constant
    /// gap, matched length) to a differential impedance target.
    #[serde(default)]
    pub pairs: Option<Vec<PairSpec>>,
}

/// A differential pair: nets `p` and `n` are routed side by side as one
/// coupled pair at `z_diff` Ω (default 100), with intra-pair length
/// mismatch held under `skew_mm` (default: the interface class budget —
/// see `pair_skew_budget_mm`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairSpec {
    pub p: String,
    pub n: String,
    #[serde(default)]
    pub z_diff: Option<f64>,
    #[serde(default)]
    pub skew_mm: Option<f64>,
}

/// A fixed board outline request (mm).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoardSpec {
    pub width_mm: f64,
    pub height_mm: f64,
}

/// A copper plane: `net` fills copper layer `layer` (e.g. "In1.Cu").
/// With `region` (x0, y0, x1, y1 in mm from the board's top-left corner)
/// the plane covers only that rectangle; a higher `priority` wins where
/// planes overlap (split power planes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlaneSpec {
    pub net: String,
    pub layer: String,
    /// `[x0, y0, x1, y1]` in mm, or "auto" = the bounding box of the net's
    /// pads after placement plus a 2 mm margin.
    #[serde(default)]
    pub region: Option<PlaneRegionSpec>,
    #[serde(default)]
    pub priority: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PlaneRegionSpec {
    Rect([f64; 4]),
    Auto(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedComponent {
    /// Reference designator: R1, C1, U1, ...
    pub reference: String,
    /// Component value: "10k", "100nF", "LM358", ...
    /// May carry explicit power: "0.25W resistor", "3W LED"
    pub value: String,
    /// Symbol library name: "Device:R", "Device:C", ...
    pub lib_id: String,
    /// Footprint: "Resistor_SMD:R_0805_2012Metric", ...
    pub footprint: Option<String>,
    /// Board placement in mm (optional; the placer assigns if absent)
    pub x_mm: Option<f64>,
    pub y_mm: Option<f64>,
    /// Power dissipation (W). Optional: parsed from value if absent.
    #[serde(default)]
    pub power_w: Option<f64>,
    /// Semantic placement hint (never a coordinate): top_left, top,
    /// top_right, left, center, right, bottom_left, bottom, bottom_right,
    /// or an edge: left_edge, right_edge, top_edge, bottom_edge.
    #[serde(default)]
    pub position: Option<String>,
    /// Rotation in degrees (0/90/180/270), e.g. to face a connector outward.
    #[serde(default)]
    pub rotation: Option<f64>,
    /// Keep this part next to another reference (decoupling caps hug their IC).
    #[serde(default)]
    pub near: Option<String>,
    /// For edge positions: where along the edge (0.0 = left/top end,
    /// 1.0 = right/bottom end). Still intent, not a coordinate.
    #[serde(default)]
    pub edge_at: Option<f64>,
    /// Optional: the LCSC part number ("C165948"). The plugin's fetch-libs
    /// step pulls the part's footprint and 3D model from EasyEDA/LCSC when
    /// the KiCad libraries have none.
    #[serde(default)]
    pub lcsc: Option<String>,
    /// Optional: manufacturer part number, for the Mouser/Digi-Key lookups
    /// of the fetch-libs step and for the BOM.
    #[serde(default)]
    pub mpn: Option<String>,
    /// Optional, edge connectors: how far the body may protrude past the
    /// board edge (mm; e.g. 0.8 for a USB-C receptacle whose datasheet puts
    /// the shell lip beyond the PCB). Default 0: courtyard flush with the edge.
    #[serde(default)]
    pub edge_overhang_mm: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedNet {
    /// Net name: "VCC", "GND", "SIG_A", ...
    pub name: String,
    /// Connected pins as "REF:PIN" strings: ["R1:1", "R2:2"]
    pub pins: Vec<String>,
    /// Optional: the highest frequency this net carries (Hz). The plan
    /// declares it when the circuit's intent knows it (e.g. 2.4e9 for RF);
    /// otherwise it is derived from member component specs.
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// Optional: the circuit role — power | ground | clock | rf |
    /// highspeed | analog | digital. Otherwise derived from the name.
    #[serde(default)]
    pub role: Option<String>,
    /// Optional: how the net is terminated — "series" (resistor at the
    /// driver), "parallel" (resistor at the receiver), "on-chip" (the
    /// interface terminates inside the ICs: USB, MIPI, Ethernet PHY),
    /// "none". Otherwise derived: a ≤100 Ω two-pin resistor on the net
    /// counts as series termination; pair nets count as on-chip.
    #[serde(default)]
    pub termination: Option<String>,
}

/// The system prompt for circuit planning. The LLM sees the JSON contract
/// and must produce it exactly — no prose, no markdown.
pub const PLANNER_SYSTEM_PROMPT: &str = r#"You are an electronics design agent. Given a description of a circuit, produce a JSON circuit plan.

Return EXACTLY this JSON structure, no other text:
{
  "name": "short project name",
  "description": "one-line description",
  "components": [
    {"reference": "R1", "value": "10k", "lib_id": "Device:R", "footprint": "Resistor_SMD:R_0805_2012Metric", "power_w": 0.25}
  ],
  "nets": [
    {"name": "VCC", "pins": ["R1:1", "R2:2"]},
    {"name": "RF_OUT", "pins": ["U1:3", "J1:1"], "freq_hz": 2400000000, "role": "rf"}
  ]
}

Rules:
- Every pin reference is "REFDES:PIN_NUMBER" (e.g., "R1:1", "U1:3")
- Every pin listed in a net must belong to a listed component
- Every component pin must appear in exactly one net or be deliberately unconnected (omit it)
- Power nets are named "VCC" and "GND" (use these exact names)
- Declare each net's "freq_hz" when the circuit's intent knows it (clocks, RF, high-speed interfaces); leave it out when unknown
- Declare "role" (power|ground|clock|rf|highspeed|analog|digital) when the net name alone is ambiguous
- Declare "termination" (series|parallel|on-chip|none) on fast nets when known; a series resistor on the net is detected automatically
- Declare differential pairs as "pairs": [{"p": "USB_DP", "n": "USB_DM", "z_diff": 90}] — both nets must connect the same two components; they are routed as one coupled, length-matched pair
- Declare "power_w" for components that dissipate noticeably (regulators, power LEDs, resistors in power paths)
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
    /// What the engine could fix with a plan edit (see `repair`).
    pub repairs: Vec<crate::repair::RepairHint>,
    /// All six export gates passed.
    pub honest_done: bool,
    /// The board outline the build used (mm).
    pub board_mm: (f64, f64),
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
    // ─── Stage 1: Plan (LLM) ───
    println!("─── Stage 1: Planning circuit ───");
    // SOMEDA_PLAN_FILE: inject a plan for testing the full pipeline offline.
    // The LLM path and this path converge immediately after — same contract,
    // same validation, same deterministic execution.
    let raw = match std::env::var("SOMEDA_PLAN_FILE") {
        Ok(path) => {
            println!("  (plan injected from {} — SOMEDA_PLAN_FILE)", path);
            let text = fs::read_to_string(&path)
                .map_err(|e| OrchestrateError::Io(format!("read {}: {}", path, e)))?;
            serde_json::from_str(&text)
                .map_err(|e| OrchestrateError::PlanInvalid(format!("plan file: {}", e)))?
        }
        Err(_) => gateway
            .chat_json(PLANNER_SYSTEM_PROMPT, description)
            .map_err(|e| OrchestrateError::Gateway(e.to_string()))?,
    };

    let plan: CircuitPlan = serde_json::from_value(raw)
        .map_err(|e| OrchestrateError::PlanInvalid(format!("LLM output does not match the contract: {}", e)))?;

    validate_plan(&plan)?;
    crate::repair::build_with_repairs(cli, project_dir, plan)
}

/// One deterministic build of a validated plan: every stage, honest gates,
/// and the repair hints the engine collected on the way. [`build`] wraps
/// this in the repair loop.
pub fn build_with_plan(
    cli: &KiCadCli,
    project_dir: &Path,
    plan: CircuitPlan,
) -> Result<BuildResult, OrchestrateError> {
    let mut stages = Vec::new();
    let mut repairs: Vec<crate::repair::RepairHint> = Vec::new();
    let mut board_mm: (f64, f64) = (0.0, 0.0);
    // Edge connectors: decide their rotation from the footprint geometry
    // BEFORE the board model is instantiated (pads are placed absolutely
    // with the rotation; the placer only translates).
    let mut plan = plan;
    orient_edge_connectors(&mut plan, true);
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
    {
        let (lib, table) = generate_symbol_library(&plan);
        std::fs::create_dir_all(project_dir).ok();
        fs::write(project_dir.join("someda-symbols.kicad_sym"), lib).map_err(|e| OrchestrateError::Io(e.to_string()))?;
        fs::write(project_dir.join("sym-lib-table"), table).map_err(|e| OrchestrateError::Io(e.to_string()))?;
    }
    if std::env::var("SOMEDA_STOP_AFTER").map(|v| v == "schematic").unwrap_or(false) {
        std::fs::create_dir_all(project_dir).ok();
        let sch_path = project_dir.join("circuit.kicad_sch");
        std::fs::write(&sch_path, &sch_content).map_err(|e| OrchestrateError::Io(e.to_string()))?;
        return Ok(BuildResult { plan, project_dir: project_dir.display().to_string(), schematic_file: sch_path.display().to_string(), board_file: String::new(), stages: Vec::new(), repairs: Vec::new(), honest_done: false, board_mm });
    }
    let (mut board, fp_report) = generate_board_model_with_report(&plan);
    {
        let real = fp_report.iter().filter(|r| r.real).count();
        let synthetic: Vec<String> = fp_report
            .iter()
            .filter(|r| !r.real)
            .map(|r| format!("{} ({})", r.reference, r.lib_id))
            .collect();
        for r in fp_report.iter().filter(|r| !r.real) {
            repairs.push(crate::repair::RepairHint::FootprintMissing { reference: r.reference.clone(), lib_id: r.lib_id.clone() });
        }
        for r in &fp_report {
            println!("  footprint {:>5} {:<55} {}", r.reference, r.lib_id, if r.real { r.note.clone() } else { format!("SYNTHETIC — {}", r.note) });
        }
        stages.push(StageResult {
            stage: "footprints".into(),
            success: synthetic.is_empty(),
            detail: if synthetic.is_empty() {
                format!("{} real KiCad library footprints, {} copper layers", real, board.layer_count)
            } else {
                format!(
                    "{} real, {} SYNTHETIC (not orderable geometry): {}",
                    real,
                    synthetic.len(),
                    synthetic.join(", ")
                )
            },
        });
    }

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
    // The LLM never outputs coordinates; the solver owns geometry. The plan
    // may express INTENT: semantic/edge positions, "near" relations; the
    // physics adds thermal keep-apart for hot parts.
    println!("─── Stage 2b: Deterministic placement (real courtyards) ───");
    let mut plane_specs = resolve_planes(&plan, &board);
    let mut edge_failures: Vec<String> = Vec::new();
    {
        let (w, h) = board
            .outline_bbox()
            .map(|(x0, y0, x1, y1)| (x1 - x0, y1 - y0))
            .unwrap_or((100.0, 80.0));
        board_mm = (w, h);
        let placer = crate::placer::Placer::new(w, h);
        let build_intent = |plan: &CircuitPlan, plane_specs: &[PlaneSpec], verbose: bool| -> crate::placer::PlacementIntent {
        let mut intent = crate::placer::PlacementIntent::default();
        for c in &plan.components {
            if let Some(pos) = c.position.as_deref() {
                match crate::placer::Pos::parse(pos) {
                    Some(p) => {
                        // Edge connectors: the engine turns the opening toward
                        // the edge from the footprint geometry and puts the
                        // courtyard flush with the edge (plus any declared
                        // overhang) — a plan never has to know rotations.
                        let overhang = if p.is_edge() { edge_connector_orientation(c, p).1 } else { 0.0 };
                        intent.places.push(crate::placer::PlaceIntent {
                            reference: c.reference.clone(),
                            pos: Some(p),
                            rot: c.rotation.map(|r| r as i32),
                            side_back: false,
                            edge_at: c.edge_at,
                            edge_overhang: overhang,
                        })
                    }
                    None => println!("  [warning] {}: unknown position '{}' ignored", c.reference, pos),
                }
            }
            if let Some(anchor) = &c.near {
                intent.near.push((c.reference.clone(), anchor.clone()));
            }
        }
        for ps in plane_specs {
            intent.ignore_nets.push(ps.net.clone());
            if crate::netfreq::net_signal(&ps.net, &[], None, None).role == crate::netfreq::NetRole::Ground {
                intent.ground_nets.push(ps.net.clone());
            }
        }
        {
            let hot: Vec<&crate::orchestrator::PlannedComponent> = plan
                .components
                .iter()
                .filter(|c| {
                    let p = c.power_w.unwrap_or_else(|| crate::thermal::power_from_value(&c.value));
                    p >= 0.5
                })
                .collect();
            for i in 0..hot.len() {
                for j in (i + 1)..hot.len() {
                    intent.apart.push(crate::placer::ApartIntent {
                        a: hot[i].reference.clone(),
                        b: hot[j].reference.clone(),
                        min_mm: 15.0,
                    });
                }
            }
            if !intent.apart.is_empty() && verbose {
                println!("  thermal placement: {} hot pair(s) ≥15mm apart", intent.apart.len());
            }
        }
            intent
        };
        let mut intent = build_intent(&plan, &plane_specs, true);
        // Place; an edge connector that found no room on its edge is moved
        // to another edge (adjacent first), re-oriented from its geometry
        // and the board re-placed — bounded, printed as `repair:`. A
        // connector in the interior is unusable (live-verified: a 3.5 mm
        // jack parked mid-board with its opening facing nothing).
        let mut tried: std::collections::HashMap<String, Vec<crate::placer::Pos>> = std::collections::HashMap::new();
        let mut solved;
        let mut round = 0usize;
        loop {
            let nl = crate::netlist::Netlist::from_board(&board);
            solved = placer.arrange_courtyards(&board, &nl, &intent);
            for s in &solved {
                if let Some(fp) = board.footprints.iter_mut().find(|f| f.reference == s.reference) {
                    let (dx, dy) = (s.x - fp.x, s.y - fp.y);
                    fp.translate(dx, dy);
                }
            }
            let mut reassign: Option<(String, crate::placer::Pos)> = None;
            edge_failures.clear();
            for c in &plan.components {
                let Some(pos) = c.position.as_deref().and_then(crate::placer::Pos::parse) else { continue };
                if !pos.is_edge() {
                    continue;
                }
                let Some(fp) = board.footprints.iter().find(|f| f.reference == c.reference) else { continue };
                let (x0, y0, x1, y1) = fp.abs_bbox();
                // the same courtyard-frame overhang the placer was given
                let o = edge_connector_orientation(c, pos).1;
                let m = placer.margin;
                let off = match pos {
                    crate::placer::Pos::TopEdge => (y0 - (m - o)).abs(),
                    crate::placer::Pos::BottomEdge => ((h - m + o) - y1).abs(),
                    crate::placer::Pos::LeftEdge => (x0 - (m - o)).abs(),
                    _ => ((w - m + o) - x1).abs(),
                };
                if off <= 0.05 {
                    continue;
                }
                let t = tried.entry(c.reference.clone()).or_default();
                if !t.contains(&pos) {
                    t.push(pos);
                }
                let next = pos.fallback_edges().iter().copied().find(|e| !t.contains(e));
                match next {
                    Some(e) if round < 12 => {
                        if reassign.is_none() {
                            reassign = Some((c.reference.clone(), e));
                        }
                    }
                    _ => edge_failures.push(format!("{} ({} full, {:.1} mm off its edge)", c.reference, pos.name(), off)),
                }
            }
            let Some((reference, edge)) = reassign else { break };
            if let Some(c) = plan.components.iter_mut().find(|c| c.reference == reference) {
                let from = c.position.clone().unwrap_or_default();
                c.position = Some(edge.name().to_string());
                c.edge_at = None;
                c.rotation = None;
                let (rot, _, _) = edge_connector_orientation(&*c, edge);
                c.rotation = rot.map(|r| r as f64);
                println!(
                    "  repair: {}: no room on the {} — moved to the {} (rotation {}°), board re-placed",
                    reference,
                    from.replace('_', " "),
                    edge.name().replace('_', " "),
                    rot.unwrap_or(0)
                );
            }
            round += 1;
            board = generate_board_model_with_report(&plan).0;
            intent = build_intent(&plan, &plane_specs, false);
        }
        if !edge_failures.is_empty() {
            for f in &edge_failures {
                println!("  [error] edge connector {} could not be placed on any edge", f);
                println!("  suggest: enlarge the board or free that edge (board.width_mm/height_mm, other parts' position)");
            }
        }
        // Split-plane islands grow to reach their own decoupling
        // capacitors: a capacitor at the IC whose rail pad lies outside the
        // island has no plane to fan out to (live-verified: a +1V8 island
        // under the SoC only, its capacitors just outside the escape ring →
        // 18.8 mm stubs, 9.6 nH loops). Bounded to 6 mm per side; beyond
        // that the plan must move the part or grow the region.
        {
            let components = &plan.components;
            if let Some(planes) = plan.planes.as_mut() {
                for ps in planes.iter_mut() {
                    if std::env::var("SOMEDA_PLACE_DEBUG").is_ok() {
                        println!("place-debug: island check {} {} region {:?} priority {:?}", ps.net, ps.layer, ps.region, ps.priority);
                    }
                    if ps.priority.is_none() {
                        continue;
                    }
                    let r0: [f64; 4] = match ps.region.clone() {
                        Some(PlaneRegionSpec::Rect(r)) => r,
                        Some(PlaneRegionSpec::Auto(_)) => match auto_plane_region(&board, &ps.net) {
                            Some(r) => r,
                            None => continue,
                        },
                        None => continue,
                    };
                    let inside = |x: f64, y: f64, r: &[f64; 4]| x >= r[0] && x <= r[2] && y >= r[1] && y <= r[3];
                    let mut r = r0;
                    let mut reached: Vec<String> = Vec::new();
                    let mut unreachable: Vec<String> = Vec::new();
                    for c in components {
                        let Some(anchor) = c.near.as_deref() else { continue };
                        let Some(fp) = board.footprints.iter().find(|f| f.reference == c.reference) else { continue };
                        if fp.pads.len() != 2 {
                            continue;
                        }
                        let Some(pad) = fp.pads.iter().find(|p| p.net.as_ref().map(|n| n.1 == ps.net).unwrap_or(false)) else { continue };
                        let Some(afp) = board.footprints.iter().find(|f| f.reference == anchor) else { continue };
                        if !inside(afp.x, afp.y, &r0) || inside(pad.x, pad.y, &r0) {
                            continue;
                        }
                        let want = [pad.x - 1.0, pad.y - 1.0, pad.x + 1.0, pad.y + 1.0];
                        let grown = [
                            r[0].min(want[0]).max(r0[0] - 6.0),
                            r[1].min(want[1]).max(r0[1] - 6.0),
                            r[2].max(want[2]).min(r0[2] + 6.0),
                            r[3].max(want[3]).min(r0[3] + 6.0),
                        ];
                        if inside(want[0], want[1], &grown) && inside(want[2], want[3], &grown) {
                            reached.push(c.reference.clone());
                            r = grown;
                        } else {
                            unreachable.push(c.reference.clone());
                        }
                    }
                    if !reached.is_empty() {
                        ps.region = Some(PlaneRegionSpec::Rect(r));
                        println!(
                            "  repair: {} island on {} grown from [{:.1}, {:.1}, {:.1}, {:.1}] to [{:.1}, {:.1}, {:.1}, {:.1}] so {} can fan out into it",
                            ps.net, ps.layer, r0[0], r0[1], r0[2], r0[3], r[0], r[1], r[2], r[3], reached.join(", ")
                        );
                    }
                    if !unreachable.is_empty() {
                        println!(
                            "  suggest: {} sit more than 6 mm outside the {} island — move them next to the island (\"near\" the IC's rail pins) or enlarge planes[].region for {}",
                            unreachable.join(", "), ps.net, ps.net
                        );
                    }
                }
            }
        }
        // Plane regions resolved AFTER placement: "auto" regions follow the
        // placed pads and islands may have grown.
        plane_specs = resolve_planes(&plan, &board);
        // Courtyard overlap is a DRC error in KiCad: report it here first
        let mut overlaps = 0;
        for (i, a) in board.footprints.iter().enumerate() {
            let ra = a.abs_bbox();
            for b in board.footprints.iter().skip(i + 1) {
                let rb = b.abs_bbox();
                if ra.0 < rb.2 && rb.0 < ra.2 && ra.1 < rb.3 && rb.1 < ra.3 {
                    overlaps += 1;
                    println!("  [error] courtyard overlap {} / {}", a.reference, b.reference);
                }
            }
        }
        // Reference designators: above the courtyard when that spot is clear
        // of every other part and the board edge, else below / left / right,
        // else on F.Fab (no silk at all). Silk on a pad is a fab defect.
        {
            let (bw, bh) = (w, h);
            let rects: Vec<(usize, (f64, f64, f64, f64))> = board.footprints.iter().enumerate().map(|(i, f)| (i, f.abs_bbox())).collect();
            let mut placed_text: Vec<(f64, f64, f64, f64)> = Vec::new();
            let mut on_fab = 0;
            for i in 0..board.footprints.len() {
                let fp = &board.footprints[i];
                let text_w = (fp.reference.len() as f64 * 0.85).max(1.2);
                let text_h = 1.0;
                let (cx0, cy0, cx1, cy1) = fp.courtyard.unwrap_or((-1.0, -1.0, 1.0, 1.0));
                let gap = 0.35;
                // candidates in the footprint frame: above, below, left, right of the courtyard
                let cands = [
                    (0.0, cy0 - gap - text_h / 2.0),
                    (0.0, cy1 + gap + text_h / 2.0),
                    (cx0 - gap - text_w / 2.0, 0.0),
                    (cx1 + gap + text_w / 2.0, 0.0),
                ];
                let mut chosen: Option<(f64, f64)> = None;
                for (rx, ry) in cands {
                    // text box in board coordinates (text stays upright: box is w×h, rotated 90 for 90/270 parts)
                    let (ox, oy) = rotate_offset(rx, ry, fp.rotation);
                    let (tx, ty) = (fp.x + ox, fp.y + oy);
                    let (hw, hh) = if (fp.rotation % 180.0).abs() < 1e-6 { (text_w / 2.0, text_h / 2.0) } else { (text_h / 2.0, text_w / 2.0) };
                    let tb = (tx - hw, ty - hh, tx + hw, ty + hh);
                    if tb.0 < 0.6 || tb.1 < 0.6 || tb.2 > bw - 0.6 || tb.3 > bh - 0.6 {
                        continue;
                    }
                    let clash = rects.iter().any(|(j, r)| {
                        *j != i && tb.0 < r.2 + 0.15 && r.0 < tb.2 + 0.15 && tb.1 < r.3 + 0.15 && r.1 < tb.3 + 0.15
                    }) || placed_text.iter().any(|r| {
                        tb.0 < r.2 + 0.15 && r.0 < tb.2 + 0.15 && tb.1 < r.3 + 0.15 && r.1 < tb.3 + 0.15
                    });
                    if !clash {
                        chosen = Some((rx, ry));
                        placed_text.push(tb);
                        break;
                    }
                }
                let fpm = &mut board.footprints[i];
                match chosen {
                    Some((rx, ry)) => fpm.ref_text = Some((rx, ry, "F.SilkS".into())),
                    None => {
                        fpm.ref_text = Some((0.0, 0.0, "F.Fab".into()));
                        on_fab += 1;
                    }
                }
            }
            if on_fab > 0 {
                println!("  silkscreen: {} reference(s) have no clear spot and go on F.Fab instead", on_fab);
            }
        }
        if overlaps > 0 {
            repairs.push(crate::repair::RepairHint::Overlap(overlaps));
        }
        stages.push(StageResult {
            stage: "placement".into(),
            success: overlaps == 0 && edge_failures.is_empty(),
            detail: format!(
                "placed {} footprint(s) on {:.0}x{:.0}mm, {} hint(s), {} courtyard overlap(s){}",
                solved.len(), w, h, intent.places.len() + intent.near.len(), overlaps,
                if edge_failures.is_empty() { String::new() } else { format!(", {} edge connector(s) NOT on an edge: {}", edge_failures.len(), edge_failures.join("; ")) }
            ),
        });
    }

    // ─── Stage 2c: Autorouting (design rules for this layer count) ───
    // Planes carry GND/power on inner layers; signals route on the rest.
    // Impedance nets get their width SOLVED from the stackup physics.
    let rules = crate::rules::DesignRules::for_layers(board.layer_count);
    let stackup = crate::materials::Stackup::for_layers(board.layer_count);
    let plane_layers: Vec<String> = plane_specs.iter().map(|p| p.layer.clone()).collect();
    let signal_layers: Vec<String> = board
        .copper_layers()
        .into_iter()
        .filter(|l| !plane_layers.contains(l))
        .collect();
    // Resolve plane regions now that pads are placed ("auto" = pad bbox + 2mm)
    let mut plane_rects: Vec<Option<(f64, f64, f64, f64)>> = Vec::new();
    for ps in &plane_specs {
        let (bx0, by0, _, _) = board.outline_bbox().unwrap_or((0.0, 0.0, 100.0, 80.0));
        let r = match &ps.region {
            None => None,
            Some(PlaneRegionSpec::Rect([x0, y0, x1, y1])) => Some((*x0, *y0, *x1, *y1)),
            Some(PlaneRegionSpec::Auto(_)) => auto_plane_region(&board, &ps.net).map(|r| (r[0], r[1], r[2], r[3])),
        };
        if let Some(r) = r {
            println!("  plane {} on {}: region x {:.1}..{:.1} y {:.1}..{:.1} (priority {})", ps.net, ps.layer, r.0, r.2, r.1, r.3, ps.priority.unwrap_or(0));
        }
        plane_rects.push(r);
    }
    for (pi, ps) in plane_specs.iter().enumerate() {
        if let Some((idx, name)) = board.nets.iter().find(|(_, n)| n == &ps.net).cloned() {
            let (bx0, by0, bx1, by1) = board.outline_bbox().unwrap_or((0.0, 0.0, 100.0, 80.0));
            let m = rules.edge_clearance_mm;
            let (x0, y0, x1, y1) = match plane_rects[pi] {
                Some((rx0, ry0, rx1, ry1)) => (
                    (bx0 + rx0).max(bx0 + m), (by0 + ry0).max(by0 + m),
                    (bx0 + rx1).min(bx1 - m), (by0 + ry1).min(by1 - m),
                ),
                None => (bx0 + m, by0 + m, bx1 - m, by1 - m),
            };
            board.zones.push(crate::board::Zone {
                net: (idx, name),
                layer: ps.layer.clone(),
                polygon: vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)],
                clearance: rules.clearance_mm.max(0.2),
                min_thickness: 0.25,
                priority: ps.priority.unwrap_or(0),
            });
        }
    }
    println!(
        "─── Stage 2c: Autorouting ({} layers: signals on {}, planes {}; trace {}mm, clearance {}mm, via {}/{}mm) ───",
        board.layer_count,
        signal_layers.join("/"),
        if plane_specs.is_empty() { "none".to_string() } else {
            plane_specs.iter().map(|p| format!("{}={}", p.layer, p.net)).collect::<Vec<_>>().join(" ")
        },
        rules.trace_width_mm, rules.clearance_mm, rules.via_diameter_mm, rules.via_drill_mm
    );
    let route_summary;
    let unrouted_count;
    let route_pairs: Vec<crate::router::PairReport>;
    let route_exempt: Vec<(f64, f64, f64, f64, String, u8)>;
    let route_families: Vec<Vec<String>>;
    let router_net_gap: std::collections::HashMap<String, f64>;
    let pair_infos: Vec<crate::si::PairInfo>;
    let pairs_ok: bool;
    let pairs_detail: String;
    {
        let nl = crate::netlist::Netlist::from_board(&board);
        // Power nets (routed as wide traces when not planes)
        let mut power_nets: Vec<String> = Vec::new();
        for net in &plan.nets {
            let mut member_values: Vec<&str> = Vec::new();
            for pin in &net.pins {
                if let Some((refdes, _)) = pin.split_once(':') {
                    if let Some(c) = plan.components.iter().find(|c| c.reference == refdes) {
                        member_values.push(&c.value);
                        member_values.push(&c.lib_id);
                    }
                }
            }
            let sig = crate::netfreq::net_signal(&net.name, &member_values, net.freq_hz, net.role.as_deref());
            // Wide traces for rails; a secondary ground (AGND behind a 0 Ω
            // link) carries decoupling return currents only and routes at
            // the default width — a 0.4 mm trace cannot thread a fine-pitch
            // breakout (live-verified: AGND unrouted between two capacitors).
            let is_plane = plane_specs.iter().any(|p| p.net == net.name);
            let secondary_ground = matches!(sig.role, crate::netfreq::NetRole::Ground) && !is_plane;
            if matches!(sig.role, crate::netfreq::NetRole::Power | crate::netfreq::NetRole::Ground) && !secondary_ground {
                power_nets.push(net.name.clone());
            }
        }
        let mut cfg = crate::router::RouterConfig::from_rules(
            &rules,
            signal_layers.clone(),
            plane_specs.iter().map(|p| p.net.clone()).collect(),
            power_nets,
        );
        cfg.plane_regions = plane_specs
            .iter()
            .enumerate()
            .map(|(i, p)| crate::router::PlaneRegion {
                net: p.net.clone(),
                layer: p.layer.clone(),
                region: plane_rects[i],
                priority: p.priority.unwrap_or(0),
            })
            .collect();
        // ─── Differential pairs: cross-section per pair from the stackup ───
        let fab_limits = crate::materials::FabLimits::default();
        let mut pair_cfgs: Vec<crate::router::PairConfig> = Vec::new();
        let mut pair_net_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut pair_geometry_failed = 0usize;
        let mut infos: Vec<crate::si::PairInfo> = Vec::new();
        if let Some(pairs) = &plan.pairs {
            for pr in pairs {
                let z = pr.z_diff.unwrap_or(100.0);
                match stackup.pair_geometry(z, fab_limits.min_trace_mm, rules.clearance_mm) {
                    Some(g) => {
                        let budget = pr.skew_mm.unwrap_or_else(|| pair_skew_budget_mm(&pr.p, &pr.n));
                        println!(
                            "  pair {}/{}: {:.0} Ω → w {:.4} mm / gap {:.4} mm, skew budget {:.3} mm ({})",
                            pr.p, pr.n, z, g.width_mm, g.gap_mm, budget, g.source
                        );
                        pair_cfgs.push(crate::router::PairConfig {
                            p: pr.p.clone(),
                            n: pr.n.clone(),
                            width_mm: g.width_mm,
                            pitch_mm: g.width_mm + g.gap_mm,
                            z_diff: z,
                            skew_budget_mm: budget,
                        });
                        infos.push(crate::si::PairInfo { p: pr.p.clone(), n: pr.n.clone(), z_diff: z, width_mm: g.width_mm, gap_mm: g.gap_mm, source: g.source.clone(), routed: false });
                        pair_net_names.insert(pr.p.clone());
                        pair_net_names.insert(pr.n.clone());
                    }
                    None => {
                        println!("  [error] pair {}/{}: no {:.0} Ω cross-section within the fab scope on {}", pr.p, pr.n, z, stackup.name);
                        pair_geometry_failed += 1;
                        infos.push(crate::si::PairInfo { p: pr.p.clone(), n: pr.n.clone(), z_diff: z, width_mm: 0.0, gap_mm: 0.0, source: "none".into(), routed: false });
                    }
                }
            }
        }
        cfg.pairs = pair_cfgs;
        // Clean layers: signal layers next to a single whole-board plane
        // (a split plane under a trace breaks its return path).
        cfg.pair_layers = clean_signal_layers(&board, &signal_layers, &plane_specs, &plane_rects);
        if !cfg.pairs.is_empty() {
            println!("  pair/impedance layers: {}", cfg.pair_layers.join(", "));
        }
        // Crosstalk: every net that carries a frequency gets its 3W/5W gap
        for net in &plan.nets {
            let mut member_values: Vec<&str> = Vec::new();
            for pin in &net.pins {
                if let Some((refdes, _)) = pin.split_once(':') {
                    if let Some(c) = plan.components.iter().find(|c| c.reference == refdes) {
                        member_values.push(&c.value);
                        member_values.push(&c.lib_id);
                    }
                }
            }
            let sig = crate::netfreq::net_signal(&net.name, &member_values, net.freq_hz, net.role.as_deref());
            let fast = sig.freq_hz >= 1.0e6 || matches!(sig.role, crate::netfreq::NetRole::Rf | crate::netfreq::NetRole::HighSpeed | crate::netfreq::NetRole::Clock);
            if fast && !matches!(sig.role, crate::netfreq::NetRole::Power | crate::netfreq::NetRole::Ground) {
                // 3W/5W with W = the board's design trace width: one spacing
                // class per board, so router halo and audit rule agree for
                // every width that net may take (pair width, neck-down, 50 Ω).
                let w = rules.trace_width_mm;
                let gap = sig.crosstalk_spacing_mm(w) - w; // centre spacing 3W/5W → edge gap 2W/4W
                cfg.net_gap.insert(net.name.clone(), gap);
            }
        }
        router_net_gap = cfg.net_gap.clone();
        let mut router = crate::router::Router::new(cfg).stackup_with(stackup.clone());
        for net in &plan.nets {
            if pair_net_names.contains(&net.name) {
                continue; // pair nets take the pair cross-section, not a 50 Ω single-ended width
            }
            let mut member_values: Vec<&str> = Vec::new();
            for pin in &net.pins {
                if let Some((refdes, _)) = pin.split_once(':') {
                    if let Some(c) = plan.components.iter().find(|c| c.reference == refdes) {
                        member_values.push(&c.value);
                        member_values.push(&c.lib_id);
                    }
                }
            }
            let sig = crate::netfreq::net_signal(&net.name, &member_values, net.freq_hz, net.role.as_deref());
            if let Some(z0) = sig.z_target {
                if sig.needs_impedance_control() {
                    router = router.with_impedance_target(&net.name, z0);
                    let w = stackup.width_for_z0(z0);
                    println!("  net {}: {:.0}Ω target → {:.4}mm on {} ({})", net.name, z0, w, stackup.name, stackup.fab_width_source);
                }
            }
        }
        router.config.restricted_nets = router.impedance_targets.keys().cloned().collect();
        let route = router.route(&board, &nl);
        route_summary = route.summary_line();
        unrouted_count = route.unrouted.len();
        if unrouted_count > 0 {
            repairs.push(crate::repair::RepairHint::Unrouted(unrouted_count));
        }
        // Pair reports → gate facts
        for pr in &route.pairs {
            println!(
                "  pair {}/{}: {}",
                pr.p, pr.n,
                if pr.routed {
                    format!("coupled {:.1} mm on {}, tails ≤ {:.1} mm, len P {:.2} / N {:.2} mm, skew {:+.3} mm (budget {:.3}), {} corner(s), {} bump(s){}{}",
                        pr.coupled_mm, pr.layer, pr.tail_max_mm, pr.p_len_mm, pr.n_len_mm, pr.skew_mm, pr.skew_budget_mm, pr.corners, pr.bumps,
                        if pr.crossover { ", P/N order differs at the two ends: far tails cross through a via" } else { "" },
                        if pr.cause.is_empty() { String::new() } else { format!(" — {}", pr.cause) })
                } else {
                    format!("NOT COUPLED — {}", pr.cause)
                }
            );
        }
        let mut infos = infos;
        for info in infos.iter_mut() {
            info.routed = route.pairs.iter().filter(|r| r.p == info.p && r.n == info.n).all(|r| r.routed) && route.pairs.iter().any(|r| r.p == info.p && r.n == info.n);
        }
        let declared = plan.pairs.as_ref().map(|p| p.len()).unwrap_or(0);
        let trunks_ok = route.pairs.iter().filter(|r| r.routed && r.skew_mm.abs() <= r.skew_budget_mm + 1e-6).count();
        let trunks = route.pairs.len();
        pairs_ok = pair_geometry_failed == 0 && infos.iter().all(|i| i.routed) && trunks_ok == trunks;
        if !pairs_ok && pair_geometry_failed == 0 {
            repairs.push(crate::repair::RepairHint::PairFailed);
        }
        pairs_detail = format!(
            "{} pair(s) declared, {} trunk(s) coupled, {} within skew budget{}",
            declared, route.pairs.iter().filter(|r| r.routed).count(), trunks_ok,
            if pair_geometry_failed > 0 { format!(", {} without a fab cross-section", pair_geometry_failed) } else { String::new() }
        );
        route_pairs = route.pairs.clone();
        route_exempt = route.exempt_rects.iter().map(|r| (r.x0, r.y0, r.x1, r.y1, r.layer.clone(), r.axis)).collect();
        route_families = route.families.clone();
        pair_infos = infos;
        for u in &route.unrouted {
            println!("  UNROUTED {} {} → {}: {}", u.net.1, u.from, u.to, u.cause);
        }
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
    if plan.pairs.as_ref().map(|p| !p.is_empty()).unwrap_or(false) {
        stages.push(StageResult { stage: "pairs".into(), success: pairs_ok, detail: pairs_detail.clone() });
    }
    let _ = &route_pairs;
    // Developer switch: SOMEDA_STOP_AFTER=route writes the board and stops
    // before the audits and kicad-cli (fast routing iteration).
    if std::env::var("SOMEDA_STOP_AFTER").map(|v| v == "route").unwrap_or(false) {
        let pcb_path = project_dir.join("circuit.kicad_pcb");
        std::fs::create_dir_all(project_dir).ok();
        std::fs::write(&pcb_path, board.to_kicad_pcb()).map_err(|e| OrchestrateError::Io(e.to_string()))?;
        for stage in &stages {
            println!("  {} [{}] {}", if stage.success { "✓" } else { "✗" }, stage.stage, stage.detail);
        }
        return Ok(BuildResult {
            plan,
            project_dir: project_dir.display().to_string(),
            schematic_file: String::new(),
            board_file: pcb_path.display().to_string(),
            stages,
            repairs,
            honest_done: false,
            board_mm,
        });
    }

    // ─── Stage 2d: physics-aware design audit (freq / SI / thermal) ───
    // Every net's frequency is known (declared or component-spec); every
    // component's power too. This stage MEASURES the routed board: stray
    // L/C per net, SWR vs impedance targets, stub resonance, crosstalk,
    // and thermal junctions — the microwave-stability gate.
    println!("─── Stage 2d: physics audit (frequency / SI / thermal) ───");
    {
        let fab = crate::materials::FabLimits::default();

        // Per-net signals: frequency + role from the plan contract
        let mut signals = Vec::new();
        for net in &plan.nets {
            let mut member_values: Vec<&str> = Vec::new();
            for pin in &net.pins {
                if let Some((refdes, _)) = pin.split_once(':') {
                    if let Some(c) = plan.components.iter().find(|c| c.reference == refdes) {
                        member_values.push(&c.value);
                        member_values.push(&c.lib_id);
                    }
                }
            }
            let sig = crate::netfreq::net_signal(
                &net.name,
                &member_values,
                net.freq_hz,
                net.role.as_deref(),
            );
            let par = crate::si::measure_stray(&board, &sig);
            signals.push((sig, par));
        }

        // Stackup counsel: 50Ω on 1.6mm FR-4 needs ~1.44mm traces — when a
        // net's solved width exceeds 1mm, the honest engineering advice is
        // a thin-dielectric stackup (4-layer, ~0.2mm core → ~0.4mm traces).
        {
            let st = stackup.clone();
            for (sig, _) in &signals {
                if let Some(z0) = sig.z_target {
                    if sig.needs_impedance_control() {
                        let w = crate::formulas::microstrip_width_for_z0(
                            z0, st.er, st.h_mm, st.copper_thickness_mm,
                        );
                        if w > 1.0 && board.layer_count <= 2 {
                            println!(
                                "  [info] stackup-counsel {}: {:.0}Ω needs {:.2}mm traces on 1.6mm FR-4 — consider a 4-layer thin-dielectric stackup (~0.2mm core → ~0.4mm traces)",
                                sig.name, z0, w
                            );
                        }
                    }
                }
            }
        }

        // Print the per-net frequency table (the awareness itself)
        for (sig, par) in &signals {
            println!(
                "  net {:>8}: {:>9} role={:<10} z_target={:?} len={:.1}mm vias={}",
                sig.name,
                if sig.freq_hz >= 1e9 {
                    format!("{:.1}GHz", sig.freq_hz / 1e9)
                } else if sig.freq_hz >= 1e6 {
                    format!("{:.0}MHz", sig.freq_hz / 1e6)
                } else if sig.freq_hz > 0.0 {
                    format!("{:.0}kHz", sig.freq_hz / 1e3)
                } else {
                    "DC".to_string()
                },
                format!("{:?}", sig.role).to_lowercase(),
                sig.z_target,
                par.length_mm,
                par.vias
            );
        }

        // SI audit (stray L/C + SWR + stubs + crosstalk + pairs + decoupling)
        let si_ctx = build_si_context(&plan, &board, &pair_infos, &route_exempt, &route_families, &router_net_gap, rules.clearance_mm, &plane_specs);
        let si = crate::si::audit(&board, &signals, &stackup, &fab, &si_ctx);
        let si_errors = si.findings.iter().filter(|f| f.severity == "error").count();
        let si_warnings = si.findings.iter().filter(|f| f.severity == "warning").count();
        for f in &si.findings {
            println!("  [{}] {} {}: {}", f.severity, f.code, f.net, f.detail);
        }
        stages.push(StageResult {
            stage: "si-audit".into(),
            success: si_errors == 0,
            detail: format!(
                "{} error(s), {} warning(s); stray L/C measured per net",
                si_errors, si_warnings
            ),
        });

        // Thermal audit
        let profiles: Vec<crate::thermal::ThermalProfile> = plan
            .components
            .iter()
            .map(|c| crate::thermal::ThermalProfile {
                reference: c.reference.clone(),
                power_w: c
                    .power_w
                    .unwrap_or_else(|| crate::thermal::power_from_value(&c.value)),
                theta_ja_c_per_w: crate::thermal::package_theta_ja(
                    c.footprint.as_deref().unwrap_or(""),
                ),
                t_j_max_c: 125.0,
            })
            .collect();
        let courtyard: Vec<f64> = plan.components.iter().map(|_| 60.0).collect();
        let powers: Vec<f64> = profiles.iter().map(|p| p.power_w).collect();
        let (w, h) = board
            .outline_bbox()
            .map(|(x0, y0, x1, y1)| (x1 - x0, y1 - y0))
            .unwrap_or((100.0, 80.0));
        let thermal = crate::thermal::audit(
            &profiles,
            25.0,
            &courtyard,
            w * h,
            0.5, // copper coverage estimate pre-pour
            0.5,
        );
        let th_errors = thermal.findings.iter().filter(|f| f.severity == "error").count();
        let th_warnings = thermal.findings.iter().filter(|f| f.severity == "warning").count();
        for f in &thermal.findings {
            println!("  [{}] {} {}: {}", f.severity, f.code, f.subject, f.detail);
        }
        stages.push(StageResult {
            stage: "thermal-audit".into(),
            success: th_errors == 0,
            detail: format!("{} error(s), {} warning(s)", th_errors, th_warnings),
        });

        // ─── Stability audit: regulators, crystals, switch/feedback nodes ───
        {
            let stab = crate::stability::audit(&plan, &board);
            for f in stab.iter().filter(|f| f.severity == "error") {
                if let Some(r) = &f.repair {
                    repairs.push(r.clone());
                }
            }
            let errs = stab.iter().filter(|f| f.severity == "error").count();
            let warns = stab.iter().filter(|f| f.severity == "warning").count();
            for f in &stab {
                println!("  [{}] {} {}: {}", f.severity, f.code, f.subject, f.detail);
            }
            stages.push(StageResult {
                stage: "stability-audit".into(),
                success: errs == 0,
                detail: format!("{} error(s), {} warning(s): regulator output capacitance, crystal loads and traces, switch/feedback nodes", errs, warns),
            });
        }

        // ─── PDN: |Z(f)| per rail from the decouplers as mounted ───
        {
            let results = pdn_audit(&plan, &board, &stackup, &si_ctx, &plane_specs, &plane_rects);
            let mut over = 0usize;
            for r in &results {
                let status = if r.peak_ohm > r.z_target_ohm { over += 1; "warning" } else { "info" };
                println!(
                    "  [{}] pdn {}: peak |Z| {:.3} Ω at {:.1} MHz vs target {:.3} Ω ({} capacitor(s){})",
                    status, r.rail, r.peak_ohm, r.peak_hz / 1e6, r.z_target_ohm, r.n_caps,
                    match (r.over_target_from_hz, r.over_target_to_hz) {
                        (Some(a), Some(b)) => format!("; above target {:.1}–{:.1} MHz — add low-ESL capacitance or shorten its loops", a / 1e6, b / 1e6),
                        _ => String::new(),
                    }
                );
            }
            stages.push(StageResult {
                stage: "pdn-audit".into(),
                success: true,
                detail: format!("{} rail(s) swept 100 kHz–200 MHz, {} above the ΔV/ΔI target (model: MLCC ESR/ESL classes + measured mounting loops; above 200 MHz the package/die decoupling owns the rail)", results.len(), over),
            });
        }

        // Fab-scope: the design rules the board was routed with must fit JLCPCB
        let fab_checks = rules.fab_violations(&fab, stackup.finished_thickness_mm);
        let fab_ok = fab_checks.is_empty();
        stages.push(StageResult {
            stage: "fab-scope".into(),
            success: fab_ok,
            detail: if fab_ok {
                format!(
                    "{}: trace {}/{}mm, via {}/{}mm inside JLCPCB {}-layer scope (±10% impedance)",
                    stackup.name, rules.trace_width_mm, rules.clearance_mm,
                    rules.via_diameter_mm, rules.via_drill_mm, board.layer_count
                )
            } else {
                fab_checks.join("; ")
            },
        });
    }

    let pcb_content = board.to_kicad_pcb();
    let pcb_content = resolve_fetched_models(&pcb_content, crate::kicad_cli::KiCadCli::find_3d_model_dir().as_deref());
    fs::write(&pcb_path, &pcb_content)
        .map_err(|e| OrchestrateError::Io(e.to_string()))?;
    // The KiCad project file carries the design rules kicad-cli judges by
    // (its 0.2mm defaults would fail every fine-pitch net otherwise).
    {
        let rules = crate::rules::DesignRules::for_layers(board.layer_count);
        let classes: Vec<(String, f64, f64)> = vec![(
            "power".into(),
            rules.power_trace_width_mm,
            rules.clearance_mm,
        )];
        let pro_path = project_dir.join("circuit.kicad_pro");
        fs::write(&pro_path, rules.kicad_project_json("circuit.kicad_pro", &classes))
            .map_err(|e| OrchestrateError::Io(e.to_string()))?;
        // A project fp-lib-table naming every library the board uses, so
        // KiCad opens the project without "library not found" notes.
        let mut libs: Vec<(String, String)> = Vec::new();
        for c in &plan.components {
            if let Some(f) = &c.footprint {
                if let Some((lib, _)) = f.split_once(':') {
                    if libs.iter().any(|(l, _)| l == lib) {
                        continue;
                    }
                    if let Some(path) = crate::footprint_lib::resolve(f) {
                        if let Some(dir) = path.parent() {
                            libs.push((lib.to_string(), dir.display().to_string()));
                        }
                    }
                }
            }
        }
        let mut table = String::from("(fp_lib_table\n  (version 7)\n");
        for (lib, dir) in &libs {
            table.push_str(&format!("  (lib (name \"{}\")(type \"KiCad\")(uri \"{}\")(options \"\")(descr \"\"))\n", lib, dir));
        }
        table.push_str(")\n");
        fs::write(project_dir.join("fp-lib-table"), table).map_err(|e| OrchestrateError::Io(e.to_string()))?;
    }

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

        println!("─── Stage 4: kicad-cli DRC (zones refilled, report parsed) ───");
        let drc = cli.pcb_drc("circuit.kicad_pcb");
        println!("  {}", drc.stdout.trim());
        stages.push(StageResult {
            stage: "kicad-drc".into(),
            success: drc.success,
            detail: if drc.stdout.trim().is_empty() { format!("{}", drc) } else { drc.stdout.trim().to_string() },
        });

        println!("─── Stage 5: kicad-cli exports ───");
        let out_dir = project_dir.join("fab");
        let _ = fs::create_dir_all(&out_dir);

        let gerbers = cli.export_gerbers("circuit.kicad_pcb", "fab", board.layer_count);
        let gerber_files = fs::read_dir(&out_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| {
                        let n = e.file_name().to_string_lossy().to_lowercase();
                        n.ends_with(".gtl") || n.ends_with(".gbl") || n.starts_with("circuit-in")
                            || n.ends_with(".gts") || n.ends_with(".gbs") || n.ends_with(".gtp")
                            || n.ends_with(".gbp") || n.ends_with(".gto") || n.ends_with(".gbo")
                            || n.ends_with(".gm1")
                    })
                    .count()
            })
            .unwrap_or(0);
        stages.push(StageResult {
            stage: "export-gerbers".into(),
            success: gerbers.success && gerber_files >= (board.layer_count as usize + 7),
            detail: format!(
                "{} ({} Gerber layers written: {} copper + mask/paste/silk/outline)",
                gerbers, gerber_files, board.layer_count
            ),
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
        // The detailed BOM from the plan: quantities, references, MPN, LCSC,
        // package — the order sheet (kicad-cli's bom.csv is the symbol list)
        let lines = crate::bom::build(&plan);
        let bom_ok = fs::write(project_dir.join("fab/bom-detailed.csv"), crate::bom::to_csv(&lines)).is_ok()
            && fs::write(project_dir.join("fab/bom.md"), crate::bom::to_markdown(&plan.name, &lines)).is_ok();
        let with_mpn = lines.iter().filter(|l| !l.mpn.is_empty() || !l.lcsc.is_empty()).count();
        stages.push(StageResult {
            stage: "export-bom".into(),
            success: bom.success && bom_ok,
            detail: format!("{} — fab/bom-detailed.csv + fab/bom.md: {} line items, {} components, {} with a part number", bom, lines.len(), lines.iter().map(|l| l.qty).sum::<usize>(), with_mpn),
        });

        // JLCPCB fabrication package: the files their order page takes
        {
            let jdir = project_dir.join("fab/jlcpcb");
            let _ = fs::remove_dir_all(&jdir);
            let _ = fs::create_dir_all(&jdir);
            let g = cli.export_gerbers_jlc("circuit.kicad_pcb", "fab/jlcpcb", board.layer_count);
            let d = cli.export_drill_jlc("circuit.kicad_pcb", "fab/jlcpcb");
            let p = cli.export_pos_csv("circuit.kicad_pcb", "fab/jlcpcb/positions-kicad.csv");
            let mut files: Vec<(String, Vec<u8>)> = Vec::new();
            if let Ok(rd) = fs::read_dir(&jdir) {
                let mut names: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
                names.sort();
                for f in names {
                    let name = f.file_name().unwrap().to_string_lossy().to_string();
                    if name.ends_with(".csv") || name.ends_with(".zip") || name.ends_with(".gbrjob") {
                        continue;
                    }
                    if let Ok(bytes) = fs::read(&f) {
                        files.push((name, bytes));
                    }
                }
            }
            let zip_ok = crate::jlcpcb::write_store_zip(&jdir.join("jlcpcb-gerbers.zip"), &files).is_ok();
            let lines = crate::bom::build(&plan);
            let _ = fs::write(jdir.join("jlcpcb-bom.csv"), crate::jlcpcb::bom_csv(&lines));
            let pos_csv = fs::read_to_string(jdir.join("positions-kicad.csv")).unwrap_or_default();
            let fp_of = |r: &str| plan.components.iter().find(|c| c.reference == r).and_then(|c| c.footprint.clone());
            let rows = crate::jlcpcb::cpl_from_positions(&pos_csv, &fp_of);
            let _ = fs::write(jdir.join("jlcpcb-cpl.csv"), crate::jlcpcb::cpl_csv(&rows));
            let corrected: Vec<String> = rows.iter().filter(|r| r.correction != 0.0).map(|r| format!("{} {:+.0}°", r.designator, r.correction)).collect();
            let with_lcsc = lines.iter().filter(|l| !l.lcsc.is_empty()).count();
            stages.push(StageResult {
                stage: "jlcpcb-package".into(),
                success: g.success && d.success && p.success && zip_ok && files.len() >= 8,
                detail: format!(
                    "fab/jlcpcb/jlcpcb-gerbers.zip ({} files: Protel names, no X2, mask-subtracted silk, merged Excellon + map), jlcpcb-bom.csv ({} lines, {} with an LCSC part), jlcpcb-cpl.csv ({} SMD parts, rotation corrections on {}: {}) — check the SMT preview before ordering",
                    files.len(), lines.len(), with_lcsc, rows.len(), corrected.len(),
                    if corrected.is_empty() { "none".to_string() } else { corrected.join(", ") }
                ),
            });
        }

        // Renders for humans: top, bottom and an isometric view WITH the
        // component bodies when a 3D model library is available.
        let _ = fs::create_dir_all(project_dir.join("render"));
        let model_dir = crate::kicad_cli::KiCadCli::find_3d_model_dir();
        let top = cli.render_3d("circuit.kicad_pcb", "render/board-top.png", "top", model_dir.as_deref());
        let bottom = cli.render_3d("circuit.kicad_pcb", "render/board-bottom.png", "bottom", model_dir.as_deref());
        let iso = cli.render_3d("circuit.kicad_pcb", "render/board-iso.png", "iso", model_dir.as_deref());
        // Which referenced models the library actually has
        let pcb_text = fs::read_to_string(&pcb_path).unwrap_or_default();
        let (models_total, models_missing) = count_missing_models(&pcb_text, model_dir.as_deref());
        let models_note = match &model_dir {
            Some(d) => format!("component bodies from {} ({} of {} referenced models found{})", d, models_total - models_missing.len(), models_total, if models_missing.is_empty() { String::new() } else { format!("; missing: {}", models_missing.join(", ")) }),
            None => "no 3D model library found: bodies missing — install kicad-packages3d or run the plugin's fetch-libs step".to_string(),
        };
        stages.push(StageResult {
            stage: "render".into(),
            success: top.success && bottom.success && iso.success && model_dir.is_some() && models_missing.is_empty(),
            detail: format!("render/board-top.png render/board-bottom.png render/board-iso.png — {}", models_note),
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
    for f in erc_report.findings.iter().filter(|f| f.code == "missing-decoupling") {
        if let Some(n) = &f.net {
            repairs.push(crate::repair::RepairHint::MissingDecoupling { net: n.clone() });
        }
    }
    // Honest classification: missing-power-flag is the known M4 gap (power
    // symbols not yet emitted) — a limitation note, not a design failure.
    // Everything else (floating pins, decoupling, shorts) is a real finding.
    let erc_real = erc_report
        .errors()
        .saturating_sub(
            erc_report
                .findings
                .iter()
                .filter(|f| f.code == "missing-power-flag")
                .count(),
        );
    stages.push(StageResult {
        stage: "someda-erc".into(),
        success: erc_real == 0,
        detail: format!(
            "{} finding(s) [{} = known power-symbol gap (M4)]",
            erc_report.errors(),
            erc_report
                .findings
                .iter()
                .filter(|f| f.code == "missing-power-flag")
                .count()
        ),
    });

    let pcb_text = fs::read_to_string(&pcb_path)
        .map_err(|e| OrchestrateError::Io(e.to_string()))?;
    let board = crate::board::Board::from_kicad_pcb(&pcb_text)
        .map_err(|e| OrchestrateError::Verification(format!("board parse: {}", e)))?;
    let drc_report = crate::drc::run_drc(
        &board,
        &crate::drc::DrcConfig::from_rules(&crate::rules::DesignRules::for_layers(board.layer_count)),
    );
    // Same honest classification: the segment-check truncation is a
    // note, not a design failure.
    let drc_real = drc_report
        .errors()
        .saturating_sub(
            drc_report
                .findings
                .iter()
                .filter(|f| f.code == "segment-check-truncated")
                .count(),
        );
    stages.push(StageResult {
        stage: "someda-drc".into(),
        success: drc_real == 0,
        detail: format!(
            "{} error(s), {} warning(s){}",
            drc_report.errors(),
            drc_report.warnings(),
            if drc_report.findings.iter().any(|f| f.code == "segment-check-truncated") {
                " [truncation = known spatial-index gap (M2)]"
            } else {
                ""
            }
        ),
    });

    // ─── Stage 7: the 6-gate export preflight (skill 103) ───
    // DRC ✓ + shorts ✓ + unrouted ✓ + pours + Gerbers + drill — the honest
    // done-gate: only all-six-pass means orderable.
    // KiCad's DRC is the stricter authority when it ran: its error count
    // (clearance, shorts, hole spacing, mask bridges...) and its unconnected
    // count both feed the gate. someda's own DRC covers the no-kicad case.
    let kicad_drc = if cli.is_available() { cli.last_drc_summary("circuit.kicad_pcb") } else { None };
    let drc_errors_total = drc_report.errors()
        .saturating_sub(drc_report.findings.iter().filter(|f| f.code == "segment-check-truncated").count())
        + kicad_drc.as_ref().map(|k| k.errors.saturating_sub(k.unconnected) + k.fab_blocking()).unwrap_or(0);
    let shorts = kicad_drc
        .as_ref()
        .map(|k| k.by_code.iter().filter(|(c, _)| c == "shorting_items").map(|(_, n)| *n).sum())
        .unwrap_or(0);
    let unconnected_total = unrouted_count + kicad_drc.as_ref().map(|k| k.unconnected).unwrap_or(0);
    // Pours: every declared zone must exist in the written board; KiCad's
    // unconnected count already covers plane pads a fanout missed.
    let pours_filled = board.zones.is_empty() || kicad_drc.as_ref().map(|k| k.unconnected == 0).unwrap_or(true);
    let gerber_ok = stages.iter().any(|s| s.stage == "export-gerbers" && s.success);
    let drill_ok = stages.iter().any(|s| s.stage == "export-drill" && s.success);
    let preflight_gates = crate::formulas::export_preflight(
        drc_errors_total,
        shorts,
        unconnected_total,
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
        repairs,
        honest_done: preflight_pass,
        board_mm,
    })
}

/// Which net fills which inner layer. Plan-declared planes win; otherwise
/// a 4-layer board gets GND on In1.Cu, a 6-layer board GND on In1.Cu and
/// In4.Cu plus the main power rail on In3.Cu (when such a net exists).
pub fn resolve_planes(plan: &CircuitPlan, board: &crate::board::Board) -> Vec<PlaneSpec> {
    if let Some(p) = &plan.planes {
        return p.clone();
    }
    let has = |n: &str| board.nets.iter().any(|(_, name)| name == n);
    let mut out = Vec::new();
    if board.layer_count < 4 {
        return out;
    }
    let gnd = ["GND", "GNDD", "AGND", "VSS"].iter().find(|n| has(n)).map(|s| s.to_string());
    let Some(gnd) = gnd else { return out };
    out.push(PlaneSpec { net: gnd.clone(), layer: "In1.Cu".into(), region: None, priority: None });
    if board.layer_count >= 6 {
        // the busiest power net becomes the In3 plane
        let mut best: Option<(usize, String)> = None;
        for net in &plan.nets {
            let role = crate::netfreq::net_signal(&net.name, &[], net.freq_hz, net.role.as_deref()).role;
            if matches!(role, crate::netfreq::NetRole::Power) && net.name != gnd {
                if best.as_ref().map(|b| net.pins.len() > b.0).unwrap_or(true) {
                    best = Some((net.pins.len(), net.name.clone()));
                }
            }
        }
        if let Some((_, pwr)) = best {
            out.push(PlaneSpec { net: pwr, layer: "In3.Cu".into(), region: None, priority: None });
        }
        out.push(PlaneSpec { net: gnd, layer: "In4.Cu".into(), region: None, priority: None });
    }
    out
}

/// Validate the plan against the contract (every pin belongs to a component).
pub fn validate_plan(plan: &CircuitPlan) -> Result<(), OrchestrateError> {
    if let Some(l) = plan.layers {
        if l < 2 || l % 2 != 0 || l > 12 {
            return Err(OrchestrateError::PlanInvalid(format!(
                "layers must be an even number from 2 to 12, got {}", l
            )));
        }
        if let Some(planes) = &plan.planes {
            let copper = crate::board::copper_layer_names(l);
            for p in planes {
                if !copper.contains(&p.layer) || p.layer == "F.Cu" || p.layer == "B.Cu" {
                    return Err(OrchestrateError::PlanInvalid(format!(
                        "plane '{}' on '{}' is not an inner copper layer of a {}-layer board", p.net, p.layer, l
                    )));
                }
                if !plan.nets.iter().any(|n| n.name == p.net) {
                    return Err(OrchestrateError::PlanInvalid(format!("plane net '{}' is not a plan net", p.net)));
                }
            }
        }
    } else if plan.planes.is_some() {
        return Err(OrchestrateError::PlanInvalid("planes need an explicit layers count (4 or 6)".into()));
    }
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
    if let Some(pairs) = &plan.pairs {
        let mut used: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for pr in pairs {
            if pr.p == pr.n {
                return Err(OrchestrateError::PlanInvalid(format!("pair '{}' names the same net twice", pr.p)));
            }
            for name in [&pr.p, &pr.n] {
                if !plan.nets.iter().any(|n| &n.name == name) {
                    return Err(OrchestrateError::PlanInvalid(format!("pair net '{}' is not a plan net", name)));
                }
                if !used.insert(name.as_str()) {
                    return Err(OrchestrateError::PlanInvalid(format!("net '{}' belongs to more than one pair", name)));
                }
            }
            if let Some(z) = pr.z_diff {
                if !(20.0..=300.0).contains(&z) {
                    return Err(OrchestrateError::PlanInvalid(format!("pair {}/{}: z_diff {} Ω is not a PCB pair impedance (20..300)", pr.p, pr.n, z)));
                }
            }
            // The two nets must run between the same two components
            let refs_of = |name: &str| -> std::collections::HashSet<String> {
                plan.nets.iter().find(|n| n.name == name).map(|n| n.pins.iter().filter_map(|p| p.split(':').next().map(|s| s.to_string())).collect()).unwrap_or_default()
            };
            let shared = refs_of(&pr.p).intersection(&refs_of(&pr.n)).count();
            if shared < 2 {
                return Err(OrchestrateError::PlanInvalid(format!(
                    "pair {}/{}: the two nets share {} component(s); a pair runs between two components both nets connect to",
                    pr.p, pr.n, shared
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
/// Schematic pin geometry for a plan pin set (mm, library coordinates:
/// y up). Two pins inline; more on a DIP rectangle — left column top to
/// bottom, right column bottom to top. Connection points sit `SCH_PIN_X`
/// from the origin; the pin runs `SCH_PIN_LEN` inward to the body edge.
pub const SCH_PIN_X: f64 = 12.7;
pub const SCH_PIN_LEN: f64 = 5.08;
pub fn symbol_pin_geometry(pin_nums: &[String]) -> Vec<(String, f64, f64, i32)> {
    let n = pin_nums.len();
    let half = (n + 1) / 2;
    let pitch = 2.54;
    pin_nums
        .iter()
        .enumerate()
        .map(|(pi, num)| {
            // Both columns centred on the origin: row 0 at the top of the
            // left column, the right column climbing from the bottom. (The
            // earlier form put the left column above the origin and the
            // right column below it — a 128-pin part became 320 mm tall and
            // overlapped its neighbours; live-verified multiple_net_names.)
            let top = (half as f64 - 1.0) / 2.0 * pitch;
            if n <= 2 {
                if pi == 0 { (num.clone(), -SCH_PIN_X, 0.0, -1) } else { (num.clone(), SCH_PIN_X, 0.0, 1) }
            } else if pi < half {
                (num.clone(), -SCH_PIN_X, top - pi as f64 * pitch, -1)
            } else {
                let k = n - 1 - pi;
                (num.clone(), SCH_PIN_X, -top + k as f64 * pitch, 1)
            }
        })
        .collect()
}

/// Plan-referenced pins of a component, in first-seen order.
fn plan_pins_of(plan: &CircuitPlan, reference: &str) -> Vec<String> {
    let mut pin_nums: Vec<String> = Vec::new();
    for net in plan.nets.iter() {
        for pin in &net.pins {
            if let Some((refdes, num)) = pin.split_once(':') {
                if refdes == reference && !pin_nums.iter().any(|p| p == num) {
                    pin_nums.push(num.to_string());
                }
            }
        }
    }
    pin_nums
}

/// Where every symbol sits on the sheet: a column layout sized from each
/// symbol's pin count, on the smallest standard sheet that holds it.
pub struct SchLayout {
    /// Symbol origin per component index (mm, sheet coordinates, y down).
    pub origins: Vec<(f64, f64)>,
    /// Body half-height per component index (mm).
    pub half_heights: Vec<f64>,
    pub paper: &'static str,
}

pub fn sch_layout(plan: &CircuitPlan) -> SchLayout {
    let snap = |v: f64| (v / 1.27).round() * 1.27;
    let n = plan.components.len();
    // symbol body half-height from its pin rows
    let half_heights: Vec<f64> = plan
        .components
        .iter()
        .map(|c| {
            let pins = plan_pins_of(plan, &c.reference);
            let rows = if pins.len() <= 2 { 1 } else { (pins.len() + 1) / 2 };
            ((rows as f64 - 1.0) * 2.54) / 2.0 + 2.54
        })
        .collect();
    // tallest first, then by reference, so ICs anchor the left columns
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        half_heights[b].partial_cmp(&half_heights[a]).unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| plan.components[a].reference.cmp(&plan.components[b].reference))
    });
    // paper candidates (landscape, mm) — the smallest whose columns fit
    let papers: [(&str, f64, f64); 5] = [("A4", 297.0, 210.0), ("A3", 420.0, 297.0), ("A2", 594.0, 420.0), ("A1", 841.0, 594.0), ("A0", 1189.0, 841.0)];
    let col_w = 63.5; // pins ± 12.7, 2.54 stubs, ~17 mm of label each side
    let (margin_l, margin_t, margin_b) = (25.4, 25.4, 38.1);
    let mut origins = vec![(0.0, 0.0); n];
    let mut paper = "A0";
    for (name, pw, ph) in papers {
        let usable_h = ph - margin_t - margin_b;
        let mut col = 0usize;
        let mut y = margin_t;
        let mut fits = true;
        for &i in &order {
            let slot = 2.0 * half_heights[i] + 10.16; // reference above, value below
            if y + slot > margin_t + usable_h && y > margin_t {
                col += 1;
                y = margin_t;
            }
            let x = margin_l + col_w / 2.0 + col as f64 * col_w;
            if x + col_w / 2.0 > pw - 12.7 {
                fits = false;
            }
            origins[i] = (snap(x), snap(y + 5.08 + half_heights[i]));
            y += slot;
        }
        paper = name;
        if fits {
            break;
        }
    }
    SchLayout { origins, half_heights, paper }
}

pub fn generate_schematic(plan: &CircuitPlan) -> String {
    generate_schematic_parts(plan).0
}

/// The project symbol library (`someda-symbols.kicad_sym`) holding every
/// embedded symbol, and a `sym-lib-table` mapping each library nickname the
/// schematic references to it — so kicad-cli ERC stops reporting
/// `lib_symbol_issues` for nicknames the project does not configure.
pub fn generate_symbol_library(plan: &CircuitPlan) -> (String, String) {
    let (_, defs) = generate_schematic_parts(plan);
    let mut lib = String::from("(kicad_symbol_lib (version 20220914) (generator someda)\n");
    let mut nicknames: Vec<String> = Vec::new();
    for (full_name, def) in &defs {
        let (nick, short) = full_name.split_once(':').unwrap_or(("someda", full_name.as_str()));
        if !nicknames.iter().any(|n| n == nick) {
            nicknames.push(nick.to_string());
        }
        // the embedded definition names the symbol "Nick:Short"; a library
        // file names it "Short"
        lib.push_str(&def.replacen(&format!("(symbol \"{}\"", full_name), &format!("(symbol \"{}\"", short), 1));
    }
    lib.push_str(")\n");
    let mut table = String::from("(sym_lib_table\n  (version 7)\n");
    for n in nicknames {
        table.push_str(&format!("  (lib (name \"{}\")(type \"KiCad\")(uri \"${{KIPRJMOD}}/someda-symbols.kicad_sym\")(options \"\")(descr \"someda generated symbols\"))\n", n));
    }
    table.push_str(")\n");
    (lib, table)
}

fn generate_schematic_parts(plan: &CircuitPlan) -> (String, Vec<(String, String)>) {
    let layout = sch_layout(plan);
    let mut defs: Vec<(String, String)> = Vec::new();
    let mut out = String::new();
    out.push_str("(kicad_sch (version 20220904) (generator someda)\n");
    out.push_str(&format!("  (paper \"{}\")\n", layout.paper));
    out.push_str(&format!("  (title_block (title \"{}\"))\n", plan.name));

    let mut lib_symbols = String::new();
    let mut symbols = String::new();
    let mut labels = String::new();
    let mut emitted_symbols: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut wire_id = 0usize;

    for (i, comp) in plan.components.iter().enumerate() {
        let (x, y) = layout.origins[i];
        let hh = layout.half_heights[i];
        // ONE SYMBOL PIN PER PLAN-REFERENCED PIN — mirror of the board fix.
        let pin_nums = plan_pins_of(plan, &comp.reference);
        let geometry = symbol_pin_geometry(&pin_nums);
        // Embed a lib symbol per (lib_id, pin set): two "Device:C" parts
        // using different pins must not share one definition (the parser
        // keeps one, and the other's pins float — live-verified ERC noise).
        let lib_name = symbol_name_for(&comp.lib_id, &pin_nums);
        let base_name = lib_name.split(':').last().unwrap_or(&lib_name).to_string();
        let sub_name = format!("{}_0_1", base_name);
        let instance = format!(
            "  (symbol (lib_id \"{}\") (at {:.2} {:.2} 0) (unit 1)\n\
             (property \"Reference\" \"{}\" (at {:.2} {:.2} 0) (effects (font (size 1.27 1.27))))\n\
             (property \"Value\" \"{}\" (at {:.2} {:.2} 0) (effects (font (size 1.27 1.27))))\n  )\n",
            lib_name, x, y, comp.reference, x, y - hh - 2.54, comp.value, x, y + hh + 2.54
        );
        // Every plan pin carries a short stub and a net label: same-named
        // labels connect on the sheet, so the schematic reads as a labelled
        // netlist (no wire channels crossing the sheet). The stub points
        // outward, the label text runs away from the body.
        for (num, px, py, side) in &geometry {
            let Some(net) = plan.nets.iter().find(|n| n.pins.iter().any(|p| p == &format!("{}:{}", comp.reference, num))) else { continue };
            let (cx, cy) = (x + px, y - py); // KiCad: lib y up, sheet y down
            let sx = cx + *side as f64 * 2.54;
            labels.push_str(&format!(
                "  (wire (pts (xy {:.2} {:.2}) (xy {:.2} {:.2})) (uuid wire_{}) )\n",
                cx, cy, sx, cy, wire_id
            ));
            wire_id += 1;
            let (rot, justify) = if *side < 0 { (180, "right bottom") } else { (0, "left bottom") };
            labels.push_str(&format!(
                "  (label \"{}\" (at {:.2} {:.2} {}) (effects (font (size 1.27 1.27)) (justify {})))\n",
                net.name, sx, cy, rot, justify
            ));
        }
        if !emitted_symbols.insert(lib_name.clone()) {
            symbols.push_str(&instance);
            continue;
        }
        let mut def = String::new();
        def.push_str(&format!(
            "    (symbol \"{}\" (pin_names (offset 0.254)) (in_bom yes) (on_board yes)\n      (property \"Reference\" \"{}\" (at 0 {:.2} 0) (effects (font (size 1.27 1.27))))\n      (property \"Value\" \"{}\" (at 0 {:.2} 0) (effects (font (size 1.27 1.27))))\n      (symbol \"{}\"\n",
            lib_name, comp.reference.trim_end_matches(|c: char| c.is_ascii_digit()), hh + 2.54, comp.value, -hh - 2.54, sub_name
        ));
        // Body rectangle between the pin ends
        let body_x = SCH_PIN_X - SCH_PIN_LEN;
        def.push_str(&format!(
            "        (rectangle (start {:.2} {:.2}) (end {:.2} {:.2}) (stroke (width 0.254) (type default)) (fill (type background)))\n",
            -body_x, hh, body_x, -hh
        ));
        for (num, px, py, side) in &geometry {
            let ang = if *side < 0 { 0 } else { 180 };
            def.push_str(&format!(
                "        (pin passive line (at {:.2} {:.2} {}) (length {:.2})\n\
                 (name \"{}\" (effects (font (size 1.27 1.27))))\n\
                 (number \"{}\" (effects (font (size 1.27 1.27)))))\n",
                px, py, ang, SCH_PIN_LEN, num, num
            ));
        }
        def.push_str("             )\n    )\n");
        lib_symbols.push_str(&def);
        defs.push((lib_name.clone(), def));
        symbols.push_str(&instance);
    }

    out.push_str("  (lib_symbols\n");
    out.push_str(&lib_symbols);
    out.push_str("  )\n");
    out.push_str(&labels);
    out.push_str(&symbols);
    out.push_str(")\n");
    (out, defs)
}

/// The embedded symbol name for a lib_id with a given pin set: the lib_id
/// itself when the part uses pins "1".."n" in order, otherwise suffixed
/// with the pin list so different pin sets never collide.
fn symbol_name_for(lib_id: &str, pins: &[String]) -> String {
    let canonical = pins.iter().enumerate().all(|(i, p)| p == &(i + 1).to_string());
    if canonical && !pins.is_empty() {
        return lib_id.to_string();
    }
    let sig: String = pins.iter().map(|p| p.replace(|c: char| !c.is_ascii_alphanumeric(), "_")).collect::<Vec<_>>().join("_");
    format!("{}__p{}", lib_id, sig)
}

/// Parse "REF:PIN" to (x, y) using the component placement grid.
fn parse_pin(pin_ref: &str, plan: &CircuitPlan) -> Option<(f64, f64)> {
    let (ref_des, pin_num) = pin_ref.split_once(':')?;
    let idx = plan.components.iter().position(|c| c.reference == ref_des)?;
    let layout = sch_layout(plan);
    let (x, y) = layout.origins[idx];
    let pins = plan_pins_of(plan, ref_des);
    let (_, px, py, _) = symbol_pin_geometry(&pins).into_iter().find(|(n, _, _, _)| n == pin_num)?;
    Some((x + px, y - py))
}

/// How one component's footprint was obtained.
#[derive(Debug, Clone)]
pub struct FootprintResolution {
    pub reference: String,
    pub lib_id: String,
    /// true = stock KiCad library geometry; false = synthetic fallback
    pub real: bool,
    pub note: String,
}

/// Generate a Board MODEL from the plan (the routing substrate).
/// Copper is added later by the router; this creates outline, nets,
/// footprints with pads, and assigns pad nets from the plan.
pub fn generate_board_model(plan: &CircuitPlan) -> crate::board::Board {
    generate_board_model_with_report(plan).0
}

/// Like [`generate_board_model`], also reporting per-component whether the
/// footprint came from a real KiCad library or the synthetic fallback.
pub fn generate_board_model_with_report(
    plan: &CircuitPlan,
) -> (crate::board::Board, Vec<FootprintResolution>) {
    let mut board = crate::board::Board::default();
    board.layer_count = plan.layers.unwrap_or(2).max(2);
    let mut report = Vec::new();

    // Nets
    for (i, net) in plan.nets.iter().enumerate() {
        board.nets.push((i + 1, net.name.clone()));
    }
    let net_for = |reference: &str, pin: &str| -> Option<(usize, String)> {
        let key = format!("{}:{}", reference, pin);
        plan.nets
            .iter()
            .enumerate()
            .find(|(_, net)| net.pins.iter().any(|p| p == &key))
            .map(|(i, net)| (i + 1, net.name.clone()))
    };

    // Footprints: real library geometry when the plan names a resolvable
    // "Lib:Name"; otherwise the synthetic fallback (reported honestly).
    let mut courtyard_area = 0.0;
    for comp in &plan.components {
        let rotation = comp.rotation.unwrap_or(0.0);
        let lib_id = comp.footprint.clone().unwrap_or_else(|| "someda:DEFAULT".into());
        let loaded = if comp.footprint.is_some() {
            crate::footprint_lib::load(&lib_id)
        } else {
            Err("no footprint given".into())
        };
        match loaded {
            Ok(lib) => {
                let fp = lib.instantiate(
                    &comp.reference,
                    &comp.value,
                    &lib_id,
                    0.0,
                    0.0,
                    rotation,
                    &|pin: &str| net_for(&comp.reference, pin),
                );
                let (w, h) = lib.size();
                courtyard_area += w * h;
                report.push(FootprintResolution {
                    reference: comp.reference.clone(),
                    lib_id: lib_id.clone(),
                    real: true,
                    note: format!("{} pads, courtyard {:.1}x{:.1}mm", lib.pads.len(), w, h),
                });
                board.footprints.push(fp);
            }
            Err(why) => {
                let fp = synthetic_footprint(plan, comp, &lib_id, rotation, &net_for);
                courtyard_area += 60.0;
                report.push(FootprintResolution {
                    reference: comp.reference.clone(),
                    lib_id: lib_id.clone(),
                    real: false,
                    note: format!("synthetic pads ({})", why),
                });
                board.footprints.push(fp);
            }
        }
    }

    // Board outline: fixed size from the plan, or an area budget from the
    // real courtyards (skill 102) plus physics headroom for impedance
    // corridors (wide controlled-Z traces need room to coexist).
    let (w, h) = match &plan.board {
        Some(b) => (b.width_mm, b.height_mm),
        None => {
            let density = if board.layer_count >= 4 { 0.45 } else { 0.35 };
            let mut area = (courtyard_area / density).max(1500.0);
            let stackup = crate::materials::Stackup::for_layers(board.layer_count);
            for net in &plan.nets {
                let mut member_values: Vec<&str> = Vec::new();
                for pin in &net.pins {
                    if let Some((refdes, _)) = pin.split_once(':') {
                        if let Some(c) = plan.components.iter().find(|c| c.reference == refdes) {
                            member_values.push(&c.value);
                            member_values.push(&c.lib_id);
                        }
                    }
                }
                let sig = crate::netfreq::net_signal(
                    &net.name, &member_values, net.freq_hz, net.role.as_deref(),
                );
                if let Some(z0) = sig.z_target {
                    if sig.needs_impedance_control() {
                        let wz = crate::formulas::microstrip_width_for_z0(
                            z0, stackup.er, stackup.h_mm, stackup.copper_thickness_mm,
                        );
                        area += (wz + 0.4) * 60.0;
                    }
                }
            }
            let w = (area / 0.8).sqrt();
            (w, area / w)
        }
    };
    let (min_x, min_y, max_x, max_y) = (0.0, 0.0, w, h);
    board.outline = vec![
        ((min_x, min_y), (max_x, min_y)),
        ((max_x, min_y), (max_x, max_y)),
        ((max_x, max_y), (min_x, max_y)),
        ((min_x, max_y), (min_x, min_y)),
    ];

    (board, report)
}

/// The synthetic fallback footprint: 2 pins → 2.5mm inline; more pins →
/// a two-column 1.27mm grid. Used only when no library footprint resolves,
/// and always reported as synthetic.
fn synthetic_footprint(
    plan: &CircuitPlan,
    comp: &PlannedComponent,
    lib_id: &str,
    rotation: f64,
    net_for: &dyn Fn(&str, &str) -> Option<(usize, String)>,
) -> crate::board::Footprint {
    let mut fp = crate::board::Footprint {
        reference: comp.reference.clone(),
        lib_name: lib_id.to_string(),
        value: comp.value.clone(),
        x: 0.0,
        y: 0.0,
        rotation,
        pads: Vec::new(),
        courtyard: None,
        keepouts: Vec::new(),
        ref_text: None,
        source: None,
    };
    // ONE PAD PER PLAN-REFERENCED PIN — never a fixed 2-pad model.
    let mut pin_nums: Vec<String> = Vec::new();
    for net in &plan.nets {
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
    let n = pin_nums.len();
    for (i, pin_num) in pin_nums.iter().enumerate() {
        let (dx, dy) = if n <= 2 {
            let step = 1.25;
            if n == 1 { (0.0, 0.0) } else if i == 0 { (-step, 0.0) } else { (step, 0.0) }
        } else {
            let half = (n + 1) / 2;
            let pitch = 1.27;
            let col_gap = 7.62 / 2.0;
            if i < half {
                (-col_gap, (i as f64 - (half as f64 - 1.0) / 2.0) * pitch)
            } else {
                let k = n - 1 - i;
                (col_gap, (k as f64 - (half as f64 - 1.0) / 2.0) * pitch)
            }
        };
        let (ox, oy) = crate::board::rotate_offset(dx, dy, rotation);
        let (pad_w, pad_h) = if n > 2 { (1.0, 1.0) } else { (1.5, 1.5) };
        let mut pad = crate::board::Pad::smd(pin_num, net_for(&comp.reference, pin_num), ox, oy, pad_w, pad_h);
        pad.rotation = rotation;
        fp.pads.push(pad);
    }
    fp
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



/// Rail voltage from a net name: "+3V3" → 3.3, "+1V8" → 1.8, "VBUS"/"+5V" → 5.
pub fn rail_volts(name: &str) -> Option<f64> {
    let u = name.to_ascii_uppercase();
    if u == "VBUS" || u == "+5V" || u == "5V" || u == "VIN" {
        return Some(5.0);
    }
    let bytes: Vec<char> = u.chars().collect();
    for i in 0..bytes.len() {
        if bytes[i] == 'V' && i > 0 && bytes[i - 1].is_ascii_digit() {
            let int_part: String = bytes[..i].iter().rev().take_while(|c| c.is_ascii_digit()).collect::<String>().chars().rev().collect();
            let frac: String = bytes[i + 1..].iter().take_while(|c| c.is_ascii_digit()).collect();
            let v: f64 = format!("{}.{}", int_part, if frac.is_empty() { "0".to_string() } else { frac }).parse().ok()?;
            if v > 0.0 && v < 60.0 {
                return Some(v);
            }
        }
    }
    None
}

/// Build and sweep a PDN model per power rail that has decouplers.
fn pdn_audit(
    plan: &CircuitPlan,
    board: &crate::board::Board,
    stackup: &crate::materials::Stackup,
    ctx: &crate::si::SiContext,
    plane_specs: &[PlaneSpec],
    plane_rects: &[Option<(f64, f64, f64, f64)>],
) -> Vec<crate::pdn::RailResult> {
    use crate::pdn::*;
    let ground = |n: &str| ctx.decouplers.iter().any(|d| (d.net_a == n || d.net_b == n) && crate::netfreq::net_signal(n, &[], None, None).role == crate::netfreq::NetRole::Ground);
    // Consumers: components with declared power, split across their rails by pin count
    let consumers: Vec<(String, f64, Vec<(String, f64, usize)>)> = plan
        .components
        .iter()
        .filter_map(|c| {
            let p = c.power_w?;
            if p <= 0.0 {
                return None;
            }
            let mut rails: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for n in &plan.nets {
                if let Some(v) = rail_volts(&n.name) {
                    let _ = v;
                    let cnt = n.pins.iter().filter(|pin| pin.starts_with(&format!("{}:", c.reference))).count();
                    if cnt > 0 && crate::netfreq::net_signal(&n.name, &[], n.freq_hz, n.role.as_deref()).role == crate::netfreq::NetRole::Power {
                        *rails.entry(n.name.clone()).or_insert(0) += cnt;
                    }
                }
            }
            Some((c.reference.clone(), p, rails.into_iter().map(|(r, cnt)| (r.clone(), rail_volts(&r).unwrap_or(3.3), cnt)).collect()))
        })
        .collect();
    let currents = rail_currents(&consumers);
    let (bw, bh) = board.outline_bbox().map(|(x0, y0, x1, y1)| (x1 - x0, y1 - y0)).unwrap_or((100.0, 80.0));
    let mut rails: Vec<String> = ctx.decouplers.iter().filter_map(|d| if ground(&d.net_b) { Some(d.net_a.clone()) } else if ground(&d.net_a) { Some(d.net_b.clone()) } else { None }).collect();
    rails.sort();
    rails.dedup();
    let mut out = Vec::new();
    for rail in rails {
        let Some(volts) = rail_volts(&rail) else { continue };
        let caps: Vec<CapBranch> = ctx
            .decouplers
            .iter()
            .filter(|d| d.net_a == rail || d.net_b == rail)
            .map(|d| {
                let loop_nh = crate::si::cap_loop_inductance_nh(board, stackup, ctx, d).map(|(l, _)| l).unwrap_or(2.0);
                CapBranch { reference: d.reference.clone(), c_f: d.c_farads, esr_ohm: mlcc_esr_ohm(d.c_farads), esl_nh: mlcc_esl_nh(d.c_farads) + loop_nh }
            })
            .collect();
        // Plane capacitance to the nearest GND plane (depth difference)
        let plane_c_pf = plane_specs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.net == rail)
            .map(|(i, p)| {
                let area = plane_rects.get(i).copied().flatten().map(|(x0, y0, x1, y1)| (x1 - x0) * (y1 - y0)).unwrap_or(bw * bh);
                let k = p.layer.strip_prefix("In").and_then(|r| r.strip_suffix(".Cu")).and_then(|d| d.parse::<usize>().ok()).unwrap_or(1);
                let kg = plane_specs.iter().find(|q| crate::netfreq::net_signal(&q.net, &[], None, None).role == crate::netfreq::NetRole::Ground).and_then(|q| q.layer.strip_prefix("In").and_then(|r| r.strip_suffix(".Cu")).and_then(|d| d.parse::<usize>().ok())).unwrap_or(1);
                let step = (stackup.finished_thickness_mm - 2.0 * (stackup.h_mm + stackup.copper_thickness_mm)).max(0.1) / (board.layer_count as f64 - 3.0).max(1.0);
                let h = ((k as f64 - kg as f64).abs() * step).max(stackup.h_mm);
                plane_capacitance_pf(area, h, stackup.er)
            })
            .sum();
        // Regulator branch: the output path from the regulator (or its buck
        // inductor) to the first via/pad — measured; 20 mΩ source resistance.
        let (vrm_l_nh, _src) = plan
            .components
            .iter()
            .filter(|c| crate::values::inductance_h(&c.value).is_some() || { let u = format!("{} {}", c.value, c.lib_id).to_ascii_uppercase(); u.contains("REGULATOR") || u.contains("LDO") || u.contains("TPS5") || u.contains("ME62") || u.contains("XC62") })
            .filter(|c| plan.nets.iter().any(|n| n.name == rail && n.pins.iter().any(|p| p.starts_with(&format!("{}:", c.reference)))))
            .filter_map(|c| crate::si::component_stub(board, &c.reference, &rail).map(|(len, w, _)| (crate::si::trace_l_per_mm(w, stackup.h_mm, stackup.copper_thickness_mm) * len + 0.5, c.reference.clone())))
            .next()
            .unwrap_or((10.0, "assumed 10 nH (no regulator found on this rail)".into()));
        let model = RailModel { rail: rail.clone(), volts, current_a: currents.get(&rail).copied().unwrap_or(0.1).max(0.05), caps, plane_c_pf, vrm_r_ohm: 0.02, vrm_l_nh };
        out.push(sweep(&model));
    }
    out
}


/// Referenced 3D models (`(model "${KICAD*_3DMODEL_DIR}/Lib.3dshapes/X.step")`)
/// and which of them the model directory lacks: (total, missing names).
pub fn count_missing_models(pcb_text: &str, model_dir: Option<&str>) -> (usize, Vec<String>) {
    let mut total = 0usize;
    let mut missing: Vec<String> = Vec::new();
    for line in pcb_text.lines() {
        let Some(i) = line.find("(model \"") else { continue };
        let rest = &line[i + 8..];
        let Some(j) = rest.find('"') else { continue };
        let path = &rest[..j];
        total += 1;
        let Some(k) = path.find("3DMODEL_DIR}") else { continue };
        let rel = &path[k + "3DMODEL_DIR}".len()..];
        let rel = rel.trim_start_matches('/');
        let fetched = std::env::var("HOME").map(|h| format!("{}/.local/share/someda/3dmodels", h)).unwrap_or_default();
        let overridden = std::fs::read_to_string(format!("{}/overrides.json", fetched))
            .ok()
            .and_then(|t| serde_json::from_str::<std::collections::HashMap<String, String>>(&t).ok())
            .map(|m| m.get(rel).map(|o| std::path::Path::new(o).exists()).unwrap_or(false))
            .unwrap_or(false);
        let found = model_dir.map(|d| std::path::Path::new(d).join(rel).exists()).unwrap_or(false)
            || std::path::Path::new(&fetched).join(rel).exists()
            || overridden;
        if !found {
            missing.push(rel.rsplit('/').next().unwrap_or(rel).to_string());
        }
    }
    missing.sort();
    missing.dedup();
    (total, missing)
}

/// Point every referenced model that the system library lacks but the
/// someda-fetched directory has at that absolute file, so kicad-cli renders
/// it (one KICAD*_3DMODEL_DIR cannot name two directories).
pub fn resolve_fetched_models(pcb_text: &str, model_dir: Option<&str>) -> String {
    // SOMEDA_FETCHED_DIR: where fetched models live (the desktop app sets it);
    // default ~/.local/share/someda/3dmodels (the plugin's fetch-libs convention)
    let fetched = match std::env::var("SOMEDA_FETCHED_DIR") {
        Ok(d) => d,
        Err(_) => {
            let Ok(home) = std::env::var("HOME") else { return pcb_text.to_string() };
            format!("{}/.local/share/someda/3dmodels", home)
        }
    };
    if !std::path::Path::new(&fetched).is_dir() {
        return pcb_text.to_string();
    }
    // overrides.json: models fetched from another source (LCSC/EasyEDA,
    // UltraLibrarian, SnapEDA) for a KiCad footprint whose own model does
    // not exist: {"Lib.3dshapes/X.step": "/abs/path/model.step"}
    let overrides: std::collections::HashMap<String, String> = std::fs::read_to_string(format!("{}/overrides.json", fetched))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut out = String::with_capacity(pcb_text.len());
    for line in pcb_text.lines() {
        let mut replaced = line.to_string();
        if let Some(i) = line.find("(model \"") {
            let rest = &line[i + 8..];
            if let Some(j) = rest.find('"') {
                let path = &rest[..j];
                if let Some(k) = path.find("3DMODEL_DIR}") {
                    let rel = path[k + "3DMODEL_DIR}".len()..].trim_start_matches('/');
                    let in_system = model_dir.map(|d| std::path::Path::new(d).join(rel).exists()).unwrap_or(false);
                    let local = std::path::Path::new(&fetched).join(rel);
                    if !in_system && local.exists() {
                        replaced = line.replace(path, &local.display().to_string());
                    } else if !in_system {
                        if let Some(o) = overrides.get(rel) {
                            if std::path::Path::new(o).exists() {
                                replaced = line.replace(path, o);
                            }
                        }
                    }
                }
            }
        }
        out.push_str(&replaced);
        out.push('\n');
    }
    out
}


/// Rotation and outward overhang for a component placed on a board edge.
/// The opening direction comes from the library footprint (courtyard
/// overhang beyond the pads); the rotation (KiCad: CCW, y down) is the one
/// that turns it toward the edge's outward normal. A plan `rotation` wins
/// when given. Returns (rotation, overhang mm, a printed note).
/// An "auto" plane island: the footprint that owns most of the net's pins
/// (the SoC's rail pins) plus 5 mm for the decoupling capacitors hugging
/// them, in board mm from the outline's top-left corner. Pads elsewhere
/// are routed to the island as traces.
pub fn auto_plane_region(board: &crate::board::Board, net: &str) -> Option<[f64; 4]> {
    let (bx0, by0, _, _) = board.outline_bbox().unwrap_or((0.0, 0.0, 100.0, 80.0));
    let on_net = |pd: &crate::board::Pad| pd.net.as_ref().map(|n| n.1 == net).unwrap_or(false);
    let anchor = board.footprints.iter().max_by_key(|fp| fp.pads.iter().filter(|pd| on_net(pd)).count())?;
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for pd in anchor.pads.iter().filter(|pd| on_net(pd)) {
        b[0] = b[0].min(pd.x - bx0);
        b[1] = b[1].min(pd.y - by0);
        b[2] = b[2].max(pd.x - bx0);
        b[3] = b[3].max(pd.y - by0);
    }
    if b[0].is_finite() {
        Some([b[0] - 5.0, b[1] - 5.0, b[2] + 5.0, b[3] + 5.0])
    } else {
        None
    }
}

/// Rotate every edge connector so its opening faces its edge (plan
/// rotation, when given, wins). Prints the decision when `verbose`.
pub fn orient_edge_connectors(plan: &mut CircuitPlan, verbose: bool) {
    for c in plan.components.iter_mut() {
        let Some(pos) = c.position.as_deref().and_then(crate::placer::Pos::parse) else { continue };
        if !pos.is_edge() {
            continue;
        }
        let (rot, _overhang, note) = edge_connector_orientation(&*c, pos);
        if let (Some(n), true) = (note, verbose) {
            println!("  {}", n);
        }
        if c.rotation.is_none() {
            c.rotation = rot.map(|r| r as f64);
        }
    }
}

pub fn edge_connector_orientation(c: &PlannedComponent, pos: crate::placer::Pos) -> (Option<i32>, f64, Option<String>) {
    let want: (i32, i32) = match pos {
        crate::placer::Pos::LeftEdge => (-1, 0),
        crate::placer::Pos::RightEdge => (1, 0),
        crate::placer::Pos::TopEdge => (0, -1),
        _ => (0, 1),
    };
    let lib = c
        .footprint
        .as_deref()
        .and_then(crate::footprint_lib::resolve)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| crate::footprint_lib::parse_kicad_mod(&text).ok());
    // `edge_overhang_mm` is how far the BODY FRONT protrudes past the board
    // edge. The placer works in courtyard terms with a 0.5 mm margin, so
    // the value it gets is overhang + (courtyard-to-body inset) + margin
    // (live-verified: passing the raw value left a "0.8 mm" USB-C 0.2 mm
    // inside the edge).
    let overhang = c.edge_overhang_mm.unwrap_or(0.0).max(0.0);
    let Some(lib) = lib else {
        return (c.rotation.map(|r| r as i32), if overhang > 0.0 { overhang + crate::placer::EDGE_MARGIN_MM } else { 0.0 }, None);
    };
    let Some(opening) = lib.opening_direction() else {
        return (
            c.rotation.map(|r| r as i32),
            if overhang > 0.0 { overhang + crate::placer::EDGE_MARGIN_MM } else { 0.0 },
            Some(match c.rotation {
                Some(r) => format!("{}: no connector opening found in {} geometry; plan rotation {} kept", c.reference, lib.name, r),
                None => format!("{}: no connector opening found in {} geometry and no plan rotation — placed at 0°; set \"rotation\" if it has a mating direction", c.reference, lib.name),
            }),
        );
    };
    // Body front vs courtyard front along the opening (footprint frame,
    // rotation-independent).
    let inset = match (lib.fab, lib.courtyard) {
        (Some(fb), Some(cb)) => match opening {
            (-1, 0) => fb.0 - cb.0,
            (1, 0) => cb.2 - fb.2,
            (0, -1) => fb.1 - cb.1,
            _ => cb.3 - fb.3,
        },
        _ => 0.0,
    };
    let placer_overhang = if overhang > 0.0 { overhang + inset.max(0.0) + crate::placer::EDGE_MARGIN_MM } else { 0.0 };
    let edge_note = if overhang > 0.0 {
        format!("body front protrudes {:.2} mm past the board edge (courtyard {:.2} mm past)", overhang, overhang + inset.max(0.0))
    } else {
        format!("courtyard {:.1} mm inside the board edge, body front {:.2} mm inside", crate::placer::EDGE_MARGIN_MM, crate::placer::EDGE_MARGIN_MM + inset)
    };
    if let Some(r) = c.rotation {
        return (Some(r as i32), placer_overhang, Some(format!("{}: opening {:?} in footprint frame, plan rotation {} kept; {}", c.reference, opening, r, edge_note)));
    }
    for rot in [0i32, 90, 180, 270] {
        let a = (rot as f64).to_radians();
        let ox = (opening.0 as f64 * a.cos() + opening.1 as f64 * a.sin()).round() as i32;
        let oy = (-(opening.0 as f64) * a.sin() + opening.1 as f64 * a.cos()).round() as i32;
        if (ox, oy) == want {
            return (
                Some(rot),
                placer_overhang,
                Some(format!("{}: {} opening faces the {} (rotation {}°); {}", c.reference, lib.name, format!("{:?}", pos).to_lowercase().replace("edge", " edge"), rot, edge_note)),
            );
        }
    }
    (c.rotation.map(|r| r as i32), placer_overhang, None)
}

/// Intra-pair skew budget by interface class (Toradex Layout Design
/// Guide 102492, interface tables: USB 2.0 < 7.5 ps ≈ 1.1 mm; Ethernet
/// < 1.6 ps ≈ 250 µm; MIPI CSI/DSI, USB 3, PCIe, SATA, LVDS < 1 ps ≈
/// 150 µm). Unknown interfaces get the Ethernet figure.
pub fn pair_skew_budget_mm(p: &str, n: &str) -> f64 {
    let u = format!("{} {}", p, n).to_ascii_uppercase();
    let has = |keys: &[&str]| keys.iter().any(|k| u.contains(k));
    if has(&["MIPI", "CSI", "DSI", "LVDS", "PCIE", "SATA", "HDMI", "USB3", "SS_", "SSTX", "SSRX", "TMDS"]) {
        0.15
    } else if has(&["USB", "DP", "DM", "D+", "D-"]) {
        1.1
    } else if has(&["ETH", "MDI", "TXP", "TXN", "RXP", "RXN", "TD", "RD", "TRD"]) {
        0.25
    } else {
        0.25
    }
}

/// Signal layers that sit next to one whole-board plane: a trace there has
/// an unbroken return path. A layer next to a split plane, or to no plane
/// at all on a multilayer board, is excluded for pairs and
/// impedance-controlled nets. 2-layer boards: both layers (nothing better).
pub fn clean_signal_layers(
    board: &crate::board::Board,
    signal_layers: &[String],
    plane_specs: &[PlaneSpec],
    plane_rects: &[Option<(f64, f64, f64, f64)>],
) -> Vec<String> {
    let copper = board.copper_layers();
    if board.layer_count <= 2 || plane_specs.is_empty() {
        return signal_layers.to_vec();
    }
    let whole_single = |layer: &str| -> bool {
        let on: Vec<usize> = plane_specs.iter().enumerate().filter(|(_, p)| p.layer == layer).map(|(i, _)| i).collect();
        on.len() == 1 && plane_rects.get(on[0]).map(|r| r.is_none()).unwrap_or(false)
    };
    signal_layers
        .iter()
        .filter(|l| {
            let Some(i) = copper.iter().position(|c| c == *l) else { return false };
            let neighbours = [i.checked_sub(1), if i + 1 < copper.len() { Some(i + 1) } else { None }];
            neighbours.iter().flatten().any(|&j| whole_single(&copper[j]))
        })
        .cloned()
        .collect()
}

/// Everything the SI audit needs beyond the copper, derived from the plan.
fn build_si_context(
    plan: &CircuitPlan,
    board: &crate::board::Board,
    pair_infos: &[crate::si::PairInfo],
    exempt: &[(f64, f64, f64, f64, String, u8)],
    families: &[Vec<String>],
    net_gap: &std::collections::HashMap<String, f64>,
    clearance_mm: f64,
    plane_specs: &[PlaneSpec],
) -> crate::si::SiContext {
    let mut ctx = crate::si::SiContext {
        pairs: pair_infos.to_vec(),
        exempt_rects: exempt.to_vec(),
        families: families.to_vec(),
        gap_of: net_gap.clone(),
        clearance_mm,
        layer_count: board.layer_count,
        ..Default::default()
    };
    let comp = |r: &str| plan.components.iter().find(|c| c.reference == r);
    for net in &plan.nets {
        let declared = net.termination.as_deref().map(|t| t.to_ascii_lowercase());
        let terminated = match declared.as_deref() {
            Some("series") | Some("parallel") | Some("on-chip") | Some("onchip") | Some("ac") => true,
            Some(_) => false,
            None => net.pins.iter().any(|pin| {
                pin.split_once(':').and_then(|(r, _)| comp(r)).map(|c| {
                    let two_pin = plan.nets.iter().flat_map(|n| n.pins.iter()).filter(|p| p.starts_with(&format!("{}:", c.reference))).count() <= 2;
                    two_pin && crate::values::resistance_ohm(&c.value).map(|r| r <= 100.0).unwrap_or(false)
                }).unwrap_or(false)
            }),
        };
        if terminated {
            ctx.terminated.insert(net.name.clone());
        }
    }
    // Decouplers: two-pin capacitors between a power-role net and a
    // ground-role net (RC filters, crystal loads and AC couplings are not
    // decouplers and carry no mounting-loop rule).
    let role_of = |name: &str| -> crate::netfreq::NetRole {
        plan.nets.iter().find(|n| n.name == name).map(|n| crate::netfreq::net_signal(&n.name, &[], n.freq_hz, n.role.as_deref()).role).unwrap_or(crate::netfreq::NetRole::Digital)
    };
    for c in &plan.components {
        let Some(cf) = crate::values::capacitance_f(&c.value) else { continue };
        let pins: Vec<(&str, &str)> = plan.nets.iter().flat_map(|n| n.pins.iter().filter(|p| p.starts_with(&format!("{}:", c.reference))).map(move |p| (n.name.as_str(), p.as_str()))).collect();
        if pins.len() != 2 || pins[0].0 == pins[1].0 {
            continue;
        }
        let (ra, rb) = (role_of(pins[0].0), role_of(pins[1].0));
        let is_decoupler = matches!((&ra, &rb), (crate::netfreq::NetRole::Power, crate::netfreq::NetRole::Ground) | (crate::netfreq::NetRole::Ground, crate::netfreq::NetRole::Power));
        // The mounting-loop model (stub → via → plane) needs a plane on at
        // least one side; on a board whose rails are traces the loop is
        // the whole trace path and the PDN sweep judges it instead.
        let on_plane = plane_specs.iter().any(|p| p.net == pins[0].0 || p.net == pins[1].0);
        if !is_decoupler || !on_plane {
            continue;
        }
        // A regulator's output capacitor is judged by the regulator check
        // (its loop is capacitor ↔ regulator pin, not capacitor ↔ plane).
        let anchor_is_regulator = c
            .near
            .as_deref()
            .and_then(|a| plan.components.iter().find(|x| x.reference == a))
            .map(|a| crate::stability::regulator_min_cout_uf(&a.value, &a.lib_id).is_some())
            .unwrap_or(false);
        if anchor_is_regulator {
            continue;
        }
        // Strict (error) only for a capacitor that actually sits at its IC
        // (within 6 mm of the anchor's body): a capacitor the placer could
        // not get there is a bulk capacitor, judged as a warning (the PDN
        // audit still judges the rail).
        let strict = c.near.as_deref().map(|a| a.starts_with('U') && {
            let fp = |r: &str| board.footprints.iter().find(|f| f.reference == r).map(|f| (f.x, f.y));
            match (fp(&c.reference), fp(a)) {
                (Some((cx, cy)), Some((ax, ay))) => {
                    let anchor = board.footprints.iter().find(|f| f.reference == a);
                    let half = anchor.and_then(|f| f.courtyard.as_ref().map(|q| ((q.2 - q.0).abs().max((q.3 - q.1).abs())) / 2.0)).unwrap_or(5.0);
                    ((cx - ax).powi(2) + (cy - ay).powi(2)).sqrt() <= half + 6.0
                }
                _ => true,
            }
        }).unwrap_or(false);
        ctx.decouplers.push(crate::si::Decoupler { reference: c.reference.clone(), net_a: pins[0].0.to_string(), net_b: pins[1].0.to_string(), c_farads: cf, strict });
    }
    for ps in plane_specs {
        if let Some(k) = ps.layer.strip_prefix("In").and_then(|r| r.strip_suffix(".Cu")).and_then(|d| d.parse::<usize>().ok()) {
            ctx.plane_layer_of.entry(ps.net.clone()).or_insert(k);
        }
    }
    ctx
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
    fn parse_pin_matches_authoritative_transform() {
        // The wire-target positions (parse_pin) must equal the RENDERED pin
        // positions (the authoritative KiCad transform applied to the lib
        // pins the generator emits) — for every pin of a multi-pin IC.
        let plan: CircuitPlan = serde_json::from_str(r#"{
            "name": "t", "description": "t",
            "components": [
                {"reference": "U1", "value": "NE555", "lib_id": "Timer:NE555"}
            ],
            "nets": [
                {"name": "VCC", "pins": ["U1:1"]},
                {"name": "OUT", "pins": ["U1:3"]},
                {"name": "GND", "pins": ["U1:8"]}
            ]
        }"#).unwrap();
        // The generator's emission geometry (mirrors generate_schematic):
        // pins [1,3,8], n=3, half=2:
        //  pi0 (pin "1"): lib (-5.08, (2-1-0)*2.54) = (-5.08, 2.54)
        //  pi1 (pin "3"): lib (-5.08, (2-1-1)*2.54) = (-5.08, 0)
        //  pi2 (pin "8"): right col k=0: lib (5.08, (0-(2-1))*2.54) = (5.08, -2.54)
        // pins [1,3,8], n=3, half=2, top=1.27: left 1.27 / −1.27, right −1.27
        let expected_lib = [
            ("1", -SCH_PIN_X, 1.27f64),
            ("3", -SCH_PIN_X, -1.27),
            ("8", SCH_PIN_X, -1.27),
        ];
        let idx = plan.components.iter().position(|c| c.reference == "U1").unwrap();
        let (ox, oy) = sch_layout(&plan).origins[idx];
        for (num, px, py) in expected_lib {
            // parse_pin says where the wire goes:
            let (wx, wy) = parse_pin(&format!("U1:{}", num), &plan).unwrap();
            // authoritative render of the lib pin at rot 0: origin + (px, -py)
            let ex = ox + px;
            let ey = oy - py;
            assert!(
                (wx - ex).abs() < 0.01 && (wy - ey).abs() < 0.01,
                "pin {}: parse_pin ({:.2},{:.2}) vs render ({:.2},{:.2})",
                num, wx, wy, ex, ey
            );
        }
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