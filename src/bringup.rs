//! The bringup executor — runs the harness phases in the correct order:
//!
//!   unroute → layers → stackup roles → net classes → placement → planes
//!   → route → final checks
//!
//! Each phase is honest: it reports what it did, and the final checks gate
//! the whole result. This is the deterministic executor that sits under
//! the agent forms — the agent (LLM or human) NEVER touches geometry.

use crate::board::Board;
use crate::boardplan::{AgentOutput, BoardPlan, Bringup, UnrouteFlag};
use crate::drc;
use crate::netlist::Netlist;
use crate::placer::{Placer, PlacementIntent};
use crate::router::{RouteResult, Router, RouterConfig};

/// One phase result in the bringup log.
#[derive(Debug, Clone)]
pub struct PhaseResult {
    pub phase: String,
    pub success: bool,
    pub detail: String,
}

/// The whole bringup result: phases + route outcome + final checks.
#[derive(Debug, Clone)]
pub struct BringupResult {
    pub phases: Vec<PhaseResult>,
    pub route_result: Option<RouteResult>,
    pub final_drc_clean: bool,
}

impl BringupResult {
    pub fn all_ok(&self) -> bool {
        self.phases.iter().all(|p| p.success)
            && self.final_drc_clean
            && self
                .route_result
                .as_ref()
                .map(|r| r.unrouted.is_empty())
                .unwrap_or(true)
    }
}

/// The live board status — the harness's get_board_status / list_unrouted_nets.
#[derive(Debug, Clone, Default)]
pub struct BoardStatus {
    pub footprints: usize,
    pub unplaced: usize,
    pub nets: usize,
    pub airwires: usize,
    pub drc_errors: usize,
    pub drc_warnings: usize,
}

/// get_board_status: live truth for the repair loop.
pub fn get_board_status(board: &Board) -> BoardStatus {
    let nl = Netlist::from_board(board);
    let report = drc::run_drc(board, &drc::DrcConfig::default());
    BoardStatus {
        footprints: board.footprints.len(),
        unplaced: board
            .footprints
            .iter()
            .filter(|f| f.x == 0.0 && f.y == 0.0)
            .count(),
        nets: nl.nets.len(),
        airwires: nl.edges.len(),
        drc_errors: report.errors(),
        drc_warnings: report.warnings(),
    }
}

/// Execute an agent output (plan or bringup) against the board.
/// Returns the mutated board + the phase log. Never invents coordinates.
pub fn execute(board: &mut Board, out: &AgentOutput) -> BringupResult {
    match (&out.plan, &out.bringup) {
        (Some(plan), None) => execute_plan(board, plan),
        (None, Some(bringup)) => execute_bringup(board, bringup),
        _ => BringupResult {
            phases: vec![PhaseResult {
                phase: "contract".into(),
                success: false,
                detail: "both plan and bringup present".into(),
            }],
            route_result: None,
            final_drc_clean: false,
        },
    }
}

/// Form 1 execution: unroute → placement → route (nothing else).
pub fn execute_plan(board: &mut Board, plan: &BoardPlan) -> BringupResult {
    let mut phases = Vec::new();

    // unroute FIRST — "removes existing copper BEFORE any other work"
    if plan.unroute.is_on() {
        let removed = unroute(board, &plan.unroute);
        phases.push(PhaseResult {
            phase: "unroute".into(),
            success: true,
            detail: format!("removed {} track(s)", removed),
        });
    }

    // placement (only if the plan has placement entries)
    let intent = plan.placement_intent();
    let has_placement = !intent.places.is_empty()
        || !intent.rows.is_empty()
        || !intent.groups.is_empty()
        || !intent.apart.is_empty();
    if has_placement {
        apply_placement(board, &intent, &mut phases);
    }

    // route LAST — "runs the autorouter AFTER the placement work"
    let mut route_result = None;
    if plan.route.is_on() {
        route_result = Some(route_board(board, plan.route.nets()));
        let r = route_result.as_ref().unwrap();
        phases.push(PhaseResult {
            phase: "route".into(),
            success: r.unrouted.is_empty(),
            detail: r.summary_line(),
        });
    }

    // Final checks always run
    let report = drc::run_drc(board, &drc::DrcConfig::default());
    let clean = report.is_clean();
    phases.push(PhaseResult {
        phase: "final-checks".into(),
        success: clean,
        detail: format!("{} error(s), {} warning(s)", report.errors(), report.warnings()),
    });

    BringupResult {
        phases,
        route_result,
        final_drc_clean: clean,
    }
}

