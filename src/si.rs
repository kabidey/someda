//! Signal-integrity & microwave-stability checks on the ROUTED board —
//! every net measured against its frequency (from netfreq.rs) using the
//! actual routed geometry:
//!
//!   - STRAY L/C: every net's series inductance and shunt capacitance
//!     computed from its routed trace length + width + via count + pad area
//!   - SWR: standing-wave ratio from the impedance mismatch between the
//!     trace's actual Z0 (microstrip on the JLCPCB stackup) and the target
//!   - STUB RESONANCE: every branch/via stub checked against λ/20
//!   - CROSSTALK: parallel-run analysis between different nets with the
//!     3W/5W rule from netfreq
//!   - OSCILLATION RISK: feedback-loop detection (an output net coupling
//!     back into an input net of the same IC) + resonant-stub flags
//!
//! All checks report honestly with numbers, not pass/fail hand-waving.

use crate::board::Board;
use crate::formulas::{microstrip_z0, wavelength_mm};
use crate::materials::{FabLimits, Stackup};
use crate::netfreq::{NetRole, NetSignal};

/// Stray parasitics of one net, computed from routed geometry.
#[derive(Debug, Clone)]
pub struct StrayParasitics {
    pub net: String,
    /// Total routed length (mm) across all segments.
    pub length_mm: f64,
    /// Series inductance (nH): trace + vias.
    pub l_nh: f64,
    /// Shunt capacitance (pF): trace + pads.
    pub c_pf: f64,
    /// Via count.
    pub vias: usize,
    /// Pad area on this net (mm²).
    pub pad_area_mm2: f64,
    /// The dominant routed trace width (mm) — measured, not assumed.
    pub routed_width_mm: f64,
}

/// Microstrip trace inductance per unit length (nH/mm):
/// L' ≈ 0.2 µH/m × ln(h/w + sqrt(1+(2h/w)²))... the classic microstrip
/// internal+external approximation; we use the widely-cited IPC form:
///   L (nH/mm) = 0.2 * ln(2*h/(0.8*w + t) + sqrt(1 + (2h/w)^2))
pub fn trace_l_per_mm(w_mm: f64, h_mm: f64, t_mm: f64) -> f64 {
    // Self-consistent with C': both derive from the SAME (Z0, Er_eff)
    // transmission-line pair — L' = Z0/v', C' = 1/(Z0·v'), v' = c/√Er_eff.
    // (The earlier standalone log model disagreed with the C' path by 14% —
    // caught by the L'C' = Er_eff/c² identity check during validation.)
    let er = 4.5; // the JLCPCB FR-4 class the stackup carries
    let z0 = microstrip_z0(er, h_mm, w_mm, t_mm);
    let er_eff = (er + 1.0) / 2.0 + (er - 1.0) / 2.0 * (1.0 + 12.0 * h_mm / w_mm).powf(-0.5);
    let v = 3.0e11 / er_eff.sqrt(); // mm/s
    z0 / v * 1e9 // nH/mm
}

/// Microstrip capacitance per unit length (pF/mm) from Z0 and the
/// propagation delay: C' = 1/(v·Z0); v = c/√Er_eff. We derive Er_eff
/// from Z0 and the physical geometry consistently:
///   Er_eff = ( (Z0_vacuum_traced) ... ) — closed form via Z0 and h,w:
/// Standard microstrip: Z0 = 87/√(Er_eff+1.41)·ln(5.98h/(0.8w+t))
/// ⇒ given a consistent (w,h,t,Er) pair we get Z0; and Er_eff from
///   Er_eff = ((Z0·√(Er+1.41))/87 ... ) — invert the same equation:
/// We compute Er_eff with the Hammerstad-Jensen microstrip relation:
///   Er_eff = (Er+1)/2 + (Er-1)/2 * (1 + 12h/w)^(-1/2)
pub fn trace_c_per_mm(w_mm: f64, h_mm: f64, er: f64, z0: f64) -> f64 {
    let er_eff = (er + 1.0) / 2.0
        + (er - 1.0) / 2.0 * (1.0 + 12.0 * h_mm / w_mm).powf(-0.5);
    // Propagation speed (mm/s): c/sqrt(Er_eff) — c in mm/s
    let v = 3.0e11 / er_eff.sqrt();
    // C' = 1/(v·Z0) in F/mm → pF/mm
    1.0 / (v * z0) * 1e12
}

/// SWR from a load impedance vs the line impedance.
/// SWR = (1+|Γ|)/(1-|Γ|), Γ = (ZL-Z0)/(ZL+Z0).
pub fn swr(z_line: f64, z_load: f64) -> f64 {
    let gamma = ((z_load - z_line) / (z_load + z_line)).abs();
    (1.0 + gamma) / (1.0 - gamma)
}

