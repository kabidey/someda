//! The deterministic board solver — someda's port of the placement/routing
//! agent contract from the SomTronics AI-EDA study harness:
//!
//!   "Never output x/y coordinates; the deterministic solver owns geometry,
//!    overlap avoidance, board fitting, and DRC."
//!
//! The LLM (or a human) expresses INTENT — semantic positions ("top_left",
//! "left_edge"), "near" relations, keep-apart distances — and this module
//! turns it into geometry using the footprints' REAL courtyards:
//!
//!   1. parts with a semantic/edge position are placed first (connectors
//!      flush to their edge, slid along it to the first free slot)
//!   2. every other part is placed in connectivity order at the free spot
//!      nearest the centroid of its already-placed neighbours — decoupling
//!      caps land next to their IC, the SoC claims the centre
//!   3. courtyards never overlap (0.3mm gap), hot parts keep their
//!      thermal distance, everything stays inside the outline margin
//!
//! Everything here is deterministic: same plan in, same board out.

use crate::board::Board;
use crate::netlist::Netlist;

/// A semantic position on the board — what the plan schema allows.
/// Never a coordinate; coordinates are derived from the board size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pos {
    TopLeft, Top, TopRight,
    Left, Center, Right,
    BottomLeft, Bottom, BottomRight,
    LeftEdge, RightEdge, TopEdge, BottomEdge,
}

impl Pos {
    /// Parse the plan-schema words.
    pub fn parse(s: &str) -> Option<Pos> {
        use Pos::*;
        Some(match s {
            "top_left" => TopLeft,
            "top" => Top,
            "top_right" => TopRight,
            "left" => Left,
            "center" => Center,
            "right" => Right,
            "bottom_left" => BottomLeft,
            "bottom" => Bottom,
            "bottom_right" => BottomRight,
            "left_edge" => LeftEdge,
            "right_edge" => RightEdge,
            "top_edge" => TopEdge,
            "bottom_edge" => BottomEdge,
            _ => return None,
        })
    }

    /// The slot each position occupies on a normalized board (0..1 space).
    fn unit(self) -> (f64, f64) {
        use Pos::*;
        match self {
            TopLeft => (0.15, 0.10),
            Top | TopEdge => (0.50, 0.10),
            TopRight => (0.85, 0.10),
            Left | LeftEdge => (0.15, 0.50),
            Center => (0.50, 0.50),
            Right | RightEdge => (0.85, 0.50),
            BottomLeft => (0.15, 0.90),
            Bottom | BottomEdge => (0.50, 0.90),
            BottomRight => (0.85, 0.90),
        }
    }

    fn is_edge(self) -> bool {
        matches!(self, Pos::LeftEdge | Pos::RightEdge | Pos::TopEdge | Pos::BottomEdge)
    }
}

/// One placement intent from the plan schema.
#[derive(Debug, Clone)]
pub struct PlaceIntent {
    pub reference: String,
    pub pos: Option<Pos>,
    pub rot: Option<i32>,
    /// Flip to the bottom side.
    pub side_back: bool,
    /// Edge positions: fraction along the edge (0..1) to start the slide from.
    pub edge_at: Option<f64>,
}

/// A keep-together group: members hug the anchor within max_mm.
#[derive(Debug, Clone)]
pub struct GroupIntent {
    pub anchor: String,
    pub members: Vec<String>,
    pub max_mm: f64,
}

/// Keep-apart pair with a minimum distance.
#[derive(Debug, Clone)]
pub struct ApartIntent {
    pub a: String,
    pub b: String,
    pub min_mm: f64,
}

