//! Design rules: the one place the fab contract (JLCPCB) turns into the
//! numbers the router routes with, the DRC checks against, and the KiCad
//! project file KiCad verifies against. Every consumer reads the same
//! struct, so the router can never route finer than the DRC accepts.
//!
//! JLCPCB capabilities (fetched 2026-10-19): 2-layer min track/space
//! 0.127/0.127 mm (5/5 mil) standard, 4+ layers 0.09/0.09 mm; via 0.3 hole
//! / 0.5 pad is the routine via for every layer count; copper-to-edge
//! 0.3 mm (0.5 recommended); hole-to-hole 0.5 mm; solder mask dam 0.1 mm.

use crate::materials::FabLimits;

#[derive(Debug, Clone, PartialEq)]
pub struct DesignRules {
    pub layer_count: u32,
    /// Default signal trace width (mm)
    pub trace_width_mm: f64,
    /// Power trace width (mm) for non-plane power nets
    pub power_trace_width_mm: f64,
    /// Copper-to-copper clearance between nets (mm)
    pub clearance_mm: f64,
    pub via_diameter_mm: f64,
    pub via_drill_mm: f64,
    pub hole_to_hole_mm: f64,
    /// Copper to drilled hole edge (mm)
    pub hole_clearance_mm: f64,
    pub edge_clearance_mm: f64,
    pub solder_mask_min_width_mm: f64,
    /// Router grid pitch (mm); traces are placed on this lattice
    pub grid_mm: f64,
}

impl DesignRules {
    /// Rules for a layer count: 2-layer hobby class (skill 101's 0.25/0.2
    /// defaults with the 0.6/0.3 via) or the multilayer fine-pitch class
    /// JLCPCB offers on 4- and 6-layer boards.
    pub fn for_layers(layer_count: u32) -> DesignRules {
        if layer_count <= 2 {
            DesignRules {
                layer_count: 2,
                trace_width_mm: 0.25,
                power_trace_width_mm: 0.5,
                clearance_mm: 0.2,
                via_diameter_mm: 0.6,
                via_drill_mm: 0.3,
                hole_to_hole_mm: 0.5,
                hole_clearance_mm: 0.25,
                edge_clearance_mm: 0.5,
                solder_mask_min_width_mm: 0.1,
                grid_mm: 0.5,
            }
        } else {
            DesignRules {
                layer_count,
                // 5 mil trace / 5 mil space: inside JLCPCB's 0.09 multilayer
                // minimum with margin, fine enough to escape 0.4mm pitch
                trace_width_mm: 0.127,
                power_trace_width_mm: 0.4,
                clearance_mm: 0.127,
                // JLCPCB multilayer via minimum 0.2 hole / 0.35 pad: the only
                // via that fits between 0.4mm-pitch escape stubs
                via_diameter_mm: 0.35,
                via_drill_mm: 0.2,
                hole_to_hole_mm: 0.45,
                hole_clearance_mm: 0.2,
                edge_clearance_mm: 0.5,
                solder_mask_min_width_mm: 0.1,
                grid_mm: 0.127,
            }
        }
    }

    /// Check these rules against the fab's published limits.
    pub fn fab_violations(&self, fab: &FabLimits, board_thickness_mm: f64) -> Vec<String> {
        let mut v = fab.check_trace(self.trace_width_mm, self.clearance_mm);
        v.extend(fab.check_via(self.via_drill_mm, self.via_diameter_mm, board_thickness_mm));
        if self.edge_clearance_mm < fab.edge_clearance_mm {
            v.push(format!(
                "edge clearance {:.3}mm < fab min {:.3}mm",
                self.edge_clearance_mm, fab.edge_clearance_mm
            ));
        }
        v
    }

