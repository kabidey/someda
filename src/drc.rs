//! DRC engine and the honest-done repair loop.
//!
//! The core idea from the architecture study: "done" is a fact the board
//! states, not a claim an agent makes. The engine re-verifies every
//! done-claim against the board itself.

use crate::board::Board;

#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Severity {
    Error,
    Warning,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Error => write!(f, "ERROR"),
            Severity::Warning => write!(f, "WARNING"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DrcReport {
    pub findings: Vec<Finding>,
}

impl DrcReport {
    pub fn errors(&self) -> usize {
        self.findings.iter().filter(|f| f.severity == Severity::Error).count()
    }
    pub fn warnings(&self) -> usize {
        self.findings.iter().filter(|f| f.severity == Severity::Warning).count()
    }
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct DrcConfig {
    pub min_clearance_mm: f64,
    pub edge_clearance_mm: f64,
    pub min_trace_width_mm: f64,
}

impl Default for DrcConfig {
    fn default() -> Self {
        DrcConfig {
            min_clearance_mm: 0.2,
            edge_clearance_mm: 0.25,
            min_trace_width_mm: 0.2,
        }
    }
}

pub fn run_drc(board: &Board, config: &DrcConfig) -> DrcReport {
    run_drc_with_classes(board, config, None)
}

pub fn run_drc_with_classes(board: &Board, config: &DrcConfig, classes: Option<&crate::netclass::NetClassTable>) -> DrcReport {
    let mut findings = Vec::new();

    check_pad_outside_outline(board, &mut findings);
    check_pad_clearance(board, config, &mut findings);
    check_outline_closed(board, &mut findings);
    check_trace_width(board, config, classes, &mut findings);
    check_segment_clearance(board, config, &mut findings);

    DrcReport { findings }
}

fn check_pad_outside_outline(board: &Board, findings: &mut Vec<Finding>) {
    if let Some((x0, y0, x1, y1)) = board.outline_bbox() {
        for fp in &board.footprints {
            for pad in &fp.pads {
                let inside = pad.x >= x0 && pad.x <= x1 && pad.y >= y0 && pad.y <= y1;
                if !inside {
                    findings.push(Finding {
                        severity: Severity::Error,
                        code: "pad-outside-board".into(),
                        message: format!(
                            "Pad {}:{} has copper outside the board outline. \
                             Move or rotate the footprint so its solder pads are supported; \
                             connector bodies may overhang.",
                            fp.reference, pad.number
                        ),
                    });
                }
            }
        }
    } else if !board.footprints.is_empty() {
        findings.push(Finding {
            severity: Severity::Error,
            code: "no-outline".into(),
            message: "Board has footprints but no Edge.Cuts outline.".into(),
        });
    }
}

fn check_pad_clearance(board: &Board, config: &DrcConfig, findings: &mut Vec<Finding>) {
    // Collect all pads with positions
    let mut all_pads: Vec<(String, String, f64, f64)> = Vec::new();
    for fp in &board.footprints {
        for pad in &fp.pads {
            all_pads.push((fp.reference.clone(), pad.number.clone(), pad.x, pad.y));
        }
    }
    // O(n^2) pairwise check (fine for the scales this tool targets)
    for i in 0..all_pads.len() {
        for j in (i + 1)..all_pads.len() {
            let (ref_a, num_a, ax, ay) = &all_pads[i];
            let (ref_b, num_b, bx, by) = &all_pads[j];
            if ref_a == ref_b {
                continue; // same footprint: pads are placed by the land pattern
            }
            let dx = ax - bx;
            let dy = ay - by;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist < config.min_clearance_mm && dist > 0.0 {
                findings.push(Finding {
                    severity: Severity::Error,
                    code: "pad-clearance".into(),
                    message: format!(
                        "Pads {}:{} and {}:{} are {:.3} mm apart (min {:.3} mm).",
                        ref_a, num_a, ref_b, num_b, dist, config.min_clearance_mm
                    ),
                });
            }
        }
    }
}

fn check_outline_closed(board: &Board, findings: &mut Vec<Finding>) {
    if board.outline.is_empty() {
        return; // handled by no-outline when footprints exist
    }
    if board.outline.len() < 3 {
        findings.push(Finding {
            severity: Severity::Error,
            code: "outline-not-closed".into(),
            message: format!(
                "Edge.Cuts outline has {} segment(s); a closed outline needs at least 3.",
                board.outline.len()
            ),
        });
    }
}

fn check_trace_width(board: &Board, config: &DrcConfig, classes: Option<&crate::netclass::NetClassTable>, findings: &mut Vec<Finding>) {
    for seg in &board.segments {
        let min_width = match (&classes, &seg.net) {
            (Some(t), Some((_, net_name))) => t.trace_width_for(net_name),
            _ => config.min_trace_width_mm,
        };
        if seg.width < min_width && seg.width > 0.0 {
            let net_desc = seg.net.as_ref()
                .map(|(_, n)| n.as_str())
                .unwrap_or("unnamed");
            findings.push(Finding {
                severity: Severity::Error,
                code: "trace-width".into(),
                message: format!(
                    "Trace on net '{}' (layer {}) is {:.3} mm wide (min {:.3} mm).{}",
                    net_desc, seg.layer, seg.width, min_width,
                    if let Some(cls) = classes.and_then(|c| c.class_of(net_desc)) {
                        format!(" Net class '{}'.", cls)
                    } else { String::new() }
                ),
            });
        }
    }
}

fn check_segment_clearance(board: &Board, config: &DrcConfig, findings: &mut Vec<Finding>) {
    use crate::geometry::{RotRect, roirect_segment_clearance};

    // Segment-to-segment clearance (same layer, different nets)
    let seg_count = board.segments.len();
    if seg_count > 200 {
        // O(n^2) pairwise: cap at 200 segments for now to avoid pathological runtime
        // on dense boards. Full spatial-indexing is an M2 item.
        findings.push(Finding {
            severity: Severity::Warning,
            code: "segment-check-truncated".into(),
            message: format!("Board has {} segments; pairwise clearance checked on the first 200 only (spatial index lands in M2).", seg_count),
        });
    }
    let check_n = seg_count.min(200);
    for i in 0..check_n {
        let a = &board.segments[i];
        for j in (i + 1)..check_n {
            let b = &board.segments[j];
            if a.layer != b.layer { continue; }
            let same_net = match (&a.net, &b.net) {
                (Some((_, x)), Some((_, y))) if !x.is_empty() && !y.is_empty() && x == y => true,
                (Some((ia, _)), Some((ib, _))) if ia == ib && *ia > 0 => true,
                _ => false,
            };
            if same_net { continue; }
            let clearance = segment_segment_distance(
                a.start, a.end, b.start, b.end,
            ).min(segment_segment_distance(b.start, b.end, a.start, a.end));
            // Subtract half-widths for edge-to-edge clearance
            let edge_gap = clearance - (a.width + b.width) / 2.0;
            if edge_gap < config.min_clearance_mm && edge_gap >= 0.0 {
                let na = a.net.as_ref().map(|(_, n)| n.as_str()).unwrap_or("?");
                let nb = b.net.as_ref().map(|(_, n)| n.as_str()).unwrap_or("?");
                findings.push(Finding {
                    severity: Severity::Error,
                    code: "segment-clearance".into(),
                    message: format!(
                        "Traces on nets '{}' and '{}' (layer {}) are {:.3} mm apart (min {:.3} mm).",
                        na, nb, a.layer, edge_gap, config.min_clearance_mm
                    ),
                });
            }
        }
    }

    // Segment-to-pad clearance: use the existing RotRect geometry
    let pad_list: Vec<(&crate::board::Footprint, &crate::board::Pad)> = board.footprints.iter()
        .flat_map(|fp| fp.pads.iter().map(move |p| (fp, p)))
        .collect();
    for seg in board.segments.iter().take(200) {
        for (fp, pad) in &pad_list {
            let same_net = match (&seg.net, &pad.net) {
                (Some((ia, a)), Some((ib, b))) if !a.is_empty() && !b.is_empty() && a == b => true,
                // Old-format files: net names may be empty; fall back to net index
                (Some((ia, _)), Some((ib, _))) if *ia == *ib && *ia > 0 => true,
                _ => false,
            };
            if same_net { continue; }
            let rect = RotRect::new(pad.x, pad.y, pad.width.max(0.01), pad.height.max(0.01), pad.rotation);
            let d = roirect_segment_clearance(
                &rect, seg.start.0, seg.start.1, seg.end.0, seg.end.1,
            ) - seg.width / 2.0;
            if d < config.min_clearance_mm && d >= 0.0 {
                let sn = seg.net.as_ref().map(|(_, n)| n.as_str()).unwrap_or("?");
                // Distance ~0 with an endpoint inside the pad = possible intentional
                // connection (jumpers, old-format files) — warn, don't hard-fail.
                let touching = d < 0.02 && (
                    crate::geometry::point_segment_distance(pad.x, pad.y, seg.start.0, seg.start.1, seg.end.0, seg.end.1) < pad.radius()
                );
                findings.push(Finding {
                    severity: if touching { Severity::Warning } else { Severity::Error },
                    code: "segment-pad-clearance".into(),
                    message: format!(
                        "Trace on net '{}' is {:.3} mm from pad {}:{} (min {:.3} mm).{}",
                        sn, d, fp.reference, pad.number, config.min_clearance_mm,
                        if touching { " Trace endpoint inside pad — check for intended connection (jumper) or short." } else { "" }
                    ),
                });
            }
        }
    }
}

/// Minimum distance between two segments (center-line to center-line).
fn segment_segment_distance(
    a1: (f64, f64), a2: (f64, f64),
    b1: (f64, f64), b2: (f64, f64),
) -> f64 {
    use crate::geometry::point_segment_distance;
    let mut d = f64::INFINITY;
    // endpoint-to-segment
    d = d.min(point_segment_distance(a1.0, a1.1, b1.0, b1.1, b2.0, b2.1));
    d = d.min(point_segment_distance(a2.0, a2.1, b1.0, b1.1, b2.0, b2.1));
    d = d.min(point_segment_distance(b1.0, b1.1, a1.0, a1.1, a2.0, a2.1));
    d = d.min(point_segment_distance(b2.0, b2.1, a1.0, a1.1, a2.0, a2.1));
    d
}

// ---------------------------------------------------------------------------
// The honest-done repair loop
// ---------------------------------------------------------------------------

/// A repair action the loop can propose.
#[derive(Debug, Clone)]
pub enum RepairAction {
    MoveFootprint { reference: String, dx: f64, dy: f64 },
    NudgeIntoOutline { reference: String },
}

/// The outcome of a repair round: either continue with actions, or claim done.
#[derive(Debug, Clone)]
pub enum RoundOutcome {
    Actions { actions: Vec<RepairAction>, note: String },
    Done { summary: String },
    Stop { reason: String },
}

/// The repair loop state. Enforces the honest-done contract:
/// a Done claim is only accepted if the board's own DRC agrees.
pub struct RepairLoop<'a> {
    board: &'a mut Board,
    config: DrcConfig,
    rounds: usize,
    max_rounds: usize,
    history: Vec<DrcReport>,
}

impl<'a> RepairLoop<'a> {
    pub fn new(board: &'a mut Board, config: DrcConfig) -> Self {
        RepairLoop { board, config, rounds: 0, max_rounds: 8, history: Vec::new() }
    }

    /// Run one round: re-inspect the board, propose or accept.
    pub fn round(&mut self, proposal: RoundOutcome) -> Result<RoundOutcome, String> {
        let report = run_drc(self.board, &self.config);
        self.history.push(report.clone());
        self.rounds += 1;

        match proposal {
            RoundOutcome::Done { summary } => {
                // The honest-done contract: verify against the live board.
                if report.is_clean() {
                    Ok(RoundOutcome::Done { summary })
                } else {
                    Err(format!(
                        "done-claim REJECTED: the board still has {} error(s), {} warning(s). \
                         Fix the remaining findings and claim done again. (Your summary was: {})",
                        report.errors(), report.warnings(), summary
                    ))
                }
            }
            RoundOutcome::Actions { actions, note } => {
                if self.rounds >= self.max_rounds {
                    return Ok(RoundOutcome::Stop {
                        reason: format!("round budget exhausted ({} rounds); escalating.", self.max_rounds),
                    });
                }
                for action in actions {
                    self.apply(action)?;
                }
                Ok(RoundOutcome::Actions { actions: Vec::new(), note })
            }
            other => Ok(other),
        }
    }

    pub fn report(&self) -> &DrcReport {
        self.history.last().expect("no round run yet")
    }

    pub fn rounds_used(&self) -> usize {
        self.rounds
    }

    fn apply(&mut self, action: RepairAction) -> Result<(), String> {
        match action {
            RepairAction::MoveFootprint { reference, dx, dy } => {
                let fp = self.board.footprints.iter_mut()
                    .find(|f| f.reference == reference)
                    .ok_or_else(|| format!("footprint '{}' not found", reference))?;
                fp.x += dx;
                fp.y += dy;
                for pad in &mut fp.pads {
                    pad.x += dx;
                    pad.y += dy;
                }
                Ok(())
            }
            RepairAction::NudgeIntoOutline { reference } => {
                let bbox = self.board.outline_bbox()
                    .ok_or("cannot nudge: board has no outline")?;
                let (x0, y0, x1, y1) = bbox;
                let fp = self.board.footprints.iter_mut()
                    .find(|f| f.reference == reference)
                    .ok_or_else(|| format!("footprint '{}' not found", reference))?;
                // Clamp footprint origin into the outline (pads follow)
                let nx = fp.x.clamp(x0 + 1.0, x1 - 1.0);
                let ny = fp.y.clamp(y0 + 1.0, y1 - 1.0);
                let (dx, dy) = (nx - fp.x, ny - fp.y);
                fp.x = nx;
                fp.y = ny;
                for pad in &mut fp.pads {
                    pad.x += dx;
                    pad.y += dy;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board_with_pad_outside() -> Board {
        // Pad at (150, 15) with outline (0,0)-(100,100): outside!
        Board::from_kicad_pcb(r#"(kicad_pcb
          (gr_line (start 0 0) (end 100 0) (layer "Edge.Cuts"))
          (gr_line (start 100 0) (end 100 100) (layer "Edge.Cuts"))
          (gr_line (start 100 100) (end 0 100) (layer "Edge.Cuts"))
          (gr_line (start 0 100) (end 0 0) (layer "Edge.Cuts"))
          (footprint "somlib:R0805" (at 150 15 0)
            (fp_text reference "R1" (at 0 0) (layer "F.SilkS"))
            (pad "1" smd rect (at -1 0) (size 1.133 1.377) (net 1 "A"))
          )
        )"#).unwrap()
    }

    #[test]
    fn drc_catches_pad_outside() {
        let b = board_with_pad_outside();
        let report = run_drc(&b, &DrcConfig::default());
        assert_eq!(report.errors(), 1);
        assert!(report.findings[0].code == "pad-outside-board");
    }

    #[test]
    fn drc_clean_board() {
        let b = Board::from_kicad_pcb(r#"(kicad_pcb
          (gr_line (start 0 0) (end 100 0) (layer "Edge.Cuts"))
          (gr_line (start 100 0) (end 100 100) (layer "Edge.Cuts"))
          (gr_line (start 100 100) (end 0 100) (layer "Edge.Cuts"))
          (gr_line (start 0 100) (end 0 0) (layer "Edge.Cuts"))
          (footprint "somlib:R0805" (at 50 50 0)
            (fp_text reference "R1" (at 0 0) (layer "F.SilkS"))
            (pad "1" smd rect (at -1 0) (size 1.133 1.377) (net 1 "A"))
          )
        )"#).unwrap();
        let report = run_drc(&b, &DrcConfig::default());
        assert!(report.is_clean(), "{:?}", report.findings);
    }

    #[test]
    fn done_claim_rejected_when_dirty() {
        let mut b = board_with_pad_outside();
        let mut rl = RepairLoop::new(&mut b, DrcConfig::default());
        let result = rl.round(RoundOutcome::Done { summary: "all fixed!".into() });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("REJECTED"));
    }

    #[test]
    fn repair_loop_converges() {
        let mut b = board_with_pad_outside();
        let mut rl = RepairLoop::new(&mut b, DrcConfig::default());

        // Round 1: inspect + nudge the stray footprint
        let out = rl.round(RoundOutcome::Actions {
            actions: vec![RepairAction::NudgeIntoOutline { reference: "R1".into() }],
            note: "nudging R1 inside outline".into(),
        }).unwrap();
        assert!(matches!(out, RoundOutcome::Actions { .. }));

        // Round 2: claim done — must now pass the board's own verification
        let out = rl.round(RoundOutcome::Done { summary: "R1 nudged; DRC clean".into() }).unwrap();
        assert!(matches!(out, RoundOutcome::Done { .. }));
        assert!(rl.report().is_clean());
    }
}