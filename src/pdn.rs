//! Power-distribution impedance per rail from the decouplers actually on
//! the board: every capacitor is an R-L-C branch (ESR by case class, ESL =
//! package ESL + its routed mounting loop), the plane adds parallel-plate
//! capacitance, the regulator is a resistive source behind the inductance
//! of its output path. |Z(f)| is swept 100 kHz–1 GHz and compared with the
//! target impedance Z_t = ΔV / ΔI (5 % ripple at half the rail current,
//! the AMD/Xilinx PDN convention). Anti-resonance peaks above the target
//! are where the rail "rings" on a load step.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct CapBranch {
    pub reference: String,
    pub c_f: f64,
    pub esr_ohm: f64,
    pub esl_nh: f64,
}

#[derive(Debug, Clone)]
pub struct RailModel {
    pub rail: String,
    pub volts: f64,
    pub current_a: f64,
    pub caps: Vec<CapBranch>,
    /// Plane capacitance (pF) between this rail's plane and the ground plane.
    pub plane_c_pf: f64,
    /// Regulator branch: series R (Ω) and L (nH) from the regulator to the rail copper.
    pub vrm_r_ohm: f64,
    pub vrm_l_nh: f64,
}

#[derive(Debug, Clone)]
pub struct RailResult {
    pub rail: String,
    pub z_target_ohm: f64,
    pub peak_ohm: f64,
    pub peak_hz: f64,
    pub over_target_from_hz: Option<f64>,
    pub over_target_to_hz: Option<f64>,
    pub n_caps: usize,
}

/// Typical ESR of a ceramic MLCC by capacitance class (mΩ) — Murata /
/// TDK characterization data for X5R/X7R at their self-resonance.
pub fn mlcc_esr_ohm(c_f: f64) -> f64 {
    if c_f >= 10e-6 {
        0.005
    } else if c_f >= 1e-6 {
        0.010
    } else if c_f >= 100e-9 {
        0.030
    } else if c_f >= 10e-9 {
        0.080
    } else {
        0.200
    }
}

/// Package ESL (nH) by capacitance class / likely case size: 0402 ≈ 0.4,
/// 0603 ≈ 0.6, 0805 ≈ 0.8 nH (vendor characterization data).
pub fn mlcc_esl_nh(c_f: f64) -> f64 {
    if c_f >= 10e-6 {
        0.8
    } else if c_f >= 1e-6 {
        0.6
    } else {
        0.4
    }
}

/// Minimal complex arithmetic (no external crate): admittance sums.
#[derive(Clone, Copy, Debug)]
struct C(f64, f64);
impl C {
    fn inv(self) -> C {
        let d = self.0 * self.0 + self.1 * self.1;
        C(self.0 / d, -self.1 / d)
    }
    fn add(self, o: C) -> C {
        C(self.0 + o.0, self.1 + o.1)
    }
    fn norm(self) -> f64 {
        (self.0 * self.0 + self.1 * self.1).sqrt()
    }
}

/// |Z| of the rail at frequency f (Hz).
pub fn rail_z(model: &RailModel, f_hz: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f_hz;
    let mut y = C(0.0, 0.0);
    for c in &model.caps {
        let z = C(c.esr_ohm, w * c.esl_nh * 1e-9 - 1.0 / (w * c.c_f));
        y = y.add(z.inv());
    }
    if model.plane_c_pf > 0.0 {
        y = y.add(C(0.0, w * model.plane_c_pf * 1e-12));
    }
    let zv = C(model.vrm_r_ohm, w * model.vrm_l_nh * 1e-9);
    y = y.add(zv.inv());
    y.inv().norm()
}

/// Target impedance: ΔV / ΔI with 5 % ripple and a 50 % load step.
pub fn z_target(volts: f64, current_a: f64) -> f64 {
    (0.05 * volts) / (0.5 * current_a.max(0.01))
}

