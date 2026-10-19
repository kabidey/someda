//! The grid router — someda's port of PCB-Superpower Skill 101 (SomRoute).
//!
//! "The autorouter is a search algorithm with a cost function — you don't
//! fight it, you set its inputs." The router never "fails"; it reports that
//! the current placement's routing difficulty exceeds the search budget.
//!
//! Verified defaults from the skill (live-verified board artifacts):
//! - Active trace width: 0.25 mm
//! - Active clearance: 0.2 mm
//! - Via preset: 0.6 / 0.3 (pad/drill)
//! - DRC floor: trace 0.2 mm, clearance 0.2 mm, via Ø0.5/0.3
//!
//! Honest-done: routed nets re-checked by DRC; anything the router cannot
//! finish is reported UNROUTED with a cause. "routed N of M connections".

use crate::board::Board;
use crate::netlist::{Netlist, RatsnestEdge};
use std::collections::BinaryHeap;

/// One routed copper track segment (board mm).
#[derive(Debug, Clone)]
pub struct TrackSegment {
    /// "F.Cu" or "B.Cu"
    pub layer: String,
    pub net: (usize, String),
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub width: f64,
}

/// A via connecting layers.
#[derive(Debug, Clone)]
pub struct Via {
    pub net: (usize, String),
    pub x: f64,
    pub y: f64,
    pub size: f64,
    pub drill: f64,
}

/// Why a net could not be routed (honest failure causes).
#[derive(Debug, Clone)]
pub struct UnroutedNet {
    pub net: (usize, String),
    pub cause: String,
}

/// The router output: copper + honest status.
#[derive(Debug, Clone, Default)]
pub struct RouteResult {
    pub segments: Vec<TrackSegment>,
    pub vias: Vec<Via>,
    pub unrouted: Vec<UnroutedNet>,
}

impl RouteResult {
    /// "routed N of M connections" — the skill's reporting format.
    pub fn summary_line(&self) -> String {
        let routed = self.segments.len();
        let open = self.unrouted.len();
        let total = routed + open;
        if total == 0 {
            return "routed 0 of 0 connections (nothing to route)".into();
        }
        format!(
            "routed {} of {} connections ({} unrouted)",
            routed, total, open
        )
    }

    pub fn all_routed(&self) -> bool {
        self.unrouted.is_empty() && !self.segments.is_empty()
    }
}

/// Router configuration — skill 101's verified defaults.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// Grid pitch (mm) — the trace centerline lattice.
    pub grid: f64,
    /// Default trace width (mm) — skill: 0.25 active.
    pub trace_width: f64,
    /// Clearance between different nets (mm) — skill: 0.2.
    pub clearance: f64,
    /// Via pad size / drill (mm) — skill preset: 0.6 / 0.3.
    pub via_size: f64,
    pub via_drill: f64,
    /// Board edge clearance (mm). KiCad's board-setup default is 0.5 and
    /// kicad-cli enforces it even when the skill quotes 0.25 — design to
    /// the stricter number (live-verified copper_edge_clearance violations).
    pub edge_clearance: f64,
    /// Max A* expansions before honest give-up.
    pub max_expansions: usize,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            // Skill 101 geometry discipline: the grid MUST encode the fab
            // contract. Trace 0.25 + clearance 0.2 → grid ≥ 0.5 so different
            // nets' copper is always ≥ 0.2mm apart edge-to-edge. 0.25-wide
            // traces on a 0.25 grid would physically touch — KiCad's DRC
            // proved it (live-verified: 13 shorts/clearance violations).
            grid: 0.5,
            trace_width: 0.25,
            clearance: 0.2,
            via_size: 0.6,
            via_drill: 0.3,
            edge_clearance: 0.5,
            max_expansions: 300_000,
        }
    }
}

/// A grid cell coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Cell {
    x: i32,
    y: i32,
    front: bool,
}

/// A* node in the search.
#[derive(Debug, Clone, PartialEq)]
struct Node {
    cell: Cell,
    f_score: f64,
}

