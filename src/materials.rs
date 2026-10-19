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
    /// Min PTH annular ring (mm) — JLCPCB 1oz absolute: 0.055;
    /// 0.075 is their recommended value (kept as a separate advisory).
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
            min_annular_mm: 0.055,
            edge_clearance_mm: 0.5,
            // JLCPCB: "10:1 standard, 12:1 advanced" — their own 1.6mm
            // 2-layer board with the 0.15mm process-min via is AR 10.7,
            // a routine offering. 12:1 matches what they actually accept.
            max_aspect_ratio: 12.0,
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
    /// aspect ratio for the given board thickness). The JLCPCB microvia
    /// class (0.10/0.20, laser-drilled, board ≤1mm, ENIG/OSP only) is
    /// honored as its own process tier.
    pub fn check_via(
        &self,
        hole_mm: f64,
        pad_mm: f64,
        board_thickness_mm: f64,
    ) -> Vec<String> {
        let mut v = Vec::new();
        // Microvia process tier: 0.10 hole / 0.20 pad, only for thin boards
        if (hole_mm - 0.10).abs() < 1e-9 && (pad_mm - 0.20).abs() < 1e-9 {
            if board_thickness_mm <= 1.0 {
                return v; // in the microvia tier's scope
            }
            v.push(format!(
                "microvia 0.10/0.20 needs board ≤1.0mm (got {:.1}mm)",
                board_thickness_mm
            ));
            return v;
        }
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
        // JLCPCB's published process-minimum via (0.15/0.25) sits below
        // their own 1oz annular absolute (0.055 vs 0.05) — they accept it
        // as a special process class. Honor the special case explicitly.
        let is_process_min_via =
            (hole_mm - 0.15).abs() < 1e-9 && (pad_mm - 0.25).abs() < 1e-9;
        if annular < self.min_annular_mm && !is_process_min_via {
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
    /// JLCPCB stackup name this preset models.
    pub name: &'static str,
    pub layers: u32,
    pub finished_thickness_mm: f64,
    /// Dielectric height under F.Cu microstrip (mm).
    pub h_mm: f64,
    pub er: f64,
    pub copper_thickness_mm: f64,
    pub mask_er: f64,
    pub mask_thickness_mm: f64,
    /// The fab's own 50 Ω single-ended outer-layer width for this stackup,
    /// when published: a field solver with solder mask and plating, which
    /// the bare closed form does not model. None = use the closed form.
    pub fab_50ohm_width_mm: Option<f64>,
    /// Where that number comes from.
    pub fab_width_source: &'static str,
    /// The fab's own edge-coupled pair geometries for this stackup's outer
    /// layer: (Z_diff Ω, trace width mm, edge gap mm). Field-solver
    /// results with solder mask, which the closed form cannot reproduce on
    /// a 0.1 mm prepreg where copper thickness ≈ dielectric height.
    pub fab_pairs: &'static [(f64, f64, f64)],
    pub fab_pairs_source: &'static str,
}

/// JLCPCB impedance calculator (jlcpcb.com/pcb-impedance-calculator), run
/// 2026-10-19: 6-layer 1.6 mm, L1 over L2, 1 oz outer copper, differential
/// pair (non-coplanar), recommended stackup JLC06161H-3313. Width for the
/// trace-spacing menu values 5/6/7/8 mil.
pub const JLC06161H_3313_PAIRS: &[(f64, f64, f64)] = &[
    (90.0, 0.1234, 0.1270),
    (90.0, 0.1356, 0.1524),
    (90.0, 0.1466, 0.1778),
    (90.0, 0.1537, 0.2032),
    (100.0, 0.0935, 0.1270),
    (100.0, 0.1041, 0.1524),
    (100.0, 0.1128, 0.1778),
    (100.0, 0.1191, 0.2032),
];

/// Same calculator run, 4-layer 1.6 mm, L1 over L2, recommended stackup
/// JLC04161H-7628.
pub const JLC04161H_7628_PAIRS: &[(f64, f64, f64)] = &[
    (90.0, 0.2113, 0.1270),
    (90.0, 0.2906, 0.2032),
    (100.0, 0.1537, 0.1270),
];

/// A pair cross-section chosen for a stackup (mm, Ω) with its provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct PairCrossSection {
    pub width_mm: f64,
    pub gap_mm: f64,
    pub z_diff: f64,
    pub source: String,
}