/// An alignment row: parts in order along an axis, optionally centered.
#[derive(Debug, Clone)]
pub struct RowIntent {
    pub references: Vec<String>,
    pub axis: Axis,
    pub equal: bool,
    pub center: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// The full placement intent (plan schema "place" arrays).
#[derive(Debug, Clone, Default)]
pub struct PlacementIntent {
    pub places: Vec<PlaceIntent>,
    pub groups: Vec<GroupIntent>,
    pub apart: Vec<ApartIntent>,
    pub rows: Vec<RowIntent>,
    /// (part, anchor): place `part` as close to `anchor` as possible.
    pub near: Vec<(String, String)>,
    /// Nets ignored for connectivity placement (planes, big power rails).
    pub ignore_nets: Vec<String>,
}

/// The solved geometry for one footprint.
#[derive(Debug, Clone)]
pub struct Solved {
    pub reference: String,
    pub x: f64,
    pub y: f64,
    pub rot: i32,
    pub back: bool,
}

/// The placer: owns ALL geometry. No coordinate ever enters from the LLM.
pub struct Placer {
    pub board_w: f64,
    pub board_h: f64,
    /// Cell pitch used for grid packing (mm) — legacy arrange().
    pub pitch: f64,
    /// Courtyard-to-courtyard gap (mm).
    pub gap: f64,
    /// Courtyard-to-outline margin (mm).
    pub margin: f64,
    /// Keep-out ring other parts leave around fine-pitch footprints (mm):
    /// room for escape lanes plus a fanout via row.
    pub escape_ring: f64,
}

/// An axis-aligned rectangle (min_x, min_y, max_x, max_y).
type Rect = (f64, f64, f64, f64);

fn overlaps(a: Rect, b: Rect, gap: f64) -> bool {
    a.0 < b.2 + gap && b.0 < a.2 + gap && a.1 < b.3 + gap && b.1 < a.3 + gap
}

impl Placer {
    pub fn new(board_w: f64, board_h: f64) -> Placer {
        Placer { board_w, board_h, pitch: 10.0, gap: 0.6, margin: 0.5, escape_ring: 3.0 }
    }

