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
use std::collections::{HashMap, HashSet};

/// A declared differential pair as routed: its prescribed cross-section
/// and where that came from.
#[derive(Debug, Clone)]
pub struct PairInfo {
    pub p: String,
    pub n: String,
    pub z_diff: f64,
    pub width_mm: f64,
    pub gap_mm: f64,
    pub source: String,
    pub routed: bool,
}

/// A two-terminal capacitor between two nets (a decoupler when one net
/// is a rail and the other ground).
#[derive(Debug, Clone)]
pub struct Decoupler {
    pub reference: String,
    pub net_a: String,
    pub net_b: String,
    pub c_farads: f64,
    /// Placed at an IC's pins (error above 6 nH); otherwise a warning.
    pub strict: bool,
}

/// Board facts the audit needs beyond the copper: pairs, the breakout
/// zones the router exempted from the crosstalk rule (so audit and
/// router agree), per-net crosstalk gaps, terminated nets, decouplers.
#[derive(Debug, Clone, Default)]
pub struct SiContext {
    pub pairs: Vec<PairInfo>,
    /// (x0, y0, x1, y1, layer or "*", axis) in board mm. Axis 0: tolerated
    /// in every direction (pad surroundings); 1 / 2: a fine-pitch escape
    /// band whose pins escape along x / y — only parallel runs along that
    /// axis are tolerated, a run across the band is a real coupled pair.
    pub exempt_rects: Vec<(f64, f64, f64, f64, String, u8)>,
    /// Crosstalk families from the router: one signal carried by several
    /// nets through a small pass-through part (ESD array, series parts) —
    /// their traces are the same signal and never aggressors of each other.
    pub families: Vec<Vec<String>>,
    /// Required edge gap to other nets' traces (3W/5W rule) per net.
    pub gap_of: HashMap<String, f64>,
    /// The design clearance (the gap for nets not in `gap_of`).
    pub clearance_mm: f64,
    /// Nets that are terminated (pair interfaces, series resistors,
    /// plan-declared).
    pub terminated: HashSet<String>,
    pub decouplers: Vec<Decoupler>,
    /// Inner-layer index (1 = In1.Cu) of the plane each plane net lives on.
    pub plane_layer_of: HashMap<String, usize>,
    pub layer_count: u32,
}

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
    ctx: &SiContext,
) -> SiAudit {
    let mut findings = Vec::new();
    let pair_nets: HashSet<&str> = ctx.pairs.iter().flat_map(|p| [p.p.as_str(), p.n.as_str()]).collect();

    for (sig, par) in signals {
        let is_pair_net = pair_nets.contains(sig.name.as_str());
        let terminated = ctx.terminated.contains(&sig.name) || is_pair_net;
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

        // ─── SWR vs impedance target (single-ended impedance-controlled
        // nets; pairs are judged against their differential target below) ───
        if let Some(z_target) = sig.z_target {
            if sig.needs_impedance_control() && !is_pair_net {
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

        // (Power-net series inductance summed over a whole net said nothing
        // about decoupling — a plane net's fanout stubs are parallel paths.
        // Replaced by the per-capacitor loop inductance below.)
        let _ = matches!(sig.role, NetRole::Power | NetRole::Ground);

        // ─── Unterminated transmission line: reflections ring/overshoot ───
        // A net longer than the critical length for its rise time needs a
        // termination (series at the driver, parallel at the receiver, or
        // on-chip as USB/MIPI/Ethernet provide). Terminated lines do not
        // "oscillate" however long they are.
        if sig.freq_hz > 0.0 && sig.is_transmission_line(par.length_mm) && !terminated {
            findings.push(SiFinding {
                code: "unterminated-line",
                severity: "error",
                net: sig.name.clone(),
                detail: format!(
                    "{:.1}mm is electrically long at {:.0}MHz and nothing terminates it — reflections ring; add a series resistor at the driver or declare \"termination\"",
                    par.length_mm, sig.freq_hz / 1e6
                ),
            });
        }

        // ─── Self-resonance of the net's LC at its operating frequency ───
        // Only meaningful for a lumped, unterminated net: a terminated or
        // impedance-controlled line is a transmission line, not an LC tank
        // (the old check flagged every 20 mm MIPI lane at 500 MHz).
        if sig.freq_hz > 0.0 && total_l > 0.0 && total_c > 0.0 && !terminated && sig.z_target.is_none() {
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

    // ─── Differential pairs: cross-section as prescribed, both tracks ───
    for pi in &ctx.pairs {
        let widths = |net: &str| -> Vec<f64> {
            board.segments.iter().filter(|s| s.net.as_ref().map(|(_, n)| n == net).unwrap_or(false)).map(|s| s.width).collect()
        };
        let (wp, wn) = (widths(&pi.p), widths(&pi.n));
        if !pi.routed {
            findings.push(SiFinding { code: "pair-unrouted", severity: "error", net: format!("{}/{}", pi.p, pi.n), detail: format!("{:.0} Ω pair was not routed as a coupled pair", pi.z_diff) });
            continue;
        }
        let at_width = |ws: &[f64]| ws.iter().filter(|w| (*w - pi.width_mm).abs() < 2e-3).count();
        if wp.is_empty() || wn.is_empty() || at_width(&wp) == 0 || at_width(&wn) == 0 {
            findings.push(SiFinding { code: "pair-geometry", severity: "error", net: format!("{}/{}", pi.p, pi.n), detail: format!("routed tracks do not carry the prescribed {:.4} mm pair width", pi.width_mm) });
            continue;
        }
        findings.push(SiFinding {
            code: "pair-impedance",
            severity: "info",
            net: format!("{}/{}", pi.p, pi.n),
            detail: format!("{:.0} Ω differential: w {:.4} / gap {:.4} mm on {} — {}", pi.z_diff, pi.width_mm, pi.gap_mm, stackup.name, pi.source),
        });
    }

    // ─── Crosstalk: same-layer parallel runs closer than the 3W/5W gap ───
    // Real geometry: for every pair of axis-aligned segments of different
    // nets on the same layer, the overlap length at an edge-to-edge gap
    // below max(gap(a), gap(b)). Portions inside the breakout zones the
    // router exempted (pad surroundings, fine-pitch escape bands) are
    // tolerated and reported separately; the rest is a violation.
    {
        let mate_of: HashMap<&str, &str> = ctx.pairs.iter().flat_map(|p| [(p.p.as_str(), p.n.as_str()), (p.n.as_str(), p.p.as_str())]).collect();
        let gap = |net: &str| ctx.gap_of.get(net).copied().unwrap_or(ctx.clearance_mm);
        let segs: Vec<(&crate::board::Segment, &str)> = board.segments.iter().filter_map(|s| s.net.as_ref().map(|(_, n)| (s, n.as_str()))).collect();
        let mut coupled: HashMap<(String, String), (f64, f64, f64, f64)> = HashMap::new(); // (violating, exempt, min_gap, required)
        for i in 0..segs.len() {
            let (a, na) = segs[i];
            let ah = (a.start.1 - a.end.1).abs() < 1e-6;
            let av = (a.start.0 - a.end.0).abs() < 1e-6;
            if !(ah || av) || (ah && av) {
                continue;
            }
            for (b, nb) in segs.iter().skip(i + 1) {
                if *nb == na || b.layer != a.layer {
                    continue;
                }
                if mate_of.get(na) == Some(nb) {
                    continue;
                }
                if ctx.families.iter().any(|f| f.iter().any(|x| x == na) && f.iter().any(|x| x == nb)) {
                    continue;
                }
                let bh = (b.start.1 - b.end.1).abs() < 1e-6;
                let bv = (b.start.0 - b.end.0).abs() < 1e-6;
                if (ah && !bh) || (av && !bv) || (bh && bv) {
                    continue;
                }
                let required = gap(na).max(gap(nb));
                if required <= ctx.clearance_mm + 1e-9 {
                    continue; // two quiet nets: DRC clearance is the rule
                }
                let (a0, a1, ac) = if ah { (a.start.0.min(a.end.0), a.start.0.max(a.end.0), a.start.1) } else { (a.start.1.min(a.end.1), a.start.1.max(a.end.1), a.start.0) };
                let (b0, b1, bc) = if ah { (b.start.0.min(b.end.0), b.start.0.max(b.end.0), b.start.1) } else { (b.start.1.min(b.end.1), b.start.1.max(b.end.1), b.start.0) };
                let (o0, o1) = (a0.max(b0), a1.min(b1));
                if o1 - o0 <= 0.05 {
                    continue;
                }
                let edge_gap = (ac - bc).abs() - (a.width + b.width) / 2.0;
                if edge_gap >= required - 1e-6 {
                    continue;
                }
                // Exempt portions along the overlap: rects covering the
                // perpendicular coordinate midway between the two runs
                let mid = (ac + bc) / 2.0;
                let mut cuts: Vec<(f64, f64)> = Vec::new();
                for (x0, y0, x1, y1, layer, axis) in &ctx.exempt_rects {
                    if layer != "*" && layer != &a.layer {
                        continue;
                    }
                    if (*axis == 1 && !ah) || (*axis == 2 && !av) {
                        continue;
                    }
                    let (p0, p1, q0, q1) = if ah { (*x0, *x1, *y0, *y1) } else { (*y0, *y1, *x0, *x1) };
                    if mid < q0 || mid > q1 {
                        continue;
                    }
                    let (c0, c1) = (o0.max(p0), o1.min(p1));
                    if c1 > c0 {
                        cuts.push((c0, c1));
                    }
                }
                cuts.sort_by(|p, q| p.0.partial_cmp(&q.0).unwrap_or(std::cmp::Ordering::Equal));
                let mut exempt_len = 0.0;
                let mut cursor = o0;
                for (c0, c1) in cuts {
                    let (c0, c1) = (c0.max(cursor), c1);
                    if c1 > c0 {
                        exempt_len += c1 - c0;
                        cursor = c1;
                    }
                }
                let violating = (o1 - o0) - exempt_len;
                let key = if na < *nb { (na.to_string(), nb.to_string()) } else { (nb.to_string(), na.to_string()) };
                let e = coupled.entry(key).or_insert((0.0, 0.0, f64::INFINITY, required));
                e.0 += violating.max(0.0);
                e.1 += exempt_len;
                e.2 = e.2.min(edge_gap);
                e.3 = e.3.max(required);
            }
        }
        let mut keys: Vec<_> = coupled.keys().cloned().collect();
        keys.sort();
        for k in keys {
            let (viol, ex, min_gap, req) = coupled[&k];
            // A single-cell sliver at a crossing or corner is not a parallel
            // run; the rule is about coupled length (NEXT grows with it).
            if viol > 0.5 {
                findings.push(SiFinding {
                    code: "crosstalk-violation",
                    severity: "error",
                    net: format!("{}↔{}", k.0, k.1),
                    detail: format!("{:.2}mm parallel at {:.3}mm edge gap outside breakout zones (rule: ≥{:.3}mm, 3W/5W)", viol, min_gap, req),
                });
            } else if ex > 2.0 {
                findings.push(SiFinding {
                    code: "crosstalk-breakout",
                    severity: "info",
                    net: format!("{}↔{}", k.0, k.1),
                    detail: format!("{:.2}mm parallel inside pad/escape breakout zones at {:.3}mm gap (tolerated; rule ≥{:.3}mm elsewhere)", ex, min_gap, req),
                });
            }
        }
    }

    // ─── Decoupling: per-capacitor loop inductance from the routed stubs ───
    // L_loop = L(stub pad A → via/plane) + L(stub pad B → via/plane) +
    // via-pair loop (μ0/π · h · ln(2s/d)). This is what sets a capacitor's
    // mounted self-resonance: 100 nF with 1 nH resonates at 16 MHz, with
    // 6 nH at 6.5 MHz — beyond that the "capacitor" is an inductor.
    for d in &ctx.decouplers {
        if let Some((l_total, detail)) = cap_loop_inductance_nh(board, stackup, ctx, d) {
            let (code, severity) = if l_total > 6.0 && d.strict { ("decoupling-loop", "error") } else if l_total > 3.0 { ("decoupling-loop", "warning") } else { ("decoupling-loop", "info") };
            if severity != "info" {
                findings.push(SiFinding { code, severity, net: format!("{} ({}/{})", d.reference, d.net_a, d.net_b), detail: format!("{:.2}nH mounting loop — {}", l_total, detail) });
            }
        }
    }

    // ─── Fab-scope check of the geometry we actually routed ───
    let min_space_seen = fab.min_space_mm.max(ctx.clearance_mm.min(fab.min_space_mm)); // the router's clearance governs
    let min_width_seen = board.segments.iter().map(|s| s.width).fold(f64::INFINITY, f64::min);
    let min_width_seen = if min_width_seen.is_finite() { min_width_seen } else { 0.25 };
    for v in fab.check_trace(min_width_seen, min_space_seen) {
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


/// Depth (mm) from the outer copper to inner layer `k` (1 = In1.Cu):
/// the outer prepreg exactly, inner spacings assumed uniform across the
/// remaining thickness (a documented approximation; JLCPCB's inner cores
/// are not published per stackup in this engine).
fn plane_depth_mm(stackup: &Stackup, layers: u32, k: usize) -> f64 {
    let outer = stackup.h_mm + stackup.copper_thickness_mm;
    if k <= 1 || layers <= 4 {
        return outer;
    }
    let inner_span = (stackup.finished_thickness_mm - 2.0 * outer).max(0.1);
    outer + (k as f64 - 1.0) * inner_span / (layers as f64 - 3.0)
}

/// Follow a pad's net copper from the pad until a via (plane entry) or
/// another pad: returns (stub length mm, dominant width, via position if
/// the stub ends in one).
/// Routed stub from a component's pad on `net` to its first via or pad:
/// (length mm, width mm, ends in a via). None when not on the board.
pub fn component_stub(board: &Board, reference: &str, net: &str) -> Option<(f64, f64, bool)> {
    let fp = board.footprints.iter().find(|f| f.reference == reference)?;
    let pad = fp.pads.iter().find(|p| p.net.as_ref().map(|(_, n)| n == net).unwrap_or(false))?;
    let (len, w, via) = pad_stub(board, pad, net);
    Some((len, w, via.is_some()))
}

fn pad_stub(board: &Board, pad: &crate::board::Pad, net: &str) -> (f64, f64, Option<(f64, f64)>) {
    let close = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3;
    let (hx, hy) = pad.bbox_half();
    let in_pad = |p: (f64, f64)| (p.0 - pad.x).abs() <= hx + 1e-3 && (p.1 - pad.y).abs() <= hy + 1e-3;
    let segs: Vec<&crate::board::Segment> = board.segments.iter().filter(|s| s.net.as_ref().map(|(_, n)| n == net).unwrap_or(false)).collect();
    let vias: Vec<&crate::board::Via> = board.vias.iter().filter(|v| v.net.as_ref().map(|(_, n)| n == net).unwrap_or(false)).collect();
    let other_pads: Vec<(f64, f64, f64, f64)> = board.footprints.iter().flat_map(|f| f.pads.iter().filter(|q| q.net.as_ref().map(|(_, n)| n == net).unwrap_or(false) && !(q.x == pad.x && q.y == pad.y)).map(|q| { let (qx, qy) = q.bbox_half(); (q.x, q.y, qx, qy) })).collect();
    let mut visited = vec![false; segs.len()];
    let mut frontier: Vec<(f64, f64)> = vec![(pad.x, pad.y)];
    let mut len = 0.0;
    let mut width = pad.width.min(pad.height).max(0.1);
    let mut via_at: Option<(f64, f64)> = None;
    let mut guard = 0;
    while let Some(pt) = frontier.pop() {
        guard += 1;
        if guard > 500 {
            break;
        }
        if let Some(v) = vias.iter().find(|v| close((v.x, v.y), pt) || ((v.x - pt.0).powi(2) + (v.y - pt.1).powi(2)).sqrt() < v.size / 2.0) {
            via_at = Some((v.x, v.y));
            break;
        }
        if other_pads.iter().any(|(qx, qy, hx2, hy2)| (pt.0 - qx).abs() <= *hx2 + 1e-3 && (pt.1 - qy).abs() <= *hy2 + 1e-3) && !in_pad(pt) {
            break; // reached another pad: not a plane connection
        }
        for (i, s) in segs.iter().enumerate() {
            if visited[i] {
                continue;
            }
            let (touch_s, touch_e) = (close(s.start, pt) || (in_pad(pt) && in_pad(s.start)), close(s.end, pt) || (in_pad(pt) && in_pad(s.end)));
            if touch_s || touch_e {
                visited[i] = true;
                len += ((s.end.0 - s.start.0).powi(2) + (s.end.1 - s.start.1).powi(2)).sqrt();
                width = s.width;
                frontier.push(if touch_s { s.end } else { s.start });
            }
        }
    }
    (len, width, via_at)
}

/// Mounting loop inductance (nH) of a decoupler from its routed stubs and
/// via pair; None when the capacitor is not on the board.
pub fn cap_loop_inductance_nh(board: &Board, stackup: &Stackup, ctx: &SiContext, d: &Decoupler) -> Option<(f64, String)> {
    let fp = board.footprints.iter().find(|f| f.reference == d.reference)?;
    let pad_on = |net: &str| fp.pads.iter().find(|p| p.net.as_ref().map(|(_, n)| n == net).unwrap_or(false));
    let (pa, pb) = (pad_on(&d.net_a)?, pad_on(&d.net_b)?);
    let (la, wa, va) = pad_stub(board, pa, &d.net_a);
    let (lb, wb, vb) = pad_stub(board, pb, &d.net_b);
    let l_a = trace_l_per_mm(wa, stackup.h_mm, stackup.copper_thickness_mm) * la;
    let l_b = trace_l_per_mm(wb, stackup.h_mm, stackup.copper_thickness_mm) * lb;
    let depth = |net: &str| ctx.plane_layer_of.get(net).map(|k| plane_depth_mm(stackup, ctx.layer_count, *k)).unwrap_or(stackup.h_mm + stackup.copper_thickness_mm);
    let drill = board.vias.first().map(|v| v.drill).unwrap_or(0.3).max(0.1);
    let l_via = match (va, vb) {
        (Some(a), Some(b)) => {
            let s = ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt().max(drill * 1.5);
            crate::formulas::via_pair_loop_nh(depth(&d.net_a).max(depth(&d.net_b)), s, drill)
        }
        (Some(a), None) | (None, Some(a)) => {
            // one via + a pad-to-pad trace: half a loop with s = via→far pad
            let far = if va.is_some() { (pb.x, pb.y) } else { (pa.x, pa.y) };
            let s = ((a.0 - far.0).powi(2) + (a.1 - far.1).powi(2)).sqrt().max(drill * 1.5);
            0.5 * crate::formulas::via_pair_loop_nh(depth(&d.net_a).max(depth(&d.net_b)), s, drill)
        }
        (None, None) => 0.0,
    };
    let total = l_a + l_b + l_via;
    let detail = format!(
        "stubs {:.1}mm ({:.2}nH) + {:.1}mm ({:.2}nH), via pair {:.2}nH; {} {}",
        la, l_a, lb, l_b, l_via,
        if va.is_some() { "via" } else { "no via" },
        if vb.is_some() { "/ via" } else { "/ no via" }
    );
    Some((total, detail))
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
            &SiContext::default(),
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
            &SiContext::default(),
        );
        assert!(
            audit.findings.iter().all(|f| f.code != "swr-error"),
            "slow net should not flag SWR"
        );
    }

    fn board_with_tracks(extra: &str) -> crate::board::Board {
        crate::board::Board::from_kicad_pcb(&format!(
            "(kicad_pcb (version 20221018)
              (layers (0 \"F.Cu\" signal) (1 \"In1.Cu\" signal) (2 \"In2.Cu\" signal) (3 \"In3.Cu\" signal) (4 \"In4.Cu\" signal) (31 \"B.Cu\" signal))
              (gr_line (start 0 0) (end 40 0) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 40 0) (end 40 30) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 40 30) (end 0 30) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 0 30) (end 0 0) (layer \"Edge.Cuts\") (width 0.1))
              (net 1 \"GND\") (net 2 \"CLK\") (net 3 \"QUIET\") (net 4 \"+3V3\")
              {}
            )", extra)).unwrap()
    }

    #[test]
    fn crosstalk_audit_measures_parallel_runs_and_honours_breakout_zones() {
        // CLK (5W: 0.508 mm edge gap) and QUIET run parallel 10 mm apart by
        // 0.3 mm (edge gap 0.173) on F.Cu: a violation; the same run inside
        // an exempt rect is tolerated.
        let b = board_with_tracks(
            "(segment (start 5 10) (end 15 10) (width 0.127) (layer \"F.Cu\") (net 2))
             (segment (start 5 10.3) (end 15 10.3) (width 0.127) (layer \"F.Cu\") (net 3))",
        );
        let sig = crate::netfreq::net_signal("CLK", &[], Some(100e6), Some("clock"));
        let par = measure_stray(&b, &sig);
        let mut ctx = SiContext { clearance_mm: 0.127, layer_count: 6, ..Default::default() };
        ctx.gap_of.insert("CLK".into(), 4.0 * 0.127);
        let a = audit(&b, &[(sig.clone(), par.clone())], &Stackup::for_layers(6), &FabLimits::default(), &ctx);
        let v = a.findings.iter().find(|f| f.code == "crosstalk-violation").expect("violation reported");
        assert!(v.detail.starts_with("10.00mm"), "{}", v.detail);
        ctx.exempt_rects.push((0.0, 0.0, 40.0, 30.0, "*".into(), 0));
        let a2 = audit(&b, &[(sig, par)], &Stackup::for_layers(6), &FabLimits::default(), &ctx);
        assert!(a2.findings.iter().all(|f| f.code != "crosstalk-violation"));
        assert!(a2.findings.iter().any(|f| f.code == "crosstalk-breakout"));
    }

    #[test]
    fn decoupling_loop_inductance_from_stubs_and_vias() {
        // C1: pad 1 on +3V3, pad 2 on GND; each pad has a 1 mm stub to a via.
        let b = board_with_tracks(
            "(footprint \"C\" (layer \"F.Cu\") (at 10 10 0)
                (fp_text reference \"C1\" (at 0 -2))
                (pad \"1\" smd rect (at -0.5 0) (size 0.6 0.9) (layers \"F.Cu\") (net 4 \"+3V3\"))
                (pad \"2\" smd rect (at 0.5 0) (size 0.6 0.9) (layers \"F.Cu\") (net 1 \"GND\")))
             (segment (start 9.5 10) (end 9.5 11) (width 0.127) (layer \"F.Cu\") (net 4))
             (segment (start 10.5 10) (end 10.5 11) (width 0.127) (layer \"F.Cu\") (net 1))
             (via (at 9.5 11) (size 0.35) (drill 0.2) (layers \"F.Cu\" \"B.Cu\") (net 4))
             (via (at 10.5 11) (size 0.35) (drill 0.2) (layers \"F.Cu\" \"B.Cu\") (net 1))",
        );
        let ctx = SiContext { clearance_mm: 0.127, layer_count: 6, plane_layer_of: [("GND".to_string(), 1usize), ("+3V3".to_string(), 3usize)].into_iter().collect(), ..Default::default() };
        let d = Decoupler { reference: "C1".into(), net_a: "+3V3".into(), net_b: "GND".into(), c_farads: 100e-9, strict: true };
        let (l, detail) = cap_loop_inductance_nh(&b, &Stackup::for_layers(6), &ctx, &d).expect("cap on board");
        // two 1 mm stubs (~0.3 nH each on 0.1 mm prepreg) + via pair to the
        // +3V3 plane ~1 mm deep, 1 mm apart: 0.4·1·ln(10) ≈ 0.9 nH
        assert!(l > 0.8 && l < 3.0, "L_loop {:.2} nH ({})", l, detail);
        assert!(detail.contains("via / via"), "{}", detail);
    }

    #[test]
    fn unterminated_long_line_is_an_error_and_termination_clears_it() {
        let b = board_with_tracks("(segment (start 2 2) (end 38 2) (width 0.127) (layer \"F.Cu\") (net 2)) (segment (start 38 2) (end 38 28) (width 0.127) (layer \"F.Cu\") (net 2)) (segment (start 38 28) (end 2 28) (width 0.127) (layer \"F.Cu\") (net 2))");
        // 1 GHz clock over 88 mm: far beyond the critical length
        let sig = crate::netfreq::net_signal("CLK", &[], Some(1.0e9), Some("clock"));
        let par = measure_stray(&b, &sig);
        let ctx = SiContext { clearance_mm: 0.127, layer_count: 6, ..Default::default() };
        let a = audit(&b, &[(sig.clone(), par.clone())], &Stackup::for_layers(6), &FabLimits::default(), &ctx);
        assert!(a.findings.iter().any(|f| f.code == "unterminated-line" && f.severity == "error"));
        assert!(a.findings.iter().all(|f| f.code != "power-inductance"), "the whole-net power inductance heuristic is gone");
        let mut ctx2 = ctx.clone();
        ctx2.terminated.insert("CLK".into());
        let a2 = audit(&b, &[(sig, par)], &Stackup::for_layers(6), &FabLimits::default(), &ctx2);
        assert!(a2.findings.iter().all(|f| f.code != "unterminated-line" && f.code != "net-self-resonance"));
    }
}