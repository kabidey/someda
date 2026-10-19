//! The deterministic board solver — someda's port of the placement/routing
//! agent contract from the SomTronics AI-EDA study harness:
//!
//!   "Never output x/y coordinates; the deterministic solver owns geometry,
//!    overlap avoidance, board fitting, and DRC."
//!
//! The LLM (or a human) expresses INTENT — semantic positions ("top_left",
//! "center"), groups, rows, spacing constraints — and this module turns it
//! into geometry. Two layers:
//!
//!   1. `place`   — footprint arrangement: grid packing with constraints
//! 2. `route`    — grid router: A* with clearance, vias, honest failure
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
            _ => return None,
        })
    }

    /// The slot each position occupies on a normalized board (0..1 space).
    fn unit(self) -> (f64, f64) {
        use Pos::*;
        match self {
            TopLeft => (0.15, 0.10),
            Top => (0.50, 0.10),
            TopRight => (0.85, 0.10),
            Left => (0.15, 0.50),
            Center => (0.50, 0.50),
            Right => (0.85, 0.50),
            BottomLeft => (0.15, 0.90),
            Bottom => (0.50, 0.90),
            BottomRight => (0.85, 0.90),
        }
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
    /// Cell pitch used for grid packing (mm).
    pub pitch: f64,
}

impl Placer {
    pub fn new(board_w: f64, board_h: f64) -> Placer {
        Placer { board_w, board_h, pitch: 10.0 }
    }

    /// Arrange footprints from intent + connectivity.
    ///
    /// Deterministic order:
    /// 1. Explicit semantic positions win.
    /// 2. Rows align their members on the axis.
    /// 3. Groups hug their anchors (within max_mm).
    /// 4. Everything else packs on a pitch grid ordered by ratsnest
    ///    centrality — the most-connected parts claim the center.
    pub fn arrange(
        &self,
        board: &Board,
        netlist: &Netlist,
        intent: &PlacementIntent,
    ) -> Vec<Solved> {
        let mut solved: Vec<Solved> = Vec::new();
        let mut placed: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 1. Semantic positions
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

        // 2. Rows: spread members along the axis, centered if asked
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
                    solved.push(Solved {
                        reference: r.clone(),
                        x,
                        y,
                        rot: fp.rotation as i32,
                        back: false,
                    });
                    placed.insert(r.clone());
                }
            }
        }

        // 3. Groups: hug the anchor (snap to next free pitch cell within max_mm)
        for g in &intent.groups {
            if let Some(anchor) = solved.iter().find(|s| s.reference == g.anchor).cloned() {
                for m in &g.members {
                    if placed.contains(m) {
                        continue;
                    }
                    let mut k = 1;
                    loop {
                        let (dx, dy) = (
                            anchor.x + (k as f64) * self.pitch,
                            anchor.y + (k as f64) * self.pitch * 0.6,
                        );
                        if dx > anchor.x + g.max_mm && dy > anchor.y + g.max_mm {
                            break;
                        }
                        if !solved.iter().any(|s| s.reference == *m) {
                            if let Some(fp) =
                                board.footprints.iter().find(|f| f.reference == *m)
                            {
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

        // 4. Everything else: grid packing ordered by ratsnest centrality
        let mut rest: Vec<&crate::board::Footprint> = board
            .footprints
            .iter()
            .filter(|f| !placed.contains(&f.reference))
            .collect();

        // Order by net degree (most connected = most central = early center cells)
        let degree = |refdes: &str| -> usize {
            netlist
                .edges
                .iter()
                .filter(|e| e.from_ref == refdes || e.to_ref == refdes)
                .count()
        };
        rest.sort_by_key(|f| std::cmp::Reverse(degree(&f.reference)));

        // Spiral-ish center-out cell order: deterministic ring walk around the center
        let cells = self.center_out_cells(rest.len());
        for (i, fp) in rest.iter().enumerate() {
            let (cx, cy) = cells[i];
            solved.push(Solved {
                reference: fp.reference.clone(),
                x: cx,
                y: cy,
                rot: fp.rotation as i32,
                back: false,
            });
        }

        // 5. Enforce apart constraints: push b away from a until the
        // CENTER DISTANCE ≥ min_mm (use 2D distance; fall back to x-axis push).
        for ap in &intent.apart {
            let (ia, ib) = (
                solved.iter().position(|s| s.reference == ap.a),
                solved.iter().position(|s| s.reference == ap.b),
            );
            if let (Some(ia), Some(ib)) = (ia, ib) {
                let d = ((solved[ib].x - solved[ia].x).powi(2)
                    + (solved[ib].y - solved[ia].y).powi(2))
                    .sqrt();
                if d < ap.min_mm {
                    // Push b along the a→b direction to exactly min_mm
                    let dx = solved[ib].x - solved[ia].x;
                    let dy = solved[ib].y - solved[ia].y;
                    if dx.abs() > 1e-9 || dy.abs() > 1e-9 {
                        let scale = ap.min_mm / d;
                        solved[ib].x = solved[ia].x + dx * scale;
                        solved[ib].y = solved[ia].y + dy * scale;
                    } else {
                        // coincident: push along +x deterministically
                        solved[ib].x = solved[ia].x + ap.min_mm;
                    }
                }
            }
        }

        // 6. Clamp all inside the outline with margin
        // Clamp inside the outline with a small mounting margin (1 mm),
        // NOT the packing pitch — semantic edge positions are legitimate.
        let margin = 1.0;
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
        // Walk concentric square rings, clockwise from top-left, ring by ring
        let mut ring: i32 = 1;
        while cells.len() < n {
            let step = self.pitch;
            // ring corners and edges: perimeter walk of the square at radius ring
            let r = ring as f64;
            let (left, right) = (cx - r * step, cx + r * step);
            let (top, bottom) = (cy - r * step, cy + r * step);
            // top edge (left→right), right edge (top→bottom),
            // bottom edge (right→left), left edge (bottom→top)
            let k = ring as usize; // cells per side
            for i in 0..k {
                cells.push((left + i as f64 * step, top)); // top
            }
            for i in 0..k {
                cells.push((right, top + i as f64 * step)); // right
            }
            for i in 0..k {
                cells.push((right - i as f64 * step, bottom)); // bottom
            }
            for i in 0..k {
                cells.push((left, bottom - i as f64 * step)); // left
            }
            ring += 1;
            if ring > 64 {
                break; // safety bound
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
        assert_eq!(Pos::parse("nonsense"), None);
    }

    #[test]
    fn semantic_positions_place_correctly() {
        let b = board_2comp();
        let nl = netlist_2comp(&b);
        let p = Placer::new(100.0, 80.0);
        let intent = PlacementIntent {
            places: vec![PlaceIntent {
                reference: "R1".into(),
                pos: Pos::parse("top_left"),
                rot: None,
                side_back: false,
            }],
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
        // Everything inside the board
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
        assert!(d >= 15.0 - 1e-9,
            "apart violated: R1=({}, {}) R2=({}, {}) d={}", r1.x, r1.y, r2.x, r2.y, d);
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
}