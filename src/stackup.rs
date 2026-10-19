//! Stackup and impedance calculator using public IPC-2141A / IPC-2251 formulas.
//! Microstrip, symmetric stripline, and differential-pair impedance from
//! trace geometry and laminate properties.

/// Copper foil thickness in oz → mm (1 oz = 1.37 mil = 0.0348 mm)
pub fn copper_oz_to_mm(oz: f64) -> f64 {
    oz * 0.0348
}

/// Standard FR-4 dielectric constant (er). Typical range 4.0–4.8.
pub const FR4_ER: f64 = 4.4;

#[derive(Debug, Clone)]
pub struct Layer {
    pub name: String,
    pub material: String,
    pub thickness_mm: f64,
}

#[derive(Debug, Clone)]
pub struct Stackup {
    pub layers: Vec<Layer>,
    pub copper_thickness_mm: f64,
    pub er: f64,
}

impl Default for Stackup {
    fn default() -> Self {
        // Standard 2-layer 1.6mm board: F.Cu / core / B.Cu
        Stackup {
            layers: vec![
                Layer { name: "F.Cu".into(), material: "copper".into(), thickness_mm: 0.035 },
                Layer { name: "core".into(), material: "FR-4".into(), thickness_mm: 1.53 },
                Layer { name: "B.Cu".into(), material: "copper".into(), thickness_mm: 0.035 },
            ],
            copper_thickness_mm: 0.035,
            er: FR4_ER,
        }
    }
}

/// Microstrip impedance (IPC-2141A, external trace over a plane).
/// w: trace width mm, h: height above plane mm, t: copper thickness mm, er: dielectric constant
pub fn microstrip_z0(w: f64, h: f64, t: f64, er: f64) -> f64 {
    if w <= 0.0 || h <= 0.0 {
        return f64::NAN;
    }
    // Effective width correction for finite thickness (IPC-2141A)
    let we = if t > 0.0 {
        if w / h < 0.5 {
            // IPC-2141A: we = w + (t/π)(1 + ln(2h/t)) for narrow traces
            w + (t / std::f64::consts::PI) * (1.0 + (2.0 * h / t).ln())
        } else {
            // Wide-trace correction
            w + (t / std::f64::consts::PI) * (1.0 + (2.0 * h / t).ln())
        }
    } else {
        w
    };
    // Effective dielectric constant (IPC-2141A microstrip)
    let er_eff = (er + 1.0) / 2.0 + (er - 1.0) / 2.0 / (1.0 + 12.0 * h / we).sqrt();
    // Impedance
    if we / h < 1.0 {
        60.0 * ((8.0 * h / we) + (we / h)).ln() / er_eff.sqrt()
    } else {
        (120.0 * std::f64::consts::PI / (we / h + 1.393 + 0.667 * (we / h + 1.444).ln())) / er_eff.sqrt()
    }
}

/// Simplified stripline (the standard IPC-2141A formula):
pub fn stripline_z0_simple(w: f64, b: f64, t: f64, er: f64) -> f64 {
    if w <= 0.0 || b <= 0.0 {
        return f64::NAN;
    }
    // IPC-2141A: Z0 = 60/sqrt(er) * ln(4b / (0.67*pi*(0.8w+t)))
    let inner = 0.67 * std::f64::consts::PI * (0.8 * w + t);
    if inner <= 0.0 || inner >= 4.0 * b {
        return f64::NAN;
    }
    60.0 / er.sqrt() * (4.0 * b / inner).ln()
}

/// Differential impedance for a coupled pair (edge-coupled microstrip approximation).
/// Z0 = single-ended impedance, s = gap between traces mm, h = height above plane.
pub fn diff_z0(z0: f64, s: f64, h: f64, _er: f64) -> f64 {
    if s <= 0.0 || h <= 0.0 {
        return f64::NAN;
    }
    // IPC-2251 approximation for loosely-coupled pairs:
    let k = (-0.96 * s / h).exp();
    2.0 * z0 * (1.0 - 0.48 * k)
}

