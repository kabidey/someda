//! Report module: DRC/ERC findings as Markdown and JSON, with stable finding codes.

use crate::drc::{DrcReport, Severity};
use std::fmt;

impl fmt::Display for DrcReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "DRC: {} error(s), {} warning(s)", self.errors(), self.warnings())?;
        for finding in &self.findings {
            writeln!(f, "  [{}] {}: {}", finding.severity, finding.code, finding.message)?;
        }
        Ok(())
    }
}

impl DrcReport {
    /// JSON output for tooling. Stable finding codes; sorted for deterministic diffs.
    pub fn to_json(&self) -> String {
        let mut findings = Vec::new();
        for f in &self.findings {
            findings.push(format!(
                "{{\"severity\":\"{}\",\"code\":\"{}\",\"message\":\"{}\"}}",
                if f.severity == Severity::Error { "error" } else { "warning" },
                f.code,
                f.message.replace('\\', "\\\\").replace('"', "\\\"")
            ));
        }
        findings.sort();
        format!(
            "{{\"errors\":{},\"warnings\":{},\"findings\":[{}]}}",
            self.errors(), self.warnings(), findings.join(",")
        )
    }

    /// Markdown output for humans.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# DRC Report\n\n");
        out.push_str(&format!("**{} error(s), {} warning(s)**\n\n", self.errors(), self.warnings()));
        if self.findings.is_empty() {
            out.push_str("No findings. The board is clean.\n");
        } else {
            out.push_str("| Severity | Code | Message |\n|---|---|---|\n");
            for f in &self.findings {
                out.push_str(&format!("| {} | `{}` | {} |\n", f.severity, f.code, f.message));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_report_shape() {
        let r = crate::drc::run_drc(
            &Board::from_kicad_pcb("(kicad_pcb)").unwrap(),
            &crate::drc::DrcConfig::default(),
        );
        let json = r.to_json();
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains("\"errors\":"));
    }

    #[test]
    fn markdown_report_shape() {
        let r = crate::drc::run_drc(
            &Board::from_kicad_pcb("(kicad_pcb)").unwrap(),
            &crate::drc::DrcConfig::default(),
        );
        let md = r.to_markdown();
        assert!(md.contains("# DRC Report"));
    }

    use crate::board::Board;
}