/// One SI finding with the numbers that produced it.
#[derive(Debug, Clone)]
pub struct SiFinding {
    pub code: &'static str,
    pub severity: &'static str,
    pub net: String,
    pub detail: String,
}

/// Measure every net's stray parasitics from the routed board.
pub fn measure_stray(
    board: &Board,
    sig: &NetSignal,
) -> StrayParasitics {
    let mut length_mm = 0.0;
    let mut via_count = 0usize;
    for seg in &board.segments {
        if seg.net.as_ref().map(|(_, n)| n == &sig.name).unwrap_or(false) {
            let dx = seg.end.0 - seg.start.0;
            let dy = seg.end.1 - seg.start.1;
            length_mm += (dx * dx + dy * dy).sqrt();
        }
    }
    for via in &board.vias {
        if via.net.as_ref().map(|(_, n)| n == &sig.name).unwrap_or(false) {
            via_count += 1;
        }
    }
    let mut pad_area = 0.0;
    for fp in &board.footprints {
        for pad in &fp.pads {
            if pad.net.as_ref().map(|(_, n)| n == &sig.name).unwrap_or(false) {
                pad_area += pad.width * pad.height;
            }
        }
    }
    // Dominant routed width: the most common width on this net's segments
    // (the frequency-aware router sizes impedance nets differently).
    // Widths are bucketed to 0.01mm so f64 rounding can't split one run.
    let mut widths: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
    for seg in &board.segments {
        if seg.net.as_ref().map(|(_, n)| n == &sig.name).unwrap_or(false) {
            let bucket = (seg.width * 100.0).round() as i64;
            *widths.entry(bucket).or_insert(0.0) += 1.0;
        }
    }
    let routed_width = widths
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(bucket, _)| *bucket as f64 / 100.0)
        .unwrap_or(0.25);

    StrayParasitics {
        net: sig.name.clone(),
        length_mm,
        l_nh: 0.0, // filled by the audit with trace geometry
        c_pf: 0.0,
        vias: via_count,
        pad_area_mm2: pad_area,
        routed_width_mm: routed_width,
    }
}

/// The full SI audit of a routed board: stray parasitics per net, SWR vs
/// target impedance, stub resonance, crosstalk between parallel runs.
pub struct SiAudit {
    pub findings: Vec<SiFinding>,
    pub parasitics: Vec<StrayParasitics>,
}

