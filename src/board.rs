//! Board model: footprints, pads, nets, copper, zones and the Edge.Cuts
//! outline, parsed from and written to KiCad-format s-expressions.
//!
//! Format authority (live-probed against kicad-cli 10.0.6, 2026-10-19):
//! - A board written in the 20221018 dialect with legacy layer ids
//!   (F.Cu 0, In1..In4 1..4, B.Cu 31) is accepted and migrated by KiCad 10.
//! - Pad `(at x y angle)` inside a board footprint is ABSOLUTE: the
//!   library pad angle plus the footprint rotation.
//! - Footprint rotation is counter-clockwise on screen with y pointing
//!   down: relative (−1, 0) at 90° lands at (0, +1); relative (0, +2.54)
//!   lands at (+2.54, 0). Both verified from kicad-cli DRC coordinates.
//! - Stock `.kicad_mod` footprints embedded verbatim (with `(at ...)`,
//!   reference and pad nets injected) parse, pass DRC and plot.

use crate::sexp::{parse, Sexp};

/// The kind of pad: surface mount, plated through-hole, or a plain hole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadType {
    Smd,
    ThruHole,
    NpThruHole,
}

#[derive(Debug, Clone)]
pub struct Pad {
    pub number: String,
    pub net: Option<(usize, String)>,
    /// Absolute pad center in board mm
    pub x: f64,
    pub y: f64,
    /// Pad size as written (before rotation)
    pub width: f64,
    pub height: f64,
    /// KiCad shape word: rect, roundrect, circle, oval, custom, trapezoid
    pub shape: String,
    pub pad_type: PadType,
    /// Drill diameter (0 for SMD); oval drills carry both axes
    pub drill: f64,
    pub drill_oval: Option<(f64, f64)>,
    /// Absolute pad rotation in degrees (library angle + footprint rotation)
    pub rotation: f64,
    /// Layer list as written ("F.Cu", "*.Cu", "F.Mask", ...)
    pub layers: Vec<String>,
}

impl Pad {
    /// A plain front-side rectangular SMD pad (the synthetic fallback).
    pub fn smd(number: &str, net: Option<(usize, String)>, x: f64, y: f64, w: f64, h: f64) -> Pad {
        Pad {
            number: number.to_string(),
            net,
            x,
            y,
            width: w,
            height: h,
            shape: "rect".into(),
            pad_type: PadType::Smd,
            drill: 0.0,
            drill_oval: None,
            rotation: 0.0,
            layers: vec!["F.Cu".into(), "F.Mask".into(), "F.Paste".into()],
        }
    }

    /// True when the pad has copper on every layer (through-hole barrel).
    pub fn is_through(&self) -> bool {
        self.pad_type != PadType::Smd
            || self.layers.iter().any(|l| l == "*.Cu")
    }

    /// True when the pad has copper on the named copper layer.
    pub fn on_copper(&self, layer: &str) -> bool {
        if self.is_through() {
            return true;
        }
        if self.layers.is_empty() {
            return layer == "F.Cu";
        }
        self.layers.iter().any(|l| l == layer)
    }

    /// Axis-aligned half extents of the (rotated) pad rectangle.
    pub fn bbox_half(&self) -> (f64, f64) {
        let a = self.rotation.to_radians();
        let (c, s) = (a.cos().abs(), a.sin().abs());
        let hw = self.width / 2.0;
        let hh = self.height / 2.0;
        (hw * c + hh * s, hw * s + hh * c)
    }

    /// Conservative circular radius covering the pad.
    pub fn radius(&self) -> f64 {
        let (hx, hy) = self.bbox_half();
        hx.max(hy)
    }
}

/// A keep-out area (absolute board mm) carried by a footprint.
#[derive(Debug, Clone)]
pub struct Keepout {
    pub layers: Vec<String>,
    pub polygon: Vec<(f64, f64)>,
    pub tracks: bool,
    pub vias: bool,
}

impl Keepout {
    pub fn on_layer(&self, layer: &str) -> bool {
        self.layers.iter().any(|l| l == layer || l == "*.Cu" || (l == "F&B.Cu" && (layer == "F.Cu" || layer == "B.Cu")))
    }
    /// Even-odd point-in-polygon.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        let n = self.polygon.len();
        let mut inside = false;
        let mut j = n - 1;
        for i in 0..n {
            let (xi, yi) = self.polygon[i];
            let (xj, yj) = self.polygon[j];
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            j = i;
        }
        inside
    }
    pub fn bbox(&self) -> (f64, f64, f64, f64) {
        let mut b = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for &(x, y) in &self.polygon {
            b.0 = b.0.min(x); b.1 = b.1.min(y); b.2 = b.2.max(x); b.3 = b.3.max(y);
        }
        b
    }
}

#[derive(Debug, Clone)]
pub struct Footprint {
    pub reference: String,
    pub lib_name: String,
    pub value: String,
    /// Footprint origin in board mm
    pub x: f64,
    pub y: f64,
    pub rotation: f64,
    pub pads: Vec<Pad>,
    /// Courtyard bbox relative to the origin in the footprint's own
    /// (unrotated) frame: (min_x, min_y, max_x, max_y)
    pub courtyard: Option<(f64, f64, f64, f64)>,
    /// Keep-out areas (absolute) the router must honour
    pub keepouts: Vec<Keepout>,
    /// The library definition this footprint was instantiated from, if
    /// any. The writer re-emits it verbatim (with placement, reference
    /// and pad nets injected) so silkscreen, fab and 3D data survive.
    pub source: Option<Sexp>,
}