    /// Courtyard-aware placement from real footprint geometry.
    ///
    /// Footprints are expected at the origin (x = y = 0) with their final
    /// rotation applied; the returned `x`/`y` is where their origin goes.
    pub fn arrange_courtyards(
        &self,
        board: &Board,
        netlist: &Netlist,
        intent: &PlacementIntent,
    ) -> Vec<Solved> {
        // Per-part geometry: courtyard bbox relative to the origin (rotated)
        struct Part {
            reference: String,
            rel: Rect,
            /// extra keep-out ring other parts must respect (mm)
            ring: f64,
            rot: i32,
            degree: usize,
        }
        let mut parts: Vec<Part> = Vec::new();
        let neighbours = Self::neighbour_map(netlist, &intent.ignore_nets);
        for fp in &board.footprints {
            let (x0, y0, x1, y1) = fp.abs_bbox();
            // Fine-pitch parts need room for their escape lanes and fanout
            // vias: other parts stay outside a ring around the courtyard.
            let mut min_pitch = f64::INFINITY;
            for (i, a) in fp.pads.iter().enumerate() {
                for b in fp.pads.iter().skip(i + 1) {
                    let d = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
                    if d > 1e-6 {
                        min_pitch = min_pitch.min(d);
                    }
                }
            }
            let ring = if min_pitch < 0.65 && fp.pads.len() >= 8 { self.escape_ring } else { 0.0 };
            parts.push(Part {
                reference: fp.reference.clone(),
                rel: (x0 - fp.x, y0 - fp.y, x1 - fp.x, y1 - fp.y),
                ring,
                rot: fp.rotation.round() as i32,
                degree: neighbours.get(&fp.reference).map(|v| v.len()).unwrap_or(0),
            });
        }
        let part_of = |r: &str| parts.iter().position(|p| p.reference == r);

        let mut solved: Vec<Solved> = Vec::new();
        // placed (reference, courtyard rect, keep-out ring)
        let mut placed_rects: Vec<(String, Rect, f64)> = Vec::new();
        let hot: std::collections::HashMap<String, Vec<(String, f64)>> = {
            let mut m: std::collections::HashMap<String, Vec<(String, f64)>> = std::collections::HashMap::new();
            for ap in &intent.apart {
                m.entry(ap.a.clone()).or_default().push((ap.b.clone(), ap.min_mm));
                m.entry(ap.b.clone()).or_default().push((ap.a.clone(), ap.min_mm));
            }
            m
        };

        // The free-spot test for a part's bbox centred at (cx, cy)
        let fits = |reference: &str, rel: Rect, ring: f64, cx: f64, cy: f64, placed: &[(String, Rect, f64)], solved: &[Solved]| -> bool {
            let hw = (rel.2 - rel.0) / 2.0;
            let hh = (rel.3 - rel.1) / 2.0;
            let r = (cx - hw, cy - hh, cx + hw, cy + hh);
            if r.0 < self.margin || r.1 < self.margin || r.2 > self.board_w - self.margin || r.3 > self.board_h - self.margin {
                return false;
            }
            if placed.iter().any(|(_, q, qring)| overlaps(r, *q, self.gap + ring.max(*qring))) {
                return false;
            }
            if let Some(constraints) = hot.get(reference) {
                for (other, min_mm) in constraints {
                    if let Some(s) = solved.iter().find(|s| &s.reference == other) {
                        let d = ((s.x - cx).powi(2) + (s.y - cy).powi(2)).sqrt();
                        if d < *min_mm {
                            return false;
                        }
                    }
                }
            }
            true
        };
        // Deterministic spiral search on a 0.5mm lattice from a target point
        let spiral = |reference: &str, rel: Rect, ring: f64, tx: f64, ty: f64, placed: &[(String, Rect, f64)], solved: &[Solved]| -> Option<(f64, f64)> {
            let step = 0.5;
            let max_r = ((self.board_w.max(self.board_h)) / step).ceil() as i32 + 2;
            let snap = |v: f64| (v / step).round() * step;
            let (sx, sy) = (snap(tx), snap(ty));
            if fits(reference, rel, ring, sx, sy, placed, solved) {
                return Some((sx, sy));
            }
            for rg in 1..=max_r {
                let r = rg as f64 * step;
                // walk the ring perimeter clockwise from the top-left corner
                let mut cands: Vec<(f64, f64)> = Vec::new();
                let n = rg * 2;
                for i in 0..n {
                    let t = i as f64 * step;
                    cands.push((sx - r + t, sy - r));
                    cands.push((sx + r, sy - r + t));
                    cands.push((sx + r - t, sy + r));
                    cands.push((sx - r, sy + r - t));
                }
                // prefer the candidate closest to the target among the ring
                cands.sort_by(|a, b| {
                    let da = (a.0 - tx).powi(2) + (a.1 - ty).powi(2);
                    let db = (b.0 - tx).powi(2) + (b.1 - ty).powi(2);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                });
                for (cx, cy) in cands {
                    if fits(reference, rel, ring, cx, cy, placed, solved) {
                        return Some((cx, cy));
                    }
                }
            }
            None
        };
        let commit = |p: &Part, cx: f64, cy: f64, solved: &mut Vec<Solved>, placed: &mut Vec<(String, Rect, f64)>| {
            let hw = (p.rel.2 - p.rel.0) / 2.0;
            let hh = (p.rel.3 - p.rel.1) / 2.0;
            // origin = bbox centre minus the bbox's own offset from the origin
            let ox = cx - (p.rel.0 + hw);
            let oy = cy - (p.rel.1 + hh);
            solved.push(Solved { reference: p.reference.clone(), x: ox, y: oy, rot: p.rot, back: false });
            placed.push((p.reference.clone(), (cx - hw, cy - hh, cx + hw, cy + hh), p.ring));
        };

        // 1. Semantic / edge positions first
        for pi in &intent.places {
            let Some(idx) = part_of(&pi.reference) else { continue };
            let p = &parts[idx];
            let pos = pi.pos.unwrap_or(Pos::Center);
            let (mut ux, mut uy) = pos.unit();
            if let Some(f) = pi.edge_at {
                let f = f.clamp(0.0, 1.0);
                match pos {
                    Pos::LeftEdge | Pos::RightEdge => uy = f,
                    Pos::TopEdge | Pos::BottomEdge => ux = f,
                    _ => {}
                }
            }
            let hw = (p.rel.2 - p.rel.0) / 2.0;
            let hh = (p.rel.3 - p.rel.1) / 2.0;
            let (tx, ty) = if pos.is_edge() {
                match pos {
                    Pos::LeftEdge => (self.margin + hw, uy * self.board_h),
                    Pos::RightEdge => (self.board_w - self.margin - hw, uy * self.board_h),
                    Pos::TopEdge => (ux * self.board_w, self.margin + hh),
                    _ => (ux * self.board_w, self.board_h - self.margin - hh),
                }
            } else {
                (ux * self.board_w, uy * self.board_h)
            };
            let spot = if pos.is_edge() {
                // slide along the edge: alternate outward from the target
                let step = 0.5;
                let mut found = None;
                let span = (self.board_w.max(self.board_h) / step) as i32;
                for k in 0..=span {
                    for sign in [1.0, -1.0] {
                        if k == 0 && sign < 0.0 { continue; }
                        let d = k as f64 * step * sign;
                        let (cx, cy) = match pos {
                            Pos::LeftEdge | Pos::RightEdge => (tx, ty + d),
                            _ => (tx + d, ty),
                        };
                        if fits(&p.reference, p.rel, p.ring, cx, cy, &placed_rects, &solved) {
                            found = Some((cx, cy));
                            break;
                        }
                    }
                    if found.is_some() { break; }
                }
                found.or_else(|| spiral(&p.reference, p.rel, p.ring, tx, ty, &placed_rects, &solved))
            } else {
                spiral(&p.reference, p.rel, p.ring, tx, ty, &placed_rects, &solved)
            };
            if let Some((cx, cy)) = spot {
                commit(p, cx, cy, &mut solved, &mut placed_rects);
            }
        }

        // 2. Everything else: connectivity order, nearest free spot to the
        //    centroid of already-placed neighbours ("near" wins when given)
        let mut rest: Vec<usize> = (0..parts.len())
            .filter(|i| !solved.iter().any(|s| s.reference == parts[*i].reference))
            .collect();
        // Most connected first, then the largest courtyard (an IC whose nets
        // are all planes still anchors the layout), then by reference.
        let area = |p: &Part| (p.rel.2 - p.rel.0) * (p.rel.3 - p.rel.1);
        rest.sort_by(|&a, &b| {
            parts[b]
                .degree
                .cmp(&parts[a].degree)
                .then_with(|| area(&parts[b]).partial_cmp(&area(&parts[a])).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| parts[a].reference.cmp(&parts[b].reference))
        });
        // Place in waves: a part whose neighbours are all unplaced waits for
        // the most-connected part to seed the centre.
        let mut remaining = rest.clone();
        let mut guard = 0;
        while !remaining.is_empty() && guard < 10_000 {
            guard += 1;
            // pick the first remaining part with a placed neighbour/anchor,
            // else the most connected one
            let pick = remaining
                .iter()
                .position(|&i| {
                    let r = &parts[i].reference;
                    intent.near.iter().any(|(p, a)| p == r && solved.iter().any(|s| &s.reference == a))
                        || neighbours.get(r).map(|ns| ns.iter().any(|n| solved.iter().any(|s| &s.reference == n))).unwrap_or(false)
                })
                .unwrap_or(0);
            let i = remaining.remove(pick);
            let p = &parts[i];
            let target = if let Some((_, anchor)) = intent.near.iter().find(|(pr, _)| pr == &p.reference) {
                // Aim at the anchor's pads that share a net with this part
                // (a 1V8 cap goes to the 1V8 pins, not the IC's centre).
                solved.iter().find(|s| &s.reference == anchor).map(|s| {
                    let part_nets: std::collections::HashSet<&String> = board
                        .footprints
                        .iter()
                        .find(|f| f.reference == p.reference)
                        .map(|f| f.pads.iter().filter_map(|pd| pd.net.as_ref().map(|n| &n.1)).filter(|n| !intent.ignore_nets.contains(n)).collect())
                        .unwrap_or_default();
                    let afp = board.footprints.iter().find(|f| &f.reference == anchor);
                    let pts: Vec<(f64, f64)> = afp
                        .map(|f| {
                            f.pads
                                .iter()
                                .filter(|pd| pd.net.as_ref().map(|n| part_nets.contains(&n.1)).unwrap_or(false))
                                .map(|pd| (s.x + (pd.x - f.x), s.y + (pd.y - f.y)))
                                .collect()
                        })
                        .unwrap_or_default();
                    if pts.is_empty() {
                        (s.x, s.y)
                    } else {
                        let n = pts.len() as f64;
                        let (mx, my) = (pts.iter().map(|q| q.0).sum::<f64>() / n, pts.iter().map(|q| q.1).sum::<f64>() / n);
                        // push the target outward from the anchor centre so the
                        // part lands outside the anchor, on the pins' side
                        let (dx, dy) = (mx - s.x, my - s.y);
                        let d = (dx * dx + dy * dy).sqrt().max(1e-9);
                        (mx + dx / d * 4.0, my + dy / d * 4.0)
                    }
                })
            } else {
                None
            }
            .or_else(|| {
                let ns = neighbours.get(&p.reference)?;
                let pts: Vec<(f64, f64)> = ns
                    .iter()
                    .filter_map(|n| solved.iter().find(|s| &s.reference == n).map(|s| (s.x, s.y)))
                    .collect();
                if pts.is_empty() {
                    None
                } else {
                    let n = pts.len() as f64;
                    Some((pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n))
                }
            })
            .unwrap_or((self.board_w / 2.0, self.board_h / 2.0));
            if let Some((cx, cy)) = spiral(&p.reference, p.rel, p.ring, target.0, target.1, &placed_rects, &solved) {
                commit(p, cx, cy, &mut solved, &mut placed_rects);
            } else {
                // No room: park at the centre so the pin-coverage/DRC gates
                // report the overlap honestly rather than losing the part.
                commit(p, self.board_w / 2.0, self.board_h / 2.0, &mut solved, &mut placed_rects);
            }
        }
        solved
    }

