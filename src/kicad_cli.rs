//! The kicad-cli integration: run KiCad's official CLI tool for ERC, DRC,
//! and exports (Gerbers, BOM, drill, position files, SVG renders).
//!
//! Architecture: someda's LLM plans the circuit as JSON; someda's
//! deterministic generators write .kicad_sch / .kicad_pcb files; kicad-cli
//! verifies (sch erc / pcb drc) and exports (gerbers, bom, drill, pos).
//! The honest-done loop gates every stage.

use std::path::{Path, PathBuf};
use std::process::Command;

/// What kicad-cli's DRC report says, counted from the report file itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrcSummary {
    pub violations: usize,
    pub unconnected: usize,
    pub footprint_errors: usize,
    pub errors: usize,
    pub warnings: usize,
    /// Finding codes with their counts, e.g. ("clearance", 3)
    pub by_code: Vec<(String, usize)>,
}

impl DrcSummary {
    /// Parse the text report. Codes appear as `[code]:` lines; the line
    /// after carries `; error` or `; warning`.
    pub fn parse(text: &str) -> DrcSummary {
        let mut s = DrcSummary::default();
        let mut by_code: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("** Found ") {
                let n: usize = rest.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
                if rest.contains("DRC violation") {
                    s.violations = n;
                } else if rest.contains("unconnected") {
                    s.unconnected = n;
                } else if rest.contains("Footprint error") {
                    s.footprint_errors = n;
                }
            }
            if t.starts_with('[') {
                if let Some(end) = t.find("]:") {
                    let code = t[1..end].to_string();
                    *by_code.entry(code).or_insert(0) += 1;
                    let sev = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
                    if sev.ends_with("; error") {
                        s.errors += 1;
                    } else if sev.ends_with("; warning") {
                        s.warnings += 1;
                    }
                }
            }
        }
        s.by_code = by_code.into_iter().collect();
        s
    }

    /// Finding codes a fab would act on even though KiCad rates them as
    /// warnings by default: silk on pads or over the edge (the fab clips or
    /// rejects it), touching courtyards, keep-out violations, copper necks.
    pub const FAB_BLOCKING: &'static [&'static str] = &[
        "silk_over_copper",
        "silk_overlap",
        "silk_edge_clearance",
        "courtyards_overlap",
        "items_not_allowed",
        "connection_width",
        "solder_mask_bridge",
    ];

    /// Count of findings whose code is fab-blocking regardless of severity.
    pub fn fab_blocking(&self) -> usize {
        self.by_code.iter().filter(|(c, _)| Self::FAB_BLOCKING.contains(&c.as_str())).map(|(_, n)| *n).sum()
    }

    /// Clean = no error-severity findings, no unconnected items, and no
    /// fab-blocking findings. Other warnings (missing-library notes) are
    /// reported, not failed.
    pub fn is_clean(&self) -> bool {
        self.errors == 0 && self.unconnected == 0 && self.fab_blocking() == 0
    }
}

impl std::fmt::Display for DrcSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} error(s), {} warning(s), {} unconnected, {} fab-blocking",
            self.errors, self.warnings, self.unconnected, self.fab_blocking()
        )?;
        let real: Vec<String> = self
            .by_code
            .iter()
            .filter(|(c, _)| c != "lib_footprint_issues")
            .map(|(c, n)| format!("{}×{}", c, n))
            .collect();
        if !real.is_empty() {
            write!(f, " [{}]", real.join(" "))?;
        }
        Ok(())
    }
}

/// Which kicad-cli binary to invoke (configurable for testing / different installs).
#[derive(Debug, Clone)]
pub struct KiCadCli {
    pub binary: String,
    pub project_dir: PathBuf,
}

#[derive(Debug)]
pub struct CliResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl std::fmt::Display for CliResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[exit {}]", self.exit_code)?;
        if !self.stderr.is_empty() {
            write!(f, "\nstderr: {}", self.stderr.trim())?;
        }
        Ok(())
    }
}

impl KiCadCli {
    pub fn new(project_dir: &Path) -> Self {
        KiCadCli {
            binary: std::env::var("SOMEDA_KICAD_CLI")
                .unwrap_or_else(|_| "kicad-cli".into()),
            project_dir: project_dir.to_path_buf(),
        }
    }

