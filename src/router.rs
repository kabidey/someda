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

mod pairs;
pub use pairs::{PairConfig, PairReport};

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
    /// Fine-pitch signal pads given a pre-assigned escape via
    pub escapes: usize,
    /// Escape vias no route used (left in place; harmless)
    pub escapes_unused: usize,
    /// One report per routed/attempted differential-pair trunk.
    pub pairs: Vec<PairReport>,
    /// Breakout zones where the crosstalk spacing rule is relaxed (board
    /// mm, per layer name or "*"): the SI audit uses the same zones.
    pub exempt_rects: Vec<ExemptRect>,
}

/// A rectangle (board mm) where parallel runs closer than the crosstalk
/// spacing are tolerated: pad breakouts and fine-pitch escape bands.
#[derive(Debug, Clone)]
pub struct ExemptRect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    /// Copper layer name, or "*" for every layer.
    pub layer: String,
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
        if self.escapes > 0 {
            s.push_str(&format!(", {} escape vias ({} unused)", self.escapes, self.escapes_unused));
        }
        s
    }

    pub fn all_routed(&self) -> bool {
        self.unrouted.is_empty() && (self.routed_edges > 0 || self.fanouts > 0)
    }
}

/// A plane's extent on one layer (board-relative mm) with its priority.
#[derive(Debug, Clone)]
pub struct PlaneRegion {
    pub net: String,
    pub layer: String,
    /// None = the whole board
    pub region: Option<(f64, f64, f64, f64)>,
    pub priority: i32,
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
    /// Copper to drilled-hole edge clearance (mm).
    pub hole_clearance: f64,
    /// Board edge clearance (mm).
    pub edge_clearance: f64,
    /// Max A* expansions per connection before honest give-up.
    pub max_expansions: usize,
    /// Copper layers traces may use, outer to inner order.
    pub signal_layers: Vec<String>,
    /// Nets served by copper planes: fanned out to vias, never routed.
    pub plane_nets: Vec<String>,
    /// Where each plane net's copper actually is (split planes).
    pub plane_regions: Vec<PlaneRegion>,
    /// Nets routed at `power_trace_width`.
    pub power_nets: Vec<String>,
    /// Longest fanout stub from a plane-net pad to its via (mm).
    pub fanout_max_mm: f64,
    /// Rip-up-and-reroute passes after the first pass.
    pub rip_up_passes: usize,
    /// Cost of a layer change, in mm of trace.
    pub via_cost_mm: f64,
    /// Escape lane reserved outward from every fine-pitch pad (mm).
    pub escape_mm: f64,
    /// Soft-cost band beyond the fine-pitch pad tips (mm).
    pub escape_band_mm: f64,
    /// Extra cost per sideways step inside an escape band (grid units).
    pub band_side_cost: f64,
    /// Differential pairs to route as coupled corridors.
    pub pairs: Vec<PairConfig>,
    /// Layers a pair corridor (and any impedance-controlled net) may use,
    /// in preference order: layers over a solid reference plane.
    pub pair_layers: Vec<String>,
    /// Nets confined to `pair_layers` (impedance-controlled single-ended).
    pub restricted_nets: Vec<String>,
    /// Required edge-to-edge gap to OTHER nets' traces (crosstalk 3W/5W
    /// rule), per net name; absent = the design clearance.
    pub net_gap: std::collections::HashMap<String, f64>,
    /// Distance from any pad within which the crosstalk rule is relaxed
    /// (breakouts cannot honour 3W).
    pub xt_exempt_pad_mm: f64,
    /// Depth of the breakout zone beyond fine-pitch pad tips in which the
    /// crosstalk rule is relaxed (pairs leave a dense IC side by side and
    /// spread out past it).
    pub xt_exempt_band_mm: f64,
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
            hole_clearance: 0.25,
            edge_clearance: 0.5,
            max_expansions: 300_000,
            signal_layers: vec!["F.Cu".into(), "B.Cu".into()],
            plane_nets: Vec::new(),
            plane_regions: Vec::new(),
            power_nets: Vec::new(),
            fanout_max_mm: 3.0,
            rip_up_passes: 2,
            via_cost_mm: 2.0,
            escape_mm: 1.0,
            escape_band_mm: 3.2,
            band_side_cost: 4.0,
            pairs: Vec::new(),
            pair_layers: Vec::new(),
            restricted_nets: Vec::new(),
            net_gap: std::collections::HashMap::new(),
            xt_exempt_pad_mm: 1.0,
            xt_exempt_band_mm: 6.0,
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
        let cells_per_layer_estimate = 4_000_000usize;
        RouterConfig {
            grid: rules.grid_mm,
            trace_width: rules.trace_width_mm,
            power_trace_width: rules.power_trace_width_mm,
            clearance: rules.clearance_mm,
            via_size: rules.via_diameter_mm,
            via_drill: rules.via_drill_mm,
            hole_to_hole: rules.hole_to_hole_mm,
            hole_clearance: rules.hole_clearance_mm,
            edge_clearance: rules.edge_clearance_mm,
            max_expansions: cells_per_layer_estimate * signal_layers.len().max(1),
            signal_layers,
            plane_nets,
            plane_regions: Vec::new(),
            power_nets,
            fanout_max_mm: 3.0,
            rip_up_passes: 3,
            via_cost_mm: 2.0,
            escape_mm: 1.0,
            escape_band_mm: 3.2,
            band_side_cost: 4.0,
            pairs: Vec::new(),
            pair_layers: Vec::new(),
            restricted_nets: Vec::new(),
            net_gap: std::collections::HashMap::new(),
            xt_exempt_pad_mm: 1.0,
            xt_exempt_band_mm: 6.0,
        }
    }
}

const FREE: u32 = 0;
const BLOCK: u32 = u32::MAX;

/// A grid cell coordinate (x, y, signal-layer index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Cell {
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
    /// circle / oval pads: copper is the inscribed ellipse, not the box
    round: bool,
    drill: f64,
    /// signal-layer indexes the pad has copper on
    layers: Vec<u8>,
    fine_pitch: bool,
    /// footprint origin (board-relative) — the pad escapes away from it
    fp_cx: f64,
    fp_cy: f64,
    /// A pair-corridor terminal, not real copper of a footprint.
    virtual_pad: bool,
}

/// Dense occupancy over (layer, y, x).
#[derive(Clone)]
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
    /// Per cell/layer extra step cost (×0.25 grid units): the escape bands
    /// around fine-pitch parts are expensive to run along, cheap to cross.
    cost: Vec<u8>,
    /// 2D: net tag owning an escape lane through this cell (0 = none).
    /// No via of another net may sit within its halo of a lane.
    lane: Vec<u32>,
    /// Per cell/layer: escape band orientation — 1 = lanes run along x
    /// (sideways = y moves are penalized), 2 = lanes along y, 0 = none.
    band_axis: Vec<u8>,
    /// Per cell/layer: crosstalk ownership — cells inside an aggressor
    /// net's 3W/5W halo (FREE, net+1, BLOCK). Pair mates share.
    xt: Vec<u32>,
    /// Per cell/layer: trace copper of some net within the 3W (near3) or
    /// 5W (near5) radius — what an aggressor arriving later must avoid.
    near3: Vec<u32>,
    near5: Vec<u32>,
    /// Per cell/layer: breakout zone (pad proximity, escape band or lane)
    /// where the crosstalk rule is relaxed.
    exempt: Vec<bool>,
    /// Per cell/layer: inside a pair corridor's halo (the pair's P tag) —
    /// tails of other families stay out (they join only through their own
    /// terminal approach cells).
    corridor: Vec<u32>,
    /// Per-net-tag facts the stamping/search code needs.
    tags: TagMaps,
}

