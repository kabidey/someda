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
    let pcb_content = generate_board(&plan);

    let sch_path = project_dir.join("circuit.kicad_sch");
    let pcb_path = project_dir.join("circuit.kicad_pcb");

    fs::write(&sch_path, &sch_content)
        .map_err(|e| OrchestrateError::Io(e.to_string()))?;
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
        let x = 50.0 + (i % 2) as f64 * 60.0;
        let y = 50.0 + (i / 2) as f64 * 40.0;

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