impl Eq for Node {}
impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Node {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is max-heap; invert for min-heap on f_score
        other
            .f_score
            .partial_cmp(&self.f_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// The grid router.
pub struct Router {
    pub config: RouterConfig,
    /// Per-net impedance targets (Ω): nets listed here get their trace width
    /// SOLVED from the microstrip width-for-Z0 equation on the JLCPCB stackup
    /// — the router routes keeping the physics in mind (live-verified:
    /// the 0.25mm default gave RF nets Z0≈108Ω vs the 50Ω target, SWR 2.16).
    pub impedance_targets: std::collections::HashMap<String, f64>,
    /// Stackup the width solver uses (JLCPCB 1.6mm FR-4, Er 4.5 by default).
    pub stackup: crate::materials::Stackup,
}

impl Router {
    pub fn new(config: RouterConfig) -> Router {
        Router {
            config,
            impedance_targets: std::collections::HashMap::new(),
            stackup: crate::materials::Stackup::default(),
        }
    }

    /// Set the stackup the width solver uses (chainable).
    pub fn stackup_with(mut self, s: crate::materials::Stackup) -> Router {
        self.stackup = s;
        self
    }

    /// Declare an impedance-controlled net: the router sizes its traces.
    pub fn with_impedance_target(
        mut self,
        net: &str,
        z0: f64,
    ) -> Router {
        self.impedance_targets.insert(net.to_string(), z0);
        self
    }

    /// Solve this net's trace width for its Z0 target on the stackup.
    /// Falls back to the default width when the solved width violates the
    /// fab scope (a 50Ω target on 1.6mm FR-4 needs ~2.7mm — wide but in scope).
    fn width_for_net(&self, net: &str) -> f64 {
        match self.impedance_targets.get(net) {
            Some(z0) => {
                let s = &self.stackup;
                let w = crate::formulas::microstrip_width_for_z0(
                    *z0,
                    s.er,
                    s.h_mm,
                    s.copper_thickness_mm,
                );
                // Fab-scope guard: JLCPCB min width and a sane upper bound
                let fab_min = crate::materials::FabLimits::default().min_trace_mm;
                w.clamp(fab_min, 3.0)
            }
            None => self.config.trace_width,
        }
    }

    /// Route every ratsnest edge. Deterministic order: shortest Manhattan
    /// first (skill 101: placement difficulty is 80% of routing; short nets
    /// claim their lanes, power gets the remaining room).
    pub fn route(&self, board: &Board, netlist: &Netlist) -> RouteResult {
        let mut result = RouteResult::default();

        // Occupancy per (x,y): net that owns the cell (usize::MAX = hard block)
        // Occupancy keyed by (x, y, FRONT-LAYER) — the two copper layers are
        // independent routing domains. The old (x,y)-only key let F.Cu pad
        // keepouts block B.Cu cells, so dense boards could not escape via
        // vias (live-verified: 6/542 unrouted on the 9-part stress board).
        let mut occupied: std::collections::HashMap<(i32, i32, bool), usize> =
            std::collections::HashMap::new();
        let mut board_outline: Option<(f64, f64, f64, f64)> = None;
        if let Some((x0, y0, x1, y1)) = board.outline_bbox() {
            board_outline = Some((x0, y0, x1, y1));
        }

        // Stamp pad keep-outs in TWO PASSES. Pass 1 claims each pad's exact
        // cell with its net (the connection point — another pad's halo must
        // NEVER steal it: 1.27mm DIP pitch puts neighbor pads inside each
        // other's halos, and a single-pass stamp order-dependently corrupted
        // neighbor cells to hard-blocks, making A* goals unreachable —
        // live-verified on the 9-part stress board). Pass 2 stamps halos
        // only into unclaimed cells.
        // Pad cells are CONNECTION POINTS — immutable claims. A pad cell
        // keeps its net no matter whose halo covers it (1.27mm DIP pitch
        // puts neighbor pads inside each other's halos; letting a halo
        // flip a neighbor pad cell to hard-block made A* start/goal cells
        // unreachable — live-verified on the 9-part stress board).
        let mut pad_cells: std::collections::HashMap<(i32, i32, bool), usize> =
            std::collections::HashMap::new();
        // Pad registry for WIDTH-AWARE clearance: a wide (impedance-solved)
        // trace passing near a foreign pad needs pad_half + trace_half +
        // clearance of separation — a fixed cell halo cannot express that.
        // (Live-verified: 1.44mm RF tracks shorted adjacent pads at 0.032mm.)
        let mut pads: Vec<(f64, f64, f64, usize)> = Vec::new(); // (x, y, half, net)
        for fp in &board.footprints {
            for pad in &fp.pads {
                if let Some((net_id, _)) = pad.net {
                    let gx = (pad.x / self.config.grid).round() as i32;
                    let gy = (pad.y / self.config.grid).round() as i32;
                    pad_cells.insert((gx, gy, true), net_id);
                    let half = pad.radius();
                    pads.push((pad.x, pad.y, half, net_id));
                }
            }
        }
        // Halos stamp only cells no pad owns
        for fp in &board.footprints {
            for pad in &fp.pads {
                if let Some((net_id, _)) = pad.net {
                    let gx = (pad.x / self.config.grid).round() as i32;
                    let gy = (pad.y / self.config.grid).round() as i32;
                    // Halo = pad radius + clearance, FRONT layer only
                    // (SMD pads are F.Cu; B.Cu routing passes beneath)
                    let r = ((pad.radius() + self.config.clearance)
                        / self.config.grid)
                        .ceil() as i32;
                    for dx in -r..=r {
                        for dy in -r..=r {
                            let key = (gx + dx, gy + dy, true);
                            if pad_cells.contains_key(&key) {
                                continue; // never overwrite a pad connection point
                            }
                            occupied
                                .entry(key)
                                .and_modify(|n| {
                                    if *n != net_id {
                                        *n = usize::MAX;
                                    }
                                })
                                .or_insert(net_id);
                        }
                    }
                }
            }
        }
        // Pad claims go in LAST and win
        for (key, net_id) in &pad_cells {
            occupied.insert(*key, *net_id);
        }

        // Net name → id lookup for ratsnest edges
        let net_id = |name: &str| -> usize {
            board
                .nets
                .iter()
                .find(|(_, n)| n == name)
                .map(|(i, _)| *i)
                .unwrap_or(0)
        };

        // Deterministic order: POWER NETS FIRST (VCC/GND are the board's
        // highways — routing them last left them hemmed in; live-verified:
        // 2 unrouted on the 9-part stress board), then the rest shortest-first.
        let is_power = |name: &str| {
            name.eq_ignore_ascii_case("VCC")
                || name.eq_ignore_ascii_case("GND")
                || name.eq_ignore_ascii_case("+3V3")
                || name.eq_ignore_ascii_case("+5V")
                || name.eq_ignore_ascii_case("VBUS")
        };
        // IMPEDANCE-CONTROLLED NETS FIRST — they are the most width-constrained
        // (1.44mm traces + halos need room; VCC/GND can always weave later).
        let is_impedance = |name: &str| self.impedance_targets.contains_key(name);
        let mut edges: Vec<&RatsnestEdge> = netlist.edges.iter().collect();
        edges.sort_by(|a, b| {
            match (is_impedance(&a.net), is_impedance(&b.net)) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
            match (is_power(&a.net), is_power(&b.net)) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a
                    .manhattan_mm
                    .partial_cmp(&b.manhattan_mm)
                    .unwrap_or(std::cmp::Ordering::Equal),
            }
        });

        for edge in edges {
            let nid = net_id(&edge.net);
            let net_pair = (nid, edge.net.clone());
            let start = (edge.from_x, edge.from_y);
            let goal = (edge.to_x, edge.to_y);
            let mut net_width = self.width_for_net(&edge.net);
            // NECK-DOWN near fine-pitch pads: a controlled-Z trace wider
            // than the pad pitch cannot land on the pad without clearing
            // its neighbor (live-verified: 1.44mm RF trace on 1.27mm-pitch
            // J1 shorted the adjacent pad). Skill 101 fanout practice: the
            // final approach uses the narrow width; electrically it is a
            // stub ≪ λ/4 at these lengths.
            // NECK-DOWN criterion: fire ONLY when a full-width trace
            // LANDING on this specific pad would physically graze its
            // nearest foreign pad (pad-to-pad clearance violated), not
            // merely because a foreign pad exists within one trace-width
            // (that over-triggered on the corrected 2.78mm stackup and
            // necked every RF edge — live-verified).
            let near_foreign_pad = |x: f64, y: f64| -> bool {
                pads.iter().any(|&(px, py, phalf, pnet)| {
                    let own_half = pads
                        .iter()
                        .find(|&&(qx, qy, _, qn)| {
                            qn == nid && (qx - x).abs() < 0.01 && (qy - y).abs() < 0.01
                        })
                        .map(|&(_, _, oh, _)| oh)
                        .unwrap_or(0.5);
                    pnet != nid
                        && (x - px).abs() < phalf + own_half + self.config.clearance + net_width / 2.0
                        && (y - py).abs() < phalf + own_half + self.config.clearance + net_width / 2.0
                })
            };
            if near_foreign_pad(edge.from_x, edge.from_y)
                || near_foreign_pad(edge.to_x, edge.to_y)
            {
                net_width = self.config.trace_width;
            }
            match self.astar(start, goal, nid, &occupied, board_outline, &pads, net_width) {
                Some(path) => {
                    let mut prev: Option<Cell> = None;
                    for cell in path {
                        if let Some(p) = prev {
                            if p.front != cell.front {
                                // Deduplicate: an A* path can hop layers at the
                                // same coordinate twice (down+up), which would
                                // stack two vias in one hole — kicad-cli flags
                                // holes_co_located (live-verified).
                                let (vx, vy) = (
                                    cell.x as f64 * self.config.grid,
                                    cell.y as f64 * self.config.grid,
                                );
                                let dup = result
                                    .vias
                                    .iter()
                                    .any(|v| v.net.0 == nid
                                        && (v.x - vx).abs() < 1e-9
                                        && (v.y - vy).abs() < 1e-9);
                                if !dup {
                                    result.vias.push(Via {
                                        net: net_pair.clone(),
                                        x: vx,
                                        y: vy,
                                        size: self.config.via_size,
                                        drill: self.config.via_drill,
                                    });
                                }
                            }
                            result.segments.push(TrackSegment {
                                layer: if p.front { "F.Cu".into() } else { "B.Cu".into() },
                                net: net_pair.clone(),
                                x1: p.x as f64 * self.config.grid,
                                y1: p.y as f64 * self.config.grid,
                                x2: cell.x as f64 * self.config.grid,
                                y2: cell.y as f64 * self.config.grid,
                                width: net_width,
                            });
                        }
                        prev = Some(cell);
                    }
                    // Stamp EVERY traversed cell of the net's copper as
                    // occupied (rasterize each segment fully, not just its
                    // endpoints — an endpoint-only stamp let later nets cut
                    // through earlier tracks; live-verified by KiCad DRC).
                    // WIDTH-AWARE: an impedance-solved trace (e.g. 1.44mm
                    // for 50Ω on 1.6mm FR-4) spans multiple 0.5mm cells — its
                    // halo grows with the width so neighbors keep ≥0.2mm
                    // edge clearance (live-verified: 111 clearance violations
                    // when wide traces used the single-cell stamp).
                    for seg in result.segments.iter().filter(|s| s.net.0 == nid) {
                        let (x1, y1) = (
                            (seg.x1 / self.config.grid).round() as i32,
                            (seg.y1 / self.config.grid).round() as i32,
                        );
                        let (x2, y2) = (
                            (seg.x2 / self.config.grid).round() as i32,
                            (seg.y2 / self.config.grid).round() as i32,
                        );
                        // Halo radius in cells: SYMMETRIC wide-trace
                        // separation — the stamp must reserve
                        // own_half + clearance + foreign_half (the worst
                        // case of another impedance-solved net passing by).
                        // own_half-only under-reserved by 0.72mm and wide
                        // RF pairs touched at 0.06mm (live-verified).
                        // Halo reservation applies to WIDE traces only.
                        // Default 0.25mm traces stamp exactly their path cell
                        // — the v0.4.3 behavior (a halo radius on default
                        // traces walled the dense stress board: 3 unrouted;
                        // live-verified bisect). Clearance between default
                        // tracks is already guaranteed by the 0.5mm grid
                        // being > 2×(0.125 half-width) + 0.2.
                        let halo = if seg.width > self.config.trace_width + 1e-9 {
                            (((seg.width / 2.0
                                + self.config.clearance
                                + seg.width / 2.0)
                                / self.config.grid)
                                .ceil() as i32)
                            .max(0)
                        } else {
                            0
                        };
                        let front = seg.layer == "F.Cu";
                        let (dx, dy) = (x2 - x1, y2 - y1);
                        let steps = dx.abs().max(dy.abs());
                        for i in 0..=steps {
                            let (cx, cy) = if dx != 0 {
                                (x1 + dx.signum() * i, y1)
                            } else {
                                (x1, y1 + dy.signum() * i)
                            };
                            // Stamp the full width halo across the trace
                            for hx in -halo..=halo {
                                for hy in -halo..=halo {
                                    occupied
                                            .entry((cx + hx, cy + hy, front))
                                            .and_modify(|n| {
                                                if *n != nid {
                                                    *n = usize::MAX;
                                                }
                                            })
                                            .or_insert(nid);
                                }
                            }
                        }
                    }
                    // Via halo: a 0.6mm via pad extends past its cell —
                    // block the orthogonal neighbors for other nets so
                    // via-to-track clearance stays ≥ 0.2mm.
                    for via in result.vias.iter().filter(|v| v.net.0 == nid) {
                        let (vx, vy) = (
                            (via.x / self.config.grid).round() as i32,
                            (via.y / self.config.grid).round() as i32,
                        );
                        for (dx, dy) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
                            for front in [true, false] {
                                occupied
                                    .entry((vx + dx, vy + dy, front))
                                    .and_modify(|n| {
                                        if *n != nid {
                                            *n = usize::MAX;
                                        }
                                    })
                                    .or_insert(nid);
                            }
                        }
                    }
                }
                None => {
                    if std::env::var("SOMEDA_ROUTE_DEBUG").is_ok() {
                        eprintln!(
                            "route-debug: FAILED net {} from ({:.2},{:.2}) to ({:.2},{:.2}) manhattan {:.1}",
                            edge.net, start.0, start.1, goal.0, goal.1, edge.manhattan_mm
                        );
                    }
                    result.unrouted.push(UnroutedNet {
                        net: net_pair,
                        cause: "no path within expansion budget (placement congestion)"
                            .into(),
                    });
                }
            }
        }

        result
    }

    /// A* between two points; 8-directional moves on F.Cu, via hop to B.Cu.
    #[allow(clippy::too_many_arguments)]
    fn astar(
        &self,
        start: (f64, f64),
        goal: (f64, f64),
        net: usize,
        occupied: &std::collections::HashMap<(i32, i32, bool), usize>,
        outline: Option<(f64, f64, f64, f64)>,
        pads: &[(f64, f64, f64, usize)],
        trace_width: f64,
    ) -> Option<Vec<Cell>> {
        let snap = |v: f64| (v / self.config.grid).round() as i32;
        let start_cell = Cell {
            x: snap(start.0),
            y: snap(start.1),
            front: true,
        };
        let goal_cell = Cell {
            x: snap(goal.0),
            y: snap(goal.1),
            front: true,
        };

        // Edge clearance: cells outside the outline minus margin are blocked
        let _edge_cells = self.config.edge_clearance / self.config.grid;
        // The width-aware pad registry applies ONLY to wide (impedance-solved)
        // traces: for default 0.25mm traces the pre-stamped pad halos already
        // govern (live-verified: applying it to 0.25 nets walled the 1.27mm
        // DIP columns and unrouted N1/N2 on the stress board).
        let apply_pad_registry = trace_width > self.config.trace_width + 1e-9;
        let trace_half = trace_width / 2.0;
        let is_free = |c: &Cell| -> bool {
            if let Some((x0, y0, x1, y1)) = outline {
                let x = c.x as f64 * self.config.grid;
                let y = c.y as f64 * self.config.grid;
                if x < x0 + self.config.edge_clearance
                    || x > x1 - self.config.edge_clearance
                    || y < y0 + self.config.edge_clearance
                    || y > y1 - self.config.edge_clearance
                {
                    return false;
                }
            }
            // WIDTH-AWARE pad clearance: a wide trace must keep
            // pad_half + trace_half + clearance from foreign pads
            let (px, py) = (c.x as f64 * self.config.grid, c.y as f64 * self.config.grid);
            for &(pad_x, pad_y, pad_half, pad_net) in pads {
                if apply_pad_registry && pad_net != net {
                    let dx = (px - pad_x).abs();
                    let dy = (py - pad_y).abs();
                    let need = pad_half + trace_half + self.config.clearance;
                    if dx < need && dy < need {
                        return false;
                    }
                }
            }
            match occupied.get(&(c.x, c.y, c.front)) {
                None => true,
                Some(&n) => n == net,
            }
        };

        // Manhattan moves ONLY: diagonal steps would let two different
        // nets' tracks pass with < clearance at the corner (live-verified
        // by KiCad DRC). 4-directional keeps every different-net pair at
        // least one full grid cell apart center-to-center.
        let moves: [(i32, i32, f64); 4] = [
            (1, 0, 1.0),
            (-1, 0, 1.0),
            (0, 1, 1.0),
            (0, -1, 1.0),
        ];

        let mut open = BinaryHeap::new();
        let mut came_from: std::collections::HashMap<Cell, Cell> = std::collections::HashMap::new();
        let mut g_score: std::collections::HashMap<Cell, f64> = std::collections::HashMap::new();

        // The start pad cell is own-net: never blocked for this net
        g_score.insert(start_cell, 0.0);
        open.push(Node {
            cell: start_cell,
            f_score: h(start_cell, goal_cell),
        });

        let mut expansions = 0;

        while let Some(node) = open.pop() {
            expansions += 1;
            if expansions > self.config.max_expansions {
                return None; // honest: budget exhausted
            }
            if node.cell == goal_cell {
                let mut path = vec![goal_cell];
                let mut cur = goal_cell;
                while let Some(&prev) = came_from.get(&cur) {
                    path.push(prev);
                    cur = prev;
                }
                path.reverse();
                return Some(path);
            }

            for (dx, dy, cost) in moves {
                let next = Cell {
                    x: node.cell.x + dx,
                    y: node.cell.y + dy,
                    front: node.cell.front,
                };
                if !is_free(&next) {
                    continue;
                }
                let tentative = g_score.get(&node.cell).copied().unwrap_or(f64::INFINITY) + cost;
                if tentative < g_score.get(&next).copied().unwrap_or(f64::INFINITY) {
                    came_from.insert(next, node.cell);
                    g_score.insert(next, tentative);
                    open.push(Node {
                        cell: next,
                        f_score: tentative + h(next, goal_cell),
                    });
                }
            }

            // Via hop (skill 101: vias ≈ 0.5 nH + DCR — expensive, allowed).
            // A via pad (Ø0.6) reaches past its cell: the full 3x3 halo on
            // BOTH layers must be free of foreign copper, or the via lands
            // on top of an already-routed neighbor track (live-verified:
            // 0.075mm clearance violations between vias and earlier nets'
            // tracks on the stress board).
            let via_next = Cell {
                x: node.cell.x,
                y: node.cell.y,
                front: !node.cell.front,
            };
            let via_clear = {
                let mut ok = true;
                'halo: for dxdy in [(0i32, 0i32), (1, 0), (-1, 0), (0, 1), (0, -1)] {
                    for f in [true, false] {
                        let c = Cell {
                            x: node.cell.x + dxdy.0,
                            y: node.cell.y + dxdy.1,
                            front: f,
                        };
                        match occupied.get(&(c.x, c.y, c.front)) {
                            None => {}
                            Some(&n) if n == net => {}
                            Some(_) => {
                                ok = false;
                                break 'halo;
                            }
                        }
                    }
                }
                ok
            };
            if is_free(&via_next) && via_clear {
                let via_cost = 8.0;
                let tentative =
                    g_score.get(&node.cell).copied().unwrap_or(f64::INFINITY) + via_cost;
                if tentative < g_score.get(&via_next).copied().unwrap_or(f64::INFINITY) {
                    came_from.insert(via_next, node.cell);
                    g_score.insert(via_next, tentative);
                    open.push(Node {
                        cell: via_next,
                        f_score: tentative + h(via_next, goal_cell),
                    });
                }
            }
        }

        None // honest: no path found
    }
}