/// Per net tag (net id + 1): pair mate tag (0 = none), required crosstalk
/// edge gap, whether the net is confined to the clean layers; plus which
/// signal layers are clean.
#[derive(Clone, Default)]
struct TagMaps {
    mate_of: Vec<u32>,
    gap_of: Vec<f64>,
    restricted: Vec<bool>,
    layer_allowed: Vec<bool>,
    /// Crosstalk family per tag (0 = none): a pair's two nets, and pairs
    /// chained through a shared component (SoC → ESD → connector carry the
    /// same signal), may sit inside each other's halos.
    family: Vec<u32>,
}
impl TagMaps {
    #[inline]
    fn mate(&self, tag: u32) -> u32 {
        self.mate_of.get(tag as usize).copied().unwrap_or(0)
    }
    /// Same net, or the same crosstalk family.
    #[inline]
    fn friendly(&self, a: u32, b: u32) -> bool {
        if a == b {
            return true;
        }
        let fa = self.family.get(a as usize).copied().unwrap_or(0);
        fa != 0 && fa == self.family.get(b as usize).copied().unwrap_or(0)
    }
    #[inline]
    fn gap(&self, tag: u32, default: f64) -> f64 {
        self.gap_of.get(tag as usize).copied().unwrap_or(default)
    }
    #[inline]
    fn is_restricted(&self, tag: u32) -> bool {
        self.restricted.get(tag as usize).copied().unwrap_or(false)
    }
    #[inline]
    fn layer_ok(&self, layer: u8) -> bool {
        self.layer_allowed.get(layer as usize).copied().unwrap_or(true)
    }
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
            cost: vec![0; n],
            lane: vec![0; w * h],
            band_axis: vec![0; n],
            xt: vec![FREE; n],
            near3: vec![FREE; n],
            near5: vec![FREE; n],
            exempt: vec![false; n],
            corridor: vec![0; n],
            tags: TagMaps::default(),
        }
    }
    /// Claim into a side map with the tag's crosstalk family treated as self.
    #[inline]
    fn claim_fam(map: &mut [u32], i: usize, tag: u32, family: &[u32]) {
        let cur = map[i];
        if cur == FREE {
            map[i] = tag;
        } else if cur != tag {
            let fa = family.get(tag as usize).copied().unwrap_or(0);
            let fb = family.get(cur as usize).copied().unwrap_or(0);
            if fa == 0 || fa != fb {
                map[i] = BLOCK;
            }
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
    /// Claim a clearance-halo cell: like `claim`, but a cell reserved as
    /// another net's escape lane keeps its owner (a BLOCKed lane would
    /// strand that pin — live-verified with 3-cell pair halos).
    #[inline]
    fn claim_halo(&mut self, x: i32, y: i32, l: u8, net_tag: u32) {
        if let Some(i) = self.idx(x, y, l) {
            let lane = self.lane[y as usize * self.w + x as usize];
            // only a lane materialized on THIS layer (the pad's layer)
            if lane != 0 && lane != net_tag && self.owner[i] == lane {
                return;
            }
        }
        self.claim(x, y, l, net_tag);
    }
    /// Is another net's escape lane (materialized on this layer) within
    /// Chebyshev radius `r` of (x, y)? Copper may not hug a lane: its halo
    /// never claims lane cells, so the search has to keep the distance.
    fn lane_adjacent(&self, x: i32, y: i32, l: u8, net_tag: u32, r: i32) -> bool {
        self.lane_adjacent_except(x, y, l, &[net_tag], r)
    }
    /// Same, with several tags treated as own (a pair's two nets).
    fn lane_adjacent_except(&self, x: i32, y: i32, l: u8, own: &[u32], r: i32) -> bool {
        for dy in -r..=r {
            for dx in -r..=r {
                if let Some(i) = self.idx(x + dx, y + dy, l) {
                    let lane = self.lane[(y + dy) as usize * self.w + (x + dx) as usize];
                    if lane != 0 && !own.contains(&lane) && self.owner[i] == lane {
                        return true;
                    }
                }
            }
        }
        false
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
#[derive(Clone)]
struct Dsu {
    parent: Vec<u32>,
}
impl Dsu {
    fn new(n: usize) -> Dsu {
        Dsu { parent: (0..n as u32).collect() }
    }
    /// Extend to `n` elements (new singletons).
    fn grow(&mut self, n: usize) {
        while self.parent.len() < n {
            self.parent.push(self.parent.len() as u32);
        }
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
        if let Some(pc) = self.config.pairs.iter().find(|p| p.p == net || p.n == net) {
            return pc.width_mm;
        }
        if let Some(z0) = self.impedance_targets.get(net) {
            let w = self.stackup.width_for_z0(*z0);
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

        // Pads as the router sees them (pair terminals are appended later)
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
                    round: matches!(pad.shape.as_str(), "circle" | "oval"),
                    drill: pad.drill.max(pad.drill_oval.map(|(a, b)| a.max(b)).unwrap_or(0.0)),
                    layers,
                    fine_pitch: fine,
                    fp_cx: fp.x - bx0,
                    fp_cy: fp.y - by0,
                    virtual_pad: false,
                });
            }
        }
        // ─── Grid, static copper, escapes, plane fanouts, PAIRS ───
        // Per-net-tag facts: pair mates, crosstalk gaps, layer restriction
        let max_tag = board.nets.iter().map(|(i, _)| *i).max().unwrap_or(0) as usize + 2;
        let mut tags = TagMaps {
            mate_of: vec![0; max_tag],
            gap_of: vec![self.config.clearance; max_tag],
            restricted: vec![false; max_tag],
            layer_allowed: self.config.signal_layers.iter().map(|l| self.config.pair_layers.is_empty() || self.config.pair_layers.contains(l)).collect(),
            family: vec![0; max_tag],
        };
        // Crosstalk families: each pair, merged with pairs that share a
        // component with it (chained through an ESD array or series parts)
        {
            let refs_of = |n: usize| -> Vec<String> {
                let mut v: Vec<String> = pads.iter().filter(|p| p.net == Some(n)).map(|p| p.reference.clone()).collect();
                v.sort();
                v.dedup();
                v
            };
            let pair_nets: Vec<Option<(usize, usize)>> = self.config.pairs.iter().map(|pc| match (net_id(&pc.p), net_id(&pc.n)) { (Some(a), Some(b)) => Some((a, b)), _ => None }).collect();
            // Only a small pass-through part (an ESD array, series
            // resistors/capacitors: ≤ 8 pads) chains two pairs into one
            // signal; the SoC and the connectors are shared by every pair
            // and must NOT merge them (live-verified: merged MIPI pairs let
            // bumps short into each other).
            let small_part = |r: &str| pads.iter().filter(|p| p.reference == r && !p.virtual_pad).count() <= 8;
            let pair_refs: Vec<Vec<String>> = pair_nets.iter().map(|pn| pn.map(|(a, b)| { let mut r = refs_of(a); r.extend(refs_of(b)); r.sort(); r.dedup(); r.retain(|x| small_part(x)); r }).unwrap_or_default()).collect();
            let mut fam: Vec<usize> = (0..pair_nets.len()).collect();
            fn root(f: &mut Vec<usize>, i: usize) -> usize { let mut i = i; while f[i] != i { f[i] = f[f[i]]; i = f[i]; } i }
            for i in 0..pair_nets.len() {
                for j in (i + 1)..pair_nets.len() {
                    if pair_refs[i].iter().any(|r| pair_refs[j].contains(r)) {
                        let (ri, rj) = (root(&mut fam, i), root(&mut fam, j));
                        if ri != rj { fam[rj] = ri; }
                    }
                }
            }
            for (i, pn) in pair_nets.iter().enumerate() {
                if let Some((a, b)) = pn {
                    let fid = root(&mut fam, i) as u32 + 1;
                    tags.family[a + 1] = fid;
                    tags.family[b + 1] = fid;
                }
            }
        }
        let mut pair_net_ids: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for pc in &self.config.pairs {
            if let (Some(a), Some(b)) = (net_id(&pc.p), net_id(&pc.n)) {
                tags.mate_of[a + 1] = b as u32 + 1;
                tags.mate_of[b + 1] = a as u32 + 1;
                tags.restricted[a + 1] = true;
                tags.restricted[b + 1] = true;
                pair_net_ids.insert(a);
                pair_net_ids.insert(b);
            }
        }
        for (name, gap) in &self.config.net_gap {
            if let Some(n) = net_id(name) {
                tags.gap_of[n + 1] = gap.max(self.config.clearance);
            }
        }
        for name in &self.config.restricted_nets {
            if let Some(n) = net_id(name) {
                tags.restricted[n + 1] = true;
            }
        }
        let gap_of: Vec<f64> = (0..max_tag).map(|i| if i == 0 { self.config.clearance } else { tags.gap_of[i] }).collect();
        let gap_by_net: Vec<f64> = (0..max_tag).map(|n| tags.gap_of.get(n + 1).copied().unwrap_or(self.config.clearance)).collect();
        let _ = &gap_of;
        let mut state = RouteState {
            routed: Vec::new(),
            fanouts: Vec::new(),
            escapes: Vec::new(),
            failed: Vec::new(),
            fanout_failed: Vec::new(),
            pairs: Vec::new(),
            pair_reports: Vec::new(),
        };
        let mut grid = Grid::new(w, h, nlayers, pitch);
        grid.tags = tags.clone();
        let mut dsu = Dsu::new(pads.len());
        self.stamp_static(&mut grid, &pads, bx1 - bx0, by1 - by0);
        self.stamp_keepouts(&mut grid, board, bx0, by0);
        // Pairs first — the most constrained copper on the board leaves the
        // fine-pitch bands before escape vias and plane fanouts fill them.
        {
            let refs_of_net = |n: usize| -> Vec<String> {
                let mut v: Vec<String> = pads.iter().filter(|p| p.net == Some(n) && !p.virtual_pad).map(|p| p.reference.clone()).collect();
                v.sort();
                v.dedup();
                v
            };
            let refs: Vec<Vec<String>> = (0..max_tag).map(|n| refs_of_net(n)).collect();
            let refs_fn = |n: usize| -> Vec<String> { refs.get(n).cloned().unwrap_or_default() };
            self.route_pairs(&mut grid, &mut pads, &mut dsu, &mut state, &net_id, &gap_by_net, &refs_fn);
        }
        self.escape_vias(&mut grid, &pads, &plane_ids, &pair_net_ids, &mut state);
        self.fanout_planes(&mut grid, &pads, &plane_ids, &mut state, &net_name);
        let exempt_rects: Vec<ExemptRect> = self
            .breakout_rects(&pads)
            .into_iter()
            .map(|(x0, y0, x1, y1, l)| ExemptRect {
                x0: x0 + bx0,
                y0: y0 + by0,
                x1: x1 + bx0,
                y1: y1 + by0,
                layer: l.map(|i| self.config.signal_layers[i as usize].clone()).unwrap_or_else(|| "*".into()),
            })
            .collect();
        let pads = pads; // frozen from here on

        // Same-numbered pads (e.g. a switch's two "1" pads) are distinct
        // copper: resolve by position, not only by name.
        let pad_lookup = |reference: &str, number: &str, x: f64, y: f64| -> Option<u32> {
            pads.iter()
                .filter(|p| p.reference == reference && p.number == number)
                .min_by(|p, q| {
                    let dp = (p.x - (x - bx0)).powi(2) + (p.y - (y - by0)).powi(2);
                    let dq = (q.x - (x - bx0)).powi(2) + (q.y - (y - by0)).powi(2);
                    dp.partial_cmp(&dq).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|p| p.id)
        };

        // Ratsnest edges to route: everything not on a plane net
        let mut fallback_edges: Vec<RatsnestEdge> = Vec::new();
        let mut edges: Vec<(usize, &RatsnestEdge, u32, u32, usize)> = Vec::new();
        for (i, e) in netlist.edges.iter().enumerate() {
            let nid = match net_id(&e.net) {
                Some(n) => n,
                None => continue,
            };
            if plane_ids.contains(&nid) {
                continue;
            }
            let (Some(a), Some(b)) = (
                pad_lookup(&e.from_ref, &e.from_pin, e.from_x, e.from_y),
                pad_lookup(&e.to_ref, &e.to_pin, e.to_x, e.to_y),
            ) else {
                continue;
            };
            if a == b {
                continue;
            }
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

        // Plane pads that found no via site are routed as ordinary
        // connections to the nearest member of their net that did.
        for &pid in &state.fanout_failed.clone() {
            let p = &pads[pid as usize];
            let Some(net) = p.net else { continue };
            let target = pads
                .iter()
                .filter(|q| q.net == Some(net) && q.id != pid && state.fanouts.iter().any(|f| f.pad == q.id))
                .min_by(|a, b| {
                    let da = (a.x - p.x).powi(2) + (a.y - p.y).powi(2);
                    let db = (b.x - p.x).powi(2) + (b.y - p.y).powi(2);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                });
            if let Some(q) = target {
                fallback_edges.push(RatsnestEdge {
                    net: net_name(net),
                    from_ref: p.reference.clone(),
                    from_pin: p.number.clone(),
                    from_x: p.x + bx0,
                    from_y: p.y + by0,
                    to_ref: q.reference.clone(),
                    to_pin: q.number.clone(),
                    to_x: q.x + bx0,
                    to_y: q.y + by0,
                    manhattan_mm: (p.x - q.x).abs() + (p.y - q.y).abs(),
                });
                state.fanout_failed.retain(|&f| f != pid);
            }
        }
        let fallback_start = edges.len();
        for (i, e) in fallback_edges.iter().enumerate() {
            let a = pad_lookup(&e.from_ref, &e.from_pin, e.from_x, e.from_y);
            let b = pad_lookup(&e.to_ref, &e.to_pin, e.to_x, e.to_y);
            if let (Some(a), Some(b), Some(nid)) = (a, b, net_id(&e.net)) {
                edges.push((netlist.edges.len() + i, e, a, b, nid));
            }
        }
        let order: Vec<usize> = { let mut o = order; o.extend(fallback_start..edges.len()); o };
        let edge_width = |e: &RatsnestEdge, a: u32, b: u32| -> f64 {
            // NECK-DOWN: a wide (power / impedance) trace cannot land on a
            // fine-pitch pad without grazing its neighbour — route that
            // connection at the default width (a stub ≪ λ/4 electrically).
            if pads[a as usize].fine_pitch || pads[b as usize].fine_pitch {
                self.config.trace_width
            } else {
                self.width_for_net(&e.net)
            }
        };
        for &k in &order {
            let (ei, e, a, b, nid) = edges[k];
            let width = edge_width(e, a, b);
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
                    .filter(|(_, r)| r.net != nid && !pair_net_ids.contains(&r.net))
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
                // Rip every routed edge of a victim's NET: later edges of
                // that net may end on the victim's copper (route-to-net),
                // and would dangle without it (live-verified by KiCad).
                let victim_nets: Vec<usize> = victims.iter().map(|v| state.routed[v.0].net).collect();
                let victim_indexes: Vec<usize> = state
                    .routed
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| victim_nets.contains(&r.net))
                    .map(|(i, _)| i)
                    .collect();
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
                g2.tags = tags.clone();
                let mut dsu2 = Dsu::new(pads.len());
                self.stamp_static(&mut g2, &pads, bx1 - bx0, by1 - by0);
                self.stamp_keepouts(&mut g2, board, bx0, by0);
                self.replay_pairs(&mut g2, &mut dsu2, &pads, &state.pairs, &gap_by_net);
                self.replay_fanouts(&mut g2, &pads, &state.escapes);
                for e in &state.escapes {
                    for c in &e.cells {
                        if c.x >= 0 && c.y >= 0 && (c.x as usize) < g2.w && (c.y as usize) < g2.h {
                            g2.lane[c.y as usize * g2.w + c.x as usize] = e.net as u32 + 1;
                        }
                    }
                }
                self.replay_fanouts(&mut g2, &pads, &state.fanouts);
                for r in &trial_routed {
                    self.stamp_path(&mut g2, &r.cells, r.net, r.width, r.from_pad);
                    let eb = edges.iter().find(|x| x.0 == r.edge_index).map(|x| x.3).unwrap_or(r.from_pad);
                    dsu2.union(r.from_pad, eb);
                }
                let mut trial_failed: Vec<usize> = state.failed.iter().cloned().filter(|&f| f != k).collect();
                // The failed edge first
                let width = edge_width(e, a, b);
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
                    let wv = edge_width(ee, aa, bb);
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
        // ─── Final retries: later rip-ups reshape the board; every edge
        // still failed gets plain attempts until no progress ───
        loop {
            let before = state.failed.len();
            let failed_now = state.failed.clone();
            for k in failed_now {
                let (ei, e, a, b, nid) = edges[k];
                let width = edge_width(e, a, b);
                if let Some(cells) = self.route_edge(&mut grid, &pads, &mut dsu, a, b, nid, width) {
                    self.stamp_path(&mut grid, &cells, nid, width, a);
                    dsu.union(a, b);
                    state.routed.push(RoutedEdge { edge_index: ei, cells, width, net: nid, from_pad: a });
                    state.failed.retain(|&f| f != k);
                }
            }
            if state.failed.len() >= before {
                break;
            }
        }
        let _ = (&grid, &dsu);

        // ─── Verify path ends (debug aid: SOMEDA_ROUTE_DEBUG) ───
        if std::env::var("SOMEDA_ROUTE_DEBUG").is_ok() {
            let verbose = std::env::var("SOMEDA_ROUTE_DEBUG").map(|v| v == "2").unwrap_or(false);
            for r in &state.routed {
                let Some(ed) = edges.iter().find(|x| x.0 == r.edge_index) else { continue };
                if verbose {
                    let path: Vec<String> = r.cells.iter().map(|c| format!("({:.3},{:.3},{})", c.x as f64 * pitch + bx0, c.y as f64 * pitch + by0, self.config.signal_layers[c.layer as usize])).collect();
                    eprintln!("route-debug: PATH {} {}:{} → {}:{} w={} {}", ed.1.net, ed.1.from_ref, ed.1.from_pin, ed.1.to_ref, ed.1.to_pin, r.width, path.join(" "));
                }
                let pb = &pads[ed.3 as usize];
                if let Some(last) = r.cells.last() {
                    let (lx, ly) = (last.x as f64 * pitch, last.y as f64 * pitch);
                    let inside = (lx - pb.x).abs() <= pb.hx + 1e-6 && (ly - pb.y).abs() <= pb.hy + 1e-6;
                    let cp = grid.idx(last.x, last.y, last.layer).map(|i| grid.copper_pad[i]).unwrap_or(0);
                    eprintln!(
                        "route-debug: {} {}:{} → {}:{} cells={} end=({:.3},{:.3},{}) inside_target={} end_copper_pad={}",
                        ed.1.net, ed.1.from_ref, ed.1.from_pin, ed.1.to_ref, ed.1.to_pin, r.cells.len(), lx + bx0, ly + by0,
                        self.config.signal_layers[last.layer as usize], inside,
                        if cp == 0 { "none".to_string() } else { format!("{}:{}", pads[(cp - 1) as usize].reference, pads[(cp - 1) as usize].number) }
                    );
                }
            }
            if verbose {
                for f in &state.fanouts {
                    let pd = &pads[f.pad as usize];
                    let path: Vec<String> = f.cells.iter().map(|c| format!("({:.3},{:.3},{})", c.x as f64 * pitch + bx0, c.y as f64 * pitch + by0, self.config.signal_layers[c.layer as usize])).collect();
                    eprintln!("route-debug: FANOUT {}:{} via={:?} {}", pd.reference, pd.number, f.via.map(|(x, y)| (x as f64 * pitch + bx0, y as f64 * pitch + by0)), path.join(" "));
                }
            }
            for &k in &state.failed {
                let (_, e, a, b, _) = edges[k];
                let (pa, pb) = (&pads[a as usize], &pads[b as usize]);
                let sa = self.pad_cells(&grid, pa, pa.layers[0]);
                let sb = self.pad_cells(&grid, pb, pb.layers[0]);
                let free_around = |c: &Cell| -> usize {
                    [(1, 0), (-1, 0), (0, 1), (0, -1)].iter().filter(|(dx, dy)| {
                        let o = grid.owner_at(Cell { x: c.x + dx, y: c.y + dy, layer: c.layer });
                        o == FREE || o == e.net.len() as u32 + 1 || o == (edges[k].4 as u32 + 1)
                    }).count()
                };
                if verbose {
                    for (label, pd) in [("from", pa), ("to", pb)] {
                        let r = (3.5 / pitch) as i32;
                        let (gx, gy) = ((pd.x / pitch).round() as i32, (pd.y / pitch).round() as i32);
                        eprintln!("route-debug: occupancy around {} {}:{} (net tag {}; '.' free, '#' block, 'o' own net, 'x' other net, 'O' own copper, 'X' other copper):", label, pd.reference, pd.number, edges[k].4 + 1);
                        for l in 0..grid.layers as u8 {
                            eprintln!("  layer {}:", self.config.signal_layers[l as usize]);
                            for y in (gy - r)..=(gy + r) {
                                let mut line = String::from("    ");
                                for x in (gx - r)..=(gx + r) {
                                    let c = Cell { x, y, layer: l };
                                    let o = grid.owner_at(c);
                                    let cp = grid.idx(x, y, l).map(|i| grid.copper_pad[i]).unwrap_or(0);
                                    let tag = edges[k].4 as u32 + 1;
                                    line.push(if o == BLOCK { '#' } else if o == FREE { '.' } else if o == tag { if cp != 0 { 'O' } else { 'o' } } else if cp != 0 { 'X' } else { 'x' });
                                }
                                eprintln!("{}", line);
                            }
                        }
                    }
                }
                eprintln!(
                    "route-debug: FAILED {} {}:{} ({:.2},{:.2} hx{:.2} hy{:.2} cells{} free-neighbours{:?}) → {}:{} ({:.2},{:.2} hx{:.2} hy{:.2} cells{} free-neighbours{:?})",
                    e.net, pa.reference, pa.number, pa.x + bx0, pa.y + by0, pa.hx, pa.hy, sa.len(), sa.iter().map(|c| free_around(c)).collect::<Vec<_>>(),
                    pb.reference, pb.number, pb.x + bx0, pb.y + by0, pb.hx, pb.hy, sb.len(), sb.iter().map(|c| free_around(c)).collect::<Vec<_>>()
                );
            }
        }

        // Escape vias that routing did not use stay in place: a via with
        // copper on one layer is electrically harmless (KiCad notes it as
        // via_dangling, a warning). Removing them after the fact broke
        // connections whose routes started on the via body — live-verified.
        let unused_escapes = {
            let mut used: std::collections::HashSet<(i32, i32, u8)> = std::collections::HashSet::new();
            for r in &state.routed {
                for c in &r.cells {
                    used.insert((c.x, c.y, c.layer));
                }
            }
            let vr = self.copper_cells(self.config.via_size) + 1;
            state.escapes.iter().filter(|e| {
                let stub_layer = e.cells.first().map(|c| c.layer).unwrap_or(0);
                !e.via.map(|(vx, vy)| (0..nlayers as u8).any(|l| l != stub_layer && (-vr..=vr).any(|dx| (-vr..=vr).any(|dy| used.contains(&(vx + dx, vy + dy, l)))))).unwrap_or(false)
            }).count()
        };
        if std::env::var("SOMEDA_ROUTE_DEBUG").is_ok() {
            eprintln!("route-debug: {} of {} escape vias unused by routing (kept)", unused_escapes, state.escapes.len());
        }

        // ─── Emit ───
        let mut result = RouteResult::default();
        result.total_edges = edges.len();
        for pr in &state.pairs {
            self.emit_pair(&mut result, pr, &net_name, bx0, by0);
        }
        result.pairs = state.pair_reports.clone();
        result.exempt_rects = exempt_rects;
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
        for e in &state.escapes {
            let name = net_name(e.net);
            self.emit_path(&mut result, &e.cells, (e.net, name.clone()), self.config.trace_width, bx0, by0);
            if let Some((vx, vy)) = e.via {
                result.vias.push(Via {
                    net: (e.net, name),
                    x: vx as f64 * pitch + bx0,
                    y: vy as f64 * pitch + by0,
                    size: self.config.via_size,
                    drill: self.config.via_drill,
                });
            }
            result.escapes += 1;
        }
        result.escapes_unused = unused_escapes;
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

    /// Footprint keep-out areas: tracks blocked on their layers, vias
    /// forbidden where vias are not allowed.
    fn stamp_keepouts(&self, grid: &mut Grid, board: &Board, bx0: f64, by0: f64) {
        let pitch = self.config.grid;
        let tw2 = self.config.trace_width / 2.0;
        for fp in &board.footprints {
            for k in &fp.keepouts {
                let (x0, y0, x1, y1) = k.bbox();
                let (gx0, gx1) = (((x0 - bx0 - tw2) / pitch).floor() as i32, ((x1 - bx0 + tw2) / pitch).ceil() as i32);
                let (gy0, gy1) = (((y0 - by0 - tw2) / pitch).floor() as i32, ((y1 - by0 + tw2) / pitch).ceil() as i32);
                let layers: Vec<u8> = (0..grid.layers as u8)
                    .filter(|&l| k.on_layer(&self.config.signal_layers[l as usize]))
                    .collect();
                for y in gy0..=gy1 {
                    for x in gx0..=gx1 {
                        let (cx, cy) = (x as f64 * pitch + bx0, y as f64 * pitch + by0);
                        // a track centred here with its half width must stay out
                        let hit = k.contains(cx, cy)
                            || k.contains(cx - tw2, cy) || k.contains(cx + tw2, cy)
                            || k.contains(cx, cy - tw2) || k.contains(cx, cy + tw2);
                        if !hit {
                            continue;
                        }
                        if k.tracks {
                            for &l in &layers {
                                grid.force(x, y, l, BLOCK, 0);
                                if let Some(i) = grid.idx(x, y, l) {
                                    grid.copper_pad[i] = 0;
                                }
                            }
                        }
                        if k.vias && x >= 0 && y >= 0 && (x as usize) < grid.w && (y as usize) < grid.h {
                            grid.via_forbidden[y as usize * grid.w + x as usize] = 1;
                        }
                    }
                }
            }
        }
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
        for p in pads.iter().filter(|p| !p.virtual_pad) {
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
            // Drilled holes: copper of OTHER nets must keep hole_clearance
            // from the hole edge on every layer (NPTH: everyone)
            if (p.through || p.npth) && p.drill > 0.0 {
                let hr = p.drill / 2.0 + self.config.hole_clearance + tw2;
                let (gx0, gx1) = (((p.x - hr) / pitch).floor() as i32, ((p.x + hr) / pitch).ceil() as i32);
                let (gy0, gy1) = (((p.y - hr) / pitch).floor() as i32, ((p.y + hr) / pitch).ceil() as i32);
                for l in 0..grid.layers as u8 {
                    for y in gy0..=gy1 {
                        for x in gx0..=gx1 {
                            let (cx, cy) = (x as f64 * pitch, y as f64 * pitch);
                            if ((cx - p.x).powi(2) + (cy - p.y).powi(2)).sqrt() <= hr + 1e-9 {
                                grid.claim(x, y, l, tag);
                            }
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
        for p in pads.iter().filter(|p| !p.virtual_pad) {
            let tag = match p.net {
                Some(n) if !p.npth => n as u32 + 1,
                _ => BLOCK,
            };
            // Copper cells: centres inside the pad rectangle shrunk by half
            // a trace width — a trace ending there lies entirely inside the
            // pad's copper (KiCad's connection-width check wants the full
            // trace width overlapping). Always at least the nearest cell.
            let mut any = false;
            let (ix, iy) = ((p.hx - tw2).max(0.0), (p.hy - tw2).max(0.0));
            let (gx0, gx1) = (((p.x - p.hx) / pitch).floor() as i32, ((p.x + p.hx) / pitch).ceil() as i32);
            let (gy0, gy1) = (((p.y - p.hy) / pitch).floor() as i32, ((p.y + p.hy) / pitch).ceil() as i32);
            for &l in &p.layers {
                for y in gy0..=gy1 {
                    for x in gx0..=gx1 {
                        let (cx, cy) = (x as f64 * pitch, y as f64 * pitch);
                        if Self::inside_pad(p, cx, cy, ix, iy) {
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
        // PAD PROXIMITY COST: cells hugging any pad (0.45mm beyond its
        // halo) cost extra, so traces of other nets do not wrap a pad and
        // box it in before its own connections are routed (live-verified:
        // targets sealed by a ring of earlier traces).
        {
            let ring = 0.45;
            for p in pads {
                if p.npth || p.virtual_pad {
                    continue;
                }
                let (rx, ry) = (p.hx + self.config.clearance + tw2 + ring, p.hy + self.config.clearance + tw2 + ring);
                let (gx0, gx1) = (((p.x - rx) / pitch).floor() as i32, ((p.x + rx) / pitch).ceil() as i32);
                let (gy0, gy1) = (((p.y - ry) / pitch).floor() as i32, ((p.y + ry) / pitch).ceil() as i32);
                for &l in &p.layers {
                    for y in gy0..=gy1 {
                        for x in gx0..=gx1 {
                            let (cx, cy) = (x as f64 * pitch, y as f64 * pitch);
                            if (cx - p.x).abs() <= rx && (cy - p.y).abs() <= ry {
                                if let Some(i) = grid.idx(x, y, l) {
                                    grid.cost[i] = grid.cost[i].max(8);
                                }
                            }
                        }
                    }
                }
            }
        }
        // ESCAPE BAND: the region from the fine-pitch pad tips outward to
        // `escape_band_mm` is expensive to traverse on the pads' layer, so
        // other nets' traces do not run along a pin row and wall the lane
        // ends (live-verified: parallel traces at the lane ends trapped
        // every remaining pin of a side).
        {
            let band = self.config.escape_band_mm;
            for p in pads {
                if !p.fine_pitch || p.npth || p.through || p.virtual_pad {
                    continue;
                }
                let along_x = p.hx >= p.hy;
                let dir = if along_x { (p.x - p.fp_cx).signum() } else { (p.y - p.fp_cy).signum() };
                if dir == 0.0 {
                    continue;
                }
                let half = if along_x { p.hy } else { p.hx };
                let start = if along_x { p.hx } else { p.hy };
                for &l in &p.layers {
                    let steps = (band / pitch).ceil() as i32;
                    for k in 0..=steps {
                        let d = start + k as f64 * pitch;
                        let (cx, cy) = if along_x { (p.x + dir * d, p.y) } else { (p.x, p.y + dir * d) };
                        let (gx, gy) = ((cx / pitch).round() as i32, (cy / pitch).round() as i32);
                        let span = ((half + 0.2) / pitch).ceil() as i32;
                        for t in -span..=span {
                            let (x, y) = if along_x { (gx, gy + t) } else { (gx + t, gy) };
                            if let Some(i) = grid.idx(x, y, l) {
                                grid.band_axis[i] = if along_x { 1 } else { 2 };
                            }
                        }
                    }
                }
            }
        }
        // ESCAPE CORRIDORS: every fine-pitch pad reserves a lane from its
        // outer end away from the footprint centre. Nets routed earlier
        // can no longer run along a pin row and wall the remaining pins in
        // (live-verified: traces hugging an LQFP row trapped U1:5/U1:11).
        // The lane is owned by the pad's net; other pads' copper is never
        // overwritten.
        let lane_len = (self.config.escape_mm / pitch).ceil() as i32;
        for p in pads {
            if !p.fine_pitch || p.npth || p.net.is_none() || p.through || p.virtual_pad {
                continue;
            }
            let tag = p.net.unwrap() as u32 + 1;
            let along_x = p.hx >= p.hy;
            let dir = if along_x { (p.x - p.fp_cx).signum() } else { (p.y - p.fp_cy).signum() };
            if dir == 0.0 {
                continue;
            }
            let (ix, iy) = ((p.hx - tw2).max(0.0), (p.hy - tw2).max(0.0));
            for &l in &p.layers {
                for k in 0..=lane_len {
                    let d = if along_x { p.hx } else { p.hy } + k as f64 * pitch;
                    let (cx, cy) = if along_x { (p.x + dir * d, p.y) } else { (p.x, p.y + dir * d) };
                    let (gx, gy) = ((cx / pitch).round() as i32, (cy / pitch).round() as i32);
                    // the lane is as wide as the pad's copper rows
                    let span = if along_x { (iy / pitch).floor() as i32 } else { (ix / pitch).floor() as i32 };
                    for t in -span..=span {
                        let (x, y) = if along_x { (gx, gy + t) } else { (gx + t, gy) };
                        if let Some(i) = grid.idx(x, y, l) {
                            if grid.copper_pad[i] != 0 && grid.owner[i] != tag {
                                continue;
                            }
                            if grid.owner[i] == FREE {
                                grid.owner[i] = tag;
                            }
                            if grid.owner[i] == tag && x >= 0 && y >= 0 && (x as usize) < grid.w && (y as usize) < grid.h {
                                grid.lane[y as usize * grid.w + x as usize] = tag;
                            }
                        }
                    }
                }
            }
        }
        // BREAKOUT ZONES: the crosstalk spacing rule (3W/5W) is relaxed
        // next to pads and inside fine-pitch escape bands and lanes, where
        // the pin pitch makes it physically impossible. The SI audit uses
        // the same rectangles (RouteResult::exempt_rects).
        for (x0, y0, x1, y1, layer) in self.breakout_rects(pads) {
            let (gx0, gx1) = ((x0 / pitch).floor() as i32, (x1 / pitch).ceil() as i32);
            let (gy0, gy1) = ((y0 / pitch).floor() as i32, (y1 / pitch).ceil() as i32);
            let layers: Vec<u8> = match layer {
                Some(l) => vec![l],
                None => (0..grid.layers as u8).collect(),
            };
            for l in layers {
                for y in gy0..=gy1 {
                    for x in gx0..=gx1 {
                        if let Some(i) = grid.idx(x, y, l) {
                            grid.exempt[i] = true;
                        }
                    }
                }
            }
        }
        for l in 0..grid.layers as u8 {
            for y in 0..grid.h as i32 {
                for x in 0..grid.w as i32 {
                    if grid.lane[y as usize * grid.w + x as usize] != 0 {
                        if let Some(i) = grid.idx(x, y, l) {
                            grid.exempt[i] = true;
                        }
                    }
                }
            }
        }
    }

    /// Debug aid: print the occupancy around a cell on its layer.
    pub(crate) fn dump_occupancy(&self, grid: &Grid, at: Cell, tag: u32, r: i32) {
        eprintln!("route-debug: occupancy around ({:.3},{:.3}) {} (tag {}; '.' free, '#' block, 'o' own, 'x' other, 'O' own copper, 'X' other copper, 'c' corridor halo, 'l' other lane):", at.x as f64 * self.config.grid, at.y as f64 * self.config.grid, self.config.signal_layers[at.layer as usize], tag);
        for y in (at.y - r)..=(at.y + r) {
            let mut line = String::from("    ");
            for x in (at.x - r)..=(at.x + r) {
                let c = Cell { x, y, layer: at.layer };
                let o = grid.owner_at(c);
                let (cp, cor) = grid.idx(x, y, at.layer).map(|i| (grid.copper_pad[i], grid.corridor[i] != 0)).unwrap_or((0, false));
                let lane = if x >= 0 && y >= 0 && (x as usize) < grid.w && (y as usize) < grid.h { grid.lane[y as usize * grid.w + x as usize] } else { 0 };
                line.push(if x == at.x && y == at.y { '@' } else if o == BLOCK { '#' } else if cp != 0 && o == tag { 'O' } else if cp != 0 { 'X' } else if cor && o != tag { 'c' } else if o == FREE { if lane != 0 && lane != tag { 'l' } else { '.' } } else if o == tag { 'o' } else { 'x' });
            }
            eprintln!("{}", line);
        }
    }

    /// Is (cx, cy) inside the pad's copper shrunk to half extents (ix, iy)?
    fn inside_pad(p: &RPad, cx: f64, cy: f64, ix: f64, iy: f64) -> bool {
        let (dx, dy) = ((cx - p.x).abs(), (cy - p.y).abs());
        if dx > ix + 1e-9 || dy > iy + 1e-9 {
            return false;
        }
        if p.round && ix > 1e-9 && iy > 1e-9 {
            // oval: a stadium (two semicircles + rectangle). Approximate by
            // the inscribed ellipse — conservative at the straight sides.
            return (dx / ix).powi(2) + (dy / iy).powi(2) <= 1.0 + 1e-9;
        }
        true
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

    /// Cells (radius) whose centres lie inside the copper of a trace of
    /// `width` — only those may be treated as copper for connectivity.
    fn copper_cells(&self, width: f64) -> i32 {
        ((width / 2.0 - 1e-9) / self.config.grid).floor().max(0.0) as i32
    }

    fn via_halo_cells(&self) -> i32 {
        let s = self.config.via_size / 2.0 + self.config.clearance + self.config.trace_width / 2.0;
        ((s / self.config.grid).ceil() as i32 - 1).max(0)
    }

    /// Which near map (0 = 3W radius, 1 = 5W radius) a net with required
    /// edge gap `gap` consults; None when the gap is just the clearance.
    fn near_radius_index(&self, gap: f64) -> Option<usize> {
        if gap <= self.config.clearance + 1e-9 {
            None
        } else if gap <= 2.0 * self.config.trace_width + 1e-9 {
            Some(0)
        } else {
            Some(1)
        }
    }

    /// (near-map index, crosstalk halo radius in cells) for a trace of
    /// width `w` whose net requires edge gap `gap` to other traces.
    fn xt_radius(&self, gap: f64, w: f64) -> (Option<usize>, i32) {
        let idx = self.near_radius_index(gap);
        let r = if idx.is_none() {
            0
        } else {
            (((w / 2.0 + gap + self.config.trace_width / 2.0) / self.config.grid).ceil() as i32 - 1).max(0)
        };
        (idx, r)
    }

    /// Dilate trace copper into the near maps: the 3W and 5W radii an
    /// aggressor of default width must keep from a victim of width `w`.
    fn stamp_near(&self, grid: &mut Grid, cells: &[Cell], w: f64, tag: u32, _mate: u32) {
        let tw = self.config.trace_width;
        let r3 = (((w / 2.0 + 2.0 * tw + tw / 2.0) / self.config.grid).ceil() as i32 - 1).max(0);
        let r5 = (((w / 2.0 + 4.0 * tw + tw / 2.0) / self.config.grid).ceil() as i32 - 1).max(0);
        for c in cells {
            for dy in -r5..=r5 {
                for dx in -r5..=r5 {
                    if let Some(i) = grid.idx(c.x + dx, c.y + dy, c.layer) {
                        if dx.abs() <= r3 && dy.abs() <= r3 {
                            Grid::claim_fam(&mut grid.near3, i, tag, &grid.tags.family);
                        }
                        Grid::claim_fam(&mut grid.near5, i, tag, &grid.tags.family);
                    }
                }
            }
        }
    }

    /// Breakout zones (board-relative mm, layer index or None = all):
    /// every pad grown by `xt_exempt_pad_mm`, and the escape band strip of
    /// every fine-pitch pad.
    fn breakout_rects(&self, pads: &[RPad]) -> Vec<(f64, f64, f64, f64, Option<u8>)> {
        let d = self.config.xt_exempt_pad_mm;
        let mut out = Vec::new();
        for p in pads {
            if p.virtual_pad {
                continue;
            }
            let layer = if p.through || p.npth { None } else { p.layers.first().copied() };
            out.push((p.x - p.hx - d, p.y - p.hy - d, p.x + p.hx + d, p.y + p.hy + d, layer));
            if p.fine_pitch && !p.through && !p.npth {
                let along_x = p.hx >= p.hy;
                let dir = if along_x { (p.x - p.fp_cx).signum() } else { (p.y - p.fp_cy).signum() };
                if dir == 0.0 {
                    continue;
                }
                let band = self.config.xt_exempt_band_mm.max(self.config.escape_band_mm);
                let r = if along_x {
                    let (x0, x1) = if dir > 0.0 { (p.x + p.hx, p.x + p.hx + band) } else { (p.x - p.hx - band, p.x - p.hx) };
                    (x0, p.y - p.hy - 0.2, x1, p.y + p.hy + 0.2, layer)
                } else {
                    let (y0, y1) = if dir > 0.0 { (p.y + p.hy, p.y + p.hy + band) } else { (p.y - p.hy - band, p.y - p.hy) };
                    (p.x - p.hx - 0.2, y0, p.x + p.hx + 0.2, y1, layer)
                };
                out.push(r);
            }
        }
        out
    }

    /// Stamp a routed path (copper + halos; vias where the layer changes),
    /// plus its crosstalk halo and the near maps other nets consult.
    fn stamp_path(&self, grid: &mut Grid, cells: &[Cell], net: usize, width: f64, pad_id: u32) {
        self.stamp_copper(grid, cells, net, width, pad_id);
        let tag = net as u32 + 1;
        let mate = grid.tags.mate(tag);
        let gap = grid.tags.gap(tag, self.config.clearance);
        let (_, xr) = self.xt_radius(gap, width);
        if xr > 0 {
            for c in cells {
                for dy in -xr..=xr {
                    for dx in -xr..=xr {
                        if let Some(i) = grid.idx(c.x + dx, c.y + dy, c.layer) {
                            if !grid.exempt[i] {
                                Grid::claim_fam(&mut grid.xt, i, tag, &grid.tags.family);
                            }
                        }
                    }
                }
            }
        }
        self.stamp_near(grid, cells, width, tag, mate);
    }

    /// Stamp copper + clearance halos and the near maps, but no crosstalk
    /// halo of its own (fanout and escape stubs: short copper that is a
    /// victim others must respect, never an aggressor).
    fn stamp_stub(&self, grid: &mut Grid, cells: &[Cell], net: usize, width: f64, pad_id: u32) {
        self.stamp_copper(grid, cells, net, width, pad_id);
        let tag = net as u32 + 1;
        let mate = grid.tags.mate(tag);
        self.stamp_near(grid, cells, width, tag, mate);
    }

    /// Copper + clearance halos only.
    fn stamp_copper(&self, grid: &mut Grid, cells: &[Cell], net: usize, width: f64, pad_id: u32) {
        let tag = net as u32 + 1;
        let halo = self.halo_cells(width);
        // Copper for connectivity: only cells inside the actual copper.
        // (Marking the clearance body as copper let later routes end up to
        // a cell away from real copper — live-verified unconnected tracks.)
        let body = self.copper_cells(width);
        let vhalo = self.via_halo_cells();
        let vbody = self.copper_cells(self.config.via_size);
        let mut prev: Option<Cell> = None;
        for c in cells {
            for dy in -halo..=halo {
                for dx in -halo..=halo {
                    grid.claim_halo(c.x + dx, c.y + dy, c.layer, tag);
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
            self.stamp_stub(grid, &f.cells, f.net, self.config.trace_width, f.pad);
            if let Some((vx, vy)) = f.via {
                self.stamp_via(grid, vx, vy, f.net, f.pad);
            }
        }
    }

    fn stamp_via(&self, grid: &mut Grid, vx: i32, vy: i32, net: usize, pad_id: u32) {
        let tag = net as u32 + 1;
        let vhalo = self.via_halo_cells();
        let vbody = self.copper_cells(self.config.via_size);
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
        for dy in -r..=r {
            for dx in -r..=r {
                let (lx, ly) = (x + dx, y + dy);
                if lx >= 0 && ly >= 0 && (lx as usize) < grid.w && (ly as usize) < grid.h {
                    let lane = grid.lane[ly as usize * grid.w + lx as usize];
                    if lane != 0 && lane != tag {
                        return false; // would wall another pin's escape lane
                    }
                }
            }
        }
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

    /// ESCAPE VIAS: every fine-pitch SIGNAL pad gets a straight stub out of
    /// its lane to a pre-assigned via, depth-staggered over three rows so
    /// adjacent 0.4mm-pitch pins' vias never touch (0.35mm vias keep
    /// 0.366mm of blocking radius: a neighbour's stub passes at 0.4mm).
    /// The pin's connection then starts on the inner layers where there is
    /// room, instead of fighting for the pin-row surface.
    fn escape_vias(&self, grid: &mut Grid, pads: &[RPad], plane_ids: &[usize], skip_nets: &std::collections::HashSet<usize>, state: &mut RouteState) {
        let pitch = self.config.grid;
        let via_r = self.config.via_size / 2.0;
        let step = (2 * self.via_halo_cells() + 1) as f64 * pitch + pitch;
        for p in pads {
            if !p.fine_pitch || p.npth || p.through || p.virtual_pad {
                continue;
            }
            let Some(net) = p.net else { continue };
            if plane_ids.contains(&net) || skip_nets.contains(&net) {
                continue;
            }
            let tag = net as u32 + 1;
            let along_x = p.hx >= p.hy;
            let dir = if along_x { (p.x - p.fp_cx).signum() } else { (p.y - p.fp_cy).signum() };
            if dir == 0.0 {
                continue;
            }
            let tip = if along_x { p.hx } else { p.hy };
            let Some(&layer) = p.layers.first() else { continue };
            let k0 = (p.id % 3) as f64;
            let mut placed = false;
            for attempt in 0..9 {
                let row = ((k0 as i32 + attempt) % 3) as f64;
                let depth = tip + self.config.escape_mm + via_r + 0.1 + row * step + (attempt / 3) as f64 * 3.0 * step;
                if depth > tip + self.config.escape_band_mm + 3.0 * step {
                    break;
                }
                let (sx, sy) = if along_x { (p.x + dir * depth, p.y) } else { (p.x, p.y + dir * depth) };
                let (gx, gy) = ((sx / pitch).round() as i32, (sy / pitch).round() as i32);
                if !self.via_ok(grid, gx, gy, tag) {
                    continue;
                }
                // the stub: pad centre cell → via cell, every cell free or own
                let (px, py) = ((p.x / pitch).round() as i32, (p.y / pitch).round() as i32);
                let mut cells: Vec<Cell> = Vec::new();
                let n = if along_x { (gx - px).abs() } else { (gy - py).abs() };
                let mut ok = true;
                for i in 0..=n {
                    let c = if along_x {
                        Cell { x: px + i * (gx - px).signum(), y: py, layer }
                    } else {
                        Cell { x: px, y: py + i * (gy - py).signum(), layer }
                    };
                    let o = grid.owner_at(c);
                    if o != FREE && o != tag {
                        ok = false;
                        break;
                    }
                    cells.push(c);
                }
                if !ok {
                    continue;
                }
                self.stamp_stub(grid, &cells, net, self.config.trace_width, p.id);
                self.stamp_via(grid, gx, gy, net, p.id);
                // the stub is this pad's lane now: nobody else's via near it
                for c in &cells {
                    if c.x >= 0 && c.y >= 0 && (c.x as usize) < grid.w && (c.y as usize) < grid.h {
                        grid.lane[c.y as usize * grid.w + c.x as usize] = tag;
                    }
                }
                state.escapes.push(Fanout { pad: p.id, net, cells, via: Some((gx, gy)) });
                placed = true;
                break;
            }
            let _ = placed;
        }
    }

    /// Plane nets: every SMD pad gets a stub + via; through-hole pads reach
    /// the plane by their barrel.
    /// Does `net`'s plane copper exist under (x, y) on at least one layer
    /// (inside its region and not under a higher-priority plane there)?
    fn plane_reaches(&self, net: &str, x: i32, y: i32) -> bool {
        let (px, py) = (x as f64 * self.config.grid, y as f64 * self.config.grid);
        let inside = |r: &Option<(f64, f64, f64, f64)>| match r {
            None => true,
            Some((x0, y0, x1, y1)) => px >= *x0 && px <= *x1 && py >= *y0 && py <= *y1,
        };
        let mine: Vec<&PlaneRegion> = self.config.plane_regions.iter().filter(|p| p.net == net).collect();
        if mine.is_empty() {
            return true; // no region knowledge: whole-board planes
        }
        mine.iter().any(|p| {
            inside(&p.region)
                && !self.config.plane_regions.iter().any(|q| {
                    q.layer == p.layer && q.net != p.net && q.priority > p.priority && inside(&q.region)
                })
        })
    }

    fn fanout_planes(&self, grid: &mut Grid, pads: &[RPad], plane_ids: &[usize], state: &mut RouteState, net_name_of: &dyn Fn(usize) -> String) {
        let max_cells = (self.config.fanout_max_mm / self.config.grid).ceil() as i32;
        for p in pads {
            let Some(net) = p.net else { continue };
            if !plane_ids.contains(&net) || p.npth || p.virtual_pad {
                continue;
            }
            if p.through {
                state.fanouts.push(Fanout { pad: p.id, net, cells: Vec::new(), via: None });
                continue;
            }
            let tag = net as u32 + 1;
            let Some(&layer) = p.layers.first() else { continue };
            // Cost-aware search (Dijkstra on the cell cost grid) from the
            // pad's copper cells on its layer to the nearest legal via site
            // or existing plane-net copper that is not a pad. Fine-pitch
            // pads prefer sites INSIDE the footprint (under the body: no
            // cost band there) and never drop the via inside their own
            // escape lane — a via at a lane end walls the neighbouring
            // lanes (live-verified: searches dying after <100 expansions).
            let starts = self.pad_cells(grid, p, layer);
            let lane_zone = |c: &Cell| -> bool {
                if !p.fine_pitch {
                    return false;
                }
                let (cx, cy) = (c.x as f64 * self.config.grid, c.y as f64 * self.config.grid);
                let along_x = p.hx >= p.hy;
                let dir = if along_x { (p.x - p.fp_cx).signum() } else { (p.y - p.fp_cy).signum() };
                let out = if along_x { (cx - p.x) * dir } else { (cy - p.y) * dir };
                let side = if along_x { (cy - p.y).abs() } else { (cx - p.x).abs() };
                out > 0.0 && out < (if along_x { p.hx } else { p.hy }) + self.config.escape_mm + self.config.via_size && side < 1.0
            };
            let mut heap: BinaryHeap<Node> = BinaryHeap::new();
            let mut came: std::collections::HashMap<Cell, Cell> = std::collections::HashMap::new();
            let mut best: std::collections::HashMap<Cell, f64> = std::collections::HashMap::new();
            for s in &starts {
                heap.push(Node { cell: *s, f_score: 0.0 });
                came.insert(*s, *s);
                best.insert(*s, 0.0);
            }
            let mut found: Option<(Cell, bool)> = None;
            while let Some(node) = heap.pop() {
                let c = node.cell;
                let d = node.f_score;
                if d > *best.get(&c).unwrap_or(&f64::INFINITY) + 1e-9 {
                    continue;
                }
                let in_pad = grid.idx(c.x, c.y, c.layer).map(|i| grid.copper_pad[i] == p.id + 1).unwrap_or(false);
                if !in_pad {
                    // existing copper of this plane net (a via or stub) counts as connected
                    let cp = grid.idx(c.x, c.y, c.layer).map(|i| grid.copper_pad[i]).unwrap_or(0);
                    if cp != 0 && grid.owner_at(c) == tag && pads.get((cp - 1) as usize).map(|q| q.id != p.id && (state.fanouts.iter().any(|f| f.pad == q.id && (f.via.is_some() || q.through)))).unwrap_or(false) {
                        found = Some((c, false));
                        break;
                    }
                    if !lane_zone(&c) && self.via_ok(grid, c.x, c.y, tag) && self.plane_reaches(&net_name_of(net), c.x, c.y) {
                        found = Some((c, true));
                        break;
                    }
                }
                if d >= max_cells as f64 * 4.0 {
                    continue;
                }
                for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                    let n = Cell { x: c.x + dx, y: c.y + dy, layer: c.layer };
                    let o = grid.owner_at(n);
                    if o != FREE && o != tag {
                        continue;
                    }
                    if grid.lane_adjacent(n.x, n.y, n.layer, tag, self.halo_cells(self.config.trace_width)) {
                        continue;
                    }
                    let extra = grid.idx(n.x, n.y, n.layer).map(|i| grid.cost[i] as f64 * 0.25).unwrap_or(0.0);
                    let nd = d + 1.0 + extra;
                    if nd < *best.get(&n).unwrap_or(&f64::INFINITY) {
                        best.insert(n, nd);
                        came.insert(n, c);
                        heap.push(Node { cell: n, f_score: nd });
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
                    self.stamp_stub(grid, &path, net, self.config.trace_width, p.id);
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
        self.route_edge_opt(grid, pads, dsu, a, b, net, width, false)
    }

    /// `tail`: a pair tail — never inside a corridor's halo except through
    /// the terminal approach cells.
    #[allow(clippy::too_many_arguments)]
    fn route_edge_opt(
        &self,
        grid: &mut Grid,
        pads: &[RPad],
        dsu: &mut Dsu,
        a: u32,
        b: u32,
        net: usize,
        width: f64,
        tail: bool,
    ) -> Option<Vec<Cell>> {
        let tag = net as u32 + 1;
        let _mate = grid.tags.mate(tag);
        let gap = grid.tags.gap(tag, self.config.clearance);
        let (near_idx, _) = self.xt_radius(gap, width);
        let restricted = grid.tags.is_restricted(tag);
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

        // Start from EVERY copper cell of pad a's connected component (its
        // pads, traces and vias), not only pad a: the component is one
        // conductor, and the nearest piece of it may be far from the pad.
        let root_a = dsu.find(a);
        let mut starts: Vec<Cell> = Vec::new();
        for &l in &pa.layers {
            starts.extend(self.pad_cells(grid, pa, l));
        }
        {
            let mut member = vec![false; pads.len()];
            for p in pads.iter() {
                if p.net == Some(net) && dsu.find(p.id) == root_a {
                    member[p.id as usize] = true;
                }
            }
            for l in 0..grid.layers as u8 {
                for y in 0..grid.h as i32 {
                    for x in 0..grid.w as i32 {
                        let i = ((l as usize) * grid.h + y as usize) * grid.w + x as usize;
                        let cp = grid.copper_pad[i];
                        if cp != 0 && grid.owner[i] == tag && member[(cp - 1) as usize] {
                            let c = Cell { x, y, layer: l };
                            if !starts.contains(&c) {
                                starts.push(c);
                            }
                        }
                    }
                }
            }
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
        // Crosstalk: never inside another aggressor's 3W/5W halo, and an
        // aggressor never within its own spacing of another net's trace
        // (outside breakout zones).
        let xt_ok = |q: &Cell, grid: &Grid| -> bool {
            match grid.idx(q.x, q.y, q.layer) {
                Some(i) => {
                    let x = grid.xt[i];
                    if x != FREE && !grid.tags.friendly(tag, x) {
                        return false;
                    }
                    if !grid.exempt[i] {
                        if let Some(m) = near_idx {
                            let v = if m == 0 { grid.near3[i] } else { grid.near5[i] };
                            if v != FREE && !grid.tags.friendly(tag, v) {
                                return false;
                            }
                        }
                    }
                    true
                }
                None => false,
            }
        };
        // Free for this net's trace body (default traces: the cell itself)
        let halo_r = self.halo_cells(width);
        let is_free = |c: &Cell, grid: &Grid| -> bool {
            if restricted && !grid.tags.layer_ok(c.layer) {
                return false;
            }
            if grid.lane_adjacent(c.x, c.y, c.layer, tag, halo_r) {
                return false;
            }
            if tail {
                if let Some(i) = grid.idx(c.x, c.y, c.layer) {
                    let cp = grid.corridor[i];
                    if cp != 0 && !grid.tags.friendly(tag, cp) {
                        return false;
                    }
                }
            }
            if body == 0 {
                let o = grid.owner_at(*c);
                return (o == FREE || o == tag) && xt_ok(c, grid);
            }
            // Wide (power) traces never enter escape lanes or bands of
            // other nets: a 0.4mm rail through a pin row walls the pins
            // (live-verified: pin 48 sealed by a +1V8 trace).
            if let Some(i) = grid.idx(c.x, c.y, c.layer) {
                if grid.band_axis[i] != 0 {
                    return false;
                }
            }
            if c.x >= 0 && c.y >= 0 && (c.x as usize) < grid.w && (c.y as usize) < grid.h {
                let lane = grid.lane[c.y as usize * grid.w + c.x as usize];
                if lane != 0 && lane != tag {
                    return false;
                }
            }
            for dy in -body..=body {
                for dx in -body..=body {
                    let q = Cell { x: c.x + dx, y: c.y + dy, layer: c.layer };
                    let o = grid.owner_at(q);
                    if o != FREE && o != tag {
                        return false;
                    }
                    if !xt_ok(&q, grid) {
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
        let debug = std::env::var("SOMEDA_ROUTE_DEBUG").is_ok();
        while let Some(node) = open.pop() {
            expansions += 1;
            if expansions > self.config.max_expansions {
                if debug {
                    eprintln!("route-debug: BUDGET {}:{} → {}:{} after {} expansions", pa.reference, pa.number, pb.reference, pb.number, expansions);
                }
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
                let extra = grid.idx(next.x, next.y, next.layer).map(|i| {
                    let iso = grid.cost[i] as f64 * 0.25;
                    let sideways = match grid.band_axis[i] {
                        1 => dy != 0,
                        2 => dx != 0,
                        _ => false,
                    };
                    iso + if sideways { self.config.band_side_cost } else { 0.0 }
                }).unwrap_or(0.0);
                let tentative = g_here + 1.0 + against + extra;
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
        if debug {
            eprintln!("route-debug: NOPATH {}:{} → {}:{} (search exhausted after {} expansions)", pa.reference, pa.number, pb.reference, pb.number, expansions);
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
    escapes: Vec<Fanout>,
    failed: Vec<usize>,
    fanout_failed: Vec<u32>,
    pairs: Vec<pairs::PairRoute>,
    pair_reports: Vec<PairReport>,
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


    fn board_pair() -> Board {
        // Two components 14 mm apart with a P/N pad couple each (0.5 mm
        // pitch), a GND plane pad, and an unrelated net that must keep 5W.
        Board::from_kicad_pcb(
            "(kicad_pcb (version 20221018)
              (layers (0 \"F.Cu\" signal) (1 \"In1.Cu\" signal) (2 \"In2.Cu\" signal) (3 \"In3.Cu\" signal) (4 \"In4.Cu\" signal) (31 \"B.Cu\" signal))
              (gr_line (start 0 0) (end 30 0) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 30 0) (end 30 20) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 30 20) (end 0 20) (layer \"Edge.Cuts\") (width 0.1))
              (gr_line (start 0 20) (end 0 0) (layer \"Edge.Cuts\") (width 0.1))
              (net 1 \"GND\")
              (net 2 \"DP\")
              (net 3 \"DN\")
              (net 4 \"QUIET\")
              (footprint \"U\" (layer \"F.Cu\") (at 6 10 0)
                (fp_text reference \"U1\" (at 0 -2))
                (pad \"1\" smd rect (at 0 -0.25) (size 0.8 0.3) (layers \"F.Cu\") (net 2 \"DP\"))
                (pad \"2\" smd rect (at 0 0.25) (size 0.8 0.3) (layers \"F.Cu\") (net 3 \"DN\"))
                (pad \"3\" smd rect (at 0 1.5) (size 0.8 0.3) (layers \"F.Cu\") (net 4 \"QUIET\")))
              (footprint \"J\" (layer \"F.Cu\") (at 22 12 0)
                (fp_text reference \"J1\" (at 0 -2))
                (pad \"1\" smd rect (at 0 -0.25) (size 0.8 0.3) (layers \"F.Cu\") (net 2 \"DP\"))
                (pad \"2\" smd rect (at 0 0.25) (size 0.8 0.3) (layers \"F.Cu\") (net 3 \"DN\"))
                (pad \"3\" smd rect (at 0 1.5) (size 0.8 0.3) (layers \"F.Cu\") (net 4 \"QUIET\"))
                (pad \"4\" smd rect (at 0 3.0) (size 0.8 0.3) (layers \"F.Cu\") (net 1 \"GND\"))))",
        )
        .unwrap()
    }

    #[test]
    fn differential_pair_routes_coupled_and_matched() {
        let b = board_pair();
        let nl = Netlist::from_board(&b);
        let rules = crate::rules::DesignRules::for_layers(6);
        let mut cfg = RouterConfig::from_rules(
            &rules,
            vec!["F.Cu".into(), "In2.Cu".into(), "B.Cu".into()],
            vec!["GND".into()],
            vec![],
        );
        cfg.pairs = vec![PairConfig { p: "DP".into(), n: "DN".into(), width_mm: 0.127, pitch_mm: 0.254, z_diff: 100.0, skew_budget_mm: 0.15 }];
        cfg.pair_layers = vec!["F.Cu".into(), "In2.Cu".into()];
        cfg.net_gap.insert("DP".into(), 4.0 * 0.127);
        cfg.net_gap.insert("DN".into(), 4.0 * 0.127);
        let router = Router::new(cfg);
        let r = router.route(&b, &nl);
        assert!(r.unrouted.is_empty(), "all routed: {} {:?}", r.summary_line(), r.unrouted);
        assert_eq!(r.pairs.len(), 1, "one pair trunk reported");
        let pr = &r.pairs[0];
        assert!(pr.routed, "pair routed: {}", pr.cause);
        assert!(pr.coupled_mm > 8.0, "coupled length {} mm", pr.coupled_mm);
        assert!(pr.skew_mm.abs() <= pr.skew_budget_mm + 1e-6, "skew {} within {}", pr.skew_mm, pr.skew_budget_mm);
        assert!(pr.tail_max_mm < 8.0, "tails short: {}", pr.tail_max_mm);
        // Both tracks on the pair layer, every P corridor segment has an N
        // segment running parallel at the pitch
        let on_layer = |net: &str| r.segments.iter().filter(|s| s.net.1 == net && s.layer == pr.layer).count();
        assert!(on_layer("DP") > 0 && on_layer("DN") > 0);
        for s in r.segments.iter().filter(|s| s.net.1 == "DP" || s.net.1 == "DN") {
            assert!((s.width - 0.127).abs() < 1e-9);
        }
        // Length measure from the geometry agrees with the report
        let len_of = |net: &str| r.segments.iter().filter(|s| s.net.1 == net).map(|s| ((s.x2 - s.x1).powi(2) + (s.y2 - s.y1).powi(2)).sqrt()).sum::<f64>();
        let (lp, ln) = (len_of("DP"), len_of("DN"));
        assert!(((lp - ln) - pr.skew_mm).abs() < 0.3, "geometry skew {} vs report {}", lp - ln, pr.skew_mm);
        // The quiet net never runs parallel within 4W of the pair outside
        // the breakout zones (pads grown by 1 mm)
        let pads: Vec<(f64, f64)> = b.footprints.iter().flat_map(|f| f.pads.iter().map(|p| (p.x, p.y))).collect();
        let exempt = |x: f64, y: f64| pads.iter().any(|(px, py)| (x - px).abs() < 1.5 && (y - py).abs() < 1.5);
        for q in r.segments.iter().filter(|s| s.net.1 == "QUIET") {
            for p in r.segments.iter().filter(|s| (s.net.1 == "DP" || s.net.1 == "DN") && s.layer == q.layer) {
                let horiz_q = (q.y1 - q.y2).abs() < 1e-9;
                let horiz_p = (p.y1 - p.y2).abs() < 1e-9;
                if horiz_q != horiz_p { continue; }
                let (q0, q1, p0, p1, qc, pc) = if horiz_q { (q.x1.min(q.x2), q.x1.max(q.x2), p.x1.min(p.x2), p.x1.max(p.x2), q.y1, p.y1) } else { (q.y1.min(q.y2), q.y1.max(q.y2), p.y1.min(p.y2), p.y1.max(p.y2), q.x1, p.x1) };
                let overlap = q1.min(p1) - q0.max(p0);
                let gap = (qc - pc).abs() - 0.127;
                if overlap > 0.0 && gap < 4.0 * 0.127 - 1e-6 {
                    let mid = (q0.max(p0) + q1.min(p1)) / 2.0;
                    let (mx, my) = if horiz_q { (mid, qc) } else { (qc, mid) };
                    assert!(exempt(mx, my), "QUIET runs {:.2}mm parallel to the pair at {:.3}mm gap outside breakouts at ({:.2},{:.2})", overlap, gap, mx, my);
                }
            }
        }
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
