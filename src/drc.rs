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
    let mut findings = Vec::new();

    check_pad_outside_outline(board, &mut findings);
    check_pad_clearance(board, config, &mut findings);
    check_outline_closed(board, &mut findings);

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