impl Footprint {
    /// Courtyard (or pad-derived) bounding box in absolute board mm,
    /// accounting for rotation.
    pub fn abs_bbox(&self) -> (f64, f64, f64, f64) {
        let rel = self.courtyard.unwrap_or_else(|| {
            let mut b = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
            for p in &self.pads {
                // pads are absolute: bring back to relative
                let (rx, ry) = inverse_rotate(p.x - self.x, p.y - self.y, self.rotation);
                let hw = p.width / 2.0 + 0.25;
                let hh = p.height / 2.0 + 0.25;
                b.0 = b.0.min(rx - hw);
                b.1 = b.1.min(ry - hh);
                b.2 = b.2.max(rx + hw);
                b.3 = b.3.max(ry + hh);
            }
            if b.0.is_infinite() {
                (-1.0, -1.0, 1.0, 1.0)
            } else {
                b
            }
        });
        let corners = [(rel.0, rel.1), (rel.2, rel.1), (rel.2, rel.3), (rel.0, rel.3)];
        let mut out = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (cx, cy) in corners {
            let (ox, oy) = rotate_offset(cx, cy, self.rotation);
            out.0 = out.0.min(self.x + ox);
            out.1 = out.1.min(self.y + oy);
            out.2 = out.2.max(self.x + ox);
            out.3 = out.3.max(self.y + oy);
        }
        out
    }

    /// Move the footprint (and its absolute pads / keep-outs) by a delta.
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        for p in &mut self.pads {
            p.x += dx;
            p.y += dy;
        }
        for k in &mut self.keepouts {
            for pt in &mut k.polygon {
                pt.0 += dx;
                pt.1 += dy;
            }
        }
    }
}

/// KiCad footprint rotation: counter-clockwise on screen, y down.
/// Returns the absolute offset of a footprint-relative point.
pub fn rotate_offset(rx: f64, ry: f64, rot_deg: f64) -> (f64, f64) {
    let r = rot_deg.to_radians();
    (rx * r.cos() + ry * r.sin(), -rx * r.sin() + ry * r.cos())
}

