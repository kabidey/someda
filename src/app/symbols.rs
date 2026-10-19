//! KiCad symbol libraries (`.kicad_sym`): the pin numbers, names and
//! electrical types of a part, read from the libraries KiCad installs.
//!
//! This is what keeps a model from inventing pinouts: the interview shows
//! the user `U1:7 = DISCH` from the library and every net assignment is
//! checked against it before anything is built.

use crate::sexp::{parse, Sexp};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub number: String,
    pub name: String,
    /// input, output, bidirectional, passive, power_in, power_out, …
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// "Lib:Name"
    pub lib_id: String,
    pub pins: Vec<Pin>,
    pub description: String,
    /// The library's default footprint, when the symbol names one.
    pub footprint: Option<String>,
}

impl Symbol {
    pub fn pin(&self, number: &str) -> Option<&Pin> {
        self.pins.iter().find(|p| p.number == number)
    }
    pub fn power_pins(&self) -> Vec<&Pin> {
        self.pins.iter().filter(|p| p.kind == "power_in").collect()
    }
}

/// Resolve "Lib:Name" to the `.kicad_sym` file that defines it.
pub fn resolve(lib_id: &str) -> Option<PathBuf> {
    let (lib, _) = lib_id.split_once(':')?;
    super::deps::symbol_dirs().into_iter().map(|d| d.join(format!("{}.kicad_sym", lib))).find(|p| p.is_file())
}

/// Load a symbol with its pins (following `extends`).
pub fn load(lib_id: &str) -> Result<Symbol, String> {
    let (lib, name) = lib_id.split_once(':').ok_or_else(|| format!("'{}' is not Lib:Name", lib_id))?;
    let path = resolve(lib_id).ok_or_else(|| format!("symbol library '{}' is not installed", lib))?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    load_from_text(&text, lib, name)
}

pub fn load_from_text(text: &str, lib: &str, name: &str) -> Result<Symbol, String> {
    let doc = parse(text).map_err(|e| e.to_string())?;
    let items = doc.as_tagged("kicad_symbol_lib").ok_or("not a kicad_symbol_lib")?;
    let find = |want: &str| -> Option<&[Sexp]> {
        items.iter().find_map(|it| {
            let body = it.as_tagged("symbol")?;
            (body.first()?.str_value()? == want).then_some(body)
        })
    };
    let body = find(name).ok_or_else(|| format!("symbol '{}' is not in {}.kicad_sym", name, lib))?;
    // `extends`: pins live on the parent
    let parent = body.iter().find_map(|it| it.as_tagged("extends")).and_then(|e| e.first()).and_then(|p| p.str_value()).map(|s| s.to_string());
    let pin_body = match &parent {
        Some(p) => find(p).ok_or_else(|| format!("symbol '{}' extends missing '{}'", name, p))?,
        None => body,
    };
    let mut pins = Vec::new();
    collect_pins(pin_body, &mut pins);
    pins.sort_by(|a, b| numeric_key(&a.number).cmp(&numeric_key(&b.number)).then_with(|| a.number.cmp(&b.number)));
    pins.dedup_by(|a, b| a.number == b.number);
    let prop = |key: &str, from: &[Sexp]| -> Option<String> {
        from.iter().find_map(|it| {
            let p = it.as_tagged("property")?;
            (p.first()?.str_value()? == key).then(|| p.get(1).and_then(|v| v.str_value()).unwrap_or("").to_string())
        })
    };
    let description = prop("Description", body).or_else(|| prop("Description", pin_body)).unwrap_or_default();
    let footprint = prop("Footprint", body).or_else(|| prop("Footprint", pin_body)).filter(|f| !f.is_empty());
    Ok(Symbol { lib_id: format!("{}:{}", lib, name), pins, description, footprint })
}

fn collect_pins(body: &[Sexp], out: &mut Vec<Pin>) {
    for it in body {
        if let Some(p) = it.as_tagged("pin") {
            let kind = p.first().and_then(|k| k.atom()).unwrap_or("passive").to_string();
            let name = p.iter().find_map(|x| x.as_tagged("name")).and_then(|n| n.first()).and_then(|n| n.str_value()).unwrap_or("").to_string();
            let number = p.iter().find_map(|x| x.as_tagged("number")).and_then(|n| n.first()).and_then(|n| n.str_value()).unwrap_or("").to_string();
            if !number.is_empty() {
                out.push(Pin { number, name, kind });
            }
        } else if let Some(unit) = it.as_tagged("symbol") {
            collect_pins(unit, out);
        }
    }
}

fn numeric_key(s: &str) -> (u32, u64) {
    match s.parse::<u64>() {
        Ok(n) => (0, n),
        Err(_) => (1, 0),
    }
}

/// Symbol names in a library (for the "which part?" question).
pub fn list_library(lib: &str) -> Vec<String> {
    let Some(path) = super::deps::symbol_dirs().into_iter().map(|d| d.join(format!("{}.kicad_sym", lib))).find(|p| p.is_file()) else { return vec![] };
    let Ok(text) = std::fs::read_to_string(&path) else { return vec![] };
    let Ok(doc) = parse(&text) else { return vec![] };
    let Some(items) = doc.as_tagged("kicad_symbol_lib") else { return vec![] };
    let mut names: Vec<String> = items.iter().filter_map(|it| it.as_tagged("symbol")?.first()?.str_value().map(|s| s.to_string())).collect();
    names.sort();
    names
}

