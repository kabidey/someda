//! Thermal-aware design harness — power dissipation drives placement and
//! via strategy (skill 014: θ_JA = θ_JC + θ_CS + θ_SA; ΔT = P·θ_JA;
//! 1 thermal via per 1-2mm² of thermal pad).
//!
//! The PLACEMENT consumes component power declarations; high-power parts
//! get separation, thermal-via budgets, and copper-balance accounting.

use crate::formulas::{
    achieved_density, board_area_budget_mm2, copper_imbalance_pct,
    junction_temp_c, theta_ja, thermal_via_count,
};

/// A component's thermal profile.
#[derive(Debug, Clone)]
pub struct ThermalProfile {
    pub reference: String,
    /// Power dissipation (W). 0 = passive.
    pub power_w: f64,
    /// Package thermal resistance, junction-to-ambient (°C/W).
    pub theta_ja_c_per_w: f64,
    /// Max junction temperature (°C) — silicon limit, default 125 for
    /// generic parts, 150 for automotive classes.
    pub t_j_max_c: f64,
}

/// Package-class θ_JA estimates (no heatsink, still air) — conservative
/// upper bounds for placement decisions, not certification numbers.
pub fn package_theta_ja(footprint_or_lib: &str) -> f64 {
    let s = footprint_or_lib.to_uppercase();
    if s.contains("DIP-8") || s.contains("PDIP") {
        85.0 // classic plastic DIP
    } else if s.contains("SOIC-8") || s.contains("SO-8") {
        120.0
    } else if s.contains("SOT-23-6") || s.contains("SOT-23-5") || s.contains("TSOT") {
        200.0 // 6/5-lead SOT with a copper pour under GND: JEDEC ~200
    } else if s.contains("SOT-23") {
        250.0 // tiny SMD
    } else if s.contains("QFN") || s.contains("DFN") || s.contains("_EP") || s.contains("THERMALVIAS") {
        40.0 // exposed pad wins
    } else if s.contains("LQFP-128") || s.contains("LQFP-144") || s.contains("LQFP-100") {
        45.0 // large QFP, 4-layer board JEDEC class
    } else if s.contains("QFP") {
        60.0
    } else if s.contains("TO-220") {
        65.0
    } else if s.contains("0402") || s.contains("0201") || s.contains("01005") {
        400.0 // passives: irrelevant at their power
    } else {
        150.0 // generic SMD assumption
    }
}

/// Parse power from a component VALUE string: "10k", "NE555", "LDO 500mW",
/// "R 0.25W", "1W LED". Returns W.
pub fn power_from_value(value: &str) -> f64 {
    let s = value.to_uppercase();
    // Explicit wattage in value: "250MW", "0.5W", "1W", "3W LED"
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() || bytes[i] == b'.' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let num: f64 = s[start..i].parse().unwrap_or(0.0);
            let rest = &s[i..];
            if rest.starts_with("W ") || rest == "W" {
                return num;
            }
            if rest.starts_with("MW ") || rest == "MW" {
                return num / 1000.0;
            }
        } else {
            i += 1;
        }
    }
    0.0
}

/// The thermal audit of a placement: junction temps, via budgets,
/// density, copper balance.
#[derive(Debug, Clone)]
pub struct ThermalFinding {
    pub code: &'static str,
    pub severity: &'static str,
    pub subject: String,
    pub detail: String,
}

pub struct ThermalAudit {
    pub findings: Vec<ThermalFinding>,
    /// Component → computed T_J at ambient.
    pub junction_temps: Vec<(String, f64)>,
}

