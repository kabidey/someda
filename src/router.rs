//! The grid router — someda's A* maze router, multilayer edition.
//!
//! "The autorouter is a search algorithm with a cost function — you don't
//! fight it, you set its inputs." The router never "fails"; it reports that
//! the current placement's routing difficulty exceeds the search budget.
//!
//! What it does (all deterministic, same input → same copper):
//! - Dense occupancy grid over every SIGNAL copper layer (planes are not
//!   routing layers); pads rasterized as their real rotated rectangles;
//!   through-hole pads block every layer; unnetted pads and NPTH holes are
//!   keep-outs; the board edge is a keep-out.
//! - Clearance encoded as cell halos sized from the design rules, so two
//!   different nets' copper is always ≥ clearance apart edge to edge.
//! - Plane nets (GND / power on inner layers) are not routed pad-to-pad:
//!   every SMD pad gets a short fanout stub to a through via that lands on
//!   the plane; through-hole pads reach the plane by their barrel.
//! - Route-to-net: an edge connects its start pad to any copper of the
//!   target pad's connected component, not only the target pad center.
//! - Via placement honours via-to-copper clearance on every layer and the
//!   fab's hole-to-hole spacing against vias and through-hole pads.
//! - Collinear cells merge into single track segments.
//! - A bounded rip-up-and-reroute pass retries unrouted edges after
//!   removing the nets that wall them in, keeping a result only when the
//!   unrouted count drops.
//!
//! Honest-done: anything the router cannot finish is reported UNROUTED with
//! a cause. "routed N of M connections".

use crate::board::{Board, PadType};
use crate::netlist::{Netlist, RatsnestEdge};
use std::collections::BinaryHeap;

/// One routed copper track segment (board mm).
#[derive(Debug, Clone)]
pub struct TrackSegment {
    /// Copper layer name ("F.Cu", "In2.Cu", "B.Cu", ...)
    pub layer: String,
    pub net: (usize, String),
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub width: f64,
}

/// A through via connecting all copper layers.
#[derive(Debug, Clone)]
pub struct Via {
    pub net: (usize, String),
    pub x: f64,
    pub y: f64,
    pub size: f64,
    pub drill: f64,
}

/// Why a connection could not be routed (honest failure causes).
#[derive(Debug, Clone)]
pub struct UnroutedNet {
    pub net: (usize, String),
    pub cause: String,
    /// "REF:PIN" endpoints of the failed connection
    pub from: String,
    pub to: String,
}

/// The router output: copper + honest status.
#[derive(Debug, Clone, Default)]
pub struct RouteResult {
    pub segments: Vec<TrackSegment>,
    pub vias: Vec<Via>,
    pub unrouted: Vec<UnroutedNet>,
    /// Ratsnest connections routed / attempted
    pub routed_edges: usize,
    pub total_edges: usize,
    /// Plane-net pads fanned out to a via (or reaching the plane directly)
    pub fanouts: usize,
}

impl RouteResult {
    /// "routed N of M connections" — the skill's reporting format.
    pub fn summary_line(&self) -> String {
        let total = self.total_edges;
        if total == 0 && self.fanouts == 0 {
            return "routed 0 of 0 connections (nothing to route)".into();
        }
        let mut s = format!(
            "routed {} of {} connections ({} unrouted)",
            self.routed_edges,
            total,
            self.unrouted.len()
        );
        if self.fanouts > 0 {
            s.push_str(&format!(", {} plane fanouts", self.fanouts));
        }
        s
    }

    pub fn all_routed(&self) -> bool {
        self.unrouted.is_empty() && (self.routed_edges > 0 || self.fanouts > 0)
    }
}

/// Router configuration — derived from the design rules.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// Grid pitch (mm) — the trace centerline lattice.
    pub grid: f64,
    /// Default trace width (mm).
    pub trace_width: f64,
    /// Width for power nets that are routed as traces (mm).
    pub power_trace_width: f64,
    /// Clearance between different nets (mm).
    pub clearance: f64,
    /// Via pad size / drill (mm).
    pub via_size: f64,
    pub via_drill: f64,
    /// Hole edge to hole edge spacing (mm) for vias and through-hole pads.
    pub hole_to_hole: f64,
    /// Board edge clearance (mm).
    pub edge_clearance: f64,
    /// Max A* expansions per connection before honest give-up.
    pub max_expansions: usize,
    /// Copper layers traces may use, outer to inner order.
    pub signal_layers: Vec<String>,
    /// Nets served by copper planes: fanned out to vias, never routed.
    pub plane_nets: Vec<String>,
    /// Nets routed at `power_trace_width`.
    pub power_nets: Vec<String>,
    /// Longest fanout stub from a plane-net pad to its via (mm).
    pub fanout_max_mm: f64,
    /// Rip-up-and-reroute passes after the first pass.
    pub rip_up_passes: usize,
    /// Cost of a layer change, in mm of trace.
    pub via_cost_mm: f64,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            // Skill 101 geometry discipline: the grid MUST encode the fab
            // contract. Trace 0.25 + clearance 0.2 → grid ≥ 0.5 so different
            // nets' copper is always ≥ 0.2mm apart edge-to-edge.
            grid: 0.5,
            trace_width: 0.25,
            power_trace_width: 0.5,
            clearance: 0.2,
            via_size: 0.6,
            via_drill: 0.3,
            hole_to_hole: 0.5,
            edge_clearance: 0.5,
            max_expansions: 300_000,
            signal_layers: vec!["F.Cu".into(), "B.Cu".into()],
            plane_nets: Vec::new(),
            power_nets: Vec::new(),
            fanout_max_mm: 3.0,
            rip_up_passes: 2,
            via_cost_mm: 2.0,
        }
    }
}

impl RouterConfig {
    /// Build the router configuration from the design rules for a board:
    /// `signal_layers` are the copper layers not consumed by planes.
    pub fn from_rules(
        rules: &crate::rules::DesignRules,
        signal_layers: Vec<String>,
        plane_nets: Vec<String>,
        power_nets: Vec<String>,
    ) -> RouterConfig {
        let cells_per_layer_estimate = 1_000_000usize;
        RouterConfig {
            grid: rules.grid_mm,
            trace_width: rules.trace_width_mm,
            power_trace_width: rules.power_trace_width_mm,
            clearance: rules.clearance_mm,
            via_size: rules.via_diameter_mm,
            via_drill: rules.via_drill_mm,
            hole_to_hole: rules.hole_to_hole_mm,
            edge_clearance: rules.edge_clearance_mm,
            max_expansions: cells_per_layer_estimate * signal_layers.len().max(1),
            signal_layers,
            plane_nets,
            power_nets,
            fanout_max_mm: 3.0,
            rip_up_passes: 2,
            via_cost_mm: 2.0,
        }
    }
}

