//! Per-NET frequency awareness — the core of the physics harness.
//!
//! "each net must be aware of the frequency it carries from the component
//! specification and what it does in the circuit"
//!
//! Three sources of frequency truth, in priority order:
//!   1. The PLAN says it: PlannedNet.freq_hz / role (LLM or human declares)
//!   2. The component spec: value strings like "2.4GHz", "100MHz", "NE555"
//!      imply operating frequencies; IC part numbers imply clock classes
//!   3. The net's ROLE: VCC/GND = DC power; names containing CLK/CLOCK/
//!      XTAL/OSC = clock; RF/ANT = RF; USB/DDR/PCIE = interface budgets
//!
//! Everything downstream (impedance targets, crosstalk spacing, stub
//! limits, SWR) reads the per-net NetSignal computed here.

/// The circuit role of a net — what it DOES in the circuit.
#[derive(Debug, Clone, PartialEq)]
pub enum NetRole {
    /// DC power distribution.
    Power,
    /// Ground / return.
    Ground,
    /// Clock or crystal.
    Clock,
    /// RF / antenna / microwave path.
    Rf,
    /// High-speed digital interface (USB3/DDR/PCIe budgets).
    HighSpeed,
    /// General analog signal.
    Analog,
    /// Slow digital / control.
    Digital,
}

impl NetRole {
    /// Classify from a net name (the circuit's own vocabulary).
    pub fn from_name(name: &str) -> Option<NetRole> {
        let n = name.to_uppercase();
        if n == "VCC" || n == "VBUS" || n.starts_with("+3V") || n.starts_with("+5V")
            || n.starts_with("VDD") || n.starts_with("VIN")
        {
            return Some(NetRole::Power);
        }
        if n == "GND" || n == "VSS" || n.starts_with("AGND") || n.starts_with("DGND") {
            return Some(NetRole::Ground);
        }
        if n.contains("CLK") || n.contains("CLOCK") || n.contains("XTAL")
            || n.contains("OSC") || n.contains("MCO")
        {
            return Some(NetRole::Clock);
        }
        if n.contains("RF") || n.contains("ANT") || n.starts_with("LNA")
            || n.contains("PA_OUT") || n.contains("MIX")
        {
            return Some(NetRole::Rf);
        }
        if n.contains("USB") || n.contains("DDR") || n.contains("PCIE")
            || n.contains("SSTX") || n.contains("SSRX") || n.contains("MDI")
        {
            return Some(NetRole::HighSpeed);
        }
        None
    }
}

/// The frequency class of a net, with the numbers the router needs.
#[derive(Debug, Clone)]
pub struct NetSignal {
    pub name: String,
    pub role: NetRole,
    /// Highest frequency this net carries (Hz). Power = 0.
    pub freq_hz: f64,
    /// The frequency source — provenance for honest reporting.
    pub source: &'static str,
    /// Single-ended impedance target (Ω). None = not impedance-controlled.
    pub z_target: Option<f64>,
}

impl NetSignal {
    /// True if this net needs impedance control at its frequency.
    pub fn needs_impedance_control(&self) -> bool {
        match &self.role {
            NetRole::Rf => true,
            NetRole::Clock => self.freq_hz >= 20.0e6, // fast clocks
            NetRole::HighSpeed => true,
            NetRole::Analog => self.freq_hz >= 100.0e6,
            _ => false,
        }
    }

    /// True if transmission-line effects matter: rise time vs trace length.
    /// Uses the skill-101 critical length (Tpd ≈ 150 ps/inch FR-4).
    pub fn is_transmission_line(&self, trace_len_mm: f64) -> bool {
        // Fastest representable edge ≈ 0.35/f for digital, 0.1/f for RF sine
        let rise_s = match &self.role {
            NetRole::Rf => 0.1 / self.freq_hz.max(1.0),
            _ => 0.35 / self.freq_hz.max(1.0),
        };
        let l_crit_mm = (rise_s * 1e12 / (2.0 * 150.0)) * 25.4;
        trace_len_mm > l_crit_mm
    }

