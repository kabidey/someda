//! KiCad footprint library loader: resolves `Lib:Name` to a stock
//! `.kicad_mod` file, parses its pads and courtyard, and instantiates a
//! placed board footprint whose pads carry the plan's nets.
//!
//! The library footprint is kept verbatim (as an s-expression) so the
//! board writer can re-emit it with placement, reference and nets
//! injected — silkscreen, fab outline, courtyard and 3D model survive.
//! KiCad is the authority on footprint geometry; someda never invents pad
//! positions for a part it can look up.

use crate::board::{Footprint, Pad, PadType};
use crate::sexp::{parse, Sexp};
use std::path::{Path, PathBuf};

/// One pad as defined in the library (footprint-relative, unrotated).
#[derive(Debug, Clone)]
pub struct LibPad {
    pub number: String,
    pub shape: String,
    pub pad_type: PadType,
    pub x: f64,
    pub y: f64,
    /// Library pad angle (degrees)
    pub rotation: f64,
    pub width: f64,
    pub height: f64,
    pub drill: f64,
    pub drill_oval: Option<(f64, f64)>,
    pub layers: Vec<String>,
    /// Raw `(net ...)` arguments when parsed from a board file
    pub net_hint: Option<Vec<Sexp>>,
}

/// A parsed library footprint.
#[derive(Debug, Clone)]
pub struct LibFootprint {
    pub name: String,
    pub pads: Vec<LibPad>,
    /// Courtyard bbox relative to origin: (min_x, min_y, max_x, max_y)
    pub courtyard: Option<(f64, f64, f64, f64)>,
    pub is_smd: bool,
    pub sexp: Sexp,
}

impl LibFootprint {
    /// Distinct pad numbers (same-numbered pads share a net in KiCad).
    pub fn pad_numbers(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for p in &self.pads {
            if !p.number.is_empty() && !v.contains(&p.number) {
                v.push(p.number.clone());
            }
        }
        v
    }

    /// Courtyard size (w, h) in the footprint frame; falls back to the pad
    /// extents plus 0.25mm when the library has no courtyard.
    pub fn size(&self) -> (f64, f64) {
        match self.courtyard {
            Some((x0, y0, x1, y1)) => (x1 - x0, y1 - y0),
            None => {
                let b = self.pad_bbox();
                (b.2 - b.0 + 0.5, b.3 - b.1 + 0.5)
            }
        }
    }

    fn pad_bbox(&self) -> (f64, f64, f64, f64) {
        let mut b = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for p in &self.pads {
            let a = p.rotation.to_radians();
            let hw = (p.width / 2.0) * a.cos().abs() + (p.height / 2.0) * a.sin().abs();
            let hh = (p.width / 2.0) * a.sin().abs() + (p.height / 2.0) * a.cos().abs();
            b.0 = b.0.min(p.x - hw);
            b.1 = b.1.min(p.y - hh);
            b.2 = b.2.max(p.x + hw);
            b.3 = b.3.max(p.y + hh);
        }
        if b.0.is_infinite() {
            (-0.5, -0.5, 0.5, 0.5)
        } else {
            b
        }
    }

    /// Instantiate a placed board footprint. `net_for` maps a pad number
    /// to its (index, name) net; pads it returns None for stay unconnected.
    pub fn instantiate(
        &self,
        reference: &str,
        value: &str,
        lib_id: &str,
        x: f64,
        y: f64,
        rotation: f64,
        net_for: &dyn Fn(&str) -> Option<(usize, String)>,
    ) -> Footprint {
        let pads = self
            .pads
            .iter()
            .map(|lp| {
                let (ox, oy) = crate::board::rotate_offset(lp.x, lp.y, rotation);
                Pad {
                    number: lp.number.clone(),
                    net: if lp.number.is_empty() { None } else { net_for(&lp.number) },
                    x: x + ox,
                    y: y + oy,
                    width: lp.width,
                    height: lp.height,
                    shape: lp.shape.clone(),
                    pad_type: lp.pad_type,
                    drill: lp.drill,
                    drill_oval: lp.drill_oval,
                    rotation: lp.rotation + rotation,
                    layers: lp.layers.clone(),
                }
            })
            .collect();
        // The stored source must carry the library id KiCad shows ("Lib:Name")
        let mut sexp = self.sexp.clone();
        if let Sexp::List(items) = &mut sexp {
            if items.len() >= 2 {
                items[1] = Sexp::Str(lib_id.to_string());
            }
        }
        Footprint {
            reference: reference.to_string(),
            lib_name: lib_id.to_string(),
            value: value.to_string(),
            x,
            y,
            rotation,
            pads,
            courtyard: self.courtyard,
            source: Some(sexp),
        }
    }
}

