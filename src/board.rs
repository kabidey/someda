//! Board model: footprints, pads, nets, and the Edge.Cuts outline,
//! parsed from KiCad-format s-expressions.

use crate::sexp::{parse, Sexp};

#[derive(Debug, Clone)]
pub struct Pad {
    pub number: String,
    pub net: Option<(usize, String)>,
    /// Absolute pad center in board mm
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone)]
pub struct Footprint {
    pub reference: String,
    pub lib_name: String,
    /// Footprint origin in board mm
    pub x: f64,
    pub y: f64,
    pub rotation: f64,
    pub pads: Vec<Pad>,
}

/// A routed trace segment on a copper layer.
#[derive(Debug, Clone)]
pub struct Segment {
    /// (x1, y1) → (x2, y2) in board mm
    pub start: (f64, f64),
    pub end: (f64, f64),
    pub width: f64,
    pub layer: String,
    pub net: Option<(usize, String)>,
}

/// A via connecting two copper layers.
#[derive(Debug, Clone)]
pub struct Via {
    pub x: f64,
    pub y: f64,
    pub size: f64,
    pub drill: f64,
    pub layers: Vec<String>,
    pub net: Option<(usize, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct Board {
    pub version: Option<String>,
    pub footprints: Vec<Footprint>,
    pub nets: Vec<(usize, String)>,
    /// Edge.Cuts outline segments: ((x1,y1),(x2,y2))
    pub outline: Vec<((f64, f64), (f64, f64))>,
    /// Net classes: (name, trace_width_mm, nets)
    pub net_classes: Vec<(String, f64, Vec<String>)>,
    /// Routed trace segments
    pub segments: Vec<Segment>,
    /// Vias
    pub vias: Vec<Via>,
}

impl Board {
    pub fn from_kicad_pcb(input: &str) -> Result<Board, String> {
        let doc = parse(input).map_err(|e| e.to_string())?;
        let root_items = doc
            .as_tagged("kicad_pcb")
            .ok_or("root element is not (kicad_pcb ...)")?;
        let mut board = Board::default();

        for item in root_items {
            if let Some(v) = item.as_tagged("version") {
                board.version = v.first().and_then(|s| s.str_value()).map(String::from);
            }
            if let Some(n) = item.as_tagged("net") {
                if n.len() >= 2 {
                    if let (Some(idx), Some(name)) = (n[0].str_value(), n[1].str_value()) {
                        board.nets.push((
                            idx.parse().unwrap_or(0),
                            name.trim_matches('"').to_string(),
                        ));
                    }
                }
            }
            if item.as_tagged("gr_line").is_some() {
                if let Some(seg) = parse_edge_cut(item) {
                    board.outline.push(seg);
                }
            }
            if item.as_tagged("footprint").is_some() {
                if let Some(fp) = parse_footprint(item) {
                    board.footprints.push(fp);
                }
            }
            if item.as_tagged("segment").is_some() {
                if let Some(seg) = parse_segment(item) {
                    board.segments.push(seg);
                }
            }
            if item.as_tagged("via").is_some() {
                if let Some(v) = parse_via(item) {
                    board.vias.push(v);
                }
            }
        }
        Ok(board)
    }

    /// Returns the board bounding box from the Edge.Cuts outline, if closed.
    pub fn outline_bbox(&self) -> Option<(f64, f64, f64, f64)> {
        if self.outline.is_empty() {
            return None;
        }
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        for ((x1, y1), (x2, y2)) in &self.outline {
            xs.extend_from_slice(&[*x1, *x2]);
            ys.extend_from_slice(&[*y1, *y2]);
        }
        Some((xs.iter().cloned().fold(f64::INFINITY, f64::min),
              ys.iter().cloned().fold(f64::INFINITY, f64::min),
              xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
              ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max)))
    }
}

fn parse_at(args: &[Sexp]) -> Option<(f64, f64, f64)> {
    if args.len() >= 2 {
        let x = args[0].f64_value()?;
        let y = args[1].f64_value()?;
        let rot = args.get(2).and_then(|s| s.f64_value()).unwrap_or(0.0);
        Some((x, y, rot))
    } else {
        None
    }
}

fn parse_edge_cut(item: &Sexp) -> Option<((f64, f64), (f64, f64))> {
    let items = match item { Sexp::List(v) => v, _ => return None };
    let mut start = None;
    let mut end = None;
    let mut is_edge = false;
    for child in items {
        if let Some(l) = child.as_tagged("layer") {
            if l.first().and_then(|s| s.str_value()) == Some("Edge.Cuts") {
                is_edge = true;
            }
        }
        if let Some(s) = child.as_tagged("start") {
            start = s.first().zip(s.get(1)).and_then(|(a, b)| {
                Some((a.f64_value()?, b.f64_value()?))
            });
        }
        if let Some(e) = child.as_tagged("end") {
            end = e.first().zip(e.get(1)).and_then(|(a, b)| {
                Some((a.f64_value()?, b.f64_value()?))
            });
        }
    }
    if is_edge {
        start.zip(end)
    } else {
        None
    }
}

fn parse_footprint(item: &Sexp) -> Option<Footprint> {
    let items = match item { Sexp::List(v) => v, Sexp::Atom(_) | Sexp::Str(_) => return None };
    let lib_name = items.get(1).and_then(|s| s.str_value())?.trim_matches('"').to_string();

    let mut at = None;
    let mut reference = String::new();
    let mut pads = Vec::new();

    for child in items.iter().skip(2) {
        if let Some(a) = child.as_tagged("at") {
            at = parse_at(a);
        }
        if let Some(t) = child.as_tagged("fp_text") {
            // (fp_text reference "R1" ...)
            if t.first().and_then(|s| s.str_value()) == Some("reference") {
                reference = t.get(1)
                    .and_then(|s| s.str_value())
                    .unwrap_or("")
                    .trim_matches('"')
                    .to_string();
            }
        }
        if child.as_tagged("pad").is_some() {
            if let Some(pad) = parse_pad(child, at) {
                pads.push(pad);
            }
        }
    }

    let (x, y, rotation) = at?;
    Some(Footprint { reference, lib_name, x, y, rotation, pads })
}

fn parse_pad(child: &Sexp, fp_at: Option<(f64, f64, f64)>) -> Option<Pad> {
    let items = match child { Sexp::List(v) => v, _ => return None };
    let number = items.get(1).and_then(|s| s.str_value())?.trim_matches('"').to_string();

    let mut rel_at = None;
    let mut size = None;
    let mut net = None;

    for sub in items.iter().skip(2) {
        if let Some(a) = sub.as_tagged("at") {
            rel_at = a.first().zip(a.get(1)).and_then(|(x, y)| {
                Some((x.f64_value()?, y.f64_value()?))
            });
        }
        if let Some(s) = sub.as_tagged("size") {
            size = s.first().zip(s.get(1)).and_then(|(w, h)| {
                Some((w.f64_value()?, h.f64_value()?))
            });
        }
        if let Some(n) = sub.as_tagged("net") {
            if n.len() >= 2 {
                net = n.first().and_then(|i| i.str_value()).and_then(|i| i.parse().ok())
                    .zip(n.get(1).and_then(|s| s.str_value()).map(String::from));
            }
        }
    }

    let (rx, ry) = rel_at?;
    let (fx, fy, rot) = fp_at?;
    let (w, h) = size.unwrap_or((0.0, 0.0));
    // KiCad rotates pad offsets by the footprint rotation
    let r = rot.to_radians();
    let ox = rx * r.cos() - ry * r.sin();
    let oy = rx * r.sin() + ry * r.cos();
    Some(Pad {
        number,
        net,
        x: fx + ox,
        y: fy + oy,
        width: w,
        height: h,
    })
}

fn parse_segment(item: &Sexp) -> Option<Segment> {
    let items = match item { Sexp::List(v) => v, _ => return None };
    let mut start = None;
    let mut end = None;
    let mut width = 0.0;
    let mut layer = String::new();
    let mut net = None;
    for child in items {
        if let Some(s) = child.as_tagged("start") {
            start = s.first().zip(s.get(1)).and_then(|(a, b)| Some((a.f64_value()?, b.f64_value()?)));
        }
        if let Some(e) = child.as_tagged("end") {
            end = e.first().zip(e.get(1)).and_then(|(a, b)| Some((a.f64_value()?, b.f64_value()?)));
        }
        if let Some(w) = child.as_tagged("width") {
            width = w.first().and_then(|s| s.f64_value()).unwrap_or(0.0);
        }
        if let Some(l) = child.as_tagged("layer") {
            layer = l.first().and_then(|s| s.str_value()).unwrap_or("").trim_matches('"').to_string();
        }
        if let Some(n) = child.as_tagged("net") {
            if n.len() >= 2 {
                net = n.first().and_then(|i| i.str_value()).and_then(|i| i.parse().ok())
                    .zip(n.get(1).and_then(|s| s.str_value()).map(String::from));
            } else if n.len() == 1 {
                net = n.first().and_then(|i| i.str_value()).and_then(|i| i.parse().ok()).map(|i| (i, String::new()));
            }
        }
    }
    let (sx, sy) = start?;
    let (ex, ey) = end?;
    Some(Segment { start: (sx, sy), end: (ex, ey), width, layer, net })
}

fn parse_via(item: &Sexp) -> Option<Via> {
    let items = match item { Sexp::List(v) => v, _ => return None };
    let mut at = None;
    let mut size = 0.0;
    let mut drill = 0.0;
    let mut layers = Vec::new();
    let mut net = None;
    for child in items {
        if let Some(a) = child.as_tagged("at") {
            at = a.first().zip(a.get(1)).and_then(|(x, y)| Some((x.f64_value()?, y.f64_value()?)));
        }
        if let Some(s) = child.as_tagged("size") {
            size = s.first().and_then(|v| v.f64_value()).unwrap_or(0.0);
        }
        if let Some(d) = child.as_tagged("drill") {
            drill = d.first().and_then(|v| v.f64_value()).unwrap_or(0.0);
        }
        if let Some(l) = child.as_tagged("layers") {
            for arg in l {
                if let Some(name) = arg.str_value() {
                    layers.push(name.trim_matches('"').to_string());
                }
            }
        }
        if let Some(n) = child.as_tagged("net") {
            if n.len() >= 2 {
                net = n.first().and_then(|i| i.str_value()).and_then(|i| i.parse().ok())
                    .zip(n.get(1).and_then(|s| s.str_value()).map(String::from));
            } else if n.len() == 1 {
                net = n.first().and_then(|i| i.str_value()).and_then(|i| i.parse().ok()).map(|i| (i, String::new()));
            }
        }
    }
    let (x, y) = at?;
    Some(Via { x, y, size, drill, layers, net })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"(kicad_pcb (version 20211014)
      (net 1 "VCC")
      (net 2 "GND")
      (gr_line (start 50.25 5.5) (end 96.75 5.5) (layer "Edge.Cuts") (width 0.1))
      (gr_line (start 96.75 5.5) (end 96.75 39.5) (layer "Edge.Cuts") (width 0.1))
      (gr_line (start 96.75 39.5) (end 50.25 39.5) (layer "Edge.Cuts") (width 0.1))
      (gr_line (start 50.25 39.5) (end 50.25 5.5) (layer "Edge.Cuts") (width 0.1))
      (footprint "somlib:R0805" (layer "F.Cu") (at 66.766 20.889 0)
        (fp_text reference "R1" (at 0 -4) (layer "F.SilkS"))
        (pad "1" smd rect (at -1 0) (size 1.133 1.377) (layers "F.Cu") (net 2 "GND"))
        (pad "2" smd rect (at 1 0) (size 1.133 1.377) (layers "F.Cu") (net 1 "VCC"))
      )
    )"#;