    /// The KiCad project file (`.kicad_pro`) that makes kicad-cli judge the
    /// board by THESE rules instead of its 0.2mm defaults. Written next to
    /// the board with the same stem.
    pub fn kicad_project_json(&self, filename: &str, net_classes: &[(String, f64, f64)]) -> String {
        let mut classes = String::new();
        let default = format!(
            r#"      {{
        "bus_width": 12,
        "clearance": {clr},
        "diff_pair_gap": {clr},
        "diff_pair_via_gap": 0.25,
        "diff_pair_width": {tw},
        "line_style": 0,
        "microvia_diameter": 0.3,
        "microvia_drill": 0.1,
        "name": "Default",
        "pcb_color": "rgba(0, 0, 0, 0.000)",
        "schematic_color": "rgba(0, 0, 0, 0.000)",
        "track_width": {tw},
        "via_diameter": {vd},
        "via_drill": {vdr},
        "wire_width": 6
      }}"#,
            clr = self.clearance_mm,
            tw = self.trace_width_mm,
            vd = self.via_diameter_mm,
            vdr = self.via_drill_mm
        );
        classes.push_str(&default);
        for (name, width, clearance) in net_classes {
            classes.push_str(&format!(
                r#",
      {{
        "bus_width": 12,
        "clearance": {clr},
        "diff_pair_gap": {clr},
        "diff_pair_via_gap": 0.25,
        "diff_pair_width": {tw},
        "line_style": 0,
        "microvia_diameter": 0.3,
        "microvia_drill": 0.1,
        "name": "{name}",
        "pcb_color": "rgba(0, 0, 0, 0.000)",
        "schematic_color": "rgba(0, 0, 0, 0.000)",
        "track_width": {tw},
        "via_diameter": {vd},
        "via_drill": {vdr},
        "wire_width": 6
      }}"#,
                clr = clearance,
                tw = width,
                vd = self.via_diameter_mm,
                vdr = self.via_drill_mm,
                name = name
            ));
        }
        format!(
            r#"{{
  "board": {{
    "design_settings": {{
      "defaults": {{}},
      "meta": {{ "version": 2 }},
      "rules": {{
        "min_clearance": {clr},
        "min_connection": {tw},
        "min_copper_edge_clearance": {edge},
        "min_hole_clearance": {hc},
        "min_hole_to_hole": {h2h},
        "min_microvia_diameter": 0.2,
        "min_microvia_drill": 0.1,
        "min_resolved_spokes": 1,
        "min_silk_clearance": 0.0,
        "min_text_height": 0.8,
        "min_text_thickness": 0.08,
        "min_through_hole_diameter": 0.2,
        "min_track_width": {fab_tw},
        "min_via_annular_width": 0.075,
        "min_via_diameter": {vd},
        "solder_mask_clearance": 0.0,
        "solder_mask_min_width": {mask},
        "solder_mask_to_copper_clearance": 0.0,
        "use_height_for_length_calcs": true
      }}
    }}
  }},
  "meta": {{ "filename": "{file}", "version": 3 }},
  "net_settings": {{
    "classes": [
{classes}
    ],
    "meta": {{ "version": 4 }},
    "net_colors": null,
    "netclass_assignments": null,
    "netclass_patterns": []
  }},
  "pcbnew": {{ "page_layout_descr_file": "" }}
}}
"#,
            fab_tw = crate::materials::FabLimits::default().min_trace_mm,
            clr = self.clearance_mm,
            tw = self.trace_width_mm,
            edge = self.edge_clearance_mm,
            h2h = self.hole_to_hole_mm,
            hc = self.hole_clearance_mm,
            vd = self.via_diameter_mm,
            mask = self.solder_mask_min_width_mm,
            file = filename,
            classes = classes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_layer_rules_keep_skill_101_defaults() {
        let r = DesignRules::for_layers(2);
        assert_eq!(r.trace_width_mm, 0.25);
        assert_eq!(r.clearance_mm, 0.2);
        assert_eq!(r.via_diameter_mm, 0.6);
        assert_eq!(r.grid_mm, 0.5);
    }

    #[test]
    fn multilayer_rules_fit_jlcpcb_scope() {
        for n in [4, 6] {
            let r = DesignRules::for_layers(n);
            assert_eq!(r.layer_count, n);
            assert!(r.fab_violations(&FabLimits::default(), 1.6).is_empty(), "{:?}", r.fab_violations(&FabLimits::default(), 1.6));
            // the grid must keep two default traces at >= clearance apart
            assert!(r.grid_mm <= r.trace_width_mm + 1e-9);
        }
    }

    #[test]
    fn project_json_is_valid_and_carries_rules() {
        let r = DesignRules::for_layers(6);
        let json = r.kicad_project_json("circuit.kicad_pro", &[("power".into(), 0.4, 0.2)]);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["board"]["design_settings"]["rules"]["min_clearance"], 0.127);
        assert_eq!(v["board"]["design_settings"]["rules"]["min_track_width"], 0.1);
        assert_eq!(v["net_settings"]["classes"][1]["name"], "power");
        assert_eq!(v["net_settings"]["classes"][1]["track_width"], 0.4);
    }
}