const FREE: u32 = 0;
const BLOCK: u32 = u32::MAX;

/// A grid cell coordinate (x, y, signal-layer index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Cell {
    x: i32,
    y: i32,
    layer: u8,
}

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
        // BinaryHeap is a max-heap; invert for min-heap on f_score, then
        // break ties deterministically on the cell.
        other
            .f_score
            .partial_cmp(&self.f_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (other.cell.x, other.cell.y, other.cell.layer).cmp(&(self.cell.x, self.cell.y, self.cell.layer)))
    }
}

/// One pad as the router sees it.
#[derive(Debug, Clone)]
struct RPad {
    id: u32,
    reference: String,
    number: String,
    net: Option<usize>,
    x: f64,
    y: f64,
    /// axis-aligned half extents (rotation applied)
    hx: f64,
    hy: f64,
    through: bool,
    npth: bool,
    drill: f64,
    /// signal-layer indexes the pad has copper on
    layers: Vec<u8>,
    fine_pitch: bool,
}

/// Dense occupancy over (layer, y, x).
struct Grid {
    w: usize,
    h: usize,
    layers: usize,
    pitch: f64,
    /// FREE, net+1, or BLOCK
    owner: Vec<u32>,
    /// pad id + 1 of the copper that stamped this cell (0 = no copper)
    copper_pad: Vec<u32>,
    /// 2D: a via center may not land here (hole-to-hole / edge)
    via_forbidden: Vec<u8>,
}

impl Grid {
    fn new(w: usize, h: usize, layers: usize, pitch: f64) -> Grid {
        let n = w * h * layers;
        Grid {
            w,
            h,
            layers,
            pitch,
            owner: vec![FREE; n],
            copper_pad: vec![0; n],
            via_forbidden: vec![0; w * h],
        }
    }
    #[inline]
    fn idx(&self, x: i32, y: i32, l: u8) -> Option<usize> {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h || l as usize >= self.layers {
            return None;
        }
        Some(((l as usize) * self.h + y as usize) * self.w + x as usize)
    }
    #[inline]
    fn owner_at(&self, c: Cell) -> u32 {
        match self.idx(c.x, c.y, c.layer) {
            Some(i) => self.owner[i],
            None => BLOCK,
        }
    }
    /// Claim a cell for a net: free → net, other net → BLOCK, same → same.
    #[inline]
    fn claim(&mut self, x: i32, y: i32, l: u8, net_tag: u32) {
        if let Some(i) = self.idx(x, y, l) {
            let cur = self.owner[i];
            if cur == FREE {
                self.owner[i] = net_tag;
            } else if cur != net_tag {
                self.owner[i] = BLOCK;
            }
        }
    }
    #[inline]
    fn force(&mut self, x: i32, y: i32, l: u8, net_tag: u32, pad_id: u32) {
        if let Some(i) = self.idx(x, y, l) {
            self.owner[i] = net_tag;
            self.copper_pad[i] = pad_id + 1;
        }
    }
    fn forbid_via_disc(&mut self, cx: f64, cy: f64, radius: f64) {
        let r = (radius / self.pitch).ceil() as i32;
        let (gx, gy) = ((cx / self.pitch).round() as i32, (cy / self.pitch).round() as i32);
        for dy in -r..=r {
            for dx in -r..=r {
                let (x, y) = (gx + dx, gy + dy);
                if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
                    continue;
                }
                let d = ((x as f64 * self.pitch - cx).powi(2) + (y as f64 * self.pitch - cy).powi(2)).sqrt();
                if d < radius {
                    self.via_forbidden[y as usize * self.w + x as usize] = 1;
                }
            }
        }
    }
}

/// The grid router.
pub struct Router {
    pub config: RouterConfig,
    /// Per-net impedance targets (Ω): nets listed here get their trace width
    /// SOLVED from the microstrip width-for-Z0 equation on the stackup.
    pub impedance_targets: std::collections::HashMap<String, f64>,
    /// Stackup the width solver uses.
    pub stackup: crate::materials::Stackup,
}

/// Copper produced for one ratsnest edge (kept for rip-up bookkeeping).
#[derive(Debug, Clone)]
struct RoutedEdge {
    edge_index: usize,
    cells: Vec<Cell>,
    width: f64,
    net: usize,
    from_pad: u32,
}

#[derive(Debug, Clone)]
struct Fanout {
    pad: u32,
    net: usize,
    cells: Vec<Cell>,
    via: Option<(i32, i32)>,
}