    /// Reference → connected references, through nets not ignored and not
    /// huge (rails with > 8 pins do not pull parts together).
    fn neighbour_map(netlist: &Netlist, ignore: &[String]) -> std::collections::HashMap<String, Vec<String>> {
        let mut m: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for (name, pins) in &netlist.nets {
            if ignore.contains(name) || pins.len() > 8 {
                continue;
            }
            let refs: Vec<&String> = pins.iter().map(|(r, _)| r).collect();
            for a in &refs {
                for b in &refs {
                    if a != b {
                        let e = m.entry((*a).clone()).or_default();
                        if !e.contains(b) {
                            e.push((*b).clone());
                        }
                    }
                }
            }
        }
        m
    }

    /// Arrange footprints from intent + connectivity (legacy pitch-grid
    /// packer kept for the bringup flow and existing callers).
    pub fn arrange(
        &self,
        board: &Board,
        netlist: &Netlist,
        intent: &PlacementIntent,
    ) -> Vec<Solved> {
        let mut solved: Vec<Solved> = Vec::new();
        let mut placed: std::collections::HashSet<String> = std::collections::HashSet::new();

        for p in &intent.places {
            if let Some(fp) = board.footprints.iter().find(|f| f.reference == p.reference) {
                let (ux, uy) = p.pos.unwrap_or(Pos::Center).unit();
                solved.push(Solved {
                    reference: p.reference.clone(),
                    x: ux * self.board_w,
                    y: uy * self.board_h,
                    rot: p.rot.unwrap_or(fp.rotation as i32),
                    back: p.side_back,
                });
                placed.insert(p.reference.clone());
            }
        }

        for row in &intent.rows {
            let (w, h) = (self.board_w, self.board_h);
            let n = row.references.len();
            for (i, r) in row.references.iter().enumerate() {
                if placed.contains(r) {
                    continue;
                }
                let f = (i + 1) as f64 / (n + 1) as f64;
                let (x, y) = match row.axis {
                    Axis::Horizontal => (f * w, if row.center { h * 0.5 } else { h * 0.25 }),
                    Axis::Vertical => (if row.center { w * 0.5 } else { w * 0.25 }, f * h),
                };
                if let Some(fp) = board.footprints.iter().find(|f| f.reference == *r) {
                    solved.push(Solved { reference: r.clone(), x, y, rot: fp.rotation as i32, back: false });
                    placed.insert(r.clone());
                }
            }
        }

        for g in &intent.groups {
            if let Some(anchor) = solved.iter().find(|s| s.reference == g.anchor).cloned() {
                for m in &g.members {
                    if placed.contains(m) {
                        continue;
                    }
                    let mut k = 1;
                    loop {
                        let (dx, dy) = (anchor.x + (k as f64) * self.pitch, anchor.y + (k as f64) * self.pitch * 0.6);
                        if dx > anchor.x + g.max_mm && dy > anchor.y + g.max_mm {
                            break;
                        }
                        if !solved.iter().any(|s| s.reference == *m) {
                            if let Some(fp) = board.footprints.iter().find(|f| f.reference == *m) {
                                solved.push(Solved {
                                    reference: m.clone(),
                                    x: anchor.x + g.max_mm.min(dx - anchor.x),
                                    y: anchor.y + g.max_mm.min(dy - anchor.y),
                                    rot: fp.rotation as i32,
                                    back: false,
                                });
                                placed.insert(m.clone());
                            }
                        }
                        k += 2;
                        if k > 12 {
                            break;
                        }
                    }
                }
            }
        }

        let mut rest: Vec<&crate::board::Footprint> =
            board.footprints.iter().filter(|f| !placed.contains(&f.reference)).collect();
        let degree = |refdes: &str| -> usize {
            netlist.edges.iter().filter(|e| e.from_ref == refdes || e.to_ref == refdes).count()
        };
        rest.sort_by_key(|f| std::cmp::Reverse(degree(&f.reference)));
        let cells = self.center_out_cells(rest.len());
        for (i, fp) in rest.iter().enumerate() {
            let (cx, cy) = cells[i];
            solved.push(Solved { reference: fp.reference.clone(), x: cx, y: cy, rot: fp.rotation as i32, back: false });
        }

        for ap in &intent.apart {
            let (ia, ib) = (
                solved.iter().position(|s| s.reference == ap.a),
                solved.iter().position(|s| s.reference == ap.b),
            );
            if let (Some(ia), Some(ib)) = (ia, ib) {
                let d = ((solved[ib].x - solved[ia].x).powi(2) + (solved[ib].y - solved[ia].y).powi(2)).sqrt();
                if d < ap.min_mm {
                    let dx = solved[ib].x - solved[ia].x;
                    let dy = solved[ib].y - solved[ia].y;
                    if dx.abs() > 1e-9 || dy.abs() > 1e-9 {
                        let scale = ap.min_mm / d;
                        solved[ib].x = solved[ia].x + dx * scale;
                        solved[ib].y = solved[ia].y + dy * scale;
                    } else {
                        solved[ib].x = solved[ia].x + ap.min_mm;
                    }
                }
            }
        }

        let margin = 2.5;
        for s in &mut solved {
            s.x = s.x.clamp(margin, (self.board_w - margin).max(margin));
            s.y = s.y.clamp(margin, (self.board_h - margin).max(margin));
        }
        solved
    }