/// Installed symbol libraries (names without extension).
pub fn libraries() -> Vec<String> {
    let mut out = Vec::new();
    for d in super::deps::symbol_dirs() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("kicad_sym") {
                    if let Some(s) = p.file_stem().and_then(|x| x.to_str()) {
                        out.push(s.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Symbols whose name contains every word of `query` (case-insensitive),
/// across all libraries, "Lib:Name", at most `limit`.
pub fn search(query: &str, limit: usize) -> Vec<String> {
    let words: Vec<String> = query.split_whitespace().map(|w| w.to_ascii_lowercase()).collect();
    let mut hits = Vec::new();
    for lib in libraries() {
        for name in list_library(&lib) {
            let l = name.to_ascii_lowercase();
            if words.iter().all(|w| l.contains(w)) {
                hits.push(format!("{}:{}", lib, name));
                if hits.len() >= limit {
                    return hits;
                }
            }
        }
    }
    hits
}

pub fn exists(lib_id: &str) -> bool {
    load(lib_id).is_ok()
}

/// Candidates for a "Lib:Name" the libraries lack, best first: names in
/// that library starting with the name (NE555 → NE555P, NE555D), then the
/// closest names there (normalized edit distance ≤ 35 %), then the exact
/// name in any other library. Empty when nothing is close.
pub fn closest(lib_id: &str, limit: usize) -> Vec<String> {
    let Some((lib, name)) = lib_id.split_once(':') else { return vec![] };
    let norm = |s: &str| s.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>();
    let want = norm(name);
    let mut out: Vec<String> = Vec::new();
    let names = list_library(lib);
    let mut starts: Vec<&String> = names.iter().filter(|n| norm(n).starts_with(&want) && !want.is_empty()).collect();
    starts.sort_by_key(|n| n.len());
    for n in starts {
        out.push(format!("{}:{}", lib, n));
    }
    let mut scored: Vec<(usize, &String)> = names
        .iter()
        .filter(|n| !out.iter().any(|o| o.ends_with(&format!(":{}", n))))
        .map(|n| (levenshtein(&want, &norm(n)), n))
        .filter(|(d, _)| d * 100 <= want.len().max(1) * 35)
        .collect();
    scored.sort();
    for (_, n) in scored {
        out.push(format!("{}:{}", lib, n));
    }
    if out.is_empty() {
        for other in libraries() {
            if other != lib && list_library(&other).iter().any(|n| n == name) {
                out.push(format!("{}:{}", other, name));
            }
        }
    }
    out.truncate(limit);
    out
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// A one-line pin map for display: "1 GND · 2 TR · 3 Q · …".
pub fn pin_map_line(sym: &Symbol) -> String {
    sym.pins.iter().map(|p| if p.name.is_empty() || p.name == "~" { p.number.clone() } else { format!("{} {}", p.number, p.name) }).collect::<Vec<_>>().join(" · ")
}

#[allow(dead_code)]
fn _unused(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    const LIB: &str = r#"(kicad_symbol_lib (version 20231120) (generator "t")
  (symbol "NE555P" (pin_names (offset 1.016)) (in_bom yes) (on_board yes)
    (property "Reference" "U" (at 0 0 0))
    (property "Value" "NE555P" (at 0 0 0))
    (property "Footprint" "Package_DIP:DIP-8_W7.62mm" (at 0 0 0))
    (property "Description" "Precision Timer, DIP-8" (at 0 0 0))
    (symbol "NE555P_0_1"
      (pin power_in line (at 0 0 0) (length 2.54) (name "GND" (effects)) (number "1" (effects)))
      (pin input line (at 0 0 0) (length 2.54) (name "TR" (effects)) (number "2" (effects)))
      (pin output line (at 0 0 0) (length 2.54) (name "Q" (effects)) (number "3" (effects)))
      (pin input line (at 0 0 0) (length 2.54) (name "R" (effects)) (number "4" (effects)))
      (pin input line (at 0 0 0) (length 2.54) (name "CV" (effects)) (number "5" (effects)))
      (pin input line (at 0 0 0) (length 2.54) (name "THR" (effects)) (number "6" (effects)))
      (pin input line (at 0 0 0) (length 2.54) (name "DIS" (effects)) (number "7" (effects)))
      (pin power_in line (at 0 0 0) (length 2.54) (name "VCC" (effects)) (number "8" (effects)))
    )
  )
  (symbol "NE555D" (extends "NE555P")
    (property "Footprint" "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm" (at 0 0 0))
  )
)"#;

    #[test]
    fn reads_pins_and_follows_extends() {
        let s = load_from_text(LIB, "Timer", "NE555D").unwrap();
        assert_eq!(s.pins.len(), 8);
        assert_eq!(s.pin("7").unwrap().name, "DIS");
        assert_eq!(s.pin("8").unwrap().kind, "power_in");
        assert_eq!(s.footprint.as_deref(), Some("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm"));
        assert_eq!(s.power_pins().len(), 2);
        assert!(pin_map_line(&s).starts_with("1 GND · 2 TR"));
        assert!(load_from_text(LIB, "Timer", "NE556").is_err());
    }

    #[test]
    fn reads_the_installed_library_when_present() {
        if resolve("Timer:NE555P").is_none() {
            return;
        }
        // KiCad ships NE555P (DIP) and NE555D (SOIC), not a bare NE555
        let c = closest("Timer:NE555", 5);
        assert!(c.iter().any(|x| x == "Timer:NE555P"), "{:?}", c);
        let s = load("Timer:NE555P").expect("installed NE555P");
        assert_eq!(s.pin("1").map(|p| p.name.as_str()), Some("GND"));
        assert_eq!(s.pin("8").map(|p| p.name.as_str()), Some("VCC"));
        assert!(search("ne555", 5).iter().any(|h| h == "Timer:NE555P"));
    }
}