/// Union-find over pad ids (connected components of a net's copper).
struct Dsu {
    parent: Vec<u32>,
}
impl Dsu {
    fn new(n: usize) -> Dsu {
        Dsu { parent: (0..n as u32).collect() }
    }
    fn find(&mut self, mut a: u32) -> u32 {
        while self.parent[a as usize] != a {
            let p = self.parent[a as usize];
            self.parent[a as usize] = self.parent[p as usize];
            a = p;
        }
        a
    }
    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[rb as usize] = ra;
        }
    }
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
    pub fn with_impedance_target(mut self, net: &str, z0: f64) -> Router {
        self.impedance_targets.insert(net.to_string(), z0);
        self
    }

    fn is_power_name(&self, name: &str) -> bool {
        if self.config.power_nets.iter().any(|n| n == name) {
            return true;
        }
        let u = name.to_ascii_uppercase();
        u == "VCC" || u == "GND" || u == "VBUS" || u == "VIN" || u == "VDD" || u == "VSS"
            || ((u.starts_with('+') || u.starts_with("VCC") || u.starts_with("VDD")) && u.chars().any(|c| c.is_ascii_digit()))
    }

    /// Solve this net's trace width: impedance target → microstrip solve on
    /// the stackup (clamped to the fab scope); power → power width; else default.
    fn width_for_net(&self, net: &str) -> f64 {
        if let Some(z0) = self.impedance_targets.get(net) {
            let s = &self.stackup;
            let w = crate::formulas::microstrip_width_for_z0(*z0, s.er, s.h_mm, s.copper_thickness_mm);
            let fab_min = crate::materials::FabLimits::default().min_trace_mm;
            return w.clamp(fab_min.max(self.config.trace_width), 3.0);
        }
        if self.is_power_name(net) {
            return self.config.power_trace_width;
        }
        self.config.trace_width
    }

    /// Route every ratsnest edge; fan out plane nets. Deterministic.
    pub fn route(&self, board: &Board, netlist: &Netlist) -> RouteResult {
        let pitch = self.config.grid;
        let (bx0, by0, bx1, by1) = board.outline_bbox().unwrap_or((0.0, 0.0, 100.0, 80.0));
        // The grid origin is the board origin; cell (i, j) centre = (i·pitch, j·pitch)
        let w = ((bx1 - bx0) / pitch).ceil() as usize + 1;
        let h = ((by1 - by0) / pitch).ceil() as usize + 1;
        let nlayers = self.config.signal_layers.len().max(1);
        let layer_index = |name: &str| -> Option<u8> {
            self.config.signal_layers.iter().position(|l| l == name).map(|i| i as u8)
        };

        // Net lookup
        let net_id = |name: &str| -> Option<usize> {
            board.nets.iter().find(|(_, n)| n == name).map(|(i, _)| *i)
        };
        let net_name = |id: usize| -> String {
            board.nets.iter().find(|(i, _)| *i == id).map(|(_, n)| n.clone()).unwrap_or_default()
        };
        let plane_ids: Vec<usize> = self.config.plane_nets.iter().filter_map(|n| net_id(n)).collect();

        // Pads as the router sees them
        let mut pads: Vec<RPad> = Vec::new();
        for fp in &board.footprints {
            // Fine pitch: the closest two same-footprint pad centres
            let mut min_pitch = f64::INFINITY;
            for (i, a) in fp.pads.iter().enumerate() {
                for b in fp.pads.iter().skip(i + 1) {
                    let d = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
                    if d > 1e-6 {
                        min_pitch = min_pitch.min(d);
                    }
                }
            }
            let fine = min_pitch < 0.65;
            for pad in &fp.pads {
                let (hx, hy) = pad.bbox_half();
                let through = pad.is_through();
                let layers: Vec<u8> = if through {
                    (0..nlayers as u8).collect()
                } else {
                    self.config
                        .signal_layers
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| pad.on_copper(l))
                        .map(|(i, _)| i as u8)
                        .collect()
                };
                pads.push(RPad {
                    id: pads.len() as u32,
                    reference: fp.reference.clone(),
                    number: pad.number.clone(),
                    net: pad.net.as_ref().map(|n| n.0),
                    x: pad.x - bx0,
                    y: pad.y - by0,
                    hx,
                    hy,
                    through,
                    npth: pad.pad_type == PadType::NpThruHole,
                    drill: pad.drill.max(pad.drill_oval.map(|(a, b)| a.max(b)).unwrap_or(0.0)),
                    layers,
                    fine_pitch: fine,
                });
            }
        }
        let pad_lookup = |reference: &str, number: &str| -> Option<u32> {
            pads.iter().find(|p| p.reference == reference && p.number == number).map(|p| p.id)
        };

        // Ratsnest edges to route: everything not on a plane net
        let mut edges: Vec<(usize, &RatsnestEdge, u32, u32, usize)> = Vec::new();
        for (i, e) in netlist.edges.iter().enumerate() {
            let nid = match net_id(&e.net) {
                Some(n) => n,
                None => continue,
            };
            if plane_ids.contains(&nid) {
                continue;
            }
            let (Some(a), Some(b)) = (pad_lookup(&e.from_ref, &e.from_pin), pad_lookup(&e.to_ref, &e.to_pin)) else {
                continue;
            };
            edges.push((i, e, a, b, nid));
        }
        // Deterministic order: impedance nets first (widest, most
        // constrained), then fine-pitch endpoints, then shortest first.
        let is_impedance = |name: &str| self.impedance_targets.contains_key(name);
        edges.sort_by(|a, b| {
            let (ea, eb) = (a.1, b.1);
            match (is_impedance(&ea.net), is_impedance(&eb.net)) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
            let fa = pads[a.2 as usize].fine_pitch || pads[a.3 as usize].fine_pitch;
            let fb = pads[b.2 as usize].fine_pitch || pads[b.3 as usize].fine_pitch;
            match (fa, fb) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
            ea.manhattan_mm
                .partial_cmp(&eb.manhattan_mm)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        let order: Vec<usize> = (0..edges.len()).collect();

        // ─── Pass 1 ───
        let mut state = RouteState {
            routed: Vec::new(),
            fanouts: Vec::new(),
            failed: Vec::new(),
            fanout_failed: Vec::new(),
        };
        let mut grid = Grid::new(w, h, nlayers, pitch);
        let mut dsu = Dsu::new(pads.len());
        self.stamp_static(&mut grid, &pads, bx1 - bx0, by1 - by0);
        self.fanout_planes(&mut grid, &pads, &plane_ids, &mut state);
        for &k in &order {
            let (ei, e, a, b, nid) = edges[k];
            let width = self.width_for_net(&e.net);
            match self.route_edge(&mut grid, &pads, &mut dsu, a, b, nid, width) {
                Some(cells) => {
                    self.stamp_path(&mut grid, &cells, nid, width, a);
                    dsu.union(a, b);
                    state.routed.push(RoutedEdge { edge_index: ei, cells, width, net: nid, from_pad: a });
                }
                None => state.failed.push(k),
            }
        }

        // ─── Rip-up and reroute ───
        for _pass in 0..self.config.rip_up_passes {
            if state.failed.is_empty() {
                break;
            }
            let failed_now = state.failed.clone();
            for k in failed_now {
                if !state.failed.contains(&k) {
                    continue;
                }
                let (_, e, a, b, nid) = edges[k];
                // Nets walling this edge in: routed edges with copper inside
                // the expanded bounding box of the failed connection.
                let margin = 2.0;
                let (xa, ya, xb, yb) = (pads[a as usize].x, pads[a as usize].y, pads[b as usize].x, pads[b as usize].y);
                let (x0, x1) = ((xa.min(xb) - margin) / pitch, (xa.max(xb) + margin) / pitch);
                let (y0, y1) = ((ya.min(yb) - margin) / pitch, (ya.max(yb) + margin) / pitch);
                let mut victims: Vec<(usize, usize)> = state
                    .routed
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.net != nid)
                    .map(|(i, r)| {
                        let inside = r
                            .cells
                            .iter()
                            .filter(|c| (c.x as f64) >= x0 && (c.x as f64) <= x1 && (c.y as f64) >= y0 && (c.y as f64) <= y1)
                            .count();
                        (i, inside)
                    })
                    .filter(|(_, n)| *n > 0)
                    .collect();
                if victims.is_empty() {
                    continue;
                }
                victims.sort_by(|p, q| q.1.cmp(&p.1).then(p.0.cmp(&q.0)));
                victims.truncate(6);
                let victim_indexes: Vec<usize> = victims.iter().map(|v| v.0).collect();
                let ripped: Vec<RoutedEdge> = victim_indexes.iter().map(|&i| state.routed[i].clone()).collect();
                let mut trial_routed: Vec<RoutedEdge> = state
                    .routed
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !victim_indexes.contains(i))
                    .map(|(_, r)| r.clone())
                    .collect();
                // Rebuild the grid from pads + surviving copper
                let mut g2 = Grid::new(w, h, nlayers, pitch);
                let mut dsu2 = Dsu::new(pads.len());
                self.stamp_static(&mut g2, &pads, bx1 - bx0, by1 - by0);
                self.replay_fanouts(&mut g2, &pads, &state.fanouts);
                for r in &trial_routed {
                    self.stamp_path(&mut g2, &r.cells, r.net, r.width, r.from_pad);
                    let eb = edges.iter().find(|x| x.0 == r.edge_index).map(|x| x.3).unwrap_or(r.from_pad);
                    dsu2.union(r.from_pad, eb);
                }
                let mut trial_failed: Vec<usize> = state.failed.iter().cloned().filter(|&f| f != k).collect();
                // The failed edge first
                let width = self.width_for_net(&e.net);
                match self.route_edge(&mut g2, &pads, &mut dsu2, a, b, nid, width) {
                    Some(cells) => {
                        self.stamp_path(&mut g2, &cells, nid, width, a);
                        dsu2.union(a, b);
                        trial_routed.push(RoutedEdge { edge_index: edges[k].0, cells, width, net: nid, from_pad: a });
                    }
                    None => trial_failed.push(k),
                }
                // Then the victims, in their original order
                let mut ripped_sorted = ripped.clone();
                ripped_sorted.sort_by_key(|r| order.iter().position(|&o| edges[o].0 == r.edge_index).unwrap_or(usize::MAX));
                for r in &ripped_sorted {
                    let (kk, ee, aa, bb, nn) = edges.iter().enumerate().find(|(_, x)| x.0 == r.edge_index).map(|(i, x)| (i, x.1, x.2, x.3, x.4)).unwrap();
                    let wv = self.width_for_net(&ee.net);
                    match self.route_edge(&mut g2, &pads, &mut dsu2, aa, bb, nn, wv) {
                        Some(cells) => {
                            self.stamp_path(&mut g2, &cells, nn, wv, aa);
                            dsu2.union(aa, bb);
                            trial_routed.push(RoutedEdge { edge_index: r.edge_index, cells, width: wv, net: nn, from_pad: aa });
                        }
                        None => trial_failed.push(kk),
                    }
                }
                if trial_failed.len() < state.failed.len() {
                    state.routed = trial_routed;
                    state.failed = trial_failed;
                    grid = g2;
                    dsu = dsu2;
                }
            }
        }
        let _ = (&grid, &dsu);

        // ─── Emit ───
        let mut result = RouteResult::default();
        result.total_edges = edges.len();
        for r in &state.routed {
            let name = net_name(r.net);
            self.emit_path(&mut result, &r.cells, (r.net, name), r.width, bx0, by0);
        }
        result.routed_edges = state.routed.len();
        for f in &state.fanouts {
            let name = net_name(f.net);
            self.emit_path(&mut result, &f.cells, (f.net, name.clone()), self.config.trace_width, bx0, by0);
            if let Some((vx, vy)) = f.via {
                result.vias.push(Via {
                    net: (f.net, name),
                    x: vx as f64 * pitch + bx0,
                    y: vy as f64 * pitch + by0,
                    size: self.config.via_size,
                    drill: self.config.via_drill,
                });
            }
            result.fanouts += 1;
        }
        for &k in &state.failed {
            let (_, e, _, _, nid) = edges[k];
            result.unrouted.push(UnroutedNet {
                net: (nid, e.net.clone()),
                cause: "no path within expansion budget (placement congestion)".into(),
                from: format!("{}:{}", e.from_ref, e.from_pin),
                to: format!("{}:{}", e.to_ref, e.to_pin),
            });
        }
        for &p in &state.fanout_failed {
            let pad = &pads[p as usize];
            let nid = pad.net.unwrap_or(0);
            result.unrouted.push(UnroutedNet {
                net: (nid, net_name(nid)),
                cause: format!("no via site within {:.1}mm fanout radius (plane net)", self.config.fanout_max_mm),
                from: format!("{}:{}", pad.reference, pad.number),
                to: "plane".into(),
            });
        }
        result
    }

    /// Cells blocked for everyone: board edge, unnetted pads, NPTH holes;
    /// pad copper and pad halos claimed for their nets.
    fn stamp_static(&self, grid: &mut Grid, pads: &[RPad], bw: f64, bh: f64) {
        let pitch = self.config.grid;
        let tw2 = self.config.trace_width / 2.0;
        // Edge keep-out on every layer
        let edge = self.config.edge_clearance + tw2;
        for l in 0..grid.layers as u8 {
            for y in 0..grid.h as i32 {
                for x in 0..grid.w as i32 {
                    let (px, py) = (x as f64 * pitch, y as f64 * pitch);
                    if px < edge || py < edge || px > bw - edge || py > bh - edge {
                        grid.force(x, y, l, BLOCK, 0);
                        if let Some(i) = grid.idx(x, y, l) {
                            grid.copper_pad[i] = 0;
                        }
                    }
                }
            }
        }
        for y in 0..grid.h as i32 {
            for x in 0..grid.w as i32 {
                let (px, py) = (x as f64 * pitch, y as f64 * pitch);
                let ve = self.config.edge_clearance + self.config.via_size / 2.0;
                if px < ve || py < ve || px > bw - ve || py > bh - ve {
                    grid.via_forbidden[y as usize * grid.w + x as usize] = 1;
                }
            }
        }
        // Halos first (claims), then pad copper (wins)
        for p in pads {
            let tag = match p.net {
                Some(n) if !p.npth => n as u32 + 1,
                _ => BLOCK,
            };
            let (hx, hy) = (p.hx + self.config.clearance + tw2, p.hy + self.config.clearance + tw2);
            let (gx0, gx1) = (((p.x - hx) / pitch).floor() as i32, ((p.x + hx) / pitch).ceil() as i32);
            let (gy0, gy1) = (((p.y - hy) / pitch).floor() as i32, ((p.y + hy) / pitch).ceil() as i32);
            for &l in &p.layers {
                for y in gy0..=gy1 {
                    for x in gx0..=gx1 {
                        let (cx, cy) = (x as f64 * pitch, y as f64 * pitch);
                        if (cx - p.x).abs() <= hx + 1e-9 && (cy - p.y).abs() <= hy + 1e-9 {
                            grid.claim(x, y, l, tag);
                        }
                    }
                }
            }
            // Holes: no via may land hole-to-hole close
            if p.through || p.npth {
                let r = p.drill / 2.0 + self.config.hole_to_hole + self.config.via_drill / 2.0;
                grid.forbid_via_disc(p.x, p.y, r.max(p.hx.max(p.hy) + self.config.clearance + self.config.via_size / 2.0));
            }
        }
        for p in pads {
            let tag = match p.net {
                Some(n) if !p.npth => n as u32 + 1,
                _ => BLOCK,
            };
            // Copper cells: centres strictly inside the pad rectangle — a
            // trace starting there stays inside the pad's copper. Always at
            // least the nearest cell.
            let mut any = false;
            let (gx0, gx1) = (((p.x - p.hx) / pitch).floor() as i32, ((p.x + p.hx) / pitch).ceil() as i32);
            let (gy0, gy1) = (((p.y - p.hy) / pitch).floor() as i32, ((p.y + p.hy) / pitch).ceil() as i32);
            for &l in &p.layers {
                for y in gy0..=gy1 {
                    for x in gx0..=gx1 {
                        let (cx, cy) = (x as f64 * pitch, y as f64 * pitch);
                        if (cx - p.x).abs() <= p.hx + 1e-9 && (cy - p.y).abs() <= p.hy + 1e-9 {
                            grid.force(x, y, l, tag, p.id);
                            any = true;
                        }
                    }
                }
            }
            if !any {
                let (gx, gy) = ((p.x / pitch).round() as i32, (p.y / pitch).round() as i32);
                for &l in &p.layers {
                    grid.force(gx, gy, l, tag, p.id);
                }
            }
        }
    }

    /// Cells of a pad's copper (as stamped) on one layer.
    fn pad_cells(&self, grid: &Grid, p: &RPad, layer: u8) -> Vec<Cell> {
        let pitch = self.config.grid;
        let mut v = Vec::new();
        let (gx0, gx1) = (((p.x - p.hx) / pitch).floor() as i32, ((p.x + p.hx) / pitch).ceil() as i32);
        let (gy0, gy1) = (((p.y - p.hy) / pitch).floor() as i32, ((p.y + p.hy) / pitch).ceil() as i32);
        for y in gy0..=gy1 {
            for x in gx0..=gx1 {
                if let Some(i) = grid.idx(x, y, layer) {
                    if grid.copper_pad[i] == p.id + 1 {
                        v.push(Cell { x, y, layer });
                    }
                }
            }
        }
        if v.is_empty() {
            let (gx, gy) = ((p.x / pitch).round() as i32, (p.y / pitch).round() as i32);
            v.push(Cell { x: gx, y: gy, layer });
        }
        v
    }

    /// Chebyshev halo radius (cells) a trace of `width` needs so a default
    /// trace of another net stays ≥ clearance away edge to edge.
    fn halo_cells(&self, width: f64) -> i32 {
        let s = width / 2.0 + self.config.clearance + self.config.trace_width / 2.0;
        ((s / self.config.grid).ceil() as i32 - 1).max(0)
    }

    /// Extra body radius (cells) a trace wider than the default occupies.
    fn body_cells(&self, width: f64) -> i32 {
        if width <= self.config.trace_width + 1e-9 {
            0
        } else {
            (((width - self.config.trace_width) / 2.0) / self.config.grid).ceil() as i32
        }
    }

    fn via_halo_cells(&self) -> i32 {
        let s = self.config.via_size / 2.0 + self.config.clearance + self.config.trace_width / 2.0;
        ((s / self.config.grid).ceil() as i32 - 1).max(0)
    }

    /// Stamp a routed path (copper + halos; vias where the layer changes).
    fn stamp_path(&self, grid: &mut Grid, cells: &[Cell], net: usize, width: f64, pad_id: u32) {
        let tag = net as u32 + 1;
        let halo = self.halo_cells(width);
        let body = self.body_cells(width);
        let vhalo = self.via_halo_cells();
        let vbody = ((self.config.via_size / 2.0) / self.config.grid).floor() as i32;
        let mut prev: Option<Cell> = None;
        for c in cells {
            for dy in -halo..=halo {
                for dx in -halo..=halo {
                    grid.claim(c.x + dx, c.y + dy, c.layer, tag);
                }
            }
            for dy in -body..=body {
                for dx in -body..=body {
                    grid.force(c.x + dx, c.y + dy, c.layer, tag, pad_id);
                }
            }
            grid.force(c.x, c.y, c.layer, tag, pad_id);
            if let Some(p) = prev {
                if p.layer != c.layer {
                    // a via: every layer
                    for l in 0..grid.layers as u8 {
                        for dy in -vhalo..=vhalo {
                            for dx in -vhalo..=vhalo {
                                grid.claim(c.x + dx, c.y + dy, l, tag);
                            }
                        }
                        for dy in -vbody..=vbody {
                            for dx in -vbody..=vbody {
                                grid.force(c.x + dx, c.y + dy, l, tag, pad_id);
                            }
                        }
                    }
                    let (vx, vy) = (c.x as f64 * self.config.grid, c.y as f64 * self.config.grid);
                    grid.forbid_via_disc(vx, vy, self.config.via_drill + self.config.hole_to_hole);
                }
            }
            prev = Some(*c);
        }
    }

    fn replay_fanouts(&self, grid: &mut Grid, _pads: &[RPad], fanouts: &[Fanout]) {
        for f in fanouts {
            self.stamp_path(grid, &f.cells, f.net, self.config.trace_width, f.pad);
            if let Some((vx, vy)) = f.via {
                self.stamp_via(grid, vx, vy, f.net, f.pad);
            }
        }
    }

    fn stamp_via(&self, grid: &mut Grid, vx: i32, vy: i32, net: usize, pad_id: u32) {
        let tag = net as u32 + 1;
        let vhalo = self.via_halo_cells();
        let vbody = ((self.config.via_size / 2.0) / self.config.grid).floor() as i32;
        for l in 0..grid.layers as u8 {
            for dy in -vhalo..=vhalo {
                for dx in -vhalo..=vhalo {
                    grid.claim(vx + dx, vy + dy, l, tag);
                }
            }
            for dy in -vbody..=vbody {
                for dx in -vbody..=vbody {
                    grid.force(vx + dx, vy + dy, l, tag, pad_id);
                }
            }
        }
        let (x, y) = (vx as f64 * self.config.grid, vy as f64 * self.config.grid);
        grid.forbid_via_disc(x, y, self.config.via_drill + self.config.hole_to_hole);
    }

    /// Can a through via of `net` land at (x, y)? Every layer within the
    /// via halo must be free or own-net, and the hole-to-hole map clear.
    fn via_ok(&self, grid: &Grid, x: i32, y: i32, tag: u32) -> bool {
        if x < 0 || y < 0 || x as usize >= grid.w || y as usize >= grid.h {
            return false;
        }
        if grid.via_forbidden[y as usize * grid.w + x as usize] != 0 {
            return false;
        }
        let r = self.via_halo_cells();
        for l in 0..grid.layers as u8 {
            for dy in -r..=r {
                for dx in -r..=r {
                    let o = grid.owner_at(Cell { x: x + dx, y: y + dy, layer: l });
                    if o != FREE && o != tag {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Plane nets: every SMD pad gets a stub + via; through-hole pads reach
    /// the plane by their barrel.
    fn fanout_planes(&self, grid: &mut Grid, pads: &[RPad], plane_ids: &[usize], state: &mut RouteState) {
        let max_cells = (self.config.fanout_max_mm / self.config.grid).ceil() as i32;
        for p in pads {
            let Some(net) = p.net else { continue };
            if !plane_ids.contains(&net) || p.npth {
                continue;
            }
            if p.through {
                state.fanouts.push(Fanout { pad: p.id, net, cells: Vec::new(), via: None });
                continue;
            }
            let tag = net as u32 + 1;
            let Some(&layer) = p.layers.first() else { continue };
            // BFS from the pad's copper cells on its layer to the nearest
            // legal via site (or existing plane-net copper that is not a pad).
            let starts = self.pad_cells(grid, p, layer);
            let mut queue: std::collections::VecDeque<(Cell, i32)> = std::collections::VecDeque::new();
            let mut came: std::collections::HashMap<Cell, Cell> = std::collections::HashMap::new();
            for s in &starts {
                queue.push_back((*s, 0));
                came.insert(*s, *s);
            }
            let mut found: Option<(Cell, bool)> = None;
            while let Some((c, d)) = queue.pop_front() {
                let in_pad = grid.idx(c.x, c.y, c.layer).map(|i| grid.copper_pad[i] == p.id + 1).unwrap_or(false);
                if !in_pad {
                    // existing copper of this plane net (a via or stub) counts as connected
                    let cp = grid.idx(c.x, c.y, c.layer).map(|i| grid.copper_pad[i]).unwrap_or(0);
                    if cp != 0 && grid.owner_at(c) == tag && pads.get((cp - 1) as usize).map(|q| q.id != p.id && (state.fanouts.iter().any(|f| f.pad == q.id && (f.via.is_some() || q.through)))).unwrap_or(false) {
                        found = Some((c, false));
                        break;
                    }
                    if self.via_ok(grid, c.x, c.y, tag) {
                        found = Some((c, true));
                        break;
                    }
                }
                if d >= max_cells {
                    continue;
                }
                for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                    let n = Cell { x: c.x + dx, y: c.y + dy, layer: c.layer };
                    if came.contains_key(&n) {
                        continue;
                    }
                    let o = grid.owner_at(n);
                    if o == FREE || o == tag {
                        came.insert(n, c);
                        queue.push_back((n, d + 1));
                    }
                }
            }
            match found {
                Some((goal, with_via)) => {
                    let mut path = vec![goal];
                    let mut cur = goal;
                    while let Some(&prev) = came.get(&cur) {
                        if prev == cur {
                            break;
                        }
                        path.push(prev);
                        cur = prev;
                    }
                    path.reverse();
                    self.stamp_path(grid, &path, net, self.config.trace_width, p.id);
                    let via = if with_via {
                        self.stamp_via(grid, goal.x, goal.y, net, p.id);
                        Some((goal.x, goal.y))
                    } else {
                        None
                    };
                    state.fanouts.push(Fanout { pad: p.id, net, cells: path, via });
                }
                None => state.fanout_failed.push(p.id),
            }
        }
    }

    /// A* from pad `a`'s copper to any copper of pad `b`'s connected
    /// component. 4-neighbour moves on a layer, via hops between layers.
    fn route_edge(
        &self,
        grid: &mut Grid,
        pads: &[RPad],
        dsu: &mut Dsu,
        a: u32,
        b: u32,
        net: usize,
        width: f64,
    ) -> Option<Vec<Cell>> {
        let tag = net as u32 + 1;
        let pa = &pads[a as usize];
        let pb = &pads[b as usize];
        if dsu.find(a) == dsu.find(b) {
            // already connected through earlier copper: nothing to add
            return Some(Vec::new());
        }
        let root_b = dsu.find(b);
        let body = self.body_cells(width);
        let via_cost = self.config.via_cost_mm / self.config.grid;
        let pitch = self.config.grid;

        let mut starts: Vec<Cell> = Vec::new();
        for &l in &pa.layers {
            starts.extend(self.pad_cells(grid, pa, l));
        }
        if starts.is_empty() {
            return None;
        }
        let goal_xy = (pb.x / pitch, pb.y / pitch);
        let h = |c: &Cell| -> f64 { (c.x as f64 - goal_xy.0).abs() + (c.y as f64 - goal_xy.1).abs() };

        let is_goal = |c: &Cell, grid: &Grid, dsu: &mut Dsu| -> bool {
            match grid.idx(c.x, c.y, c.layer) {
                Some(i) => {
                    let cp = grid.copper_pad[i];
                    cp != 0 && grid.owner[i] == tag && dsu.find(cp - 1) == root_b
                }
                None => false,
            }
        };
        // Free for this net's trace body (default traces: the cell itself)
        let is_free = |c: &Cell, grid: &Grid| -> bool {
            if body == 0 {
                let o = grid.owner_at(*c);
                return o == FREE || o == tag;
            }
            for dy in -body..=body {
                for dx in -body..=body {
                    let o = grid.owner_at(Cell { x: c.x + dx, y: c.y + dy, layer: c.layer });
                    if o != FREE && o != tag {
                        return false;
                    }
                }
            }
            true
        };

        let mut open = BinaryHeap::new();
        let mut came_from: std::collections::HashMap<Cell, Cell> = std::collections::HashMap::new();
        let mut g_score: std::collections::HashMap<Cell, f64> = std::collections::HashMap::new();
        for s in &starts {
            g_score.insert(*s, 0.0);
            open.push(Node { cell: *s, f_score: h(s) });
        }
        let mut expansions = 0usize;
        while let Some(node) = open.pop() {
            expansions += 1;
            if expansions > self.config.max_expansions {
                return None;
            }
            let g_here = *g_score.get(&node.cell).unwrap_or(&f64::INFINITY);
            if g_here + h(&node.cell) < node.f_score - 1e-9 {
                continue; // stale entry
            }
            if g_here > 0.0 && is_goal(&node.cell, grid, dsu) {
                let mut path = vec![node.cell];
                let mut cur = node.cell;
                while let Some(&prev) = came_from.get(&cur) {
                    path.push(prev);
                    cur = prev;
                }
                path.reverse();
                return Some(path);
            }
            // Layer direction preference: alternate H/V on inner signal layers
            let pref_h = node.cell.layer % 2 == 1;
            for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let next = Cell { x: node.cell.x + dx, y: node.cell.y + dy, layer: node.cell.layer };
                if !is_free(&next, grid) {
                    continue;
                }
                let against = if grid.layers > 2 && node.cell.layer != 0 && node.cell.layer as usize != grid.layers - 1 {
                    if (dx != 0) != pref_h { 0.3 } else { 0.0 }
                } else {
                    0.0
                };
                let tentative = g_here + 1.0 + against;
                if tentative < *g_score.get(&next).unwrap_or(&f64::INFINITY) {
                    came_from.insert(next, node.cell);
                    g_score.insert(next, tentative);
                    open.push(Node { cell: next, f_score: tentative + h(&next) });
                }
            }
            // Via hop to every other signal layer
            if grid.layers > 1 && self.via_ok(grid, node.cell.x, node.cell.y, tag) {
                for l in 0..grid.layers as u8 {
                    if l == node.cell.layer {
                        continue;
                    }
                    let next = Cell { x: node.cell.x, y: node.cell.y, layer: l };
                    if !is_free(&next, grid) {
                        continue;
                    }
                    let tentative = g_here + via_cost;
                    if tentative < *g_score.get(&next).unwrap_or(&f64::INFINITY) {
                        came_from.insert(next, node.cell);
                        g_score.insert(next, tentative);
                        open.push(Node { cell: next, f_score: tentative + h(&next) });
                    }
                }
            }
        }
        None
    }

    /// Convert a cell path into merged collinear segments + vias.
    fn emit_path(&self, result: &mut RouteResult, cells: &[Cell], net: (usize, String), width: f64, ox: f64, oy: f64) {
        let pitch = self.config.grid;
        let to_mm = |c: &Cell| (c.x as f64 * pitch + ox, c.y as f64 * pitch + oy);
        if cells.len() < 2 {
            return;
        }
        let mut run_start = cells[0];
        let mut dir: Option<(i32, i32)> = None;
        for i in 1..cells.len() {
            let (p, c) = (cells[i - 1], cells[i]);
            if p.layer != c.layer {
                // close the run on the previous layer, drop a via
                if run_start != p {
                    let (x1, y1) = to_mm(&run_start);
                    let (x2, y2) = to_mm(&p);
                    result.segments.push(TrackSegment {
                        layer: self.config.signal_layers[p.layer as usize].clone(),
                        net: net.clone(), x1, y1, x2, y2, width,
                    });
                }
                let (vx, vy) = to_mm(&c);
                if !result.vias.iter().any(|v| v.net.0 == net.0 && (v.x - vx).abs() < 1e-9 && (v.y - vy).abs() < 1e-9) {
                    result.vias.push(Via { net: net.clone(), x: vx, y: vy, size: self.config.via_size, drill: self.config.via_drill });
                }
                run_start = c;
                dir = None;
                continue;
            }
            let d = (c.x - p.x, c.y - p.y);
            match dir {
                None => dir = Some(d),
                Some(cur) if cur == d => {}
                Some(_) => {
                    let (x1, y1) = to_mm(&run_start);
                    let (x2, y2) = to_mm(&p);
                    result.segments.push(TrackSegment {
                        layer: self.config.signal_layers[p.layer as usize].clone(),
                        net: net.clone(), x1, y1, x2, y2, width,
                    });
                    run_start = p;
                    dir = Some(d);
                }
            }
        }
        let last = cells[cells.len() - 1];
        if run_start != last {
            let (x1, y1) = to_mm(&run_start);
            let (x2, y2) = to_mm(&last);
            result.segments.push(TrackSegment {
                layer: self.config.signal_layers[last.layer as usize].clone(),
                net, x1, y1, x2, y2, width,
            });
        }
    }
}

struct RouteState {
    routed: Vec<RoutedEdge>,
    fanouts: Vec<Fanout>,
    failed: Vec<usize>,
    fanout_failed: Vec<u32>,
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
        assert!(!result.segments.is_empty(), "expected copper, got {:?}", result.summary_line());
        let out = result.segments.iter().filter(|s| s.net.1 == "OUT").count();
        assert!(out > 0, "net OUT must have copper: {}", result.summary_line());
        assert_eq!(result.total_edges, 1);
        assert_eq!(result.routed_edges, 1);
        assert!(result.all_routed());
        // collinear merge: a straight 20mm run is ONE segment
        assert!(out <= 2, "straight run should merge into ≤2 segments, got {}", out);
    }

    #[test]
    fn unrouted_reported_honestly() {
        let b = Board::from_kicad_pcb(
            "(kicad_pcb (version 20221018)
              (gr_line (start 0 0) (end 40 0) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 40 0) (end 40 40) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 40 40) (end 0 40) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 0 40) (end 0 0) (layer \"Edge.Cuts\") (width 0.1))
              (net 5 \"ISO\")
              (footprint \"A\" (layer \"F.Cu\") (at 10 10 0)
                (fp_text reference \"R1\" (at 0 -2))
                (pad \"1\" smd rect (at 0 0) (size 1.5 1.5) (layers \"F.Cu\") (net 5 \"ISO\")))
              (footprint \"B\" (layer \"F.Cu\") (at 30 30 0)
                (fp_text reference \"R2\" (at 0 -2))
                (pad \"1\" smd rect (at 0 0) (size 1.5 1.5) (layers \"F.Cu\") (net 5 \"ISO\"))))",
        )
        .unwrap();
        let nl = Netlist::from_board(&b);
        let mut cfg = RouterConfig::default();
        cfg.max_expansions = 3; // starve: force honest failure
        cfg.rip_up_passes = 0;
        let router = Router::new(cfg);
        let result = router.route(&b, &nl);
        assert_eq!(result.unrouted.len(), 1, "starved search must report honestly");
        assert_eq!(result.unrouted[0].from, "R1:1");
        assert!(!result.all_routed());
    }

    #[test]
    fn summary_line_matches_skill_format() {
        let r = RouteResult { routed_edges: 1, total_edges: 1, ..Default::default() };
        assert!(r.summary_line().starts_with("routed 1 of 1"), "got: {}", r.summary_line());
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

    /// A 6-layer board: GND is a plane; SMD GND pads get fanout vias,
    /// through-hole GND pads need nothing; signals route on 3 signal layers.
    fn board_6layer() -> Board {
        Board::from_kicad_pcb(
            "(kicad_pcb (version 20221018)
              (layers (0 \"F.Cu\" signal) (1 \"In1.Cu\" signal) (2 \"In2.Cu\" signal) (3 \"In3.Cu\" signal) (4 \"In4.Cu\" signal) (31 \"B.Cu\" signal))
              (gr_line (start 0 0) (end 30 0) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 30 0) (end 30 20) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 30 20) (end 0 20) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 0 20) (end 0 0) (layer \"Edge.Cuts\") (width 0.1))
              (net 1 \"GND\")
              (net 2 \"SIG\")
              (net 3 \"SIG2\")
              (footprint \"C\" (layer \"F.Cu\") (at 10 10 0)
                (fp_text reference \"C1\" (at 0 -2))
                (pad \"1\" smd rect (at -0.5 0) (size 0.6 0.9) (layers \"F.Cu\") (net 2 \"SIG\"))
                (pad \"2\" smd rect (at 0.5 0) (size 0.6 0.9) (layers \"F.Cu\") (net 1 \"GND\")))
              (footprint \"C\" (layer \"F.Cu\") (at 20 10 0)
                (fp_text reference \"C2\" (at 0 -2))
                (pad \"1\" smd rect (at -0.5 0) (size 0.6 0.9) (layers \"F.Cu\") (net 2 \"SIG\"))
                (pad \"2\" smd rect (at 0.5 0) (size 0.6 0.9) (layers \"F.Cu\") (net 3 \"SIG2\")))
              (footprint \"J\" (layer \"F.Cu\") (at 15 5 0)
                (fp_text reference \"J1\" (at 0 -2))
                (pad \"1\" thru_hole circle (at 0 0) (size 1.7 1.7) (drill 1) (layers \"*.Cu\" \"*.Mask\") (net 1 \"GND\"))
                (pad \"2\" thru_hole circle (at 2.54 0) (size 1.7 1.7) (drill 1) (layers \"*.Cu\" \"*.Mask\") (net 3 \"SIG2\"))))",
        )
        .unwrap()
    }

    #[test]
    fn six_layer_planes_fanout_and_signals_route() {
        let b = board_6layer();
        assert_eq!(b.layer_count, 6);
        let nl = Netlist::from_board(&b);
        let rules = crate::rules::DesignRules::for_layers(6);
        let cfg = RouterConfig::from_rules(
            &rules,
            vec!["F.Cu".into(), "In2.Cu".into(), "B.Cu".into()],
            vec!["GND".into()],
            vec![],
        );
        let router = Router::new(cfg);
        let r = router.route(&b, &nl);
        assert!(r.unrouted.is_empty(), "all routed: {} {:?}", r.summary_line(), r.unrouted);
        // GND: C1:2 (SMD) fans out to a via; J1:1 (THT) reaches the plane directly
        assert_eq!(r.fanouts, 2, "{}", r.summary_line());
        let gnd_vias = r.vias.iter().filter(|v| v.net.1 == "GND").count();
        assert_eq!(gnd_vias, 1, "one fanout via for the SMD GND pad");
        // No GND pad-to-pad trace exists (planes carry GND)
        let gnd_tracks: Vec<&TrackSegment> = r.segments.iter().filter(|s| s.net.1 == "GND").collect();
        for s in &gnd_tracks {
            let len = ((s.x2 - s.x1).powi(2) + (s.y2 - s.y1).powi(2)).sqrt();
            assert!(len <= 3.0 + 1e-6, "GND stub longer than fanout radius: {}", len);
        }
        // Signals: SIG (2 pads) and SIG2 (2 pads) routed → 2 edges
        assert_eq!(r.total_edges, 2);
        assert_eq!(r.routed_edges, 2);
        for s in &r.segments {
            assert!(["F.Cu", "In2.Cu", "B.Cu"].contains(&s.layer.as_str()), "trace on a plane layer: {}", s.layer);
            assert!((s.width - 0.127).abs() < 1e-9, "6-layer default width");
        }
    }

    #[test]
    fn different_nets_keep_clearance_on_fine_grid() {
        // Two parallel connections 0.6mm apart on the 0.127 grid: the halo
        // must keep their copper ≥ 0.127mm apart edge to edge.
        let b = Board::from_kicad_pcb(
            "(kicad_pcb (version 20221018)
              (layers (0 \"F.Cu\" signal) (1 \"In1.Cu\" signal) (2 \"In2.Cu\" signal) (3 \"In3.Cu\" signal) (4 \"In4.Cu\" signal) (31 \"B.Cu\" signal))
              (gr_line (start 0 0) (end 20 0) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 20 0) (end 20 10) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 20 10) (end 0 10) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 0 10) (end 0 0) (layer \"Edge.Cuts\") (width 0.1))
              (net 1 \"A\") (net 2 \"B\")
              (footprint \"P\" (layer \"F.Cu\") (at 5 5 0)
                (fp_text reference \"U1\" (at 0 -2))
                (pad \"1\" smd rect (at 0 -0.3) (size 0.3 0.3) (layers \"F.Cu\") (net 1 \"A\"))
                (pad \"2\" smd rect (at 0 0.3) (size 0.3 0.3) (layers \"F.Cu\") (net 2 \"B\")))
              (footprint \"P\" (layer \"F.Cu\") (at 15 5 0)
                (fp_text reference \"U2\" (at 0 -2))
                (pad \"1\" smd rect (at 0 -0.3) (size 0.3 0.3) (layers \"F.Cu\") (net 1 \"A\"))
                (pad \"2\" smd rect (at 0 0.3) (size 0.3 0.3) (layers \"F.Cu\") (net 2 \"B\"))))",
        )
        .unwrap();
        let nl = Netlist::from_board(&b);
        let rules = crate::rules::DesignRules::for_layers(6);
        let cfg = RouterConfig::from_rules(&rules, vec!["F.Cu".into(), "In2.Cu".into(), "B.Cu".into()], vec![], vec![]);
        let r = Router::new(cfg).route(&b, &nl);
        assert!(r.unrouted.is_empty(), "{}", r.summary_line());
        // Check every pair of different-net segments on the same layer
        for s in &r.segments {
            for t in &r.segments {
                if s.net.0 == t.net.0 || s.layer != t.layer {
                    continue;
                }
                let d = crate::drc::segment_segment_distance(
                    (s.x1, s.y1), (s.x2, s.y2), (t.x1, t.y1), (t.x2, t.y2),
                );
                let edge = d - s.width / 2.0 - t.width / 2.0;
                assert!(edge >= 0.127 - 1e-6, "clearance {:.4} between {} and {}", edge, s.net.1, t.net.1);
            }
        }
    }
}