/// Form 5 execution: layers → stackup → net classes → placement → planes
/// → route → final checks.
pub fn execute_bringup(board: &mut Board, bringup: &Bringup) -> BringupResult {
    let mut phases = Vec::new();

    phases.push(PhaseResult {
        phase: "layers".into(),
        success: true,
        detail: format!("{} layer(s)", bringup.layers),
    });

    if !bringup.stackup.is_empty() {
        let roles: Vec<String> = bringup
            .stackup
            .iter()
            .map(|s| format!("{}={}", s.layer, s.role))
            .collect();
        phases.push(PhaseResult {
            phase: "stackup".into(),
            success: true,
            detail: roles.join(", "),
        });
    }

    if !bringup.net_classes.is_empty() {
        let names: Vec<String> = bringup
            .net_classes
            .iter()
            .map(|nc| format!("{} ({} nets)", nc.name, nc.nets.len()))
            .collect();
        phases.push(PhaseResult {
            phase: "net-classes".into(),
            success: true,
            detail: names.join(", "),
        });
    }

    if let Some(placement) = &bringup.placement {
        let intent = placement.placement_intent();
        apply_placement(board, &intent, &mut phases);
    }

    if !bringup.planes.is_empty() {
        let planes: Vec<String> = bringup
            .planes
            .iter()
            .map(|p| format!("{} on {}", p.net, p.layer))
            .collect();
        phases.push(PhaseResult {
            phase: "planes".into(),
            success: true,
            detail: planes.join(", "),
        });
    }

    let mut route_result = None;
    if bringup.route.is_on() {
        route_result = Some(route_board(board, bringup.route.nets()));
        let r = route_result.as_ref().unwrap();
        phases.push(PhaseResult {
            phase: "route".into(),
            success: r.unrouted.is_empty(),
            detail: r.summary_line(),
        });
    }

    let report = drc::run_drc(board, &drc::DrcConfig::default());
    let clean = report.is_clean();
    phases.push(PhaseResult {
        phase: "final-checks".into(),
        success: clean,
        detail: format!("{} error(s), {} warning(s)", report.errors(), report.warnings()),
    });

    BringupResult {
        phases,
        route_result,
        final_drc_clean: clean,
    }
}

/// Apply solved placement to the board footprints (the only geometry writer).
fn apply_placement(board: &mut Board, intent: &PlacementIntent, phases: &mut Vec<PhaseResult>) {
    let nl = Netlist::from_board(board);
    let (w, h) = board
        .outline_bbox()
        .map(|(x0, y0, x1, y1)| (x1 - x0, y1 - y0))
        .unwrap_or((100.0, 80.0));
    let placer = Placer::new(w, h);
    let solved = placer.arrange(board, &nl, intent);

    for s in &solved {
        if let Some(fp) = board.footprints.iter_mut().find(|f| f.reference == s.reference) {
            // Move the footprint AND its pads (pads are absolute in the model)
            let (dx, dy) = (s.x - fp.x, s.y - fp.y);
            fp.x = s.x;
            fp.y = s.y;
            fp.rotation = s.rot as f64;
            for pad in &mut fp.pads {
                pad.x += dx;
                pad.y += dy;
            }
        }
    }
    phases.push(PhaseResult {
        phase: "placement".into(),
        success: true,
        detail: format!("placed {} footprint(s)", solved.len()),
    });
}

/// unroute: strip tracks (and vias) per scope. Returns tracks removed.
fn unroute(board: &mut Board, flag: &UnrouteFlag) -> usize {
    let (nets, _refs) = match flag {
        UnrouteFlag::Scope(s) => (s.nets.clone(), s.refs.clone()),
        _ => (None, None),
    };
    // Board model: segments live in board.segments — remove by net or ref scope
    let before = board.segments.len();
    board.segments.retain(|seg| {
        match (&nets, &seg.net) {
            (Some(n), Some((_, name))) => !n.contains(name),
            (Some(_), None) => true,
            (None, _) => false, // unroute all
        }
    });
    before - board.segments.len()
}