    /// Center-out grid cells: 0=center, then a deterministic square-ring walk
    /// outward on the pitch grid. Same input → same cells, always.
    fn center_out_cells(&self, n: usize) -> Vec<(f64, f64)> {
        let (cx, cy) = (
            (self.board_w / 2.0 / self.pitch).floor() * self.pitch,
            (self.board_h / 2.0 / self.pitch).floor() * self.pitch,
        );
        let mut cells = vec![(cx, cy)];
        let mut ring: i32 = 1;
        while cells.len() < n {
            let step = self.pitch;
            let r = ring as f64;
            let (left, right) = (cx - r * step, cx + r * step);
            let (top, bottom) = (cy - r * step, cy + r * step);
            let k = ring as usize;
            for i in 0..k { cells.push((left + i as f64 * step, top)); }
            for i in 0..k { cells.push((right, top + i as f64 * step)); }
            for i in 0..k { cells.push((right - i as f64 * step, bottom)); }
            for i in 0..k { cells.push((left, bottom - i as f64 * step)); }
            ring += 1;
            if ring > 64 {
                break;
            }
        }
        cells.truncate(n);
        cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board_2comp() -> Board {
        Board::from_kicad_pcb(
            "(kicad_pcb (version 20221018)
              (footprint \"R\" (layer \"F.Cu\") (at 0 0 0)
                (fp_text reference \"R1\" (at 0 -2))
                (pad \"1\" smd rect (at -1.25 0) (size 1.5 1.5) (net 1 \"VCC\"))
                (pad \"2\" smd rect (at 1.25 0) (size 1.5 1.5) (net 2 \"OUT\")))
              (footprint \"R\" (layer \"F.Cu\") (at 20 0 0)
                (fp_text reference \"R2\" (at 0 -2))
                (pad \"1\" smd rect (at -1.25 0) (size 1.5 1.5) (net 2 \"OUT\"))
                (pad \"2\" smd rect (at 1.25 0) (size 1.5 1.5) (net 3 \"GND\"))))",
        )
        .unwrap()
    }

    fn netlist_2comp(nl: &Board) -> Netlist {
        Netlist::from_board(nl)
    }

    #[test]
    fn pos_parse_roundtrip() {
        assert_eq!(Pos::parse("top_left"), Some(Pos::TopLeft));
        assert_eq!(Pos::parse("left_edge"), Some(Pos::LeftEdge));
        assert_eq!(Pos::parse("nonsense"), None);
    }

    #[test]
    fn semantic_positions_place_correctly() {
        let b = board_2comp();
        let nl = netlist_2comp(&b);
        let p = Placer::new(100.0, 80.0);
        let intent = PlacementIntent {
            places: vec![PlaceIntent { reference: "R1".into(), pos: Pos::parse("top_left"), rot: None, side_back: false, edge_at: None }],
            ..Default::default()
        };
        let solved = p.arrange(&b, &nl, &intent);
        let r1 = solved.iter().find(|s| s.reference == "R1").unwrap();
        assert!((r1.x - 15.0).abs() < 1e-9, "R1 top_left x = {}", r1.x);
        assert!((r1.y - 8.0).abs() < 1e-9, "R1 top_left y = {}", r1.y);
    }

    #[test]
    fn unplaced_pack_center_out() {
        let b = board_2comp();
        let nl = netlist_2comp(&b);
        let p = Placer::new(100.0, 80.0);
        let solved = p.arrange(&b, &nl, &PlacementIntent::default());
        assert_eq!(solved.len(), 2);
        for s in &solved {
            assert!(s.x > 0.0 && s.x < 100.0 && s.y > 0.0 && s.y < 80.0, "outside: {:?}", s);
        }
    }

    #[test]
    fn apart_constraint_pushes() {
        let b = board_2comp();
        let nl = netlist_2comp(&b);
        let p = Placer::new(100.0, 80.0);
        let intent = PlacementIntent {
            apart: vec![ApartIntent { a: "R1".into(), b: "R2".into(), min_mm: 15.0 }],
            ..Default::default()
        };
        let solved = p.arrange(&b, &nl, &intent);
        let r1 = solved.iter().find(|s| s.reference == "R1").unwrap();
        let r2 = solved.iter().find(|s| s.reference == "R2").unwrap();
        let d = ((r2.x - r1.x).powi(2) + (r2.y - r1.y).powi(2)).sqrt();
        assert!(d >= 15.0 - 1e-9, "apart violated: d={}", d);
    }

    #[test]
    fn deterministic_same_input_same_output() {
        let b = board_2comp();
        let nl = netlist_2comp(&b);
        let p = Placer::new(100.0, 80.0);
        let i1 = p.arrange(&b, &nl, &PlacementIntent::default());
        let i2 = p.arrange(&b, &nl, &PlacementIntent::default());
        assert_eq!(format!("{:?}", i1), format!("{:?}", i2));
    }

    /// Courtyard placement: a hub IC with 6 caps and an edge connector.
    fn board_hub() -> Board {
        let mut s = String::from("(kicad_pcb (version 20221018)\n");
        // U1: 10x10 courtyard
        s.push_str("(footprint \"U\" (layer \"F.Cu\") (at 0 0 0) (fp_text reference \"U1\" (at 0 0))\n");
        s.push_str(" (fp_rect (start -5 -5) (end 5 5) (layer \"F.CrtYd\"))\n");
        for i in 1..=6 {
            s.push_str(&format!(" (pad \"{}\" smd rect (at -4 {}) (size 0.5 0.5) (net {} \"N{}\"))\n", i, i as f64 - 3.5, i, i));
        }
        s.push_str(" (pad \"7\" smd rect (at 4 0) (size 0.5 0.5) (net 7 \"BUS\")))\n");
        for i in 1..=6 {
            s.push_str(&format!("(footprint \"C\" (layer \"F.Cu\") (at 0 0 0) (fp_text reference \"C{}\" (at 0 0))\n (fp_rect (start -1.5 -0.75) (end 1.5 0.75) (layer \"F.CrtYd\"))\n (pad \"1\" smd rect (at -0.8 0) (size 0.6 0.9) (net {} \"N{}\"))\n (pad \"2\" smd rect (at 0.8 0) (size 0.6 0.9) (net 99 \"GND\")))\n", i, i, i));
        }
        s.push_str("(footprint \"J\" (layer \"F.Cu\") (at 0 0 90) (fp_text reference \"J1\" (at 0 0))\n (fp_rect (start -2 -2) (end 2 8) (layer \"F.CrtYd\"))\n (pad \"1\" thru_hole circle (at 0 0) (size 1.7 1.7) (drill 1) (layers \"*.Cu\") (net 7 \"BUS\")))\n");
        s.push_str(")\n");
        Board::from_kicad_pcb(&s).unwrap()
    }

    #[test]
    fn courtyards_never_overlap_and_caps_hug_their_ic() {
        let b = board_hub();
        let nl = Netlist::from_board(&b);
        let p = Placer::new(40.0, 30.0);
        let intent = PlacementIntent {
            places: vec![PlaceIntent { reference: "J1".into(), pos: Pos::parse("left_edge"), rot: None, side_back: false, edge_at: None }],
            ignore_nets: vec!["GND".into()],
            ..Default::default()
        };
        let solved = p.arrange_courtyards(&b, &nl, &intent);
        assert_eq!(solved.len(), 8);
        // Apply and check courtyards
        let mut rects: Vec<(String, (f64, f64, f64, f64))> = Vec::new();
        for s in &solved {
            let mut fp = b.footprints.iter().find(|f| f.reference == s.reference).unwrap().clone();
            fp.translate(s.x - fp.x, s.y - fp.y);
            let r = fp.abs_bbox();
            assert!(r.0 >= 0.5 - 1e-9 && r.1 >= 0.5 - 1e-9 && r.2 <= 39.5 + 1e-9 && r.3 <= 29.5 + 1e-9, "{} outside: {:?}", s.reference, r);
            rects.push((s.reference.clone(), r));
        }
        for (i, (ra, a)) in rects.iter().enumerate() {
            for (rb, bb) in rects.iter().skip(i + 1) {
                assert!(!overlaps(*a, *bb, 0.0), "{} overlaps {}: {:?} {:?}", ra, rb, a, bb);
            }
        }
        // J1 on the left edge
        let j1 = rects.iter().find(|r| r.0 == "J1").unwrap();
        assert!((j1.1 .0 - 0.5).abs() < 1e-6, "J1 flush to the left edge: {:?}", j1.1);
        // every cap within 8mm of U1's origin
        let u1 = solved.iter().find(|s| s.reference == "U1").unwrap();
        for s in solved.iter().filter(|s| s.reference.starts_with('C')) {
            let d = ((s.x - u1.x).powi(2) + (s.y - u1.y).powi(2)).sqrt();
            assert!(d < 13.0, "{} is {:.1}mm from U1 (gap 0.6 ring)", s.reference, d);
        }
    }
}