/// Inverse of [`rotate_offset`].
pub fn inverse_rotate(ox: f64, oy: f64, rot_deg: f64) -> (f64, f64) {
    let r = rot_deg.to_radians();
    (ox * r.cos() - oy * r.sin(), ox * r.sin() + oy * r.cos())
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

/// A via connecting copper layers (through-hole: spans the whole stack).
#[derive(Debug, Clone)]
pub struct Via {
    pub x: f64,
    pub y: f64,
    pub size: f64,
    pub drill: f64,
    pub layers: Vec<String>,
    pub net: Option<(usize, String)>,
}

/// A copper pour (plane) on one layer, filled by KiCad.
#[derive(Debug, Clone)]
pub struct Zone {
    pub net: (usize, String),
    pub layer: String,
    /// Outline polygon in board mm
    pub polygon: Vec<(f64, f64)>,
    pub clearance: f64,
    pub min_thickness: f64,
    /// Higher priority fills first where zones overlap (split planes)
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub struct Board {
    pub version: Option<String>,
    /// Copper layer count (2, 4, 6, ...)
    pub layer_count: u32,
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
    /// Copper pours
    pub zones: Vec<Zone>,
    /// Solder mask minimum web written into (setup); JLCPCB green: 0.1
    pub solder_mask_min_width: f64,
}

impl Default for Board {
    fn default() -> Self {
        Board {
            version: None,
            layer_count: 2,
            footprints: Vec::new(),
            nets: Vec::new(),
            outline: Vec::new(),
            net_classes: Vec::new(),
            segments: Vec::new(),
            vias: Vec::new(),
            zones: Vec::new(),
            solder_mask_min_width: 0.1,
        }
    }
}

/// Legacy (20221018-dialect) layer ids KiCad 10 accepts and migrates.
pub fn legacy_layer_id(name: &str) -> Option<u32> {
    Some(match name {
        "F.Cu" => 0,
        "B.Cu" => 31,
        "B.Adhes" => 32,
        "F.Adhes" => 33,
        "B.Paste" => 34,
        "F.Paste" => 35,
        "B.SilkS" => 36,
        "F.SilkS" => 37,
        "B.Mask" => 38,
        "F.Mask" => 39,
        "Dwgs.User" => 40,
        "Cmts.User" => 41,
        "Eco1.User" => 42,
        "Eco2.User" => 43,
        "Edge.Cuts" => 44,
        "Margin" => 45,
        "B.CrtYd" => 46,
        "F.CrtYd" => 47,
        "B.Fab" => 48,
        "F.Fab" => 49,
        other => {
            let n: u32 = other.strip_prefix("In")?.strip_suffix(".Cu")?.parse().ok()?;
            if (1..=30).contains(&n) { n } else { return None; }
        }
    })
}

impl Board {
    /// Copper layer names outer-to-inner-to-outer for this layer count.
    pub fn copper_layers(&self) -> Vec<String> {
        copper_layer_names(self.layer_count)
    }

    pub fn from_kicad_pcb(input: &str) -> Result<Board, String> {
        let doc = parse(input).map_err(|e| e.to_string())?;
        let root_items = doc
            .as_tagged("kicad_pcb")
            .ok_or("root element is not (kicad_pcb ...)")?;
        let mut board = Board::default();
        let mut copper_seen = 0u32;

        for item in root_items {
            if let Some(v) = item.as_tagged("version") {
                board.version = v.first().and_then(|s| s.str_value()).map(String::from);
            }
            if let Some(layers) = item.as_tagged("layers") {
                for l in layers {
                    if let Sexp::List(parts) = l {
                        if parts.len() >= 3
                            && parts[1].str_value().map(|n| n.ends_with(".Cu")).unwrap_or(false)
                            && parts[2].str_value() == Some("signal")
                        {
                            copper_seen += 1;
                        } else if parts.len() >= 3
                            && parts[1].str_value().map(|n| n.ends_with(".Cu")).unwrap_or(false)
                            && matches!(parts[2].str_value(), Some("power") | Some("mixed") | Some("jumper"))
                        {
                            copper_seen += 1;
                        }
                    }
                }
            }
            if let Some(n) = item.as_tagged("net") {
                if n.len() >= 2 {
                    if let (Some(idx), Some(name)) = (n[0].str_value(), n[1].str_value()) {
                        let idx: usize = idx.parse().unwrap_or(0);
                        // KiCad's net 0 is "no net" — not a user net
                        if !(idx == 0 && name.trim_matches('"').is_empty()) {
                            board.nets.push((idx, name.trim_matches('"').to_string()));
                        }
                    }
                }
            }
            if item.as_tagged("gr_line").is_some() {
                if let Some(seg) = parse_edge_cut(item) {
                    board.outline.push(seg);
                }
            }
            if item.as_tagged("footprint").is_some() {
                if let Some(fp) = parse_footprint(item, &board.nets) {
                    board.footprints.push(fp);
                }
            }
            if item.as_tagged("segment").is_some() {
                if let Some(seg) = parse_segment(item, &board.nets) {
                    board.segments.push(seg);
                }
            }
            if item.as_tagged("via").is_some() {
                if let Some(v) = parse_via(item, &board.nets) {
                    board.vias.push(v);
                }
            }
            if item.as_tagged("zone").is_some() {
                if let Some(z) = parse_zone(item, &board.nets) {
                    board.zones.push(z);
                }
            }
        }
        if copper_seen >= 2 {
            board.layer_count = copper_seen;
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

    /// Serialize the board to KiCad `.kicad_pcb` format (20221018 dialect,
    /// legacy layer ids — accepted and migrated by KiCad 7 through 10).
    pub fn to_kicad_pcb(&self) -> String {
        let mut out = String::new();
        out.push_str("(kicad_pcb (version 20221018) (generator someda)\n");
        out.push_str("  (general (thickness 1.6))\n");
        out.push_str("  (paper \"A4\")\n");
        out.push_str("  (setup\n");
        out.push_str("    (pad_to_mask_clearance 0)\n");
        out.push_str(&format!("    (solder_mask_min_width {})\n", self.solder_mask_min_width));
        out.push_str("  )\n");
        out.push_str("  (layers\n");
        for name in self.copper_layers() {
            out.push_str(&format!("    ({} \"{}\" signal)\n", legacy_layer_id(&name).unwrap_or(0), name));
        }
        for name in [
            "B.Adhes", "F.Adhes", "B.Paste", "F.Paste", "B.SilkS", "F.SilkS", "B.Mask", "F.Mask",
            "Dwgs.User", "Cmts.User", "Eco1.User", "Eco2.User", "Edge.Cuts", "Margin",
            "B.CrtYd", "F.CrtYd", "B.Fab", "F.Fab",
        ] {
            out.push_str(&format!("    ({} \"{}\" user)\n", legacy_layer_id(name).unwrap_or(0), name));
        }
        out.push_str("  )\n");

        for ((x1, y1), (x2, y2)) in &self.outline {
            out.push_str(&format!(
                "  (gr_line (start {:.4} {:.4}) (end {:.4} {:.4}) (layer \"Edge.Cuts\") (width 0.1))\n",
                x1, y1, x2, y2
            ));
        }

        // Net 0 is KiCad's "no net"; user nets follow.
        out.push_str("  (net 0 \"\")\n");
        for (idx, name) in &self.nets {
            out.push_str(&format!("  (net {} \"{}\")\n", idx, name));
        }

        for (name, width, nets) in &self.net_classes {
            let net_refs: Vec<String> =
                nets.iter().map(|n| format!("\"{}\"", n)).collect();
            out.push_str(&format!(
                "  (net_class \"{}\" \"someda\" (trace_width {}) (clearance 0.2) (nets {}))\n",
                name, width, net_refs.join(" ")
            ));
        }

        for fp in &self.footprints {
            if fp.source.is_some() {
                out.push_str(&emit_library_footprint(fp));
            } else {
                out.push_str(&self.emit_synthetic_footprint(fp));
            }
        }

        for seg in &self.segments {
            // KiCad tracks take (net N) — the name form is pad-only
            let net_part = match &seg.net {
                Some((idx, _)) => format!(" (net {})", idx),
                None => String::new(),
            };
            out.push_str(&format!(
                "  (segment (start {:.4} {:.4}) (end {:.4} {:.4}) (width {:.4}) (layer \"{}\"){})\n",
                seg.start.0, seg.start.1, seg.end.0, seg.end.1, seg.width, seg.layer, net_part
            ));
        }

        for via in &self.vias {
            let net_part = match &via.net {
                Some((idx, _)) => format!(" (net {})", idx),
                None => String::new(),
            };
            out.push_str(&format!(
                "  (via (at {:.4} {:.4}) (size {:.4}) (drill {:.4}) (layers \"F.Cu\" \"B.Cu\"){})\n",
                via.x, via.y, via.size, via.drill, net_part
            ));
        }

        for z in &self.zones {
            let pts: Vec<String> = z
                .polygon
                .iter()
                .map(|(x, y)| format!("(xy {:.4} {:.4})", x, y))
                .collect();
            out.push_str(&format!(
                "  (zone (net {}) (net_name \"{}\") (layer \"{}\") (hatch edge 0.508) (priority {})\n    (connect_pads (clearance {:.3}))\n    (min_thickness {:.3}) (filled_areas_thickness no)\n    (fill yes (thermal_gap 0.3) (thermal_bridge_width 0.3))\n    (polygon (pts {}))\n  )\n",
                z.net.0, z.net.1, z.layer, z.priority, z.clearance, z.min_thickness, pts.join(" ")
            ));
        }

        out.push_str(")\n");
        out
    }

    fn emit_synthetic_footprint(&self, fp: &Footprint) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "  (footprint \"{}\" (layer \"F.Cu\") (at {:.4} {:.4} {})\n    (attr smd)\n",
            fp.lib_name, fp.x, fp.y, fp.rotation
        ));
        // Silk text must stay inside the outline or kicad-cli flags
        // silk_edge_clearance (live-verified on edge-placed parts).
        let bbox = self.outline_bbox();
        let (tx, ty) = match bbox {
            Some((_x0, y0, _x1, y1)) => {
                let mut ty_rel = -2.0;
                if fp.y + ty_rel - 1.0 < y0 {
                    ty_rel = 2.0;
                }
                if fp.y + ty_rel + 1.0 > y1 {
                    ty_rel = 0.0;
                }
                (0.0, ty_rel)
            }
            None => (0.0, -2.0),
        };
        out.push_str(&format!(
            "    (fp_text reference \"{}\" (at {:.2} {:.2} 0) (layer \"F.SilkS\"))\n",
            fp.reference, tx, ty
        ));
        out.push_str(&format!(
            "    (fp_text value \"{}\" (at 0 0 0) (layer \"F.Fab\"))\n",
            fp.value
        ));
        for pad in &fp.pads {
            let (rx, ry) = inverse_rotate(pad.x - fp.x, pad.y - fp.y, fp.rotation);
            let net_part = match &pad.net {
                Some((idx, name)) => format!(" (net {} \"{}\")", idx, name),
                None => String::new(),
            };
            let kind = match pad.pad_type {
                PadType::Smd => "smd",
                PadType::ThruHole => "thru_hole",
                PadType::NpThruHole => "np_thru_hole",
            };
            let drill = if pad.pad_type == PadType::Smd {
                String::new()
            } else {
                match pad.drill_oval {
                    Some((a, b)) => format!(" (drill oval {:.4} {:.4})", a, b),
                    None => format!(" (drill {:.4})", pad.drill),
                }
            };
            let layers: Vec<String> = if pad.layers.is_empty() {
                vec!["\"F.Cu\"".into(), "\"F.Mask\"".into(), "\"F.Paste\"".into()]
            } else {
                pad.layers.iter().map(|l| format!("\"{}\"", l)).collect()
            };
            out.push_str(&format!(
                "    (pad \"{}\" {} {} (at {:.4} {:.4} {}) (size {:.4} {:.4}){} (layers {}){})\n",
                pad.number, kind, pad.shape, rx, ry, pad.rotation, pad.width, pad.height, drill,
                layers.join(" "), net_part
            ));
        }
        out.push_str("  )\n");
        out
    }
}

/// Copper layer names for a layer count: F.Cu, In1.Cu .. In(n-2).Cu, B.Cu.
pub fn copper_layer_names(layer_count: u32) -> Vec<String> {
    let mut v = vec!["F.Cu".to_string()];
    for i in 1..layer_count.saturating_sub(1) {
        v.push(format!("In{}.Cu", i));
    }
    if layer_count >= 2 {
        v.push("B.Cu".into());
    }
    v
}

/// Re-emit a library footprint with placement, reference/value and pad
/// nets injected. Everything else (silk, fab, courtyard, 3D model) is
/// kept verbatim — KiCad is the authority on its own footprint syntax.
fn emit_library_footprint(fp: &Footprint) -> String {
    let src = match &fp.source {
        Some(s) => s,
        None => return String::new(),
    };
    let mut items = match src {
        Sexp::List(v) => v.clone(),
        _ => return String::new(),
    };
    // Drop any pre-existing placement / uuid / reference text; rebuild.
    items.retain(|c| c.as_tagged("at").is_none() && c.as_tagged("uuid").is_none());
    // Insert (at x y rot) right after the (layer ...) child, or after the name.
    let at = Sexp::List(vec![
        Sexp::Atom("at".into()),
        Sexp::Atom(format!("{:.4}", fp.x)),
        Sexp::Atom(format!("{:.4}", fp.y)),
        Sexp::Atom(trim_angle(fp.rotation)),
    ]);
    let layer_pos = items.iter().position(|c| c.as_tagged("layer").is_some());
    match layer_pos {
        Some(p) => items.insert(p + 1, at),
        None => {
            items.insert(2.min(items.len()), Sexp::List(vec![
                Sexp::Atom("layer".into()),
                Sexp::Str("F.Cu".into()),
            ]));
            items.insert(3.min(items.len()), at);
        }
    }
    let mut saw_reference = false;
    for c in items.iter_mut() {
        if let Sexp::List(parts) = c {
            // Footprint zones (keep-outs) are stored in ABSOLUTE board
            // coordinates inside a .kicad_pcb, unlike pads and graphics
            // (live-verified: a verbatim relative polygon landed at the
            // board origin and KiCad flagged items_not_allowed there).
            if parts.first().and_then(|p| p.atom()) == Some("zone") {
                transform_zone_points(parts, fp.x, fp.y, fp.rotation);
            }
            if parts.len() >= 3 && parts[0].atom() == Some("property") {
                match parts[1].str_value() {
                    Some("Reference") => {
                        parts[2] = Sexp::Str(fp.reference.clone());
                        saw_reference = true;
                    }
                    Some("Value") => parts[2] = Sexp::Str(fp.value.clone()),
                    _ => {}
                }
            }
            if parts.len() >= 3 && parts[0].atom() == Some("fp_text") {
                match parts[1].atom() {
                    Some("reference") => {
                        parts[2] = Sexp::Str(fp.reference.clone());
                        saw_reference = true;
                    }
                    Some("value") => parts[2] = Sexp::Str(fp.value.clone()),
                    _ => {}
                }
            }
            if parts.first().and_then(|p| p.atom()) == Some("pad") && parts.len() >= 2 {
                let number = parts[1].str_value().unwrap_or("").to_string();
                // Pad angle is absolute in a board: library angle + footprint rotation
                for sub in parts.iter_mut() {
                    if let Sexp::List(at_parts) = sub {
                        if at_parts.first().and_then(|a| a.atom()) == Some("at") && at_parts.len() >= 3 {
                            let lib_angle = at_parts.get(3).and_then(|a| a.f64_value()).unwrap_or(0.0);
                            let abs = lib_angle + fp.rotation;
                            at_parts.truncate(3);
                            at_parts.push(Sexp::Atom(trim_angle(abs)));
                        }
                    }
                }
                // Remove any stale net, then attach ours (same-numbered pads share a net)
                parts.retain(|s| s.as_tagged("net").is_none());
                if let Some(pad) = fp.pads.iter().find(|p| p.number == number) {
                    if let Some((idx, name)) = &pad.net {
                        parts.push(Sexp::List(vec![
                            Sexp::Atom("net".into()),
                            Sexp::Atom(idx.to_string()),
                            Sexp::Str(name.clone()),
                        ]));
                    }
                }
            }
        }
    }
    if !saw_reference {
        items.push(Sexp::List(vec![
            Sexp::Atom("fp_text".into()),
            Sexp::Atom("reference".into()),
            Sexp::Str(fp.reference.clone()),
            Sexp::List(vec![Sexp::Atom("at".into()), Sexp::Atom("0".into()), Sexp::Atom("-2".into()), Sexp::Atom("0".into())]),
            Sexp::List(vec![Sexp::Atom("layer".into()), Sexp::Str("F.SilkS".into())]),
        ]));
    }
    format!("  {}\n", Sexp::List(items))
}

/// Rotate+translate every (xy ...) of a footprint zone's polygon.
fn transform_zone_points(parts: &mut [Sexp], fx: f64, fy: f64, rot: f64) {
    for sub in parts.iter_mut() {
        if let Sexp::List(poly) = sub {
            if poly.first().and_then(|p| p.atom()) != Some("polygon") {
                continue;
            }
            for pp in poly.iter_mut() {
                if let Sexp::List(pts) = pp {
                    if pts.first().and_then(|p| p.atom()) != Some("pts") {
                        continue;
                    }
                    for xy in pts.iter_mut() {
                        if let Sexp::List(v) = xy {
                            if v.first().and_then(|p| p.atom()) == Some("xy") && v.len() >= 3 {
                                if let (Some(x), Some(y)) = (v[1].f64_value(), v[2].f64_value()) {
                                    let (ox, oy) = rotate_offset(x, y, rot);
                                    v[1] = Sexp::Atom(format!("{:.4}", fx + ox));
                                    v[2] = Sexp::Atom(format!("{:.4}", fy + oy));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Inverse of [`transform_zone_points`]: absolute → footprint-relative.
fn untransform_zone_points(parts: &mut [Sexp], fx: f64, fy: f64, rot: f64) {
    for sub in parts.iter_mut() {
        if let Sexp::List(poly) = sub {
            if poly.first().and_then(|p| p.atom()) != Some("polygon") {
                continue;
            }
            for pp in poly.iter_mut() {
                if let Sexp::List(pts) = pp {
                    if pts.first().and_then(|p| p.atom()) != Some("pts") {
                        continue;
                    }
                    for xy in pts.iter_mut() {
                        if let Sexp::List(v) = xy {
                            if v.first().and_then(|p| p.atom()) == Some("xy") && v.len() >= 3 {
                                if let (Some(x), Some(y)) = (v[1].f64_value(), v[2].f64_value()) {
                                    let (rx, ry) = inverse_rotate(x - fx, y - fy, rot);
                                    v[1] = Sexp::Atom(format!("{:.4}", rx));
                                    v[2] = Sexp::Atom(format!("{:.4}", ry));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn trim_angle(a: f64) -> String {
    let a = ((a % 360.0) + 360.0) % 360.0;
    if (a - a.round()).abs() < 1e-9 {
        format!("{}", a.round() as i64)
    } else {
        format!("{:.4}", a)
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

/// Resolve a `(net ...)` child to (index, name) using the board net table
/// when only one of the two is present (KiCad 10 writes names only).
fn resolve_net(n: &[Sexp], nets: &[(usize, String)]) -> Option<(usize, String)> {
    if n.len() >= 2 {
        let idx: usize = n[0].str_value()?.parse().ok()?;
        let name = n[1].str_value()?.to_string();
        return Some((idx, name));
    }
    let only = n.first()?.str_value()?;
    if let Ok(idx) = only.parse::<usize>() {
        let name = nets.iter().find(|(i, _)| *i == idx).map(|(_, n)| n.clone()).unwrap_or_default();
        return Some((idx, name));
    }
    if only.is_empty() {
        return None;
    }
    let idx = nets.iter().find(|(_, nm)| nm == only).map(|(i, _)| *i).unwrap_or(0);
    Some((idx, only.to_string()))
}

/// Parse a footprint from a board file (`.kicad_pcb`) into the model.
/// Pads come out ABSOLUTE; the library definition is kept as `source`.
fn parse_footprint(item: &Sexp, nets: &[(usize, String)]) -> Option<Footprint> {
    let items = match item { Sexp::List(v) => v, Sexp::Atom(_) | Sexp::Str(_) => return None };
    let lib_name = items.get(1).and_then(|s| s.str_value())?.trim_matches('"').to_string();

    let mut at = None;
    let mut reference = String::new();
    let mut value = String::new();
    for child in items.iter().skip(2) {
        if let Some(a) = child.as_tagged("at") {
            at = parse_at(a);
        }
        if let Some(t) = child.as_tagged("fp_text") {
            match t.first().and_then(|s| s.str_value()) {
                Some("reference") => {
                    reference = t.get(1).and_then(|s| s.str_value()).unwrap_or("").trim_matches('"').to_string();
                }
                Some("value") => {
                    value = t.get(1).and_then(|s| s.str_value()).unwrap_or("").to_string();
                }
                _ => {}
            }
        }
        if let Some(p) = child.as_tagged("property") {
            match p.first().and_then(|s| s.str_value()) {
                Some("Reference") => {
                    reference = p.get(1).and_then(|s| s.str_value()).unwrap_or("").to_string();
                }
                Some("Value") => {
                    value = p.get(1).and_then(|s| s.str_value()).unwrap_or("").to_string();
                }
                _ => {}
            }
        }
    }
    let (x, y, rotation) = at?;
    let lib = crate::footprint_lib::parse_footprint_sexp(item).ok()?;
    let pads = lib
        .pads
        .iter()
        .map(|lp| {
            let (ox, oy) = rotate_offset(lp.x, lp.y, rotation);
            let net = lp.net_hint.as_ref().and_then(|n| resolve_net(n, nets));
            Pad {
                number: lp.number.clone(),
                net,
                x: x + ox,
                y: y + oy,
                width: lp.width,
                height: lp.height,
                shape: lp.shape.clone(),
                pad_type: lp.pad_type,
                drill: lp.drill,
                drill_oval: lp.drill_oval,
                // In a board file the pad angle is already absolute
                rotation: lp.rotation,
                layers: lp.layers.clone(),
            }
        })
        .collect();
    Some(Footprint {
        reference,
        lib_name,
        value,
        x,
        y,
        rotation,
        pads,
        courtyard: lib.courtyard,
        // Board-file zone polygons are already absolute
        keepouts: lib
            .keepouts
            .iter()
            .map(|k| Keepout { layers: k.layers.clone(), polygon: k.polygon.clone(), tracks: k.tracks, vias: k.vias })
            .collect(),
        // Stored sources always carry LIBRARY-relative pad angles and zone
        // points; a board file has absolute ones, so undo the placement.
        source: Some({
            let mut src = shift_pad_angles(item, -rotation);
            if let Sexp::List(items) = &mut src {
                for c in items.iter_mut() {
                    if let Sexp::List(parts) = c {
                        if parts.first().and_then(|p| p.atom()) == Some("zone") {
                            untransform_zone_points(parts, x, y, rotation);
                        }
                    }
                }
            }
            src
        }),
    })
}

/// Return a copy of a footprint s-expression with every pad angle shifted
/// by `delta` degrees (used to convert between absolute and library angles).
pub fn shift_pad_angles(fp: &Sexp, delta: f64) -> Sexp {
    let mut out = fp.clone();
    if let Sexp::List(items) = &mut out {
        for c in items.iter_mut() {
            if let Sexp::List(parts) = c {
                if parts.first().and_then(|p| p.atom()) != Some("pad") {
                    continue;
                }
                for sub in parts.iter_mut() {
                    if let Sexp::List(at_parts) = sub {
                        if at_parts.first().and_then(|a| a.atom()) == Some("at") && at_parts.len() >= 3 {
                            let a = at_parts.get(3).and_then(|v| v.f64_value()).unwrap_or(0.0) + delta;
                            at_parts.truncate(3);
                            if a.abs() > 1e-9 {
                                at_parts.push(Sexp::Atom(trim_angle(a)));
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

fn parse_segment(item: &Sexp, nets: &[(usize, String)]) -> Option<Segment> {
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
            net = resolve_net(n, nets);
        }
    }
    let (sx, sy) = start?;
    let (ex, ey) = end?;
    Some(Segment { start: (sx, sy), end: (ex, ey), width, layer, net })
}

fn parse_via(item: &Sexp, nets: &[(usize, String)]) -> Option<Via> {
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
            net = resolve_net(n, nets);
        }
    }
    let (x, y) = at?;
    Some(Via { x, y, size, drill, layers, net })
}

fn parse_zone(item: &Sexp, nets: &[(usize, String)]) -> Option<Zone> {
    let items = match item { Sexp::List(v) => v, _ => return None };
    let mut net = None;
    let mut net_name = None;
    let mut layer = String::new();
    let mut polygon = Vec::new();
    let mut clearance = 0.2;
    let mut min_thickness = 0.25;
    let mut priority = 0;
    for child in items {
        if let Some(n) = child.as_tagged("net") {
            net = resolve_net(n, nets);
        }
        if let Some(pr) = child.as_tagged("priority") {
            priority = pr.first().and_then(|s| s.f64_value()).unwrap_or(0.0) as i32;
        }
        if let Some(n) = child.as_tagged("net_name") {
            net_name = n.first().and_then(|s| s.str_value()).map(String::from);
        }
        if let Some(l) = child.as_tagged("layer") {
            layer = l.first().and_then(|s| s.str_value()).unwrap_or("").to_string();
        }
        if let Some(l) = child.as_tagged("layers") {
            layer = l.first().and_then(|s| s.str_value()).unwrap_or("").to_string();
        }
        if let Some(cp) = child.as_tagged("connect_pads") {
            for c in cp {
                if let Some(v) = c.as_tagged("clearance") {
                    clearance = v.first().and_then(|s| s.f64_value()).unwrap_or(clearance);
                }
            }
        }
        if let Some(m) = child.as_tagged("min_thickness") {
            min_thickness = m.first().and_then(|s| s.f64_value()).unwrap_or(min_thickness);
        }
        if let Some(poly) = child.as_tagged("polygon") {
            for p in poly {
                if let Some(pts) = p.as_tagged("pts") {
                    for xy in pts {
                        if let Some(c) = xy.as_tagged("xy") {
                            if let (Some(x), Some(y)) = (c.first().and_then(|v| v.f64_value()), c.get(1).and_then(|v| v.f64_value())) {
                                polygon.push((x, y));
                            }
                        }
                    }
                }
            }
        }
    }
    let (idx, mut name) = net?;
    if name.is_empty() {
        name = net_name.unwrap_or_default();
    }
    Some(Zone { net: (idx, name), layer, polygon, clearance, min_thickness, priority })
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
        assert_eq!(b.layer_count, 2);
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
    fn rotated_footprint_pad_positions_match_kicad() {
        // KiCad authority (kicad-cli 10.0.6 DRC coordinates, 2026-10-19):
        // footprint at (20, 10, 90) with pad rel (-1, 0) reports the pad at
        // (20, 11); pad rel (1, 0) at (20, 9). A 1x03 header at (8, 20, 90)
        // reports pin 2 (rel 0, 2.54) at (10.54, 20). Rotation is
        // counter-clockwise on screen with y down.
        let b = Board::from_kicad_pcb(r#"(kicad_pcb
          (gr_line (start 0 0) (end 200 0) (layer "Edge.Cuts"))
          (gr_line (start 200 0) (end 200 200) (layer "Edge.Cuts"))
          (gr_line (start 200 200) (end 0 200) (layer "Edge.Cuts"))
          (gr_line (start 0 200) (end 0 0) (layer "Edge.Cuts"))
          (footprint "test:R" (layer "F.Cu") (at 20 10 90)
            (fp_text reference "R2" (at 0 0) (layer "F.SilkS"))
            (pad "1" smd rect (at -1 0) (size 1 1) (net 1 "A"))
            (pad "2" smd rect (at 1 0) (size 1 1) (net 2 "B"))
          )
          (footprint "test:H" (layer "F.Cu") (at 8 20 90)
            (fp_text reference "J1" (at 0 0) (layer "F.SilkS"))
            (pad "1" thru_hole rect (at 0 0) (size 1.7 1.7) (drill 1) (layers "*.Cu" "*.Mask"))
            (pad "2" thru_hole circle (at 0 2.54) (size 1.7 1.7) (drill 1) (layers "*.Cu" "*.Mask"))
          )
        )"#).unwrap();
        let r2 = &b.footprints[0];
        assert!((r2.pads[0].x - 20.0).abs() < 1e-9 && (r2.pads[0].y - 11.0).abs() < 1e-9, "{:?}", r2.pads[0]);
        assert!((r2.pads[1].x - 20.0).abs() < 1e-9 && (r2.pads[1].y - 9.0).abs() < 1e-9, "{:?}", r2.pads[1]);
        let j1 = &b.footprints[1];
        assert!((j1.pads[1].x - 10.54).abs() < 1e-9 && (j1.pads[1].y - 20.0).abs() < 1e-9, "{:?}", j1.pads[1]);
        assert!(j1.pads[1].is_through());
        assert_eq!(j1.pads[1].pad_type, PadType::ThruHole);
        assert!((j1.pads[1].drill - 1.0).abs() < 1e-9);
    }

    #[test]
    fn to_kicad_pcb_roundtrip_preserves_model() {
        let input = r#"(kicad_pcb (version 20221018)
          (gr_line (start 0 0) (end 100 0) (layer "Edge.Cuts") (width 0.1))
          (gr_line (start 100 0) (end 100 80) (layer "Edge.Cuts") (width 0.1))
          (gr_line (start 100 80) (end 0 80) (layer "Edge.Cuts") (width 0.1))
          (gr_line (start 0 80) (end 0 0) (layer "Edge.Cuts") (width 0.1))
          (net 1 "VCC")
          (net 2 "GND")
          (footprint "R" (layer "F.Cu") (at 10 10 0)
            (fp_text reference "R1" (at 0 -2))
            (pad "1" smd rect (at -1.25 0) (size 1.5 1.5) (layers "F.Cu") (net 1 "VCC"))
            (pad "2" smd rect (at 1.25 0) (size 1.5 1.5) (layers "F.Cu") (net 2 "GND")))
          (segment (start 8.75 10) (end 8.75 20) (width 0.25) (layer "F.Cu") (net 1 "VCC"))
          (via (at 8.75 20) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1 "VCC")))"#;
        let b1 = Board::from_kicad_pcb(input).unwrap();
        let written = b1.to_kicad_pcb();
        let b2 = Board::from_kicad_pcb(&written).unwrap();

        assert_eq!(b1.footprints.len(), b2.footprints.len());
        assert_eq!(b1.nets, b2.nets);
        assert_eq!(b1.segments.len(), b2.segments.len());
        assert_eq!(b1.vias.len(), b2.vias.len());
        assert_eq!(b1.outline.len(), b2.outline.len());
        let f1 = &b1.footprints[0];
        let f2 = &b2.footprints[0];
        assert_eq!(f1.reference, f2.reference);
        assert!((f1.x - f2.x).abs() < 1e-6 && (f1.y - f2.y).abs() < 1e-6);
        assert_eq!(f1.pads.len(), f2.pads.len());
        for (p1, p2) in f1.pads.iter().zip(&f2.pads) {
            assert_eq!(p1.number, p2.number);
            assert!((p1.x - p2.x).abs() < 1e-6, "pad {} x: {} vs {}", p1.number, p1.x, p2.x);
            assert!((p1.y - p2.y).abs() < 1e-6);
            assert_eq!(p1.net, p2.net);
        }
        let s1 = &b1.segments[0];
        let s2 = &b2.segments[0];
        assert!((s1.start.0 - s2.start.0).abs() < 1e-6);
        assert_eq!(s1.layer, s2.layer);
        assert_eq!(s1.net.as_ref().map(|n| n.0), s2.net.as_ref().map(|n| n.0));
    }

    #[test]
    fn rotated_library_footprint_roundtrips_with_absolute_pad_angles() {
        // A 90°-rotated footprint whose library pads have angle 0 must be
        // written with pad angle 90 (absolute) and parse back to the same
        // absolute pad positions.
        let input = r#"(kicad_pcb (version 20221018)
          (net 1 "A")
          (footprint "test:R" (layer "F.Cu") (at 20 10 90)
            (property "Reference" "R2" (at 0 0 0) (layer "F.SilkS"))
            (pad "1" smd rect (at -1 0 90) (size 1 0.5) (layers "F.Cu" "F.Mask") (net 1 "A"))
            (pad "2" smd rect (at 1 0 90) (size 1 0.5) (layers "F.Cu" "F.Mask"))
          ))"#;
        let b1 = Board::from_kicad_pcb(input).unwrap();
        assert_eq!(b1.footprints[0].reference, "R2", "property Reference must be read");
        let written = b1.to_kicad_pcb();
        assert!(written.contains("(at -1 0 90)") || written.contains("(at -1.0000 0.0000 90)") || written.contains("(at -1 0 90.0000)"),
            "pad angle must stay absolute 90: {}", written);
        let b2 = Board::from_kicad_pcb(&written).unwrap();
        let (p1, p2) = (&b1.footprints[0].pads[0], &b2.footprints[0].pads[0]);
        assert!((p1.x - p2.x).abs() < 1e-6 && (p1.y - p2.y).abs() < 1e-6);
        assert!((p2.x - 20.0).abs() < 1e-6 && (p2.y - 11.0).abs() < 1e-6, "{:?}", p2);
        assert_eq!(b2.footprints[0].pads[0].net.as_ref().map(|n| n.1.as_str()), Some("A"));
    }

    #[test]
    fn six_layer_board_writes_full_layer_table_and_zone() {
        let mut b = Board::default();
        b.layer_count = 6;
        b.nets.push((1, "GND".into()));
        b.outline = vec![((0.0, 0.0), (10.0, 0.0)), ((10.0, 0.0), (10.0, 10.0)), ((10.0, 10.0), (0.0, 10.0)), ((0.0, 10.0), (0.0, 0.0))];
        b.zones.push(Zone { net: (1, "GND".into()), layer: "In1.Cu".into(), polygon: vec![(0.5, 0.5), (9.5, 0.5), (9.5, 9.5), (0.5, 9.5)], clearance: 0.2, min_thickness: 0.25, priority: 0 });
        let out = b.to_kicad_pcb();
        for l in ["F.Cu", "In1.Cu", "In2.Cu", "In3.Cu", "In4.Cu", "B.Cu", "F.Mask", "B.Mask", "F.Paste", "F.SilkS", "F.CrtYd", "Edge.Cuts"] {
            assert!(out.contains(&format!("\"{}\"", l)), "layer {} missing", l);
        }
        assert!(out.contains("(4 \"In4.Cu\" signal)"));
        assert!(out.contains("(31 \"B.Cu\" signal)"));
        assert!(out.contains("(zone (net 1) (net_name \"GND\") (layer \"In1.Cu\")"));
        let back = Board::from_kicad_pcb(&out).unwrap();
        assert_eq!(back.layer_count, 6);
        assert_eq!(back.zones.len(), 1);
        assert_eq!(back.zones[0].polygon.len(), 4);
    }

    #[test]
    fn bbox_half_accounts_for_rotation() {
        let mut p = Pad::smd("1", None, 0.0, 0.0, 2.0, 0.5);
        assert!((p.bbox_half().0 - 1.0).abs() < 1e-9);
        p.rotation = 90.0;
        let (hx, hy) = p.bbox_half();
        assert!((hx - 0.25).abs() < 1e-9 && (hy - 1.0).abs() < 1e-9);
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

    #[test]
    fn copper_layer_names_by_count() {
        assert_eq!(copper_layer_names(2), vec!["F.Cu", "B.Cu"]);
        assert_eq!(copper_layer_names(4), vec!["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"]);
        assert_eq!(copper_layer_names(6).len(), 6);
        assert_eq!(legacy_layer_id("In4.Cu"), Some(4));
        assert_eq!(legacy_layer_id("B.Cu"), Some(31));
    }
}
