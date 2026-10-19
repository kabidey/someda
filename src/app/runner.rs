//! Runs the engine (`someda ai build`) as a subprocess and reads its log.
//!
//! The CLI's stdout is the contract: stage lines `✓ [stage] detail`,
//! `repair:` / `suggest:` lines, the `HONEST-DONE` / `NOT DONE` verdict and
//! the exit code. The app never re-implements a stage; a crash in the engine
//! cannot take the window down.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver};

/// One line from the build, or its end.
#[derive(Debug, Clone, PartialEq)]
pub enum BuildEvent {
    Line(String),
    Finished { exit_code: i32 },
    Failed(String),
}

/// What to run.
#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub someda_cli: PathBuf,
    pub plan_file: PathBuf,
    pub project_dir: PathBuf,
    pub description: String,
    pub kicad_cli: Option<PathBuf>,
    pub footprint_dirs: Vec<PathBuf>,
    pub model_dir: Option<PathBuf>,
    /// Extra environment (the API key goes here, never on the command line).
    pub env: Vec<(String, String)>,
}

/// Spawn the build; lines arrive on the receiver as they are printed.
pub fn spawn(req: BuildRequest) -> Receiver<BuildEvent> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut cmd = Command::new(&req.someda_cli);
        cmd.args(["ai", "build", &req.description])
            .env("SOMEDA_PLAN_FILE", &req.plan_file)
            .env("SOMEDA_PROJECT_DIR", &req.project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(k) = &req.kicad_cli {
            cmd.env("SOMEDA_KICAD_CLI", k);
        }
        if !req.footprint_dirs.is_empty() {
            let joined = std::env::join_paths(&req.footprint_dirs).map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            cmd.env("SOMEDA_FOOTPRINT_DIRS", joined);
        }
        if let Some(m) = &req.model_dir {
            cmd.env("SOMEDA_3DMODEL_DIR", m);
        }
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flashes behind the app
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(BuildEvent::Failed(format!("could not start {}: {}", req.someda_cli.display(), e)));
                return;
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let tx2 = tx.clone();
        let err_thread = std::thread::spawn(move || {
            if let Some(e) = stderr {
                for line in BufReader::new(e).lines().map_while(Result::ok) {
                    let _ = tx2.send(BuildEvent::Line(format!("stderr: {}", line)));
                }
            }
        });
        if let Some(o) = stdout {
            for line in BufReader::new(o).lines().map_while(Result::ok) {
                let _ = tx.send(BuildEvent::Line(line));
            }
        }
        let _ = err_thread.join();
        match child.wait() {
            Ok(st) => {
                let _ = tx.send(BuildEvent::Finished { exit_code: st.code().unwrap_or(-1) });
            }
            Err(e) => {
                let _ = tx.send(BuildEvent::Failed(e.to_string()));
            }
        }
    });
    rx
}

/// One row of the engine's stage table.
#[derive(Debug, Clone, PartialEq)]
pub struct StageRow {
    pub ok: bool,
    pub stage: String,
    pub detail: String,
}

/// Everything the results screen shows, parsed from the log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BuildSummary {
    pub stages: Vec<StageRow>,
    pub repairs: Vec<String>,
    pub suggestions: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub honest_done: bool,
    /// `drc=PASS shorts=PASS …` from the export-preflight row.
    pub gates: Vec<(String, bool)>,
}

