//! The `Verified` trait: the honest-done contract generalized across all
//! verification engines. "Done" is a fact the design states, not a claim
//! an agent makes — every done-claim is re-verified against live state.

use crate::board::Board;
use crate::drc::{DrcConfig, DrcReport, RepairLoop, RoundOutcome};
use crate::schematic::{run_erc, ErcReport, Schematic};

/// The honest-done contract. Implementors verify against live design state;
/// `claim_done` only succeeds if the state itself agrees.
pub trait Verified {
    type Report;

    /// Run the verification against the current live state.
    fn verify(&self) -> Self::Report;

    /// Does the report represent "done" (no open work)?
    fn is_done(report: &Self::Report) -> bool;

    /// Claim done: the state decides, not the caller.
    /// Returns Ok(summary) if verified, Err(reason) with the open work if not.
    fn claim_done(&self, summary: &str) -> Result<(), String> {
        let report = self.verify();
        if Self::is_done(&report) {
            Ok(())
        } else {
            Err(format!(
                "done-claim REJECTED: {}\n  claimed summary: \"{}\"",
                Self::open_work_description(&report),
                summary
            ))
        }
    }

    /// Human-readable description of what remains open.
    fn open_work_description(report: &Self::Report) -> String;
}

/// A board under DRC verification.
pub struct BoardCheck<'a> {
    pub board: &'a Board,
    pub config: DrcConfig,
}

impl<'a> Verified for BoardCheck<'a> {
    type Report = DrcReport;

    fn verify(&self) -> DrcReport {
        crate::drc::run_drc(self.board, &self.config)
    }

    fn is_done(report: &DrcReport) -> bool {
        report.is_clean()
    }

    fn open_work_description(report: &DrcReport) -> String {
        format!("board still has {} error(s), {} warning(s)", report.errors(), report.warnings())
    }
}

/// A schematic under ERC verification.
pub struct SchematicCheck<'a> {
    pub schematic: &'a Schematic,
}

impl<'a> Verified for SchematicCheck<'a> {
    type Report = ErcReport;

    fn verify(&self) -> ErcReport {
        run_erc(self.schematic)
    }

    fn is_done(report: &ErcReport) -> bool {
        report.is_clean()
    }

    fn open_work_description(report: &ErcReport) -> String {
        format!("schematic still has {} ERC finding(s)", report.errors())
    }
}

/// A full project: both board (DRC) and schematic (ERC) must be clean.
pub struct ProjectCheck<'a> {
    pub board: Option<&'a Board>,
    pub schematic: Option<&'a Schematic>,
    pub config: DrcConfig,
}

#[derive(Debug)]
pub struct ProjectReport {
    pub drc: Option<DrcReport>,
    pub erc: Option<ErcReport>,
}

impl ProjectReport {
    pub fn is_clean(&self) -> bool {
        self.drc.as_ref().map(|d| d.is_clean()).unwrap_or(true)
            && self.erc.as_ref().map(|e| e.is_clean()).unwrap_or(true)
    }
}

impl<'a> Verified for ProjectCheck<'a> {
    type Report = ProjectReport;

    fn verify(&self) -> ProjectReport {
        ProjectReport {
            drc: self.board.map(|b| crate::drc::run_drc(b, &self.config)),
            erc: self.schematic.map(|s| run_erc(s)),
        }
    }

    fn is_done(report: &ProjectReport) -> bool {
        report.is_clean()
    }

    fn open_work_description(report: &ProjectReport) -> String {
        let mut parts = Vec::new();
        if let Some(drc) = &report.drc {
            if !drc.is_clean() {
                parts.push(format!("DRC: {} error(s), {} warning(s)", drc.errors(), drc.warnings()));
            }
        }
        if let Some(erc) = &report.erc {
            if !erc.is_clean() {
                parts.push(format!("ERC: {} finding(s)", erc.errors()));
            }
        }
        if parts.is_empty() {
            "no open work".to_string()
        } else {
            parts.join("; ")
        }
    }
}

/// A bounded repair session over a board: rounds of actions, then done-claims
/// verified against live state. This generalizes drc::RepairLoop with the
/// trait so callers can treat any Verified uniformly.
pub struct Session<'a> {
    board: &'a mut Board,
    config: DrcConfig,
    rounds: usize,
    max_rounds: usize,
}

impl<'a> Session<'a> {
    pub fn new(board: &'a mut Board, config: DrcConfig) -> Self {
        Session { board, config, rounds: 0, max_rounds: 8 }
    }

