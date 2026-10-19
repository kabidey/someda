//! JLCPCB materials + capabilities — the fabrication scope the design must fit.
//!
//! Live-fetched from jlcpcb.com/capabilities/pcb-capabilities and the
//! Impedance Calculator user guide (2026-10-19):
//!
//! Dielectric constants (JLCPCB published):
//!   FR-4 bulk (2-layer)      Er 4.5
//!   7628 prepreg              Er 4.4
//!   3313 prepreg              Er 4.1
//!   2116 prepreg              Er 4.16
//!   Soldermask                Er 3.8
//!   4-8 layer boards: Nan Ya NP-155F core; 10+: Shengyi S1000-2M
//!
//! Fabrication limits (1 oz unless noted):
//!   min track/space 0.10/0.10 mm (4/4 mil); multilayer 0.09
//!   min via hole/pad 0.15/0.25 mm (microvia 0.10/0.20 under conditions)
//!   PTH annular ring >= 0.075 mm (0.055 absolute min at 1 oz)
//!   copper to board edge >= 0.5 mm (KiCad's default agrees)
//!   impedance tolerance ±10% (±5% special)
//!   via hole-to-hole spacing >= 0.2 mm; via-to-track >= 0.2 mm
//!   aspect ratio <= 10:1 (12:1 advanced)
//!
//! The design rules below are the CONTRACT the placer, router, and
//! physics checks all measure against. If a design needs finer than
//! this, it is out of scope for this fab and the harness says so.

/// A prepreg/core material with its dielectric properties.
#[derive(Debug, Clone)]
pub struct Material {
    pub name: &'static str,
    /// Relative permittivity (Er / DK) at ~1 GHz.
    pub er: f64,
    /// Dissipation factor (Df / loss tangent). JLCPCB does not publish
    /// per-prepreg Df; typical FR-4 family values are used and labeled.
    pub tan_delta: f64,
    /// Nominal thickness (mm) for one sheet.
    pub thickness_mm: f64,
    pub kind: MaterialKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterialKind {
    Prepreg,
    Core,
    Soldermask,
}

/// JLCPCB's published dielectric constants (fetched 2026-10-19).
pub const ER_FR4_2LAYER: f64 = 4.5;
pub const ER_PREPREG_7628: f64 = 4.4;
pub const ER_PREPREG_3313: f64 = 4.1;
pub const ER_PREPREG_2116: f64 = 4.16;
pub const ER_SOLDERMASK: f64 = 3.8;

/// JLCPCB fabrication limits (1 oz copper).
#[derive(Debug, Clone)]
pub struct FabLimits {
    /// Min trace width (mm).
    pub min_trace_mm: f64,
    /// Min trace spacing (mm).
    pub min_space_mm: f64,
    /// Min via hole (mm).
    pub min_via_hole_mm: f64,
    /// Min via pad (mm).
    pub min_via_pad_mm: f64,
    /// Min PTH annular ring (mm).
    pub min_annular_mm: f64,
    /// Copper-to-board-edge (mm).
    pub edge_clearance_mm: f64,
    /// Max via aspect ratio.
    pub max_aspect_ratio: f64,
    /// Impedance tolerance (fraction).
    pub impedance_tolerance: f64,
    /// Standard board thickness options (mm).
    pub thicknesses_mm: &'static [f64],
}

impl Default for FabLimits {
    fn default() -> Self {
        FabLimits {
            min_trace_mm: 0.10,
            min_space_mm: 0.10,
            min_via_hole_mm: 0.15,
            min_via_pad_mm: 0.25,
            min_annular_mm: 0.075,
            edge_clearance_mm: 0.5,
            max_aspect_ratio: 10.0,
            impedance_tolerance: 0.10,
            thicknesses_mm: &[0.4, 0.6, 0.8, 1.0, 1.2, 1.6, 2.0],
        }
    }
}

impl FabLimits {
    /// Check a design's trace/space against the fab scope.
    /// Returns a list of violations (empty = in scope).
    pub fn check_trace(&self, width_mm: f64, space_mm: f64) -> Vec<String> {
        let mut v = Vec::new();
        if width_mm < self.min_trace_mm {
            v.push(format!(
                "trace {:.3}mm < fab min {:.3}mm",
                width_mm, self.min_trace_mm
            ));
        }
        if space_mm < self.min_space_mm {
            v.push(format!(
                "space {:.3}mm < fab min {:.3}mm",
                space_mm, self.min_space_mm
            ));
        }
        v
    }