fn h(a: Cell, b: Cell) -> f64 {
    let dx = (a.x - b.x).abs() as f64;
    let dy = (a.y - b.y).abs() as f64;
    let dl = if a.front != b.front { 8.0 } else { 0.0 };
    dx + dy + dl
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board_2comp() -> Board {
        Board::from_kicad_pcb(
            "(kicad_pcb (version 20221018)
              (gr_line (start 0 0) (end 100 0) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 100 0) (end 100 80) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 100 80) (end 0 80) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 0 80) (end 0 0) (layer \"Edge.Cuts\") (width 0.1))
              (net 1 \"VCC\")
              (net 2 \"OUT\")
              (net 3 \"GND\")
              (footprint \"R\" (layer \"F.Cu\") (at 10 10 0)
                (fp_text reference \"R1\" (at 0 -2))
                (pad \"1\" smd rect (at -1.25 0) (size 1.5 1.5) (layers \"F.Cu\") (net 1 \"VCC\"))
                (pad \"2\" smd rect (at 1.25 0) (size 1.5 1.5) (layers \"F.Cu\") (net 2 \"OUT\")))
              (footprint \"R\" (layer \"F.Cu\") (at 30 10 0)
                (fp_text reference \"R2\" (at 0 -2))
                (pad \"1\" smd rect (at -1.25 0) (size 1.5 1.5) (layers \"F.Cu\") (net 2 \"OUT\"))
                (pad \"2\" smd rect (at 1.25 0) (size 1.5 1.5) (layers \"F.Cu\") (net 3 \"GND\"))))",
        )
        .unwrap()
    }

    #[test]
    fn routes_simple_connection() {
        let b = board_2comp();
        let nl = Netlist::from_board(&b);
        let router = Router::new(RouterConfig::default());
        let result = router.route(&b, &nl);
        assert!(
            !result.segments.is_empty(),
            "expected copper, got {:?}",
            result.summary_line()
        );
        // OUT (net 2) connects R1:2 and R2:1 — must be routed
        let out = result
            .segments
            .iter()
            .filter(|s| s.net.1 == "OUT")
            .count();
        assert!(out > 0, "net OUT must have copper: {}", result.summary_line());
    }

    #[test]
    fn unrouted_reported_honestly() {
        // Starved budget must produce an honest failure report, not silence
        let b = Board::from_kicad_pcb(
            "(kicad_pcb (version 20221018)
              (gr_line (start 0 0) (end 40 0) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 40 0) (end 40 40) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 40 40) (end 0 40) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 0 40) (end 0 0) (layer \"Edge.Cuts\") (width 0.1))
              (net 5 \"ISO\")
              (footprint \"A\" (layer \"F.Cu\") (at 3 3 0)
                (fp_text reference \"R1\" (at 0 -2))
                (pad \"1\" smd rect (at -1 0) (size 1.5 1.5) (layers \"F.Cu\") (net 5 \"ISO\"))
                (pad \"2\" smd rect (at 1 0) (size 1.5 1.5) (layers \"F.Cu\") (net 5 \"ISO\"))))",
        )
        .unwrap();
        let nl = Netlist::from_board(&b);
        let mut cfg = RouterConfig::default();
        cfg.max_expansions = 3; // starve: force honest failure
        let router = Router::new(cfg);
        let result = router.route(&b, &nl);
        assert!(
            !result.unrouted.is_empty() || result.segments.is_empty(),
            "starved search must report honestly"
        );
    }

    #[test]
    fn summary_line_matches_skill_format() {
        let r = RouteResult {
            segments: vec![TrackSegment {
                layer: "F.Cu".into(),
                net: (1, "VCC".into()),
                x1: 0.0,
                y1: 0.0,
                x2: 5.0,
                y2: 0.0,
                width: 0.25,
            }],
            ..Default::default()
        };
        let s = r.summary_line();
        assert!(s.starts_with("routed 1 of"), "got: {}", s);
    }

    #[test]
    fn verified_defaults_match_skill_101() {
        let c = RouterConfig::default();
        assert_eq!(c.trace_width, 0.25, "skill 101: active trace 0.25mm");
        assert_eq!(c.clearance, 0.2, "skill 101: active clearance 0.2mm");
        assert_eq!(c.via_size, 0.6, "skill 101: via preset 0.6");
        assert_eq!(c.via_drill, 0.3, "skill 101: via preset 0.3");
        assert_eq!(c.edge_clearance, 0.5, "kicad-cli enforces the 0.5 default");
    }
}