/// Audit a placement thermally.
pub fn audit(
    profiles: &[ThermalProfile],
    ambient_c: f64,
    courtyard_areas_mm2: &[f64],
    board_area_mm2: f64,
    copper_top_frac: f64,
    copper_bottom_frac: f64,
) -> ThermalAudit {
    let mut findings = Vec::new();
    let mut junction_temps = Vec::new();

    for p in profiles {
        if p.power_w <= 0.0 {
            continue;
        }
        // Thermal chain (skill 014): assume θ_CS + θ_SA folded into θ_JA
        // for a bare board; vias add capability we account separately.
        let tj = junction_temp_c(ambient_c, p.power_w, p.theta_ja_c_per_w);
        junction_temps.push((p.reference.clone(), tj));

        if tj > p.t_j_max_c {
            findings.push(ThermalFinding {
                code: "thermal-overlimit",
                severity: "error",
                subject: p.reference.clone(),
                detail: format!(
                    "T_J = {:.0}°C > T_J_max {:.0}°C at P={:.2}W, θ_JA={:.0}°C/W — heatsink or vias required",
                    tj, p.t_j_max_c, p.power_w, p.theta_ja_c_per_w
                ),
            });
        } else if tj > p.t_j_max_c - 20.0 {
            findings.push(ThermalFinding {
                code: "thermal-marginal",
                severity: "warning",
                subject: p.reference.clone(),
                detail: format!(
                    "T_J = {:.0}°C within 20°C of the {:.0}°C limit — verify airflow",
                    tj, p.t_j_max_c
                ),
            });
        }

        // Thermal via budget: power parts need vias (skill 014: 1 per 1-2mm²)
        if p.power_w >= 0.5 {
            let vias = thermal_via_count(9.0); // a 3×3mm thermal pad class
            findings.push(ThermalFinding {
                code: "thermal-via-budget",
                severity: "info",
                subject: p.reference.clone(),
                detail: format!(
                    "plan ≥{} thermal vias under the pad (P={:.2}W)",
                    vias, p.power_w
                ),
            });
        }
    }

    // Hot spots near each other? High-power parts should be apart.
    // (Placement constraint: the placer receives "apart" hints for
    // any two parts each dissipating > 0.5W.)
    let hot: Vec<&ThermalProfile> =
        profiles.iter().filter(|p| p.power_w >= 0.5).collect();
    if hot.len() >= 2 {
        findings.push(ThermalFinding {
            code: "thermal-separation-hint",
            severity: "info",
            subject: hot.iter().map(|p| p.reference.clone()).collect::<Vec<_>>().join(","),
            detail: format!(
                "{} parts dissipate ≥0.5W — keep them ≥15mm apart and out of each other's airflow",
                hot.len()
            ),
        });
    }

    // Density (skill 102): router pain above 70%
    let density = achieved_density(courtyard_areas_mm2, board_area_mm2);
    if density > 0.70 {
        findings.push(ThermalFinding {
            code: "density-too-high",
            severity: "warning",
            subject: "(board)".into(),
            detail: format!(
                "density {:.0}% > 70% — thermal crowding and router pain",
                density * 100.0
            ),
        });
    }

    // Copper balance (skill 101/JLCPCB): |top−bottom|/avg < 15%
    let imbalance = copper_imbalance_pct(copper_top_frac, copper_bottom_frac);
    if imbalance >= 15.0 {
        findings.push(ThermalFinding {
            code: "copper-imbalance",
            severity: "warning",
            subject: "(board)".into(),
            detail: format!(
                "copper imbalance {:.0}% ≥ 15% — warp risk; pour the sparse side",
                imbalance
            ),
        });
    }

    ThermalAudit {
        findings,
        junction_temps,
    }
}