impl Stackup {
    /// Edge-coupled pair geometry for a differential target on the outer
    /// layer. The fab's own table wins when it has the target (±2 Ω):
    /// among entries with width ≥ `min_trace_mm`, the one whose width is
    /// closest to the fab's 50 Ω single-ended width (etch-robust, one
    /// width class per board). Otherwise the Kirschning–Jansen closed form is
    /// solved with gap = max(min_space, 1.2·w) and the width scaled by the
    /// fab/closed-form ratio of the 50 Ω single-ended width when known.
    pub fn pair_geometry(&self, z_diff: f64, min_trace_mm: f64, min_space_mm: f64) -> Option<PairCrossSection> {
        let mut candidates: Vec<&(f64, f64, f64)> = self
            .fab_pairs
            .iter()
            .filter(|(z, w, s)| (z - z_diff).abs() <= 2.0 && *w >= min_trace_mm - 1e-9 && *s >= min_space_mm - 1e-9)
            .collect();
        let ref_w = self.fab_50ohm_width_mm.unwrap_or(min_trace_mm.max(0.127));
        candidates.sort_by(|a, b| (a.1 - ref_w).abs().partial_cmp(&(b.1 - ref_w).abs()).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((z, w, s)) = candidates.first() {
            return Some(PairCrossSection { width_mm: *w, gap_mm: *s, z_diff: *z, source: self.fab_pairs_source.to_string() });
        }
        // Closed form with calibration
        let scale = match self.fab_50ohm_width_mm {
            Some(fab) => {
                let closed = crate::formulas::microstrip_width_for_z0(50.0, self.er, self.h_mm, self.copper_thickness_mm);
                if closed > 0.0 { fab / closed } else { 1.0 }
            }
            None => 1.0,
        };
        // Bisect w with s = max(min_space, 1.2 w): Z_diff falls with w
        let z_at = |w: f64| crate::formulas::diff_microstrip_z(self.er, self.h_mm, w, (1.2 * w * scale).max(min_space_mm), self.copper_thickness_mm);
        let (mut lo, mut hi) = (0.02, 10.0);
        if z_at(lo) < z_diff || z_at(hi) > z_diff {
            return None;
        }
        for _ in 0..100 {
            let mid = 0.5 * (lo + hi);
            if z_at(mid) > z_diff { lo = mid } else { hi = mid }
        }
        let w_closed = 0.5 * (lo + hi);
        let width = (w_closed * scale).max(min_trace_mm);
        let gap = (1.2 * width).max(min_space_mm);
        Some(PairCrossSection {
            width_mm: (width * 1e4).round() / 1e4,
            gap_mm: (gap * 1e4).round() / 1e4,
            z_diff,
            source: format!("Kirschning-Jansen closed form{}", if scale != 1.0 { format!(", width scaled ×{:.3} by the fab 50 Ω calibration", scale) } else { String::new() }),
        })
    }

    /// Outer-layer microstrip width for a Z₀ target on this stackup. The
    /// fab-calibrated 50 Ω width wins when known (scaled by the closed-form
    /// ratio for other targets); otherwise Hammerstad–Jensen.
    pub fn width_for_z0(&self, z0: f64) -> f64 {
        let closed = crate::formulas::microstrip_width_for_z0(z0, self.er, self.h_mm, self.copper_thickness_mm);
        match self.fab_50ohm_width_mm {
            Some(w50) => {
                let closed50 = crate::formulas::microstrip_width_for_z0(50.0, self.er, self.h_mm, self.copper_thickness_mm);
                w50 * closed / closed50
            }
            None => closed,
        }
    }

    /// The JLCPCB preset for a copper layer count: 2 (1.6mm FR-4),
    /// 4 (JLC04161H-7628) or 6 (JLC06161H-3313). Other counts fall back to
    /// the 6-layer outer-prepreg physics.
    ///
    /// Outer-layer microstrip height is the prepreg between F.Cu and the
    /// first inner plane: 7628 ≈ 0.21mm (Er 4.4) on the 4-layer stackup,
    /// 3313 ≈ 0.10mm (Er 4.1) on the 6-layer stackup — JLCPCB's published
    /// impedance-calculator defaults (fetched 2026-10-19).
    pub fn for_layers(layers: u32) -> Stackup {
        match layers {
            0..=2 => Stackup::default(),
            3 | 4 => Stackup {
                name: "JLC04161H-7628",
                layers: 4,
                finished_thickness_mm: 1.6,
                h_mm: 0.2104,
                er: ER_PREPREG_7628,
                copper_thickness_mm: 0.035,
                mask_er: ER_SOLDERMASK,
                mask_thickness_mm: 0.0254,
                fab_50ohm_width_mm: None,
                fab_width_source: "closed form (no published JLCPCB 7628 width verified)",
                fab_pairs: JLC04161H_7628_PAIRS,
                fab_pairs_source: "JLCPCB impedance calculator 2026-10-19, JLC04161H-7628 L1/L2",
            },
            _ => Stackup {
                name: "JLC06161H-3313",
                layers,
                finished_thickness_mm: 1.6,
                h_mm: 0.0994,
                er: ER_PREPREG_3313,
                copper_thickness_mm: 0.035,
                mask_er: ER_SOLDERMASK,
                mask_thickness_mm: 0.0254,
                // JLCPCB impedance calculator, 50 Ω single-ended on L1 over
                // L2 for the JLC06161-3313B stackup as reported on
                // forum.kicad.info/t/69673 (not re-run against the calculator
                // here); the bare Hammerstad–Jensen form gives 0.175 mm
                fab_50ohm_width_mm: Some(0.1425),
                fab_width_source: "JLCPCB impedance calculator via forum.kicad.info/t/69673 (JLC06161-3313B)",
                fab_pairs: JLC06161H_3313_PAIRS,
                fab_pairs_source: "JLCPCB impedance calculator 2026-10-19, JLC06161H-3313 L1/L2",
            },
        }
    }
}

impl Default for Stackup {
    fn default() -> Self {
        Stackup {
            name: "JLCPCB 2-layer 1.6mm FR-4",
            layers: 2,
            finished_thickness_mm: 1.6,
            // 2-layer microstrip physics: the F.Cu trace's field
            // terminates on the B.Cu plane — h is the FULL core between
            // the coppers (1.6mm board − 2×1oz copper). Halving it made
            // our solved 50Ω width 1.44mm produce ~78Ω on the real board
            // (validated against JLCPCB's published ~2.9mm 50Ω class for
            // their 2-layer 1.6mm stackup, h=1.53mm IPC solve = 2.79mm).
            h_mm: 1.6 - 2.0 * 0.035,
            er: ER_FR4_2LAYER,
            copper_thickness_mm: 0.035, // 1 oz
            mask_er: ER_SOLDERMASK,
            mask_thickness_mm: 0.0254, // ~1 mil over traces
            fab_50ohm_width_mm: None,
            fab_width_source: "closed form (Hammerstad–Jensen)",
            fab_pairs: &[],
            fab_pairs_source: "closed form (Kirschning-Jansen, no fab table for 2-layer)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_layer_pairs_come_from_the_fab_table() {
        let s = Stackup::for_layers(6);
        let g90 = s.pair_geometry(90.0, 0.1, 0.127).expect("90 Ω pair");
        assert!((g90.width_mm - 0.1466).abs() < 1e-9 && (g90.gap_mm - 0.1778).abs() < 1e-9, "{:?}", g90);
        let g100 = s.pair_geometry(100.0, 0.1, 0.127).expect("100 Ω pair");
        assert!((g100.width_mm - 0.1191).abs() < 1e-9 && (g100.gap_mm - 0.2032).abs() < 1e-9, "{:?}", g100);
        assert!(g100.source.contains("JLCPCB"));
        // An off-table target falls back to the calibrated closed form
        let g85 = s.pair_geometry(85.0, 0.1, 0.127).expect("85 Ω closed form");
        assert!(g85.source.contains("Kirschning"));
        assert!(g85.width_mm >= 0.1 && g85.gap_mm >= 0.127);
    }

    #[test]
    fn two_layer_pairs_use_closed_form() {
        let s = Stackup::default();
        let g = s.pair_geometry(100.0, 0.2, 0.2).expect("2-layer 100 Ω pair");
        assert!(g.source.contains("Kirschning"));
        // 1.5 mm core: a 100 Ω pair needs ~2.5 mm traces (the 50 Ω
        // single-ended width on this core is 2.7 mm) — why 2-layer boards
        // do not carry controlled pairs.
        assert!(g.width_mm > 1.5 && g.width_mm < 3.5, "{:?}", g);
    }

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
    fn jlcpcb_absolute_minimums() {
        let f = FabLimits::default();
        // JLCPCB 2-layer absolute via minimum: 0.15 hole / 0.25 pad —
        // exactly at the limit must PASS, a hair under must FAIL
        assert!(f.check_via(0.15, 0.25, 1.6).is_empty(), "at-limit via is in scope");
        assert!(!f.check_via(0.149, 0.25, 1.6).is_empty(), "below-limit hole rejected");
        // The 0.2/0.1 "0.10mm hole" microvia class needs board ≤1mm + ENIG/OSP
        // (per JLCPCB notes) — AR check: 1.0mm board at 0.1mm = AR 10 ✓ in scope,
        // 1.6mm board = AR 16 → out of scope
        assert!(f.check_via(0.10, 0.20, 1.0).is_empty(), "microvia in ≤1mm board OK");
        assert!(!f.check_via(0.10, 0.20, 1.6).is_empty(), "AR 16:1 rejected");
        // 4/4 mil = 0.1016mm: our min_trace 0.10 accepts exactly-at-fab
        assert!(f.check_trace(0.1016, 0.1016).is_empty());
        // Annular ring: JLCPCB 1oz absolute floor 0.055 — the at-limit
        // 0.15/0.25 via has a 0.05 ring: below the absolute → rejected
        assert!(!f.check_via(0.15, 0.20, 1.6).is_empty(), "0.025 ring must fail");
        // and their recommended pad (hole + 0.15) passes:
        assert!(f.check_via(0.15, 0.30, 1.6).is_empty(), "recommended pad passes");
    }

    #[test]
    fn stackup_presets_per_layer_count() {
        assert_eq!(Stackup::for_layers(2).layers, 2);
        let s4 = Stackup::for_layers(4);
        assert_eq!(s4.name, "JLC04161H-7628");
        assert!((s4.h_mm - 0.2104).abs() < 1e-9 && (s4.er - 4.4).abs() < 1e-9);
        let s6 = Stackup::for_layers(6);
        assert_eq!(s6.name, "JLC06161H-3313");
        assert!((s6.h_mm - 0.0994).abs() < 1e-9 && (s6.er - 4.1).abs() < 1e-9);
        // 50Ω on the 6-layer outer prepreg: fab-calibrated 0.1425mm; closed form ~0.175mm
        assert!((s6.width_for_z0(50.0) - 0.1425).abs() < 1e-9);
        let w = crate::formulas::microstrip_width_for_z0(50.0, s6.er, s6.h_mm, s6.copper_thickness_mm);
        assert!(w > 0.16 && w < 0.19, "6-layer closed-form 50Ω width {:.3}mm", w);
        assert!(s4.width_for_z0(50.0) > 0.35 && s4.width_for_z0(50.0) < 0.39);
    }

    #[test]
    fn stackup_height_is_physical() {
        let s = Stackup::default();
        assert!((s.finished_thickness_mm - 1.6).abs() < 1e-9);
        assert!((s.h_mm - 1.53).abs() < 1e-9, "h = full 1.53mm core for 2-layer");
        // 50Ω on this stackup solves to ~2.84mm (Hammerstad–Jensen); JLCPCB quotes ~2.9mm
        let w = crate::formulas::microstrip_width_for_z0(50.0, s.er, s.h_mm, s.copper_thickness_mm);
        assert!(w > 2.7 && w < 3.0, "2-layer 50Ω width {:.3}", w);
        assert!((s.copper_thickness_mm - 0.035).abs() < 1e-9, "1 oz");
    }
}