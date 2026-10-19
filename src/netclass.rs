//! Net classes: named width/clearance contracts per net, with diff-pair grouping.
//!
//! A net class is law for the whole net — every trace segment and via on a
//! classed net must obey the class geometry. Violations reference the class
//! name so tooling can trace rule → finding.

use crate::board::Board;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct NetClass {
    pub name: String,
    pub trace_width_mm: f64,
    pub clearance_mm: f64,
    /// Net names assigned to this class
    pub nets: Vec<String>,
    /// Differential pairs: (positive net, negative net)
    pub diff_pairs: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct NetClassTable {
    /// Default geometry for unclassed nets
    pub default_trace_width_mm: f64,
    pub default_clearance_mm: f64,
    pub classes: Vec<NetClass>,
}

impl NetClassTable {
    pub fn new(default_width: f64, default_clearance: f64) -> Self {
        NetClassTable {
            default_trace_width_mm: default_width,
            default_clearance_mm: default_clearance,
            classes: Vec::new(),
        }
    }

    pub fn add_class(&mut self, name: &str, trace_width: f64, clearance: f64, nets: Vec<String>) {
        self.classes.push(NetClass {
            name: name.into(),
            trace_width_mm: trace_width,
            clearance_mm: clearance,
            nets,
            diff_pairs: Vec::new(),
        });
    }

    pub fn add_diff_pair(&mut self, class_name: &str, positive: &str, negative: &str) {
        if let Some(c) = self.classes.iter_mut().find(|c| c.name == class_name) {
            c.diff_pairs.push((positive.into(), negative.into()));
        }
    }

    /// The trace width for a net (class override or default)
    pub fn trace_width_for(&self, net: &str) -> f64 {
        for c in &self.classes {
            if c.nets.iter().any(|n| n == net) {
                return c.trace_width_mm;
            }
        }
        self.default_trace_width_mm
    }

    /// The clearance required around a net (class override or default)
    pub fn clearance_for(&self, net: &str) -> f64 {
        for c in &self.classes {
            if c.nets.iter().any(|n| n == net) {
                return c.clearance_mm;
            }
        }
        self.default_clearance_mm
    }

    /// The class name for a net, if any (for violation reporting)
    pub fn class_of(&self, net: &str) -> Option<&str> {
        self.classes
            .iter()
            .find(|c| c.nets.iter().any(|n| n == net))
            .map(|c| c.name.as_str())
    }

    /// Net-to-net required clearance (max of both sides' requirements)
    pub fn pair_clearance(&self, net_a: &str, net_b: &str) -> f64 {
        let a = self.clearance_for(net_a);
        let b = self.clearance_for(net_b);
        a.max(b)
    }

    /// Validate against a board: every classed net must exist on the board.
    /// Returns the list of unknown nets referenced by classes.
    pub fn validate_nets(&self, board: &Board) -> Vec<String> {
        let known: std::collections::HashSet<&str> = board.nets.iter().map(|(_, n)| n.as_str()).collect();
        let mut unknown = Vec::new();
        for c in &self.classes {
            for n in c.nets.iter().chain(c.diff_pairs.iter().flat_map(|(p, q)| [p, q])) {
                if !known.contains(n.as_str()) && !unknown.iter().any(|u: &String| u.ends_with(&format!(".{}", n))) {
                    unknown.push(format!("{}.{}", c.name, n));
                }
            }
        }
        unknown
    }

    /// Summary map: net → (class, width) for reporting
    pub fn summary(&self) -> HashMap<String, (String, f64)> {
        let mut m = HashMap::new();
        for c in &self.classes {
            for n in &c.nets {
                m.insert(n.clone(), (c.name.clone(), c.trace_width_mm));
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> NetClassTable {
        let mut t = NetClassTable::new(0.25, 0.2);
        t.add_class("power", 0.5, 0.4, vec!["VCC".into(), "VIN".into()]);
        t.add_class("usb", 0.45, 0.2, vec!["USB_D+".into(), "USB_D-".into()]);
        t.add_diff_pair("usb", "USB_D+", "USB_D-");
        t
    }

    #[test]
    fn class_override_and_default() {
        let t = table();
        assert_eq!(t.trace_width_for("VCC"), 0.5);
        assert_eq!(t.trace_width_for("SOME_OTHER_NET"), 0.25);
        assert_eq!(t.clearance_for("VIN"), 0.4);
        assert_eq!(t.clearance_for("SOME_OTHER_NET"), 0.2);
    }

    #[test]
    fn pair_clearance_takes_max() {
        let t = table();
        // power (0.4) vs unclassed (0.2) → 0.4
        assert_eq!(t.pair_clearance("VCC", "RANDOM"), 0.4);
        // usb vs usb: both 0.2
        assert_eq!(t.pair_clearance("USB_D+", "USB_D-"), 0.2);
    }

    #[test]
    fn class_of_reports_name() {
        let t = table();
        assert_eq!(t.class_of("VCC"), Some("power"));
        assert_eq!(t.class_of("NOPE"), None);
    }

    #[test]
    fn validate_nets_finds_unknown() {
        let t = table();
        let board = Board::from_kicad_pcb(r#"(kicad_pcb
          (net 1 "VCC")
          (net 2 "GND")
        )"#).unwrap();
        let unknown = t.validate_nets(&board);
        // VIN, USB_D+, USB_D- are classed but don't exist on the board
        assert_eq!(unknown.len(), 3);
        assert!(unknown.contains(&"power.VIN".to_string()));
        assert!(unknown.contains(&"usb.USB_D+".to_string()));
    }

    #[test]
    fn validate_nets_clean_board() {
        let board = Board::from_kicad_pcb(r#"(kicad_pcb
          (net 1 "VCC") (net 2 "VIN") (net 3 "USB_D+") (net 4 "USB_D-")
        )"#).unwrap();
        assert!(table().validate_nets(&board).is_empty());
    }
}