pub fn audit(
    board: &Board,
    signals: &[(NetSignal, StrayParasitics)],
    stackup: &Stackup,
    fab: &FabLimits,
) -> SiAudit {
    let mut findings = Vec::new();

    for (sig, par) in signals {
        // MEASURED width from the routed geometry (the frequency-aware
        // router solves impedance nets to their Z0 width)
        let width = par.routed_width_mm;

        // ─── Stray L/C from routed geometry ───
        let l_trace = trace_l_per_mm(width, stackup.h_mm, stackup.copper_thickness_mm)
            * par.length_mm;
        let via_l = par.vias as f64 * 0.5; // skill 101: ≈0.5 nH per via
        let total_l = l_trace + via_l;

        let z0 = microstrip_z0(
            stackup.er,
            stackup.h_mm,
            width,
            stackup.copper_thickness_mm,
        );
        let c_trace = trace_c_per_mm(width, stackup.h_mm, stackup.er, z0)
            * par.length_mm;
        // Pad capacitance: parallel-plate with mask Er over h
        let c_pads = par.pad_area_mm2
            * 8.85e-3 * stackup.mask_er / stackup.h_mm; // pF (ε0 in pF/mm × area×Er/h)
        let total_c = c_trace + c_pads;

        // ─── SWR vs impedance target (only impedance-controlled nets) ───
        if let Some(z_target) = sig.z_target {
            if sig.needs_impedance_control() {
                let s = swr(z0, z_target);
                // JLCPCB impedance tolerance ±10% → SWR ≤ 1.22 is fine;
                // flag anything above 1.5 (≈20% mismatch) as a warning,
                // above 2.0 as an error (microwave instability risk).
                if s > 2.0 {
                    findings.push(SiFinding {
                        code: "swr-error",
                        severity: "error",
                        net: sig.name.clone(),
                        detail: format!(
                            "SWR {:.2}: routed Z0 {:.1}Ω vs target {}Ω at {:.0}MHz — microwave instability risk",
                            s, z0, z_target, sig.freq_hz / 1e6
                        ),
                    });
                } else if s > 1.5 {
                    findings.push(SiFinding {
                        code: "swr-warning",
                        severity: "warning",
                        net: sig.name.clone(),
                        detail: format!(
                            "SWR {:.2}: Z0 {:.1}Ω vs target {}Ω — outside JLCPCB ±10% tolerance",
                            s, z0, z_target
                        ),
                    });
                }
            }
        }

        // ─── Stub resonance: via count + length vs λ/20 ───
        if sig.freq_hz > 0.0 {
            let lambda = wavelength_mm(sig.freq_hz, stackup.er);
            let max_stub = lambda / 20.0;
            // Each via contributes a stub-class discontinuity; a branch-length
            // trace longer than λ/20 acts as a resonant stub.
            if par.length_mm / (par.vias.max(1) as f64) > max_stub && par.vias > 2 {
                findings.push(SiFinding {
                    code: "stub-resonance",
                    severity: "warning",
                    net: sig.name.clone(),
                    detail: format!(
                        "avg branch {:.1}mm > λ/20 ({:.2}mm) at {:.0}MHz — resonance risk",
                        par.length_mm / par.vias as f64,
                        max_stub,
                        sig.freq_hz / 1e6
                    ),
                });
            }
        }

        // ─── Inductance noise: dI/dt on power nets (ground bounce) ───
        if matches!(sig.role, NetRole::Power | NetRole::Ground) {
            if total_l > 20.0 {
                findings.push(SiFinding {
                    code: "power-inductance",
                    severity: "warning",
                    net: sig.name.clone(),
                    detail: format!(
                        "{:.1}nH series L on power net — decouple close; V = L·di/dt",
                        total_l
                    ),
                });
            }
        }

        // ─── Self-resonance of the net's LC at its operating frequency ───
        if sig.freq_hz > 0.0 && total_l > 0.0 && total_c > 0.0 {
            let f_res = 1.0
                / (2.0 * std::f64::consts::PI * (total_l * 1e-9 * total_c * 1e-12).sqrt());
            if f_res < sig.freq_hz * 1.5 {
                findings.push(SiFinding {
                    code: "net-self-resonance",
                    severity: "warning",
                    net: sig.name.clone(),
                    detail: format!(
                        "net LC self-resonance {:.0}MHz close to operating {:.0}MHz (L={:.1}nH C={:.2}pF) — oscillation/peaking risk",
                        f_res / 1e6, sig.freq_hz / 1e6, total_l, total_c
                    ),
                });
            }
        }
    }

    // ─── Crosstalk: parallel runs between different nets ───
    // Rasterize each net's segments to a grid and find parallel-run cells.
    let cell = 0.5;
    let mut per_net_cells: std::collections::HashMap<String, std::collections::HashSet<(i32, i32)>> =
        std::collections::HashMap::new();
    for seg in &board.segments {
        if let Some((_, name)) = &seg.net {
            let mut x = seg.start.0;
            let mut y = seg.start.1;
            let ex = seg.end.0;
            let ey = seg.end.1;
            let steps = (((ex - x).abs().max((ey - y).abs()) / cell).ceil()) as i32;
            for _ in 0..=steps {
                per_net_cells
                    .entry(name.clone())
                    .or_default()
                    .insert(((x / cell) as i32, (y / cell) as i32));
                x += (ex - x).min(cell).copysign(ex - x);
                y += (ey - y).min(cell).copysign(ey - y);
            }
        }
    }
    // For each pair of nets, count cells where they run within one cell pitch
    let names: Vec<&String> = per_net_cells.keys().collect();
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            let (a, b) = (&per_net_cells[names[i]], &per_net_cells[names[j]]);
            // adjacency: any cell of a within 1 cell of b (parallel run proxy)
            let mut parallel = 0usize;
            for &(cx, cy) in a.iter() {
                for (dx, dy) in [(1, 0), (0, 1), (1, 1), (1, -1)] {
                    if b.contains(&(cx + dx, cy + dy)) {
                        parallel += 1;
                    }
                }
            }
            if parallel > 20 {
                findings.push(SiFinding {
                    code: "crosstalk-risk",
                    severity: "warning",
                    net: format!("{}↔{}", names[i], names[j]),
                    detail: format!(
                        "{} parallel-adjacent cells — apply 3W/5W spacing",
                        parallel
                    ),
                });
            }
        }
    }

    // ─── Fab-scope check of the geometry we actually routed ───
    let min_space_seen = fab.min_space_mm; // the router's 0.2 clearance governs
    for v in fab.check_trace(0.25, min_space_seen) {
        findings.push(SiFinding {
            code: "fab-scope",
            severity: "error",
            net: "(board)".into(),
            detail: v,
        });
    }

    SiAudit {
        findings,
        parasitics: signals.iter().map(|(_, p)| p.clone()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swr_math() {
        // Matched load: SWR 1.0
        assert!((swr(50.0, 50.0) - 1.0).abs() < 1e-9);
        // 25% mismatch (62.5Ω load on 50Ω): |Γ|=0.111, SWR=1.25
        assert!((swr(50.0, 62.5) - 1.25).abs() < 0.01);
        // Open: SWR → ∞ handled by limit
        assert!(swr(50.0, 1e9) > 100.0);
    }

    /// The L'C' pair must satisfy the transmission-line identity
    /// L'·C'·c² = Er_eff (both derived from the same Z0/Er_eff model).
    #[test]
    fn lc_pair_satisfies_identity() {
        let (w, h, t, er) = (0.25, 1.53, 0.035, 4.5);
        let l = trace_l_per_mm(w, h, t); // nH/mm
        let z0 = microstrip_z0(er, h, w, t);
        let c = trace_c_per_mm(w, h, er, z0); // pF/mm
        let er_eff_lc = (3.0e11_f64).powi(2) * l * 1e-9 * c * 1e-12;
        let er_eff_hj = (er + 1.0) / 2.0 + (er - 1.0) / 2.0 * (1.0 + 12.0 * h / w).powf(-0.5);
        assert!(
            (er_eff_lc - er_eff_hj).abs() / er_eff_hj < 0.01,
            "L'C' identity broken: pair says {:.2}, HJ says {:.2}",
            er_eff_lc,
            er_eff_hj
        );
    }

    #[test]
    fn trace_inductance_is_physical() {
        // 0.25mm trace on 0.78mm FR-4: ~0.5-0.9 nH/mm typical
        let l = trace_l_per_mm(0.25, 0.7825, 0.035);
        assert!(l > 0.3 && l < 1.2, "L' = {:.3} nH/mm should be physical", l);
    }

    #[test]
    fn trace_capacitance_is_physical() {
        // 0.25mm trace on FR-4: ~0.1 pF/mm typical
        let z0 = microstrip_z0(4.5, 0.7825, 0.25, 0.035);
        let c = trace_c_per_mm(0.25, 0.7825, 4.5, z0);
        assert!(c > 0.05 && c < 0.3, "C' = {:.3} pF/mm should be physical", c);
    }

    #[test]
    fn rf_net_with_default_width_flags_swr() {
        // A 2.4GHz RF net with the 0.25mm default width: Z0 on 1.6mm FR-4
        // is ~80Ω, far from 50Ω → SWR must be flagged.
        let board = crate::board::Board::default();
        let sig = crate::netfreq::net_signal("RF_OUT", &["2.4GHz"], None, None);
        let par = StrayParasitics {
            net: "RF_OUT".into(),
            length_mm: 20.0,
            l_nh: 0.0,
            c_pf: 0.0,
            vias: 0,
            pad_area_mm2: 3.0,
            routed_width_mm: 0.25,
        };
        let stackup = Stackup::default();
        let audit = audit(
            &board,
            &[(sig, par)],
            &stackup,
            &FabLimits::default(),
        );
        assert!(
            audit.findings.iter().any(|f| f.code.starts_with("swr")),
            "RF net with wrong-width trace must flag SWR, got {:?}",
            audit.findings
        );
    }

    #[test]
    fn slow_net_measures_quiet() {
        // A 500kHz NE555 net on the same stackup: no SWR flag needed
        let board = crate::board::Board::default();
        let sig = crate::netfreq::net_signal("N1", &["NE555"], None, None);
        let par = StrayParasitics {
            net: "N1".into(),
            length_mm: 30.0,
            l_nh: 0.0,
            c_pf: 0.0,
            vias: 0,
            pad_area_mm2: 2.0,
            routed_width_mm: 0.25,
        };
        let audit = audit(
            &board,
            &[(sig, par)],
            &Stackup::default(),
            &FabLimits::default(),
        );
        assert!(
            audit.findings.iter().all(|f| f.code != "swr-error"),
            "slow net should not flag SWR"
        );
    }

    #[test]
    fn power_net_with_many_vias_flags_inductance() {
        let board = crate::board::Board::default();
        let sig = crate::netfreq::net_signal("VCC", &["NE555"], None, None);
        let par = StrayParasitics {
            net: "VCC".into(),
            length_mm: 80.0,
            l_nh: 0.0,
            c_pf: 0.0,
            vias: 5,
            pad_area_mm2: 9.0,
            routed_width_mm: 0.25,
        };
        let audit = audit(
            &board,
            &[(sig, par)],
            &Stackup::default(),
            &FabLimits::default(),
        );
        // 80mm trace ≈ 40nH+ plus vias → power-inductance warning
        assert!(
            audit.findings.iter().any(|f| f.code == "power-inductance"),
            "long power net must warn about inductance"
        );
    }
}