    #[test]
    fn parses_board() {
        let b = Board::from_kicad_pcb(SAMPLE).unwrap();
        assert_eq!(b.version.as_deref(), Some("20211014"));
        assert_eq!(b.footprints.len(), 1);
        assert_eq!(b.footprints[0].reference, "R1");
        assert_eq!(b.footprints[0].pads.len(), 2);
        assert_eq!(b.outline.len(), 4);
        assert_eq!(b.nets.len(), 2);
    }

    #[test]
    fn pad_absolute_position() {
        let b = Board::from_kicad_pcb(SAMPLE).unwrap();
        let p1 = &b.footprints[0].pads[0];
        assert!((p1.x - 65.766).abs() < 1e-9);
        assert!((p1.y - 20.889).abs() < 1e-9);
        assert_eq!(p1.net.as_ref().unwrap().1, "GND");
    }

    #[test]
    fn rotated_footprint_pad_positions() {
        // A DIP-20 rotated 90°: pad offsets must be rotated too (KiCad semantics).
        // Footprint at (150, 124, 90), pad at rel (0, 20.32):
        // rotated → absolute (150 - 20.32, 124) = (129.68, 124)
        let b = Board::from_kicad_pcb(r#"(kicad_pcb
          (gr_line (start 0 0) (end 200 0) (layer "Edge.Cuts"))
          (gr_line (start 200 0) (end 200 200) (layer "Edge.Cuts"))
          (gr_line (start 200 200) (end 0 200) (layer "Edge.Cuts"))
          (gr_line (start 0 200) (end 0 0) (layer "Edge.Cuts"))
          (footprint "somlib:DIP-20" (layer "F.Cu") (at 150 124 90)
            (fp_text reference "U1" (at 0 0) (layer "F.SilkS"))
            (pad "1" smd rect (at 0 0) (size 1.6 1.6) (net 1 "A"))
            (pad "9" smd rect (at 0 20.32) (size 1.6 1.6) (net 2 "B"))
          )
        )"#).unwrap();
        let u1 = &b.footprints[0];
        assert_eq!(u1.pads.len(), 2);
        let p1 = &u1.pads[0];
        let p9 = &u1.pads[1];
        // pad 1 at origin: absolute (150, 124)
        assert!((p1.x - 150.0).abs() < 1e-9 && (p1.y - 124.0).abs() < 1e-9);
        // pad 9 at rel (0, 20.32) rotated 90° → absolute (150 - 20.32, 124)
        assert!((p9.x - 129.68).abs() < 1e-9, "p9.x = {}", p9.x);
        assert!((p9.y - 124.0).abs() < 1e-9, "p9.y = {}", p9.y);
    }

    #[test]
    fn outline_bbox() {
        let b = Board::from_kicad_pcb(SAMPLE).unwrap();
        let (x0, y0, x1, y1) = b.outline_bbox().unwrap();
        assert!((x0 - 50.25).abs() < 1e-9);
        assert!((y0 - 5.5).abs() < 1e-9);
        assert!((x1 - 96.75).abs() < 1e-9);
        assert!((y1 - 39.5).abs() < 1e-9);
    }
}