    /// Run one repair round (delegates to drc::RepairLoop semantics).
    pub fn round(&mut self, proposal: RoundOutcome) -> Result<RoundOutcome, String> {
        self.rounds += 1;
        let mut rl = RepairLoop::new(self.board, self.config.clone());
        let out = rl.round(proposal)?;
        if self.rounds >= self.max_rounds {
            return Ok(RoundOutcome::Stop {
                reason: format!("round budget exhausted ({} rounds)", self.max_rounds),
            });
        }
        Ok(out)
    }

    /// The honest-done claim: re-verified against the live board.
    pub fn claim_done(&mut self, summary: &str) -> Result<(), String> {
        let check = BoardCheck { board: self.board, config: self.config.clone() };
        check.claim_done(summary)
    }

    pub fn rounds_used(&self) -> usize {
        self.rounds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_board() -> Board {
        Board::from_kicad_pcb(r#"(kicad_pcb
          (gr_line (start 0 0) (end 100 0) (layer "Edge.Cuts"))
          (gr_line (start 100 0) (end 100 100) (layer "Edge.Cuts"))
          (gr_line (start 100 100) (end 0 100) (layer "Edge.Cuts"))
          (gr_line (start 0 100) (end 0 0) (layer "Edge.Cuts"))
          (footprint "somlib:R0805" (at 50 50 0)
            (fp_text reference "R1" (at 0 0) (layer "F.SilkS"))
            (pad "1" smd rect (at -1 0) (size 1.133 1.377) (net 1 "A"))
          )
        )"#).unwrap()
    }

    fn dirty_board() -> Board {
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

    fn clean_schematic() -> Schematic {
        // Two resistors, both pins of both connected:
        // Net 1: R1:1 (94.92,50) --wire-- R2:1 (114.92,50)... but R2 is at (120,50) so R2:1 = (114.92,50) ✓
        // Net 2: R1:2 (105.08,50) --wire-- R2:2 (125.08,50)
        Schematic::from_kicad_sch(r##"(kicad_sch
          (lib_symbols
            (symbol "Device:R"
              (symbol "R_0_1"
                (pin passive line (at -5.08 0 0) (length 3.81) (name "~") (number "1"))
                (pin passive line (at 5.08 0 180) (length 3.81) (name "~") (number "2"))
              )
            )
          )
          (symbol (lib_id "Device:R") (at 100 50 0)
            (property "Reference" "R1" (at 0 0 0))
          )
          (symbol (lib_id "Device:R") (at 120 50 0)
            (property "Reference" "R2" (at 0 0 0))
          )
          (wire (pts (xy 94.92 50) (xy 114.92 50)))
          (wire (pts (xy 105.08 50) (xy 125.08 50)))
        )"##).unwrap()
    }

    #[test]
    fn board_done_claim_accepted_on_clean() {
        let b = clean_board();
        let check = BoardCheck { board: &b, config: DrcConfig::default() };
        assert!(check.claim_done("all clean").is_ok());
    }

    #[test]
    fn board_done_claim_rejected_on_dirty() {
        let b = dirty_board();
        let check = BoardCheck { board: &b, config: DrcConfig::default() };
        let err = check.claim_done("all clean").unwrap_err();
        assert!(err.contains("REJECTED"));
        assert!(err.contains("error(s)"));
    }

    #[test]
    fn schematic_done_claim_accepted_on_clean() {
        let s = clean_schematic();
        let check = SchematicCheck { schematic: &s };
        assert!(check.claim_done("erc clean").is_ok());
    }

    #[test]
    fn project_check_requires_both_clean() {
        let dirty = dirty_board();
        let clean = clean_schematic();
        let project = ProjectCheck {
            board: Some(&dirty),
            schematic: Some(&clean),
            config: DrcConfig::default(),
        };
        let err = project.claim_done("project done").unwrap_err();
        assert!(err.contains("DRC"));

        let clean_b = clean_board();
        let project_ok = ProjectCheck {
            board: Some(&clean_b),
            schematic: Some(&clean),
            config: DrcConfig::default(),
        };
        assert!(project_ok.claim_done("project done").is_ok());
    }

    #[test]
    fn session_converges() {
        let mut b = dirty_board();
        let mut session = Session::new(&mut b, DrcConfig::default());
        // Round 1: nudge
        session.round(RoundOutcome::Actions {
            actions: vec![crate::drc::RepairAction::NudgeIntoOutline { reference: "R1".into() }],
            note: "nudge R1".into(),
        }).unwrap();
        // Round 2: done-claim, verified
        session.claim_done("R1 nudged").unwrap();
    }

    #[test]
    fn session_rejects_premature_done() {
        let mut b = dirty_board();
        let mut session = Session::new(&mut b, DrcConfig::default());
        assert!(session.claim_done("nothing to do").is_err());
    }
}