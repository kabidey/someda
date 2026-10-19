//! Engine-driven repair loop.
//!
//! The plan author (an LLM of any size, or a human) writes intent; the
//! engine builds, judges its own gates, and fixes what a plan edit can
//! fix — bounded, deterministic, printed as `repair:` lines — then builds
//! again. What it cannot fix it prints as `suggest:` lines carrying the
//! exact plan edit. The plan it finally built is written next to the
//! project as `plan.built.json`.

use std::collections::HashSet;
use std::path::Path;

use crate::kicad_cli::KiCadCli;
use crate::orchestrator::{build_with_plan, BoardSpec, BuildResult, CircuitPlan, OrchestrateError, PlannedComponent};

/// Something a build found that a plan edit can address.
#[derive(Debug, Clone, PartialEq)]
pub enum RepairHint {
    /// A component's footprint resolved to the synthetic fallback.
    FootprintMissing { reference: String, lib_id: String },
    /// ERC: an IC rail with no decoupling capacitor on it.
    MissingDecoupling { net: String },
    /// A regulator's output carries less capacitance than its datasheet minimum.
    RegulatorOutputCap { regulator: String, net: String, need_uf: f64 },
    /// Connections the router could not complete.
    Unrouted(usize),
    /// Courtyard overlaps after placement.
    Overlap(usize),
    /// A declared differential pair could not be routed as a coupled pair.
    PairFailed,
}

/// The outcome of applying hints to a plan.
#[derive(Debug, Default)]
pub struct Applied {
    pub plan: Option<CircuitPlan>,
    /// Human-readable `repair:` lines, one per edit made.
    pub applied: Vec<String>,
    /// `suggest:` lines: edits only the plan author can make.
    pub suggestions: Vec<String>,
}

/// Default number of repair rounds after the first build (`SOMEDA_REPAIR_ROUNDS`).
pub const DEFAULT_ROUNDS: usize = 3;

