//! someda — a clean-room KiCad-compatible EDA core.
//!
//! Implements the architecture patterns documented in the SomTronics
//! AI-EDA study: s-expression board model, DRC engine, net classes,
//! and the verify-before-done ("honest-done") repair loop.

mod sexp;
mod board;
mod drc;

use std::path::Path;
use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("someda — clean-room KiCad-compatible EDA core");
        eprintln!();
        eprintln!("USAGE:");
        eprintln!("  someda info    <board.kicad_pcb>     summary of the board");
        eprintln!("  someda drc     <board.kicad_pcb>     run design-rule checks");
        eprintln!("  someda repair   <board.kicad_pcb>     run the honest-done repair loop");
        eprintln!("  someda validate <board.kicad_pcb>    full validation (parse + DRC)");
        exit(1);
    }

    let cmd = args[1].as_str();
    let path = match args.get(2) {
        Some(p) => Path::new(p),
        None => {
            eprintln!("error: command '{}' needs a board file", cmd);
            exit(1);
        }
    };

    let input = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {}: {}", path.display(), e);
            exit(1);
        }
    };

    let b = match board::Board::from_kicad_pcb(&input) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: parse failed: {}", e);
            exit(1);
        }
    };

    match cmd {
        "info" => cmd_info(&b),
        "drc" => cmd_drc(&b),
        "validate" => cmd_validate(&b),
        "repair" => cmd_repair(b),
        other => {
            eprintln!("error: unknown command '{}'", other);
            exit(1);
        }
    }
}

fn cmd_info(b: &board::Board) {
    println!("board version:  {}", b.version.as_deref().unwrap_or("?"));
    println!("footprints:      {}", b.footprints.len());
    println!("nets:            {}", b.nets.len());
    println!("outline segments: {}", b.outline.len());
    for fp in &b.footprints {
        println!("  {} ({}) at ({:.3}, {:.3}) rot {:.0} — {} pads",
            fp.reference, fp.lib_name, fp.x, fp.y, fp.rotation, fp.pads.len());
    }
    if let Some((x0, y0, x1, y1)) = b.outline_bbox() {
        let w = x1 - x0;
        let h = y1 - y0;
        println!("outline bbox:    ({:.3}, {:.3}) to ({:.3}, {:.3}) = {:.3} x {:.3} mm",
            x0, y0, x1, y1, w, h);
    }
}

fn cmd_drc(b: &board::Board) {
    let report = drc::run_drc(b, &drc::DrcConfig::default());
    print_report(&report);
    exit(if report.errors() > 0 { 1 } else { 0 });
}

fn cmd_validate(b: &board::Board) {
    println!("parse: OK");
    let report = drc::run_drc(b, &drc::DrcConfig::default());
    print_report(&report);
    if report.is_clean() {
        println!("result: CLEAN — board is parseable and passes all checks");
        exit(0);
    } else if report.errors() > 0 {
        println!("result: FAIL — {} error(s)", report.errors());
        exit(1);
    } else {
        println!("result: WARN — {} warning(s)", report.warnings());
        exit(0);
    }
}

fn cmd_repair(mut b: board::Board) {
    println!("repair loop: inspecting board...");
    let initial = drc::run_drc(&b, &drc::DrcConfig::default());
    print_report(&initial);

    if initial.is_clean() {
        println!("board already clean; nothing to repair");
        return;
    }

    // Automatic strategy: nudge every offending footprint inside, then verify.
    let bbox = b.outline_bbox();
    let offenders: Vec<String> = b.footprints.iter()
        .filter(|fp| {
            if let Some((x0, y0, x1, y1)) = bbox {
                !(fp.x >= x0 && fp.x <= x1 && fp.y >= y0 && fp.y <= y1)
            } else {
                false
            }
        })
        .map(|fp| fp.reference.clone())
        .collect();

    let mut rl = drc::RepairLoop::new(&mut b, drc::DrcConfig::default());

    if !offenders.is_empty() {
        println!("round 1: nudging {} footprint(s) into outline: {}",
            offenders.len(), offenders.join(", "));
        let actions = offenders.iter()
            .map(|r| drc::RepairAction::NudgeIntoOutline { reference: r.clone() })
            .collect();
        let _ = rl.round(drc::RoundOutcome::Actions { actions, note: "nudge into outline".into() });
    }

    // Final: claim done — the loop verifies against the live board.
    match rl.round(drc::RoundOutcome::Done { summary: "repair attempted; verify".into() }) {
        Ok(drc::RoundOutcome::Done { summary }) => {
            println!("DONE ACCEPTED: {}", summary);
            println!("rounds used: {}", rl.rounds_used());
            print_report(rl.report());
        }
        Err(rejection) => {
            println!("{}", rejection);
            print_report(rl.report());
            exit(1);
        }
        _ => unreachable!(),
    }
}

fn print_report(report: &drc::DrcReport) {
    println!("DRC: {} error(s), {} warning(s)", report.errors(), report.warnings());
    for f in &report.findings {
        println!("  [{}] {}: {}", f.severity, f.code, f.message);
    }
}