/// Directories searched for `*.pretty` libraries, in priority order.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(v) = std::env::var("SOMEDA_FOOTPRINT_DIRS") {
        for d in v.split(':').filter(|s| !s.is_empty()) {
            dirs.push(PathBuf::from(d));
        }
    }
    for var in ["KICAD10_FOOTPRINT_DIR", "KICAD9_FOOTPRINT_DIR", "KICAD8_FOOTPRINT_DIR", "KICAD7_FOOTPRINT_DIR"] {
        if let Ok(v) = std::env::var(var) {
            dirs.push(PathBuf::from(v));
        }
    }
    for d in [
        "/usr/share/kicad/footprints",
        "/usr/local/share/kicad/footprints",
        "/usr/share/kicad-nightly/footprints",
        "/Applications/KiCad/KiCad.app/Contents/SharedSupport/footprints",
        "C:\\Program Files\\KiCad\\10.0\\share\\kicad\\footprints",
        "C:\\Program Files\\KiCad\\9.0\\share\\kicad\\footprints",
        "C:\\Program Files\\KiCad\\8.0\\share\\kicad\\footprints",
    ] {
        dirs.push(PathBuf::from(d));
    }
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(Path::new(&home).join(".local/share/kicad/10.0/footprints"));
        dirs.push(Path::new(&home).join(".local/share/kicad/9.0/footprints"));
    }
    dirs
}