    /// Crosstalk spacing rule (skill 101): 3W normal, 5W for
    /// aggressive/high-speed neighbors.
    pub fn crosstalk_spacing_mm(&self, trace_width_mm: f64) -> f64 {
        match &self.role {
            NetRole::Rf | NetRole::HighSpeed => 5.0 * trace_width_mm,
            NetRole::Clock if self.freq_hz >= 100.0e6 => 5.0 * trace_width_mm,
            _ => 3.0 * trace_width_mm,
        }
    }
}

/// Extract a frequency from a component VALUE or PART NUMBER string.
/// "10pF@2.4GHz", "2.4GHz", "100MHz", "NE555" (timer ~ up to 500kHz),
/// "74HC00" (CMOS ~ tens of MHz), "LM358" (opamp ~ 1MHz class).
pub fn freq_from_component(value_or_lib: &str) -> Option<f64> {
    let s = value_or_lib.to_uppercase();
    // Explicit frequency in the string: e.g. "2.4GHZ", "100MHZ", "900MHZ"
    if let Some(f) = freq_from_text(&s) {
        return Some(f);
    }
    // Part-number classes (conservative upper bounds from datasheet families)
    if s.contains("555") {
        return Some(500.0e3); // NE555 astable practical limit
    }
    if s.starts_with("74HC") || s.starts_with("74AC") || s.starts_with("74LVC") {
        return Some(50.0e6); // fast CMOS toggle class
    }
    if s.starts_with("LM358") || s.starts_with("LM324") || s.contains("OPAMP") {
        return Some(1.0e6); // classic opamp GBW usage class
    }
    if s.contains("ESP32") || s.contains("STM32") {
        return Some(240.0e6); // MCU clock class
    }
    if s.contains("WIFI") || s.contains("BLE") || s.contains("NRF24") {
        return Some(2.4e9);
    }
    if s.contains("NE555") {
        return Some(500.0e3);
    }
    None
}

fn freq_from_text(s: &str) -> Option<f64> {
    // Match "<num>GHZ" or "<num>MHZ" or "<num>KHZ"
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let num: f64 = s[start..i].parse().ok()?;
            let rest = &s[i..];
            let mult = if rest.starts_with("GHZ") {
                1e9
            } else if rest.starts_with("MHZ") {
                1e6
            } else if rest.starts_with("KHZ") {
                1e3
            } else if rest.starts_with("HZ") {
                1.0
            } else {
                continue;
            };
            return Some(num * mult);
        }
        i += 1;
    }
    None
}

/// Derive a net's signal profile from everything the plan knows:
/// its name, its member components' values/lib_ids, and an explicit
/// plan-declared frequency (highest priority).
pub fn net_signal(
    net_name: &str,
    component_values: &[&str],
    declared_freq_hz: Option<f64>,
    declared_role: Option<&str>,
) -> NetSignal {
    let role = declared_role
        .and_then(|r| role_from_str(r))
        .or_else(|| NetRole::from_name(net_name))
        .unwrap_or(NetRole::Digital);

    // Frequency: explicit declaration wins; POWER/GROUND nets are DC (0 Hz)
    // regardless of what components sit on them — a 2.4GHz chip's VCC pin
    // still carries DC between decouplers.
    let (freq_hz, source) = if matches!(role, NetRole::Power | NetRole::Ground) {
        (0.0, "dc-by-role")
    } else if let Some(f) = declared_freq_hz {
        (f, "plan-declared")
    } else {
        // Otherwise: the highest frequency implied by member components
        let mut best: Option<f64> = None;
        for v in component_values {
            if let Some(f) = freq_from_component(v) {
                best = Some(best.map_or(f, |b: f64| b.max(f)));
            }
        }
        match best {
            Some(f) => (f, "component-spec"),
            None => (0.0, "none-found"),
        }
    };

    let z_target = match &role {
        NetRole::Rf => Some(50.0), // microwave practice: 50Ω system
        NetRole::HighSpeed => Some(50.0),
        NetRole::Clock if freq_hz >= 20.0e6 => Some(50.0),
        _ => None,
    };

    NetSignal {
        name: net_name.to_string(),
        role,
        freq_hz,
        source,
        z_target,
    }
}