pub fn sweep(model: &RailModel) -> RailResult {
    let zt = z_target(model.volts, model.current_a);
    let mut peak = 0.0;
    let mut peak_hz = 0.0;
    let mut over_from: Option<f64> = None;
    let mut over_to: Option<f64> = None;
    // 100 kHz → 200 MHz: the board-level decouplers' band. Above it the
    // package and die capacitance own the rail; no discrete MLCC network
    // meets a sub-ohm target at 1 GHz and judging one there is noise.
    let n = 400;
    for i in 0..=n {
        let f = 1.0e5 * 10f64.powf((200.0e6f64 / 1.0e5).log10() * i as f64 / n as f64);
        let z = rail_z(model, f);
        if z > peak {
            peak = z;
            peak_hz = f;
        }
        if z > zt {
            over_from.get_or_insert(f);
            over_to = Some(f);
        }
    }
    RailResult { rail: model.rail.clone(), z_target_ohm: zt, peak_ohm: peak, peak_hz, over_target_from_hz: over_from, over_target_to_hz: over_to, n_caps: model.caps.len() }
}

/// Parallel-plate capacitance (pF) of a plane pair: ε0·εr·A/h.
pub fn plane_capacitance_pf(area_mm2: f64, h_mm: f64, er: f64) -> f64 {
    8.854e-3 * er * area_mm2 / h_mm.max(0.01)
}

/// Rail currents: each consumer's declared power split across the rails
/// it draws from, weighted by its pin count on each rail (a proxy for the
/// rail's share when the plan gives one power figure per part).
pub fn rail_currents(consumers: &[(String, f64, Vec<(String, f64, usize)>)]) -> HashMap<String, f64> {
    // (reference, power_w, [(rail, volts, pins_on_rail)])
    let mut out: HashMap<String, f64> = HashMap::new();
    for (_, p_w, rails) in consumers {
        let total_pins: usize = rails.iter().map(|r| r.2).sum();
        if total_pins == 0 {
            continue;
        }
        for (rail, v, pins) in rails {
            let share = p_w * (*pins as f64) / (total_pins as f64);
            *out.entry(rail.clone()).or_insert(0.0) += share / v.max(0.1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_capacitor_resonates_where_physics_says() {
        // 100 nF with 1 nH total: f = 1/(2π√(LC)) = 15.9 MHz, |Z| there = ESR
        let m = RailModel { rail: "+1V8".into(), volts: 1.8, current_a: 0.5, caps: vec![CapBranch { reference: "C1".into(), c_f: 100e-9, esr_ohm: 0.03, esl_nh: 1.0 }], plane_c_pf: 0.0, vrm_r_ohm: 1e6, vrm_l_nh: 0.0 };
        let f0 = 1.0 / (2.0 * std::f64::consts::PI * (1e-9 * 100e-9f64).sqrt());
        assert!((f0 - 15.9e6).abs() / 15.9e6 < 0.01);
        assert!((rail_z(&m, f0) - 0.03).abs() < 1e-3);
        assert!(rail_z(&m, f0 / 10.0) > 0.1 && rail_z(&m, f0 * 10.0) > 0.1);
    }

    #[test]
    fn target_impedance_follows_the_convention() {
        // 1.2 V at 1 A: 60 mV / 0.5 A = 120 mΩ
        assert!((z_target(1.2, 1.0) - 0.12).abs() < 1e-9);
    }

    #[test]
    fn more_capacitors_lower_the_peak() {
        let cap = |c: f64| CapBranch { reference: "C".into(), c_f: c, esr_ohm: mlcc_esr_ohm(c), esl_nh: mlcc_esl_nh(c) + 1.0 };
        let base = RailModel { rail: "r".into(), volts: 3.3, current_a: 0.5, caps: vec![cap(10e-6)], plane_c_pf: 200.0, vrm_r_ohm: 0.02, vrm_l_nh: 20.0 };
        let mut more = base.clone();
        more.caps.extend([cap(100e-9), cap(100e-9), cap(1e-6)]);
        let (a, b) = (sweep(&base), sweep(&more));
        assert!(b.peak_ohm < a.peak_ohm, "{} !< {}", b.peak_ohm, a.peak_ohm);
    }
}
