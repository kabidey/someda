//! Golden-file corpus test: every real KiCad board in tests/corpus/ must parse
//! without panics and produce the expected DRC error count.
//!
//! These are real boards from the KiCad demos collection (GPL content).
//! The expected results were verified by hand against the board geometry.

use std::fs;
use std::path::Path;

/// (filename, expected_drc_errors_is_at_least, expected_drc_errors_is_at_most)
/// None means "must parse and DRC without panic; any error count accepted"
const CORPUS: &[(&str, Option<(usize, usize)>)] = &[
    // Verified clean boards (0 errors)
    ("custom_pads_test.kicad_pcb", Some((0, 0))),
    ("ecc83_v1.kicad_pcb", Some((0, 0))),
    ("flat_hierarchy.kicad_pcb", Some((0, 8))), // real trace-pad grazes (verified: net-4 trace through JP1:2's net-6 pad)
    ("microwave.kicad_pcb", Some((0, 0))),
    ("pic_programmer.kicad_pcb", Some((0, 6))), // real tight trace-pad clearances (0.07mm, verified)
    // Boards with real DRC findings (verified: pad outside outline in old demo files)
    ("complex_hierarchy.kicad_pcb", Some((1, 10))), // 1 pad-outside + trace-pad tight clearances
    // interf_u: the earlier 10-80 expectation came from the pre-0.6 rotation
    // formula, which rotated footprints the wrong way. Verified against
    // kicad-cli's IPC-D-356 export: 130/130 rotated pads match the
    // corrected transform (0/130 matched the old one). With pads where
    // KiCad puts them, only a few tight trace-pad clearances remain.
    ("interf_u.kicad_pcb", Some((0, 10))),
];

#[test]
fn corpus_boards_parse_and_drc_without_panics() {
    let corpus_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    for (filename, expected) in CORPUS {
        let path = corpus_dir.join(filename);
        let input = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {}", path.display(), e));
        let board = someda::board::Board::from_kicad_pcb(&input)
            .unwrap_or_else(|e| panic!("{}: parse failed: {}", filename, e));
        let report = someda::drc::run_drc(&board, &someda::drc::DrcConfig::default());
        if let Some((min, max)) = expected {
            assert!(
                report.errors() >= *min && report.errors() <= *max,
                "{}: expected {}-{} errors, got {} ({:?})",
                filename, min, max, report.errors(),
                report.findings.iter().map(|f| f.code.clone()).collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn corpus_rotation_math_is_stable() {
    // Every corpus board: pads must remain finite (no NaN/inf from rotation math)
    let corpus_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    for entry in fs::read_dir(&corpus_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("kicad_pcb") {
            continue;
        }
        let input = fs::read_to_string(&path).unwrap();
        let board = someda::board::Board::from_kicad_pcb(&input).unwrap();
        for fp in &board.footprints {
            for pad in &fp.pads {
                assert!(pad.x.is_finite() && pad.y.is_finite(),
                    "{}: pad {}:{} has non-finite position ({}, {})",
                    path.display(), fp.reference, pad.number, pad.x, pad.y);
            }
        }
    }
}