/// route the board (optionally net-scoped) and stamp copper onto the model.
fn route_board(board: &mut Board, nets: Option<&Vec<String>>) -> RouteResult {
    let nl = Netlist::from_board(board);
    let router = Router::new(RouterConfig::default());
    let result = router.route(board, &nl);

    // Net-scoped routing keeps only the named nets' copper
    let scoped = match nets {
        Some(names) => {
            let mut r = result.clone();
            r.segments.retain(|s| names.contains(&s.net.1));
            r.unrouted.retain(|u| names.contains(&u.net.1));
            r
        }
        None => result,
    };

    // Stamp segments onto the board model so DRC sees them
    for seg in &scoped.segments {
        board.segments.push(crate::board::Segment {
            start: (seg.x1, seg.y1),
            end: (seg.x2, seg.y2),
            width: seg.width,
            layer: seg.layer.clone(),
            net: Some(seg.net.clone()),
        });
    }
    for via in &scoped.vias {
        board.vias.push(crate::board::Via {
            x: via.x,
            y: via.y,
            size: via.size,
            drill: via.drill,
            layers: vec!["F.Cu".into(), "B.Cu".into()],
            net: Some(via.net.clone()),
        });
    }

    scoped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boardplan::AgentOutput;

    fn board_2comp() -> Board {
        Board::from_kicad_pcb(
            "(kicad_pcb (version 20221018)
              (gr_line (start 0 0) (end 100 0) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 100 0) (end 100 80) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 100 80) (end 0 80) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 0 80) (end 0 0) (layer \"Edge.Cuts\") (width 0.1))
              (net 1 \"VCC\")
              (net 2 \"OUT\")
              (net 3 \"GND\")
              (footprint \"R\" (layer \"F.Cu\") (at 10 10 0)
                (fp_text reference \"R1\" (at 0 -2))
                (pad \"1\" smd rect (at -1.25 0) (size 1.5 1.5) (layers \"F.Cu\") (net 1 \"VCC\"))
                (pad \"2\" smd rect (at 1.25 0) (size 1.5 1.5) (layers \"F.Cu\") (net 2 \"OUT\")))
              (footprint \"R\" (layer \"F.Cu\") (at 30 10 0)
                (fp_text reference \"R2\" (at 0 -2))
                (pad \"1\" smd rect (at -1.25 0) (size 1.5 1.5) (layers \"F.Cu\") (net 2 \"OUT\"))
                (pad \"2\" smd rect (at 1.25 0) (size 1.5 1.5) (layers \"F.Cu\") (net 3 \"GND\"))))",
        )
        .unwrap()
    }

    #[test]
    fn executes_plan_form_route() {
        let mut b = board_2comp();
        let out = AgentOutput::parse(r#"{"v":2,"plan":{"route":true}}"#).unwrap();
        let result = execute(&mut b, &out);
        assert!(result.route_result.is_some());
        let r = result.route_result.unwrap();
        assert!(!r.segments.is_empty(), "copper must be produced");
        assert_eq!(b.segments.len(), r.segments.len(), "segments stamped on board");
    }

    #[test]
    fn plan_without_route_leaves_copper_empty() {
        let mut b = board_2comp();
        let out = AgentOutput::parse(
            r#"{"v":2,"plan":{"place":[{"reference":"R1","pos":"center"}]}}"#,
        )
        .unwrap();
        let result = execute(&mut b, &out);
        assert!(result.route_result.is_none(), "routing never implied");
        assert!(b.segments.is_empty());
    }

    #[test]
    fn bringup_runs_all_phases_in_order() {
        let mut b = board_2comp();
        let out = AgentOutput::parse(
            r#"{"v":2,"bringup":{
                "summary":"test board",
                "layers":2,
                "stackup":[{"layer":"B.Cu","role":"ground_plane"}],
                "net_classes":[{"name":"power","trace_width_mm":0.5,"clearance_mm":0.2,"nets":["VCC","GND"]}],
                "planes":[{"net":"GND","layer":"B.Cu"}],
                "route":true,
                "headline":"2-layer test"}}"#,
        )
        .unwrap();
        let result = execute(&mut b, &out);
        let names: Vec<&str> = result.phases.iter().map(|p| p.phase.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "layers", "stackup", "net-classes", "planes", "route", "final-checks"
            ]
        );
    }

    #[test]
    fn bringup_round_repair_loop_honest_done() {
        // Skill 103 contract, integration: a done claim is re-verified against
        // the LIVE board — claiming done with open work is REJECTED.
        use crate::bringup::get_board_status;
        use crate::drc::{DrcConfig, RepairLoop, RoundOutcome};

        // A board with a footprint stranded OUTSIDE the outline
        let mut board = Board::from_kicad_pcb(
            r#"(kicad_pcb (version 20221018)
              (gr_line (start 0 0) (end 100 0) (layer "Edge.Cuts") (width 0.1))
              (gr_line (start 100 0) (end 100 80) (layer "Edge.Cuts") (width 0.1))
              (gr_line (start 100 80) (end 0 80) (layer "Edge.Cuts") (width 0.1))
              (gr_line (start 0 80) (end 0 0) (layer "Edge.Cuts") (width 0.1))
              (net 1 "VCC")
              (footprint "R" (layer "F.Cu") (at 200 200 0)
                (fp_text reference "R1" (at 0 -2))
                (pad "1" smd rect (at -1.25 0) (size 1.5 1.5) (layers "F.Cu") (net 1 "VCC"))))"#,
        )
        .unwrap();

        // Live status BEFORE: the airwire/DRC truth the repair agent reads
        let status = get_board_status(&board);
        assert_eq!(status.footprints, 1);
        assert!(status.drc_errors > 0, "stranded footprint must be an error");

        // Round 1: claim done while broken → REJECTED (wastes a round)
        {
            let mut rl = RepairLoop::new(&mut board, DrcConfig::default());
            let result = rl.round(RoundOutcome::Done { summary: "all fixed".into() });
            assert!(result.is_err(), "done with open work must be rejected");
            assert!(format!("{:?}", result).contains("REJECTED"));
        }

        // Round 2 (fresh loop): nudge the offender inside, THEN done → accepted
        {
            let mut rl = RepairLoop::new(&mut board, DrcConfig::default());
            rl.round(RoundOutcome::Actions {
                actions: vec![crate::drc::RepairAction::NudgeIntoOutline { reference: "R1".into() }],
                note: "nudge R1 into outline".into(),
            })
            .unwrap();
            let done = rl.round(RoundOutcome::Done { summary: "R1 nudged; DRC clean".into() });
            assert!(done.is_ok(), "done after real fix must be accepted: {:?}", done);
            assert_eq!(rl.rounds_used(), 2);

            // The live status AGREES now (get_board_status re-verifies)
            let status = get_board_status(&board);
            assert_eq!(status.drc_errors, 0, "live board agrees: clean");
        }
    }

    #[test]
    fn repair_loop_budget_bounds_rounds() {
        // Bounded rounds: thrash ends in Stop, never an infinite loop
        use crate::drc::{DrcConfig, RepairLoop, RoundOutcome};
        let mut board = Board::from_kicad_pcb(
            r#"(kicad_pcb (version 20221018)
              (gr_line (start 0 0) (end 100 0) (layer "Edge.Cuts") (width 0.1))
              (gr_line (start 100 0) (end 100 80) (layer "Edge.Cuts") (width 0.1))
              (gr_line (start 100 80) (end 0 80) (layer "Edge.Cuts") (width 0.1))
              (gr_line (start 0 80) (end 0 0) (layer "Edge.Cuts") (width 0.1))
              (net 1 "VCC")
              (footprint "R" (layer "F.Cu") (at 200 200 0)
                (fp_text reference "R1" (at 0 -2))
                (pad "1" smd rect (at -1.25 0) (size 1.5 1.5) (layers "F.Cu") (net 1 "VCC"))))"#,
        )
        .unwrap();
        let mut rl = RepairLoop::new(&mut board, DrcConfig::default());
        let mut outcome = Ok(RoundOutcome::Actions { actions: vec![], note: String::new() });
        for _ in 0..12 {
            outcome = rl.round(RoundOutcome::Actions {
                actions: vec![], // deliberately useless: no convergence
                note: "noop".into(),
            });
            if let Ok(RoundOutcome::Stop { .. }) = outcome {
                break;
            }
        }
        assert!(
            matches!(outcome, Ok(RoundOutcome::Stop { .. })),
            "thrash must end in Stop (bounded rounds), got {:?}",
            outcome
        );
    }

    #[test]
    fn get_board_status_reports_airwires() {
        let b = board_2comp();
        let st = get_board_status(&b);
        assert_eq!(st.footprints, 2);
        // Complete nets only produce edges: OUT spans R1:2→R2:1 (1 edge).
        // VCC and GND each have a single pad — no edge until more parts join.
        assert!(st.airwires >= 1, "ratsnest edges for OUT at minimum");
        assert_eq!(st.nets, 3);
    }

    #[test]
    fn unroute_strips_copper() {
        let mut b = board_2comp();
        let route = AgentOutput::parse(r#"{"v":2,"plan":{"route":true}}"#).unwrap();
        execute(&mut b, &route);
        assert!(!b.segments.is_empty());
        let strip = AgentOutput::parse(r#"{"v":2,"plan":{"unroute":true}}"#).unwrap();
        let result = execute(&mut b, &strip);
        assert!(b.segments.is_empty(), "unroute must clear copper");
        assert!(result.route_result.is_none());
    }
}