    /// Check a via definition against the fab scope (hole, pad, annular,
    /// aspect ratio for the given board thickness).
    pub fn check_via(
        &self,
        hole_mm: f64,
        pad_mm: f64,
        board_thickness_mm: f64,
    ) -> Vec<String> {
        let mut v = Vec::new();
        if hole_mm < self.min_via_hole_mm {
            v.push(format!(
                "via hole {:.3}mm < fab min {:.3}mm",
                hole_mm, self.min_via_hole_mm
            ));
        }
        if pad_mm < self.min_via_pad_mm {
            v.push(format!(
                "via pad {:.3}mm < fab min {:.3}mm",
                pad_mm, self.min_via_pad_mm
            ));
        }
        let annular = (pad_mm - hole_mm) / 2.0;
        if annular < self.min_annular_mm {
            v.push(format!(
                "annular ring {:.3}mm < fab min {:.3}mm",
                annular, self.min_annular_mm
            ));
        }
        let ar = board_thickness_mm / hole_mm;
        if ar > self.max_aspect_ratio {
            v.push(format!(
                "aspect ratio {:.1}:1 > fab max {:.0}:1",
                ar, self.max_aspect_ratio
            ));
        }
        v
    }
}

/// The 2-layer JLCPCB stackup the generator targets: 1.6mm FR-4,
/// 1 oz copper. h (dielectric under a microstrip) is derived from the
/// finished thickness minus copper and mask.
#[derive(Debug, Clone)]
pub struct Stackup {
    pub layers: u32,
    pub finished_thickness_mm: f64,
    /// Dielectric height under F.Cu microstrip (mm).
    pub h_mm: f64,
    pub er: f64,
    pub copper_thickness_mm: f64,
    pub mask_er: f64,
    pub mask_thickness_mm: f64,
}

impl Default for Stackup {
    fn default() -> Self {
        Stackup {
            layers: 2,
            finished_thickness_mm: 1.6,
            // 1.6mm FR-4 core (2-layer is core, not prepreg), minus 1 oz
            // copper (0.035mm) each side — h for the microstrip field:
            h_mm: (1.6 - 2.0 * 0.035) / 2.0 + 0.035,
            er: ER_FR4_2LAYER,
            copper_thickness_mm: 0.035, // 1 oz
            mask_er: ER_SOLDERMASK,
            mask_thickness_mm: 0.0254, // ~1 mil over traces
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jlcpcb_dielectric_constants_match_published() {
        // Fetched 2026-10-19 from jlcpcb.com/capabilities/pcb-capabilities
        assert_eq!(ER_FR4_2LAYER, 4.5);
        assert_eq!(ER_PREPREG_7628, 4.4);
        assert_eq!(ER_PREPREG_3313, 4.1);
        assert_eq!(ER_PREPREG_2116, 4.16);
        assert_eq!(ER_SOLDERMASK, 3.8);
    }

    #[test]
    fn fab_limits_match_jlcpcb_scope() {
        let f = FabLimits::default();
        assert_eq!(f.min_trace_mm, 0.10, "JLCPCB 4/4 mil min");
        assert_eq!(f.min_space_mm, 0.10);
        assert_eq!(f.min_via_hole_mm, 0.15);
        assert_eq!(f.min_via_pad_mm, 0.25);
        assert!((f.impedance_tolerance - 0.10).abs() < 1e-9, "±10% standard");
    }

    #[test]
    fn skill_101_router_defaults_fit_fab_scope() {
        let f = FabLimits::default();
        // The SomRoute defaults must be inside the fab scope
        assert!(f.check_trace(0.25, 0.2).is_empty(), "0.25/0.2 is in scope");
        // 0.6/0.3 via in a 1.6mm board
        assert!(f.check_via(0.3, 0.6, 1.6).is_empty(), "0.6/0.3 via in scope");
        // Below-scope designs must be rejected honestly
        assert!(!f.check_trace(0.08, 0.2).is_empty(), "0.08mm trace is out of scope");
        assert!(!f.check_via(0.1, 0.2, 1.6).is_empty(), "AR 16:1 is out of scope");
    }

    #[test]
    fn stackup_height_is_physical() {
        let s = Stackup::default();
        assert!((s.finished_thickness_mm - 1.6).abs() < 1e-9);
        assert!(s.h_mm > 0.7 && s.h_mm < 0.85, "h ≈ 0.78mm for 1.6mm FR-4");
        assert!((s.copper_thickness_mm - 0.035).abs() < 1e-9, "1 oz");
    }
}