/// The board area budget WITH thermal headroom: high-power parts want
/// more than courtyard area (airflow + copper spread). Each ≥0.5W part
/// adds 50% to its courtyard claim.
pub fn thermal_area_budget_mm2(
    courtyard_areas_mm2: &[f64],
    powers_w: &[f64],
    target_density: f64,
) -> f64 {
    let effective: Vec<f64> = courtyard_areas_mm2
        .iter()
        .zip(powers_w.iter().chain(std::iter::repeat(&0.0)))
        .map(|(a, p)| if *p >= 0.5 { a * 1.5 } else { *a })
        .collect();
    board_area_budget_mm2(&effective, target_density)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_parsing_from_values() {
        assert!((power_from_value("0.25W resistor").abs() - 0.25) < 1e-9);
        assert!((power_from_value("500mW LDO").abs() - 0.5) < 1e-9);
        assert!((power_from_value("3W LED").abs() - 3.0) < 1e-9);
        assert_eq!(power_from_value("10k"), 0.0);
    }

    #[test]
    fn package_theta_ja_classes() {
        assert_eq!(package_theta_ja("Package_DIP:DIP-8"), 85.0);
        assert_eq!(package_theta_ja("SOT-23"), 250.0);
        assert!(package_theta_ja("QFN-32") < 100.0, "exposed pad wins");
    }

    #[test]
    fn thermal_overlimit_detected() {
        // 2W in a SOT-23: T_J = 25 + 2×250 = 525°C — way over
        let profiles = vec![ThermalProfile {
            reference: "U1".into(),
            power_w: 2.0,
            theta_ja_c_per_w: 250.0,
            t_j_max_c: 150.0,
        }];
        let audit = audit(&profiles, 25.0, &[2.0], 100.0, 0.5, 0.5);
        assert!(
            audit.findings.iter().any(|f| f.code == "thermal-overlimit"),
            "2W in SOT-23 must be flagged"
        );
        assert!((audit.junction_temps[0].1 - 525.0).abs() < 0.5);
    }

    #[test]
    fn power_parsing_adversarial() {
        // Real BOM value strings with wattage markers
        assert!((power_from_value("R 0.125W") - 0.125).abs() < 1e-9);
        assert!((power_from_value("10R 2W wirewound") - 2.0).abs() < 1e-9);
        assert!((power_from_value("LDO 250mW") - 0.25).abs() < 1e-9);
        assert_eq!(power_from_value("1uF"), 0.0, "no false positive on uF");
        assert_eq!(power_from_value("10nF 50V"), 0.0, "50V is a voltage not watts");
        assert_eq!(power_from_value("NE555"), 0.0);
    }

    #[test]
    fn theta_ja_matches_datasheet_classes() {
        // Cross-checked against typical datasheet values:
        // DIP-8 ~85-100, SOIC-8 ~100-180, SOT-23 ~250-350, QFN ~20-45
        assert_eq!(package_theta_ja("Package_DIP:DIP-8"), 85.0);
        assert_eq!(package_theta_ja("Package_SO:SOIC-8"), 120.0);
        assert_eq!(package_theta_ja("SOT-23"), 250.0);
        assert!(package_theta_ja("QFN-32") <= 45.0);
        // TO-220 without heatsink: datasheet 60-70
        assert!((package_theta_ja("TO-220") - 65.0).abs() < 1e-9);
    }

    #[test]
    fn thermal_comfortable_silent() {
        // 0.01W signal diode: no findings
        let profiles = vec![ThermalProfile {
            reference: "D1".into(),
            power_w: 0.01,
            theta_ja_c_per_w: 250.0,
            t_j_max_c: 150.0,
        }];
        let audit = audit(&profiles, 25.0, &[2.0], 100.0, 0.5, 0.5);
        assert!(audit.findings.is_empty(), "tiny dissipation = silent");
    }

    #[test]
    fn thermal_area_budget_grows_with_power() {
        let small = thermal_area_budget_mm2(&[60.0], &[0.0], 0.35);
        let hot = thermal_area_budget_mm2(&[60.0], &[1.0], 0.35);
        assert!(hot > small, "power parts claim more board: {} vs {}", hot, small);
    }

    #[test]
    fn copper_balance_warns_at_imbalance() {
        let profiles: Vec<ThermalProfile> = vec![];
        let audit = audit(&profiles, 25.0, &[60.0; 4], 400.0, 0.9, 0.3);
        assert!(
            audit.findings.iter().any(|f| f.code == "copper-imbalance"),
            "90% vs 30% copper must warn"
        );
    }
}