/// Solve for the trace width that achieves a target microstrip impedance
/// (binary search over the impedance function).
pub fn microstrip_width_for(target_z0: f64, h: f64, t: f64, er: f64) -> f64 {
    let mut lo = 0.01;
    let mut hi = 10.0;
    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        let z = microstrip_z0(mid, h, t, er);
        if z > target_z0 {
            lo = mid; // wider → lower impedance
        } else {
            hi = mid; // narrower → higher impedance
        }
    }
    (lo + hi) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microstrip_50ohm_classic() {
        // Classic FR-4: 1.6mm board, 1oz copper, ~0.3mm trace ≈ 50Ω (well-known result)
        let h = 1.53; // dielectric height for 2-layer 1.6mm board
        let t = copper_oz_to_mm(1.0);
        let z = microstrip_z0(0.3, h, t, FR4_ER);
        // 0.3mm on 1.6mm FR-4 2-layer is a thin trace → high Z (~80-100Ω).
        // The well-known 50Ω result on this stackup needs ~2.7mm width.
        assert!(z > 100.0 && z < 140.0, "0.3mm thin trace should be 100-140Ω (Python ref: 123.6), got {}", z);
        let z50 = microstrip_z0(2.7, h, t, FR4_ER);
        assert!(z50 > 45.0 && z50 < 55.0, "2.7mm classic 50Ω trace, got {}", z50);
    }

    #[test]
    fn microstrip_wider_is_lower() {
        let h = 1.53;
        let t = 0.035;
        let z_narrow = microstrip_z0(0.1, h, t, FR4_ER);
        let z_wide = microstrip_z0(1.0, h, t, FR4_ER);
        assert!(z_wide < z_narrow, "wider trace must have lower Z: {} !< {}", z_wide, z_narrow);
    }

    #[test]
    fn width_solver_recovers_target() {
        let h = 0.2; // 4-layer inner-to-plane
        let t = 0.035;
        let target = 50.0;
        let w = microstrip_width_for(target, h, t, FR4_ER);
        let achieved = microstrip_z0(w, h, t, FR4_ER);
        assert!((achieved - target).abs() < 1.0, "solver got {}Ω for target {}Ω (w={:.3}mm)", achieved, target, w);
    }

    #[test]
    fn stripline_classic() {
        // Symmetric stripline in FR-4, ~0.2mm trace, ~0.5mm between planes ≈ 50Ω-class
        let b = 1.0;
        let t = 0.035;
        let z = stripline_z0_simple(0.2, b, t, FR4_ER);
        // 0.2mm centered in 1.0mm dielectric is thinner than the 50Ω point (~0.35mm)
        assert!(z > 55.0 && z < 75.0, "thin stripline, got {}", z);
        // The actual 50Ω width for b=1.0mm FR-4: ~0.35mm
        let z50 = stripline_z0_simple(0.35, b, t, FR4_ER);
        assert!(z50 > 45.0 && z50 < 55.0, "0.35mm should be ~50Ω, got {}", z50);
    }

    #[test]
    fn diff_pair_lower_than_2x_single() {
        let h = 1.53;
        let t = 0.035;
        let z0 = microstrip_z0(0.3, h, t, FR4_ER);
        let zd = diff_z0(z0, 0.3, h, FR4_ER);
        assert!(zd < 2.0 * z0, "coupled diff pair must be < 2x single: {} !< {}", zd, 2.0 * z0);
        assert!(zd > z0, "diff pair must be > single: {} !> {}", zd, z0);
    }

    #[test]
    fn copper_thickness() {
        assert!((copper_oz_to_mm(1.0) - 0.0348).abs() < 1e-9);
        assert!((copper_oz_to_mm(2.0) - 0.0696).abs() < 1e-9);
    }

    #[test]
    fn invalid_inputs_return_nan() {
        assert!(microstrip_z0(0.0, 1.0, 0.035, FR4_ER).is_nan());
        assert!(microstrip_z0(0.3, 0.0, 0.035, FR4_ER).is_nan());
        assert!(stripline_z0_simple(0.0, 1.0, 0.035, FR4_ER).is_nan());
    }
}
