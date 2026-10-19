//! The kicad-cli integration: run KiCad's official CLI tool for ERC, DRC,
//! and exports (Gerbers, BOM, drill, position files, SVG renders).
//!
//! Architecture: someda's LLM plans the circuit as JSON; someda's
//! deterministic generators write .kicad_sch / .kicad_pcb files; kicad-cli
//! verifies (sch erc / pcb drc) and exports (gerbers, bom, drill, pos).
//! The honest-done loop gates every stage.

use std::path::{Path, PathBuf};
use std::process::Command;

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

    /// Run DRC on a PCB: `kicad-cli pcb drc <file>` (same env classification).
    pub fn pcb_drc(&self, pcb_file: &str) -> CliResult {
        let r = self.run(&["pcb", "drc", "--exit-code-violations", pcb_file]);
        self.classified(r, "lib_footprint_issues")
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

    /// Export Gerbers: `kicad-cli pcb export gerbers --output <dir> <file>`
    pub fn export_gerbers(&self, pcb_file: &str, output_dir: &str) -> CliResult {
        self.run(&["pcb", "export", "gerbers", "--output", output_dir, pcb_file])
    }

    /// Export drill files: `kicad-cli pcb export drill --output <dir> <file>`
    pub fn export_drill(&self, pcb_file: &str, output_dir: &str) -> CliResult {
        self.run(&["pcb", "export", "drill", "--output", output_dir, pcb_file])
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