/// Resolve "Lib:Name" to the `.kicad_mod` path, if any search dir has it.
pub fn resolve(lib_id: &str) -> Option<PathBuf> {
    let (lib, name) = lib_id.split_once(':')?;
    for dir in search_dirs() {
        let p = dir.join(format!("{}.pretty", lib)).join(format!("{}.kicad_mod", name));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Load and parse a library footprint by "Lib:Name".
pub fn load(lib_id: &str) -> Result<LibFootprint, String> {
    let path = resolve(lib_id).ok_or_else(|| {
        format!("footprint '{}' not found in any KiCad footprint directory", lib_id)
    })?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    parse_kicad_mod(&text)
}

/// Parse `.kicad_mod` text.
pub fn parse_kicad_mod(text: &str) -> Result<LibFootprint, String> {
    let doc = parse(text).map_err(|e| e.to_string())?;
    parse_footprint_sexp(&doc)
}

/// Parse a `(footprint ...)` (or legacy `(module ...)`) s-expression.
pub fn parse_footprint_sexp(doc: &Sexp) -> Result<LibFootprint, String> {
    let items = match doc {
        Sexp::List(v) if !v.is_empty() => v,
        _ => return Err("not a footprint".into()),
    };
    match items[0].atom() {
        Some("footprint") | Some("module") => {}
        _ => return Err("root element is not (footprint ...)".into()),
    }
    let name = items.get(1).and_then(|s| s.str_value()).unwrap_or("").to_string();
    let mut pads = Vec::new();
    let mut crt = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    let mut is_smd = false;
    for child in items.iter().skip(2) {
        if let Some(attr) = child.as_tagged("attr") {
            if attr.iter().any(|a| a.atom() == Some("smd")) {
                is_smd = true;
            }
        }
        if child.as_tagged("pad").is_some() {
            if let Some(p) = parse_pad(child) {
                pads.push(p);
            }
        }
        // Courtyard graphics on F.CrtYd / B.CrtYd
        if let Sexp::List(parts) = child {
            let tag = parts.first().and_then(|t| t.atom()).unwrap_or("");
            if matches!(tag, "fp_line" | "fp_rect" | "fp_poly" | "fp_circle" | "fp_arc") {
                let on_crt = parts.iter().any(|p| {
                    p.as_tagged("layer")
                        .and_then(|l| l.first())
                        .and_then(|s| s.str_value())
                        .map(|s| s.ends_with(".CrtYd"))
                        .unwrap_or(false)
                });
                if on_crt {
                    for pt in graphic_points(parts, tag) {
                        crt.0 = crt.0.min(pt.0);
                        crt.1 = crt.1.min(pt.1);
                        crt.2 = crt.2.max(pt.0);
                        crt.3 = crt.3.max(pt.1);
                    }
                }
            }
        }
    }
    let courtyard = if crt.0.is_finite() && crt.2 > crt.0 && crt.3 > crt.1 {
        Some(crt)
    } else {
        None
    };
    if !is_smd {
        is_smd = !pads.is_empty() && pads.iter().all(|p| p.pad_type == PadType::Smd);
    }
    Ok(LibFootprint { name, pads, courtyard, is_smd, sexp: doc.clone() })
}

fn graphic_points(parts: &[Sexp], tag: &str) -> Vec<(f64, f64)> {
    let mut pts = Vec::new();
    let xy = |l: &[Sexp]| -> Option<(f64, f64)> {
        Some((l.first()?.f64_value()?, l.get(1)?.f64_value()?))
    };
    for p in parts {
        for key in ["start", "end", "mid"] {
            if let Some(v) = p.as_tagged(key) {
                if let Some(pt) = xy(v) {
                    pts.push(pt);
                }
            }
        }
        if let Some(v) = p.as_tagged("pts") {
            for q in v {
                if let Some(c) = q.as_tagged("xy") {
                    if let Some(pt) = xy(c) {
                        pts.push(pt);
                    }
                }
            }
        }
    }
    if tag == "fp_circle" {
        // (center x y) (end x y): radius from the two points
        let center = parts.iter().find_map(|p| p.as_tagged("center")).and_then(xy);
        let end = parts.iter().find_map(|p| p.as_tagged("end")).and_then(xy);
        if let (Some(c), Some(e)) = (center, end) {
            let r = ((e.0 - c.0).powi(2) + (e.1 - c.1).powi(2)).sqrt();
            pts.clear();
            pts.push((c.0 - r, c.1 - r));
            pts.push((c.0 + r, c.1 + r));
        }
    }
    pts
}

fn parse_pad(child: &Sexp) -> Option<LibPad> {
    let items = match child { Sexp::List(v) => v, _ => return None };
    let number = items.get(1).and_then(|s| s.str_value())?.to_string();
    let pad_type = match items.get(2).and_then(|s| s.atom()) {
        Some("smd") => PadType::Smd,
        Some("thru_hole") => PadType::ThruHole,
        Some("np_thru_hole") => PadType::NpThruHole,
        Some("connect") => PadType::Smd,
        _ => PadType::Smd,
    };
    let shape = items.get(3).and_then(|s| s.atom()).unwrap_or("rect").to_string();
    let mut x = 0.0;
    let mut y = 0.0;
    let mut rotation = 0.0;
    let mut width = 0.0;
    let mut height = 0.0;
    let mut drill = 0.0;
    let mut drill_oval = None;
    let mut layers = Vec::new();
    let mut net_hint = None;
    for sub in items.iter().skip(4) {
        if let Some(a) = sub.as_tagged("at") {
            x = a.first().and_then(|v| v.f64_value()).unwrap_or(0.0);
            y = a.get(1).and_then(|v| v.f64_value()).unwrap_or(0.0);
            rotation = a.get(2).and_then(|v| v.f64_value()).unwrap_or(0.0);
        }
        if let Some(s) = sub.as_tagged("size") {
            width = s.first().and_then(|v| v.f64_value()).unwrap_or(0.0);
            height = s.get(1).and_then(|v| v.f64_value()).unwrap_or(width);
        }
        if let Some(d) = sub.as_tagged("drill") {
            // (drill 0.8) | (drill oval 0.6 1.2) | (drill 0.8 (offset ...))
            let nums: Vec<f64> = d.iter().filter_map(|v| v.f64_value()).collect();
            if d.first().and_then(|v| v.atom()) == Some("oval") && nums.len() >= 2 {
                drill_oval = Some((nums[0], nums[1]));
                drill = nums[0].min(nums[1]);
            } else if let Some(n) = nums.first() {
                drill = *n;
            }
        }
        if let Some(l) = sub.as_tagged("layers") {
            for arg in l {
                if let Some(name) = arg.str_value() {
                    layers.push(name.to_string());
                }
            }
        }
        if let Some(n) = sub.as_tagged("net") {
            net_hint = Some(n.to_vec());
        }
    }
    if pad_type != PadType::Smd && layers.is_empty() {
        layers = vec!["*.Cu".into(), "*.Mask".into()];
    }
    Some(LibPad { number, shape, pad_type, x, y, rotation, width, height, drill, drill_oval, layers, net_hint })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"(footprint "R_0603_1608Metric"
	(version 20260206)
	(generator "kicad-footprint-generator")
	(layer "F.Cu")
	(descr "Resistor SMD 0603")
	(property "Reference" "REF**" (at 0 -1.43 0) (layer "F.SilkS") (effects (font (size 1 1) (thickness 0.15))))
	(property "Value" "R_0603_1608Metric" (at 0 1.43 0) (layer "F.Fab") (effects (font (size 1 1) (thickness 0.15))))
	(attr smd)
	(fp_line (start -1.48 -0.73) (end 1.48 -0.73) (stroke (width 0.05) (type solid)) (layer "F.CrtYd"))
	(fp_line (start -1.48 0.73) (end -1.48 -0.73) (stroke (width 0.05) (type solid)) (layer "F.CrtYd"))
	(fp_line (start 1.48 -0.73) (end 1.48 0.73) (stroke (width 0.05) (type solid)) (layer "F.CrtYd"))
	(fp_line (start 1.48 0.73) (end -1.48 0.73) (stroke (width 0.05) (type solid)) (layer "F.CrtYd"))
	(pad "1" smd roundrect (at -0.825 0) (size 0.8 0.95) (layers "F.Cu" "F.Mask" "F.Paste") (roundrect_rratio 0.25))
	(pad "2" smd roundrect (at 0.825 0) (size 0.8 0.95) (layers "F.Cu" "F.Mask" "F.Paste") (roundrect_rratio 0.25))
	(model "${KICAD9_3DMODEL_DIR}/Resistor_SMD.3dshapes/R_0603_1608Metric.wrl" (offset (xyz 0 0 0)) (scale (xyz 1 1 1)) (rotate (xyz 0 0 0)))
)"#;

    const THT_FIXTURE: &str = r#"(footprint "PinHeader_1x02"
	(layer "F.Cu")
	(attr through_hole)
	(fp_rect (start -1.8 -1.8) (end 1.8 4.35) (stroke (width 0.05) (type solid)) (fill no) (layer "F.CrtYd"))
	(pad "1" thru_hole rect (at 0 0) (size 1.7 1.7) (drill 1) (layers "*.Cu" "*.Mask"))
	(pad "2" thru_hole oval (at 0 2.54) (size 1.7 1.7) (drill oval 1 1.2) (layers "*.Cu" "*.Mask"))
	(pad "" np_thru_hole circle (at 3 3) (size 0.65 0.65) (drill 0.65) (layers "*.Cu" "*.Mask"))
)"#;

    #[test]
    fn parses_smd_fixture_pads_and_courtyard() {
        let fp = parse_kicad_mod(FIXTURE).unwrap();
        assert_eq!(fp.name, "R_0603_1608Metric");
        assert_eq!(fp.pads.len(), 2);
        assert!(fp.is_smd);
        let p1 = &fp.pads[0];
        assert_eq!(p1.number, "1");
        assert_eq!(p1.shape, "roundrect");
        assert_eq!(p1.pad_type, PadType::Smd);
        assert!((p1.x + 0.825).abs() < 1e-9 && (p1.width - 0.8).abs() < 1e-9 && (p1.height - 0.95).abs() < 1e-9);
        let c = fp.courtyard.unwrap();
        assert!((c.0 + 1.48).abs() < 1e-9 && (c.2 - 1.48).abs() < 1e-9 && (c.3 - 0.73).abs() < 1e-9);
        let (w, h) = fp.size();
        assert!((w - 2.96).abs() < 1e-9 && (h - 1.46).abs() < 1e-9);
    }

    #[test]
    fn parses_tht_fixture_drills_and_npth() {
        let fp = parse_kicad_mod(THT_FIXTURE).unwrap();
        assert!(!fp.is_smd);
        assert_eq!(fp.pads.len(), 3);
        assert_eq!(fp.pads[0].pad_type, PadType::ThruHole);
        assert!((fp.pads[0].drill - 1.0).abs() < 1e-9);
        assert_eq!(fp.pads[1].drill_oval, Some((1.0, 1.2)));
        assert_eq!(fp.pads[2].pad_type, PadType::NpThruHole);
        assert_eq!(fp.pad_numbers(), vec!["1", "2"], "NPTH has no number and is not a pin");
        let c = fp.courtyard.unwrap();
        assert!((c.3 - 4.35).abs() < 1e-9);
    }

    #[test]
    fn instantiate_rotates_pads_and_assigns_nets() {
        let fp = parse_kicad_mod(THT_FIXTURE).unwrap();
        let nets = |n: &str| -> Option<(usize, String)> {
            match n { "1" => Some((1, "SIG".into())), "2" => Some((2, "GND".into())), _ => None }
        };
        let placed = fp.instantiate("J1", "Conn_01x02", "Connector_PinHeader_2.54mm:PinHeader_1x02", 8.0, 20.0, 90.0, &nets);
        // KiCad authority: rel (0, 2.54) at 90° → (+2.54, 0)
        let p2 = &placed.pads[1];
        assert!((p2.x - 10.54).abs() < 1e-9 && (p2.y - 20.0).abs() < 1e-9, "{:?}", p2);
        assert_eq!(p2.net.as_ref().map(|n| n.1.as_str()), Some("GND"));
        assert!((p2.rotation - 90.0).abs() < 1e-9, "pad angle absolute = lib 0 + fp 90");
        assert!(placed.pads[2].net.is_none());
        assert_eq!(placed.lib_name, "Connector_PinHeader_2.54mm:PinHeader_1x02");
        // The re-emitted footprint carries the lib id, reference, nets and absolute pad angles
        let mut board = crate::board::Board::default();
        board.nets = vec![(1, "SIG".into()), (2, "GND".into())];
        board.footprints.push(placed);
        let out = board.to_kicad_pcb();
        assert!(out.contains("(footprint \"Connector_PinHeader_2.54mm:PinHeader_1x02\""), "{}", out);
        assert!(out.contains("(property \"Reference\" \"J1\"") || out.contains("(fp_text reference \"J1\""), "{}", out);
        assert!(out.contains("(at 0 2.54 90)"), "pad 2 angle must be absolute 90: {}", out);
        assert!(out.contains("(net 2 \"GND\")"), "{}", out);
        // and parses back to the same absolute pad position
        let back = crate::board::Board::from_kicad_pcb(&out).unwrap();
        let bp2 = &back.footprints[0].pads[1];
        assert!((bp2.x - 10.54).abs() < 1e-6 && (bp2.y - 20.0).abs() < 1e-6, "{:?}", bp2);
        assert_eq!(back.footprints[0].reference, "J1");
    }

    #[test]
    fn resolve_unknown_library_is_none() {
        assert!(resolve("NoSuchLib:NoSuchPart").is_none());
        assert!(resolve("malformed").is_none());
    }

    #[test]
    fn loads_stock_lqfp_when_kicad_libs_present() {
        // Environment-dependent: only asserts when the stock library exists.
        if resolve("Package_QFP:LQFP-128_14x14mm_P0.4mm").is_none() {
            eprintln!("skip: KiCad footprint libraries not installed");
            return;
        }
        let fp = load("Package_QFP:LQFP-128_14x14mm_P0.4mm").unwrap();
        assert_eq!(fp.pads.len(), 128);
        assert_eq!(fp.pad_numbers().len(), 128);
        let (w, h) = fp.size();
        assert!(w > 16.0 && w < 18.0 && h > 16.0 && h < 18.0, "courtyard {}x{}", w, h);
        // 0.4mm pitch: pads 1 and 2 are 0.4 apart
        let d = (fp.pads[0].y - fp.pads[1].y).abs();
        assert!((d - 0.4).abs() < 1e-6, "pitch {}", d);
    }
}