fn role_from_str(r: &str) -> Option<NetRole> {
    match r.to_uppercase().as_str() {
        "POWER" => Some(NetRole::Power),
        "GROUND" | "GND" => Some(NetRole::Ground),
        "CLOCK" | "CLK" => Some(NetRole::Clock),
        "RF" => Some(NetRole::Rf),
        "HIGHSPEED" | "HIGH-SPEED" => Some(NetRole::HighSpeed),
        "ANALOG" => Some(NetRole::Analog),
        "DIGITAL" => Some(NetRole::Digital),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_roles_from_names() {
        assert_eq!(NetRole::from_name("VCC"), Some(NetRole::Power));
        assert_eq!(NetRole::from_name("GND"), Some(NetRole::Ground));
        assert_eq!(NetRole::from_name("USB_CLK"), Some(NetRole::Clock));
        assert_eq!(NetRole::from_name("RF_IN"), Some(NetRole::Rf));
        assert_eq!(NetRole::from_name("USB3_TX"), Some(NetRole::HighSpeed));
        assert_eq!(NetRole::from_name("N1"), None);
    }

    #[test]
    fn freq_from_component_spec() {
        assert!((freq_from_component("2.4GHz").unwrap() - 2.4e9).abs() < 1.0);
        assert!((freq_from_component("100MHz").unwrap() - 1e8).abs() < 1e3);
        assert!((freq_from_component("NE555").unwrap() - 500.0e3).abs() < 1.0);
        assert!((freq_from_component("74HC00").unwrap() - 50.0e6).abs() < 1e3);
        assert!(freq_from_component("10k").is_none(), "resistors carry no frequency");
    }

    #[test]
    fn net_signal_power_net_is_dc() {
        let s = net_signal("VCC", &["NE555"], None, None);
        assert_eq!(s.role, NetRole::Power);
        assert_eq!(s.freq_hz, 0.0, "power nets are DC regardless of members");
        assert!(!s.needs_impedance_control());
    }

    #[test]
    fn net_signal_rf_gets_50_ohm_target() {
        let s = net_signal("RF_OUT", &["2.4GHz"], None, None);
        assert_eq!(s.role, NetRole::Rf);
        assert!((s.freq_hz - 2.4e9).abs() < 1e6);
        assert_eq!(s.z_target, Some(50.0));
        assert!(s.needs_impedance_control());
    }

    #[test]
    fn net_signal_declared_freq_wins() {
        let s = net_signal("N1", &["NE555"], Some(2.4e9), Some("rf"));
        assert!((s.freq_hz - 2.4e9).abs() < 1.0);
        assert_eq!(s.source, "plan-declared");
        assert_eq!(s.role, NetRole::Rf);
    }

    #[test]
    fn net_signal_component_spec_fallback() {
        // A generic net whose members include a 100MHz part
        let s = net_signal("SIG_A", &["74HC00", "10k"], None, None);
        assert!((s.freq_hz - 50.0e6).abs() < 1e3, "74HC00 implies 50MHz");
        assert_eq!(s.source, "component-spec");
    }

    #[test]
    fn clock_net_is_transmission_line_above_critical_length() {
        // 100 MHz clock: rise ≈ 3.5ns → Lcrit ≈ 296mm... rise = 0.35/1e8 = 3.5ns
        // Lcrit = 3.5e-9*1e12/(2*150) inch = 11.67in = 296mm
        let s = net_signal("CLK_OUT", &["100MHz"], None, None);
        assert_eq!(s.role, NetRole::Clock);
        assert!(!s.is_transmission_line(100.0), "100mm < 296mm Lcrit");
        assert!(s.is_transmission_line(400.0), "400mm > 296mm Lcrit");
    }

    #[test]
    fn crosstalk_spacing_follows_role() {
        let rf = net_signal("RF_OUT", &["2.4GHz"], None, None);
        let slow = net_signal("N1", &["NE555"], None, None);
        assert_eq!(rf.crosstalk_spacing_mm(0.25), 1.25, "RF = 5W");
        assert_eq!(slow.crosstalk_spacing_mm(0.25), 0.75, "digital = 3W");
    }
}