/// Build, repair, rebuild — up to `SOMEDA_REPAIR_ROUNDS` repair rounds
/// (default 3) — and return the last build.
pub fn build_with_repairs(cli: &KiCadCli, project_dir: &Path, plan: CircuitPlan) -> Result<BuildResult, OrchestrateError> {
    let rounds: usize = std::env::var("SOMEDA_REPAIR_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_ROUNDS);
    let mut plan = plan;
    let mut grown = 0usize;
    let mut round = 0usize;
    loop {
        if round > 0 {
            println!("─── Repair round {} of {} ───", round, rounds);
        }
        let result = build_with_plan(cli, project_dir, plan.clone())?;
        write_built_plan(project_dir, &result.plan);
        if result.honest_done || round >= rounds {
            if !result.honest_done && !result.repairs.is_empty() {
                let out = apply_repairs(&result.plan, &result.repairs, result.board_mm, &mut grown);
                for s in out.suggestions.iter().chain(out.applied.iter()) {
                    println!("  suggest: {} (repair rounds exhausted: SOMEDA_REPAIR_ROUNDS={})", s, rounds);
                }
            }
            return Ok(result);
        }
        let out = apply_repairs(&result.plan, &result.repairs, result.board_mm, &mut grown);
        for s in &out.suggestions {
            println!("  suggest: {}", s);
        }
        let Some(next) = out.plan else { return Ok(result) };
        if out.applied.is_empty() {
            return Ok(result);
        }
        for a in &out.applied {
            println!("  repair: {}", a);
        }
        plan = next;
        round += 1;
    }
}

/// `plan.built.json`: the plan exactly as the engine built it (rotations,
/// edge moves, grown plane regions, added capacitors, substituted
/// footprints).
pub fn write_built_plan(project_dir: &Path, plan: &CircuitPlan) {
    if std::fs::create_dir_all(project_dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(plan) {
        let _ = std::fs::write(project_dir.join("plan.built.json"), text);
    }
}

/// Apply every hint the engine can act on. `grown` counts board growths
/// across rounds (at most 2).
pub fn apply_repairs(plan: &CircuitPlan, hints: &[RepairHint], board_mm: (f64, f64), grown: &mut usize) -> Applied {
    let mut plan = plan.clone();
    let mut out = Applied::default();
    let ground = ground_net(&plan);
    let layers = plan.layers.unwrap_or(2);
    let mut seen_decap: HashSet<(String, String)> = HashSet::new();
    let mut seen_reg: HashSet<String> = HashSet::new();
    let mut board_grown = false;
    let mut uniq: Vec<&RepairHint> = Vec::new();
    for h in hints {
        if !uniq.contains(&h) {
            uniq.push(h);
        }
    }
    for h in uniq {
        match h {
            RepairHint::FootprintMissing { reference, lib_id } => match crate::footprint_lib::closest(lib_id) {
                Some(found) => {
                    if let Some(c) = plan.components.iter_mut().find(|c| &c.reference == reference) {
                        c.footprint = Some(found.clone());
                        out.applied.push(format!("{}: footprint \"{}\" is in no library — using the closest library footprint \"{}\"", reference, lib_id, found));
                    }
                }
                None => out.suggestions.push(format!(
                    "{}: footprint \"{}\" not found and nothing close — set \"lcsc\" or \"mpn\" and run fetch-libs, or name a KiCad footprint as \"Lib:Name\"",
                    reference, lib_id
                )),
            },
            RepairHint::MissingDecoupling { net } => {
                let mut ics: Vec<String> = plan
                    .nets
                    .iter()
                    .filter(|n| &n.name == net)
                    .flat_map(|n| n.pins.iter())
                    .filter_map(|p| p.split_once(':').map(|(r, _)| r.to_string()))
                    .filter(|r| r.starts_with('U'))
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();
                ics.sort();
                let Some(g) = ground.clone() else {
                    out.suggestions.push(format!("{}: no decoupling capacitor and no net named GND to return it to — add one", net));
                    continue;
                };
                if ics.is_empty() {
                    out.suggestions.push(format!("{}: no decoupling capacitor — add 100nF between {} and {} next to the part that needs it", net, net, g));
                    continue;
                }
                for ic in ics {
                    if !seen_decap.insert((net.clone(), ic.clone())) {
                        continue;
                    }
                    let r = next_reference(&plan, 'C');
                    let footprint = if layers >= 4 { "Capacitor_SMD:C_0402_1005Metric" } else { "Capacitor_SMD:C_0603_1608Metric" };
                    add_capacitor(&mut plan, &r, "100nF", footprint, &ic, net, &g);
                    out.applied.push(format!("added {} 100nF between {} and {} at {} (ERC: no decoupling capacitor on {})", r, net, g, ic, net));
                }
            }
            RepairHint::RegulatorOutputCap { regulator, net, need_uf } => {
                if !seen_reg.insert(regulator.clone()) {
                    continue;
                }
                let Some(g) = ground.clone() else {
                    out.suggestions.push(format!("{}: output {} needs ≥ {:.0} µF and there is no net named GND — add the capacitor", regulator, net, need_uf));
                    continue;
                };
                let uf = e6_at_least(*need_uf);
                let value = if (uf - uf.round()).abs() < 1e-9 { format!("{}uF", uf.round() as i64) } else { format!("{}uF", uf) };
                let footprint = if uf <= 4.7 { "Capacitor_SMD:C_0603_1608Metric" } else if uf <= 22.0 { "Capacitor_SMD:C_0805_2012Metric" } else { "Capacitor_SMD:C_1206_3216Metric" };
                let r = next_reference(&plan, 'C');
                add_capacitor(&mut plan, &r, &value, footprint, regulator, net, &g);
                out.applied.push(format!("added {} {} between {} and {} at {} (regulator needs ≥ {:.0} µF on its output)", r, value, net, g, regulator, need_uf));
            }
            RepairHint::Unrouted(_) | RepairHint::Overlap(_) | RepairHint::PairFailed => {
                let reason = match h {
                    RepairHint::Unrouted(n) => format!("{} connection(s) unrouted", n),
                    RepairHint::Overlap(n) => format!("{} courtyard overlap(s)", n),
                    _ => "a differential pair could not be routed as a coupled pair".to_string(),
                };
                let (w, h) = (board_mm.0 * 1.15, board_mm.1 * 1.15);
                if board_grown {
                    continue;
                }
                if plan.board.is_none() && *grown < 2 && board_mm.0 > 0.0 {
                    plan.board = Some(BoardSpec { width_mm: (w * 10.0).round() / 10.0, height_mm: (h * 10.0).round() / 10.0 });
                    *grown += 1;
                    board_grown = true;
                    out.applied.push(format!("board grown to {:.1}x{:.1} mm ({})", w, h, reason));
                } else {
                    out.suggestions.push(format!(
                        "{}: grow the board (\"board\": {{\"width_mm\": {:.0}, \"height_mm\": {:.0}}}) or relax the placement intent (position / near / edge_at)",
                        reason, w, h
                    ));
                }
            }
        }
    }
    out.plan = Some(plan);
    out
}

/// The plan's ground net: GND, else a conventional ground name.
pub fn ground_net(plan: &CircuitPlan) -> Option<String> {
    if plan.nets.iter().any(|n| n.name == "GND") {
        return Some("GND".into());
    }
    plan.nets
        .iter()
        .find(|n| matches!(n.name.to_ascii_uppercase().as_str(), "GNDD" | "VSS" | "AGND" | "DGND" | "GNDA" | "0V"))
        .map(|n| n.name.clone())
}

fn next_reference(plan: &CircuitPlan, prefix: char) -> String {
    let max = plan
        .components
        .iter()
        .filter_map(|c| c.reference.strip_prefix(prefix).and_then(|n| n.parse::<u32>().ok()))
        .max()
        .unwrap_or(0);
    format!("{}{}", prefix, max + 1)
}

fn add_capacitor(plan: &mut CircuitPlan, reference: &str, value: &str, footprint: &str, near: &str, net_a: &str, net_b: &str) {
    plan.components.push(PlannedComponent {
        reference: reference.to_string(),
        value: value.to_string(),
        lib_id: "Device:C".into(),
        footprint: Some(footprint.to_string()),
        x_mm: None,
        y_mm: None,
        power_w: None,
        position: None,
        rotation: None,
        near: Some(near.to_string()),
        edge_at: None,
        lcsc: None,
        mpn: None,
        edge_overhang_mm: None,
    });
    for (net, pin) in [(net_a, "1"), (net_b, "2")] {
        if let Some(n) = plan.nets.iter_mut().find(|n| n.name == net) {
            n.pins.push(format!("{}:{}", reference, pin));
        }
    }
}

/// The smallest E6 capacitance (µF) at or above `uf`.
pub fn e6_at_least(uf: f64) -> f64 {
    for v in [1.0, 2.2, 4.7, 10.0, 22.0, 47.0, 100.0, 220.0, 470.0] {
        if v + 1e-9 >= uf {
            return v;
        }
    }
    (uf / 100.0).ceil() * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::PlannedNet;

    fn plan() -> CircuitPlan {
        serde_json::from_str(
            r#"{"name":"t","description":"t","layers":2,
            "components":[
              {"reference":"U1","value":"NE555","lib_id":"Timer:NE555","footprint":"Package_DIP:DIP-8_W7.62mm"},
              {"reference":"R1","value":"10k","lib_id":"Device:R","footprint":"Resistor_SMD:R_0603_1608Metri"}],
            "nets":[{"name":"VCC","pins":["U1:8","R1:1"]},{"name":"GND","pins":["U1:1","R1:2"]}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn missing_decoupling_adds_a_100nf_capacitor_at_the_ic() {
        let p = plan();
        let mut grown = 0;
        let out = apply_repairs(&p, &[RepairHint::MissingDecoupling { net: "VCC".into() }], (40.0, 30.0), &mut grown);
        let np = out.plan.unwrap();
        let c = np.components.iter().find(|c| c.reference == "C1").expect("C1 added");
        assert_eq!(c.value, "100nF");
        assert_eq!(c.near.as_deref(), Some("U1"));
        assert!(np.nets.iter().find(|n| n.name == "VCC").unwrap().pins.contains(&"C1:1".to_string()));
        assert!(np.nets.iter().find(|n| n.name == "GND").unwrap().pins.contains(&"C1:2".to_string()));
        assert_eq!(out.applied.len(), 1);
    }

    #[test]
    fn regulator_output_cap_rounds_up_to_e6_and_board_grows_once_without_fixed_size() {
        let mut p = plan();
        p.nets.push(serde_json::from_str::<PlannedNet>(r#"{"name":"+3V3","pins":["U1:3"]}"#).unwrap());
        let mut grown = 0;
        let hints = [RepairHint::RegulatorOutputCap { regulator: "U1".into(), net: "+3V3".into(), need_uf: 1.5 }, RepairHint::Unrouted(2), RepairHint::Overlap(1)];
        let out = apply_repairs(&p, &hints, (40.0, 30.0), &mut grown);
        let np = out.plan.unwrap();
        assert_eq!(np.components.last().unwrap().value, "2.2uF");
        let b = np.board.as_ref().expect("board grown");
        assert!((b.width_mm - 46.0).abs() < 0.11 && (b.height_mm - 34.5).abs() < 0.11, "{:?}", b);
        assert_eq!(grown, 1);
        assert_eq!(out.applied.len(), 2, "{:?}", out.applied);
    }

    #[test]
    fn fixed_board_size_is_suggested_not_changed() {
        let mut p = plan();
        p.board = Some(BoardSpec { width_mm: 40.0, height_mm: 30.0 });
        let mut grown = 0;
        let out = apply_repairs(&p, &[RepairHint::Unrouted(1)], (40.0, 30.0), &mut grown);
        assert!(out.applied.is_empty());
        assert_eq!(out.suggestions.len(), 1);
        assert!(out.plan.unwrap().board.unwrap().width_mm == 40.0);
    }

    #[test]
    fn footprint_typo_is_replaced_by_the_closest_library_footprint() {
        if crate::footprint_lib::resolve("Resistor_SMD:R_0603_1608Metric").is_none() {
            return; // no KiCad library on this machine
        }
        let p = plan();
        let mut grown = 0;
        let out = apply_repairs(&p, &[RepairHint::FootprintMissing { reference: "R1".into(), lib_id: "Resistor_SMD:R_0603_1608Metri".into() }], (40.0, 30.0), &mut grown);
        let np = out.plan.unwrap();
        assert_eq!(np.components[1].footprint.as_deref(), Some("Resistor_SMD:R_0603_1608Metric"));
    }

    #[test]
    fn e6_rounding() {
        assert_eq!(e6_at_least(1.0), 1.0);
        assert_eq!(e6_at_least(1.5), 2.2);
        assert_eq!(e6_at_least(10.0), 10.0);
        assert_eq!(e6_at_least(15.0), 22.0);
    }
}