    fn run(&self, args: &[&str]) -> CliResult {
        let output = Command::new(&self.binary)
            .args(args)
            .current_dir(&self.project_dir)
            .output();

        match output {
            Ok(out) => CliResult {
                success: out.status.success(),
                exit_code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => CliResult {
                success: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("failed to run {}: {}", self.binary, e),
            },
        }
    }

    /// Run ERC on a schematic: `kicad-cli sch erc <file>`.
    /// `success` is true only for ZERO violations; `env_only` marks whether
    /// every finding is an environment issue (missing KiCad libraries) —
    /// those are not design errors and don't fail the honest-done gate.
    pub fn sch_erc(&self, sch_file: &str) -> CliResult {
        let r = self.run(&["sch", "erc", "--exit-code-violations", sch_file]);
        self.classified(r, "lib_symbol_issues")
    }

    /// Run DRC on a PCB with zones refilled, writing `<stem>-drc.rpt`, and
    /// judge the result from the REPORT FILE: errors + unconnected items
    /// fail; warnings and missing-library notes do not. (The old stdout
    /// classification marked boards with unconnected pads as passing —
    /// kicad-cli prints only a summary without finding codes to stdout.)
    pub fn pcb_drc(&self, pcb_file: &str) -> CliResult {
        let report_name = format!(
            "{}-drc.rpt",
            Path::new(pcb_file).file_stem().and_then(|s| s.to_str()).unwrap_or("circuit")
        );
        let r = self.run(&[
            "pcb", "drc", "--refill-zones", "--exit-code-violations",
            "--output", &report_name, pcb_file,
        ]);
        let report_path = self.project_dir.join(&report_name);
        match std::fs::read_to_string(&report_path) {
            Ok(text) => {
                let summary = DrcSummary::parse(&text);
                CliResult {
                    success: summary.is_clean(),
                    exit_code: r.exit_code,
                    stdout: format!("{} [report {}]", summary, report_name),
                    stderr: r.stderr,
                }
            }
            Err(e) => CliResult {
                success: false,
                exit_code: r.exit_code,
                stdout: r.stdout,
                stderr: format!("{}\n(no DRC report at {}: {})", r.stderr, report_path.display(), e),
            },
        }
    }

    /// Parse the DRC report kicad-cli last wrote for this board, if any.
    pub fn last_drc_summary(&self, pcb_file: &str) -> Option<DrcSummary> {
        let report_name = format!(
            "{}-drc.rpt",
            Path::new(pcb_file).file_stem().and_then(|s| s.to_str()).unwrap_or("circuit")
        );
        std::fs::read_to_string(self.project_dir.join(report_name))
            .ok()
            .map(|t| DrcSummary::parse(&t))
    }

    /// Classify a kicad-cli check result: if the only findings are the
    /// given environment code, mark success true with a note. The stdout
    /// of kicad-cli erc/drc lists finding types in bracketed codes.
    fn classified(&self, r: CliResult, env_code: &str) -> CliResult {
        let combined = format!("{}{}", r.stdout, r.stderr);
        let has_findings = combined.contains("Found") && !combined.contains("Found 0 violations");
        // Any finding code other than the env code means a real design issue
        let real_findings = combined.contains('[')
            && combined
                .split('[')
                .skip(1)
                .any(|seg| {
                    let code = seg.split(']').next().unwrap_or("");
                    !code.is_empty() && code != env_code
                });
        if has_findings && !real_findings && !r.success {
            CliResult {
                success: true,
                exit_code: r.exit_code,
                stdout: format!(
                    "{} [env-only: missing KiCad libraries — not design errors]",
                    r.stdout.trim_end()
                ),
                stderr: r.stderr,
            }
        } else {
            r
        }
    }

    /// Export the JLCPCB Gerber set: every copper layer for `layer_count`,
    /// both masks, both pastes, both silks and the outline, with zones
    /// checked/refilled at plot time (`--check-zones`).
    pub fn export_gerbers(&self, pcb_file: &str, output_dir: &str, layer_count: u32) -> CliResult {
        let mut layers: Vec<String> = crate::board::copper_layer_names(layer_count);
        for l in ["F.Mask", "B.Mask", "F.Paste", "B.Paste", "F.SilkS", "B.SilkS", "Edge.Cuts"] {
            layers.push(l.to_string());
        }
        let list = layers.join(",");
        // --subtract-soldermask: silkscreen is clipped at every mask
        // opening, so no legend can print on a pad (JLCPCB does the same
        // in its DFM; doing it here keeps the Gerbers honest).
        self.run(&[
            "pcb", "export", "gerbers", "--check-zones", "--subtract-soldermask", "--layers", &list,
            "--output", output_dir, pcb_file,
        ])
    }

    /// Export Excellon drill files plus a PDF drill map.
    pub fn export_drill(&self, pcb_file: &str, output_dir: &str) -> CliResult {
        self.run(&[
            "pcb", "export", "drill", "--generate-map", "--map-format", "pdf",
            "--output", output_dir, pcb_file,
        ])
    }

    /// Render the board to a PNG (3D view), top or bottom side.
    pub fn render_png(&self, pcb_file: &str, output_file: &str, side: &str) -> CliResult {
        self.run(&[
            "pcb", "render", "--side", side, "--width", "2400", "--height", "1600",
            "--quality", "high", "--background", "opaque", "--output", output_file, pcb_file,
        ])
    }

    /// Export position files (pick-and-place): `kicad-cli pcb export pos <file>`
    pub fn export_pos(&self, pcb_file: &str, output_dir: &str) -> CliResult {
        // kicad-cli 10's pos export takes an OUTPUT FILE (exit 4 when handed
        // a directory) — write fab/positions.csv explicitly.
        let pos_path = std::path::Path::new(output_dir).join("positions.csv");
        self.run(&[
            "pcb",
            "export",
            "pos",
            "--format",
            "csv",
            "--output",
            &pos_path.display().to_string(),
            pcb_file,
        ])
    }

    /// Export BOM from schematic: `kicad-cli sch export bom <file>`
    pub fn export_bom(&self, sch_file: &str, output_file: &str) -> CliResult {
        self.run(&["sch", "export", "bom", "--output", output_file, sch_file])
    }

    /// Export netlist: `kicad-cli sch export netlist <file>`
    pub fn export_netlist(&self, sch_file: &str, output_file: &str) -> CliResult {
        self.run(&["sch", "export", "netlist", "--output", output_file, sch_file])
    }

    /// Export PCB as SVG: `kicad-cli pcb export svg --output <dir> <file>`
    pub fn export_pcb_svg(&self, pcb_file: &str, output_dir: &str) -> CliResult {
        self.run(&["pcb", "export", "svg", "--output", output_dir, pcb_file])
    }

    /// Export schematic as SVG: `kicad-cli sch export svg --output <dir> <file>`
    pub fn export_sch_svg(&self, sch_file: &str, output_dir: &str) -> CliResult {
        self.run(&["sch", "export", "svg", "--output", output_dir, sch_file])
    }

    /// Get version: `kicad-cli version`
    pub fn version(&self) -> CliResult {
        self.run(&["version"])
    }

    /// Check if kicad-cli is available and working.
    pub fn is_available(&self) -> bool {
        self.version().success
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = "** Drc report for circuit.kicad_pcb **\n\n** Found 3 DRC violations **\n[lib_footprint_issues]: The current configuration does not include the footprint library 'Package_DIP'\n    Local override; warning\n    @(30.0000 mm, 20.0000 mm): Footprint U1\n[clearance]: Clearance violation ( clearance 0.2000 mm; actual 0.1000 mm)\n    Local override; error\n    @(1 mm, 1 mm): Track [SIG] on F.Cu\n[silk_overlap]: Silkscreen clearance\n    Rule: board setup constraints silk clearance; warning\n    @(2 mm, 2 mm): Text\n\n** Found 2 unconnected pads **\n[unconnected_items]: Missing connection between items\n    Local override; error\n    @(26.1900 mm, 20.6350 mm): Pad 1 [GND] of U1 on F.Cu\n[unconnected_items]: Missing connection between items\n    Local override; error\n    @(18.7500 mm, 2.5000 mm): Pad 1 [OUT] of R3 on F.Cu\n\n** Found 0 Footprint errors **\n";

    #[test]
    fn drc_summary_counts_errors_warnings_and_unconnected() {
        let s = DrcSummary::parse(REPORT);
        assert_eq!(s.violations, 3);
        assert_eq!(s.unconnected, 2);
        assert_eq!(s.errors, 3, "1 clearance + 2 unconnected are errors");
        assert_eq!(s.warnings, 2, "lib note + silk overlap are warnings");
        assert!(!s.is_clean(), "unconnected pads must never pass");
        assert!(s.by_code.iter().any(|(c, n)| c == "unconnected_items" && *n == 2));
    }

    #[test]
    fn drc_summary_warnings_only_is_clean() {
        let text = "** Found 1 DRC violations **\n[lib_footprint_issues]: missing lib\n    Local override; warning\n\n** Found 0 unconnected pads **\n";
        let s = DrcSummary::parse(text);
        assert!(s.is_clean());
        assert_eq!(s.warnings, 1);
    }

    #[test]
    fn silk_on_pads_is_not_clean_even_as_warning() {
        let text = "** Found 1 DRC violations **\n[silk_over_copper]: Silkscreen clipped by solder mask\n    Rule: board setup; warning\n    @(1 mm, 1 mm): Reference field of C1\n\n** Found 0 unconnected pads **\n";
        let s = DrcSummary::parse(text);
        assert_eq!(s.errors, 0);
        assert_eq!(s.fab_blocking(), 1);
        assert!(!s.is_clean(), "silk over a pad must fail the gate");
    }

    #[test]
    fn cli_not_available_graceful() {
        // In CI there's no kicad-cli installed — the check should fail cleanly
        let cli = KiCadCli::new(Path::new("/tmp"));
        // The binary may or may not exist in CI; just verify it doesn't panic
        let _ = cli.is_available();
    }

    #[test]
    fn cli_constructors() {
        let cli = KiCadCli::new(Path::new("/tmp/test-project"));
        assert_eq!(cli.binary, "kicad-cli");
        assert_eq!(cli.project_dir, Path::new("/tmp/test-project"));
    }

    #[test]
    fn custom_binary_via_env() {
        // Setting SOMEDA_KICAD_CLI changes the binary path
        std::env::set_var("SOMEDA_KICAD_CLI", "/custom/path/kicad-cli");
        let cli = KiCadCli::new(Path::new("/tmp"));
        assert_eq!(cli.binary, "/custom/path/kicad-cli");
        std::env::remove_var("SOMEDA_KICAD_CLI");
    }
}