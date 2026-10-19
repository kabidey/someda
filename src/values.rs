//! Component value parsing: "100nF", "10uF", "4.7uH", "33R 1%", "10k 1%",
//! "0R", "6.8k", "51R" → SI units. Returns None for anything that is not
//! a plain value with an optional SI prefix (so part numbers never parse).

fn parse_number_prefix(s: &str) -> Option<(f64, f64)> {
    // leading number, then an SI prefix letter; "4R7" / "4k7" styles too
    let t = s.trim();
    let mut num = String::new();
    let mut rest = "";
    for (i, ch) in t.char_indices() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
        } else {
            rest = &t[i..];
            break;
        }
    }
    if num.is_empty() {
        return None;
    }
    let mut mantissa: f64 = num.parse().ok()?;
    let rest = rest.trim_start();
    let mut chars = rest.chars();
    let prefix = chars.next();
    let mut mult = match prefix {
        Some('p') => 1e-12,
        Some('n') => 1e-9,
        Some('u') | Some('µ') | Some('μ') => 1e-6,
        Some('m') => 1e-3,
        Some('k') | Some('K') => 1e3,
        Some('M') => 1e6,
        Some('G') => 1e9,
        Some('R') | Some('r') | Some('F') | Some('H') | Some('Ω') => 1.0,
        None => 1.0,
        _ => return None,
    };
    // "4R7", "4k7": digits after the prefix are the fraction
    let after: String = chars.clone().take_while(|c| c.is_ascii_digit()).collect();
    if !after.is_empty() && !num.contains('.') {
        let frac: f64 = format!("0.{}", after).parse().ok()?;
        mantissa += frac;
    }
    if prefix == Some('m') && rest.to_ascii_lowercase().starts_with("mhz") {
        mult = 1e6;
    }
    Some((mantissa, mult))
}

/// Capacitance in farads from a value string, if it is one.
pub fn capacitance_f(value: &str) -> Option<f64> {
    let v = value.trim();
    let first = v.split_whitespace().next()?;
    let lower = first.to_ascii_lowercase();
    if !(lower.ends_with('f') || lower.ends_with("pf") || lower.ends_with("nf") || lower.ends_with("uf") || lower.ends_with("µf") || lower.ends_with("mf")) {
        return None;
    }
    let (m, mult) = parse_number_prefix(first)?;
    Some(m * mult)
}

/// Resistance in ohms from a value string, if it is one ("33R", "10k 1%",
/// "0R", "4k7", "1M", "51R 1%", "100 ohm").
pub fn resistance_ohm(value: &str) -> Option<f64> {
    let v = value.trim();
    let first = v.split_whitespace().next()?;
    let lower = first.to_ascii_lowercase();
    let ohmish = lower.ends_with('r') || lower.ends_with('k') || lower.ends_with('m') || lower.ends_with("ω") || lower.ends_with("ohm") || lower.ends_with("ohms")
        || lower.contains('r') && lower.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
        || lower.contains('k') && lower.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);
    if !ohmish {
        // "100 ohm"
        if v.to_ascii_lowercase().contains("ohm") {
            let (m, mult) = parse_number_prefix(first)?;
            return Some(m * mult);
        }
        return None;
    }
    // reject capacitor/inductor strings
    if lower.ends_with('f') || lower.ends_with('h') {
        return None;
    }
    let cleaned = first.trim_end_matches("ohms").trim_end_matches("ohm").trim_end_matches('Ω');
    let (m, mult) = parse_number_prefix(cleaned)?;
    Some(m * mult)
}

/// Inductance in henries from a value string, if it is one.
pub fn inductance_h(value: &str) -> Option<f64> {
    let first = value.trim().split_whitespace().next()?;
    let lower = first.to_ascii_lowercase();
    if !lower.ends_with('h') || lower.ends_with("hz") {
        return None;
    }
    let (m, mult) = parse_number_prefix(first)?;
    Some(m * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_capacitors() {
        assert!((capacitance_f("100nF").unwrap() - 100e-9).abs() < 1e-15);
        assert!((capacitance_f("10uF").unwrap() - 10e-6).abs() < 1e-12);
        assert!((capacitance_f("18pF").unwrap() - 18e-12).abs() < 1e-18);
        assert!((capacitance_f("22uF 6.3V X5R").unwrap() - 22e-6).abs() < 1e-12);
        assert!(capacitance_f("TPS562200").is_none());
        assert!(capacitance_f("33R").is_none());
    }

    #[test]
    fn parses_resistors() {
        assert!((resistance_ohm("33R").unwrap() - 33.0).abs() < 1e-9);
        assert!((resistance_ohm("51R 1%").unwrap() - 51.0).abs() < 1e-9);
        assert!((resistance_ohm("10k 1%").unwrap() - 10e3).abs() < 1e-6);
        assert!((resistance_ohm("4k7").unwrap() - 4700.0).abs() < 1e-6);
        assert!((resistance_ohm("0R").unwrap()).abs() < 1e-9);
        assert!((resistance_ohm("6.8k 1%").unwrap() - 6800.0).abs() < 1e-6);
        assert!((resistance_ohm("1M").unwrap() - 1e6).abs() < 1e-3);
        assert!(resistance_ohm("100nF").is_none());
        assert!(resistance_ohm("USBLC6-2SC6").is_none());
        assert!(resistance_ohm("4.7uH 3A").is_none());
    }

    #[test]
    fn parses_inductors() {
        assert!((inductance_h("4.7uH 3A").unwrap() - 4.7e-6).abs() < 1e-12);
        assert!(inductance_h("24MHz").is_none());
    }
}