/// Parse the whole log (all lines, in order).
pub fn summarize(lines: &[String]) -> BuildSummary {
    let mut s = BuildSummary::default();
    for raw in lines {
        let l = raw.trim();
        if let Some(rest) = l.strip_prefix("✓ [").or_else(|| l.strip_prefix("✗ [")) {
            let ok = l.starts_with('✓');
            if let Some((stage, detail)) = rest.split_once("] ") {
                let row = StageRow { ok, stage: stage.to_string(), detail: detail.to_string() };
                if stage == "export-preflight" {
                    s.gates = detail.split_whitespace().filter_map(|g| g.split_once('=')).map(|(n, v)| (n.to_string(), v == "PASS")).collect();
                }
                // the final table repeats the stages: keep the last occurrence
                s.stages.retain(|r| r.stage != row.stage);
                s.stages.push(row);
            }
        } else if let Some(r) = l.strip_prefix("repair: ") {
            s.repairs.push(r.to_string());
        } else if let Some(r) = l.strip_prefix("suggest: ") {
            s.suggestions.push(r.to_string());
        } else if let Some(r) = l.strip_prefix("[warning] ") {
            s.warnings.push(r.to_string());
        } else if let Some(r) = l.strip_prefix("[error] ") {
            s.errors.push(r.to_string());
        } else if l.contains("HONEST-DONE: all 6 gates pass") {
            s.honest_done = true;
        } else if l.contains("NOT DONE: failing gates") {
            s.honest_done = false;
        }
    }
    s
}

/// The files a finished build leaves, when present.
#[derive(Debug, Clone, Default)]
pub struct Outputs {
    pub board: Option<PathBuf>,
    pub schematic: Option<PathBuf>,
    pub project: Option<PathBuf>,
    pub iso_render: Option<PathBuf>,
    pub top_render: Option<PathBuf>,
    pub jlcpcb_zip: Option<PathBuf>,
    pub bom: Option<PathBuf>,
    pub built_plan: Option<PathBuf>,
}

pub fn outputs(project_dir: &Path) -> Outputs {
    let f = |rel: &str| {
        let p = project_dir.join(rel);
        p.is_file().then_some(p)
    };
    Outputs {
        board: f("circuit.kicad_pcb"),
        schematic: f("circuit.kicad_sch"),
        project: f("circuit.kicad_pro"),
        iso_render: f("render/board-iso.png"),
        top_render: f("render/board-top.png"),
        jlcpcb_zip: f("fab/jlcpcb/jlcpcb-gerbers.zip"),
        bom: f("fab/bom.md"),
        built_plan: f("plan.built.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_a_build_log() {
        let log: Vec<String> = [
            "─── Stage 2b ───",
            "  repair: J6: no room on the top edge — moved to the left edge (rotation 90°), board re-placed",
            "  suggest: C12 sit more than 6 mm outside the +1V8 island",
            "  [warning] decoupling-loop C58 (GND/+1V8): 7.37nH mounting loop",
            "  ✓ [route] routed 187 of 187 connections (0 unrouted)",
            "─── HONEST-DONE: all 6 gates pass ───",
            "  ✓ [route] routed 187 of 187 connections (0 unrouted)",
            "  ✗ [kicad-drc] 1 error(s)",
            "  ✓ [export-preflight] drc=PASS shorts=PASS unrouted=FAIL pours=PASS gerbers=PASS drill=PASS",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let s = summarize(&log);
        assert_eq!(s.repairs.len(), 1);
        assert_eq!(s.suggestions.len(), 1);
        assert_eq!(s.warnings.len(), 1);
        assert!(s.honest_done);
        assert_eq!(s.stages.len(), 3, "{:?}", s.stages);
        assert!(!s.stages.iter().find(|r| r.stage == "kicad-drc").unwrap().ok);
        assert_eq!(s.gates.iter().filter(|g| g.1).count(), 5);
    }

    #[test]
    fn a_missing_engine_reports_failed() {
        let rx = spawn(BuildRequest {
            someda_cli: PathBuf::from("/definitely/not/here/someda"),
            plan_file: PathBuf::from("plan.json"),
            project_dir: PathBuf::from("out"),
            description: "x".into(),
            kicad_cli: None,
            footprint_dirs: vec![],
            model_dir: None,
            env: vec![],
        });
        match rx.recv().unwrap() {
            BuildEvent::Failed(m) => assert!(m.contains("could not start")),
            other => panic!("{:?}", other),
        }
    }
}
