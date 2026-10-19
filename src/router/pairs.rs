//! Differential-pair routing: a coupled corridor between the two components
//! a pair connects, two offset tracks inside it, short single-ended tails
//! from the real pads to the corridor terminals, and intra-pair skew
//! compensation bumps.
//!
//! The corridor is searched with a direction-aware A* on ONE layer (no
//! vias inside the coupled section: a via pair would have to spread to
//! hole-to-hole spacing and uncouple anyway). P and N sit at ±pitch/2
//! from the centreline; which side P takes ("handedness") is fixed by the
//! start pads and must agree at the far end, so the search carries the
//! side and only accepts departures/arrivals whose geometry matches.
//! Every 90° corner lengthens the outer track by 2·pitch and shortens the
//! inner one by the same, so the skew after the corridor is known exactly
//! and is compensated with trapezoid bumps on the shorter track.

use super::*;

/// One pair as the router routes it (geometry already solved).
#[derive(Debug, Clone)]
pub struct PairConfig {
    pub p: String,
    pub n: String,
    /// Trace width of both tracks (mm).
    pub width_mm: f64,
    /// Centre-to-centre pitch of the two tracks (mm).
    pub pitch_mm: f64,
    pub z_diff: f64,
    /// Max |len(P) − len(N)| after compensation (mm).
    pub skew_budget_mm: f64,
}

/// What happened to one pair.
#[derive(Debug, Clone, Default)]
pub struct PairReport {
    pub p: String,
    pub n: String,
    pub routed: bool,
    pub layer: String,
    /// Coupled (corridor) length (mm, centreline).
    pub coupled_mm: f64,
    /// Longest single-ended tail from a pad to the corridor (mm).
    pub tail_max_mm: f64,
    pub p_len_mm: f64,
    pub n_len_mm: f64,
    /// len(P) − len(N) after compensation (mm).
    pub skew_mm: f64,
    pub skew_budget_mm: f64,
    pub corners: usize,
    pub bumps: usize,
    /// The pads' P/N order differs at the two ends: the tails at the far
    /// end cross (one dips through a via).
    pub crossover: bool,
    pub cause: String,
}

/// A routed pair, kept for grid replay and emission.
#[derive(Debug, Clone)]
pub(super) struct PairRoute {
    pub cfg: usize,
    pub p_net: usize,
    pub n_net: usize,
    pub layer: u8,
    pub corridor: Vec<Cell>,
    /// +1: P at +perp(dir) side, −1: P at −perp(dir).
    pub side: i32,
    /// Track polylines in grid units (cell coordinates, fractional).
    pub p_track: Vec<(f64, f64)>,
    pub n_track: Vec<(f64, f64)>,
    /// Tails: (real pad, virtual pad, cells, width).
    pub tails: Vec<RoutedEdge>,
    /// Virtual pad ids: [p_start, p_end, p_body, n_start, n_end, n_body].
    pub vpads: [u32; 6],
    pub report: PairReport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct DState {
    cell: Cell,
    dir: u8,
}

const DIRS: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];

#[inline]
fn perp(d: u8) -> (i32, i32) {
    let (dx, dy) = DIRS[d as usize];
    (-dy, dx)
}

/// Sign of the side (±1) pad P lies on relative to travel direction `d`
/// when the P−N vector is `v` (cells). 0 = inline (ambiguous).
fn side_of(d: u8, v: (f64, f64)) -> i32 {
    let p = perp(d);
    let dot = p.0 as f64 * v.0 + p.1 as f64 * v.1;
    if dot > 1e-9 {
        1
    } else if dot < -1e-9 {
        -1
    } else {
        0
    }
}

impl Router {
    /// Route every configured pair. Pads for the virtual terminals were
    /// appended to `pads` by the caller (6 per trunk); `vpad_base` is the
    /// first of them.
    pub(super) fn route_pairs(
        &self,
        grid: &mut Grid,
        pads: &mut Vec<RPad>,
        dsu: &mut Dsu,
        state: &mut RouteState,
        net_id: &dyn Fn(&str) -> Option<usize>,
        gap_of: &[f64],
        refs_of_net: &dyn Fn(usize) -> Vec<String>,
    ) {
        let pitch = self.config.grid;
        let debug = std::env::var("SOMEDA_ROUTE_DEBUG").is_ok();
        for (ci, pc) in self.config.pairs.iter().enumerate() {
            let (Some(pn), Some(nn)) = (net_id(&pc.p), net_id(&pc.n)) else {
                state.pair_reports.push(PairReport { p: pc.p.clone(), n: pc.n.clone(), cause: "pair net not on board".into(), ..Default::default() });
                continue;
            };
            // Components both nets touch, ordered along their main axis
            let rp = refs_of_net(pn);
            let rn = refs_of_net(nn);
            let mut shared: Vec<String> = rp.iter().filter(|r| rn.contains(r)).cloned().collect();
            shared.sort();
            shared.dedup();
            if shared.len() < 2 {
                state.pair_reports.push(PairReport { p: pc.p.clone(), n: pc.n.clone(), cause: format!("nets share {} component(s)", shared.len()), ..Default::default() });
                continue;
            }
            let centre = |r: &str| -> (f64, f64) {
                let ps: Vec<&RPad> = pads.iter().filter(|p| p.reference == r && !p.virtual_pad).collect();
                let n = ps.len().max(1) as f64;
                (ps.iter().map(|p| p.x).sum::<f64>() / n, ps.iter().map(|p| p.y).sum::<f64>() / n)
            };
            let cs: Vec<(f64, f64)> = shared.iter().map(|r| centre(r)).collect();
            let spread_x = cs.iter().map(|c| c.0).fold(f64::NEG_INFINITY, f64::max) - cs.iter().map(|c| c.0).fold(f64::INFINITY, f64::min);
            let spread_y = cs.iter().map(|c| c.1).fold(f64::NEG_INFINITY, f64::max) - cs.iter().map(|c| c.1).fold(f64::INFINITY, f64::min);
            let mut order: Vec<usize> = (0..shared.len()).collect();
            order.sort_by(|&a, &b| {
                let ka = if spread_x >= spread_y { cs[a].0 } else { cs[a].1 };
                let kb = if spread_x >= spread_y { cs[b].0 } else { cs[b].1 };
                ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
            });
            for t in 0..order.len() - 1 {
                let (ra, rb) = (&shared[order[t]], &shared[order[t + 1]]);
                let rep = self.route_trunk(grid, pads, dsu, state, ci, pc, pn, nn, ra, rb, gap_of);
                if debug {
                    eprintln!("route-debug: PAIR {}/{} {}↔{}: {}", pc.p, pc.n, ra, rb, if rep.routed { format!("coupled {:.1}mm on {} skew {:.3}mm (budget {:.3}) corners {} bumps {} tail_max {:.1}mm{}", rep.coupled_mm, rep.layer, rep.skew_mm, rep.skew_budget_mm, rep.corners, rep.bumps, rep.tail_max_mm, if rep.crossover { " CROSSOVER" } else { "" }) } else { rep.cause.clone() });
                }
                state.pair_reports.push(rep);
            }
        }
        let _ = pitch;
    }

    /// One coupled trunk between components `ra` and `rb`.
    #[allow(clippy::too_many_arguments)]
    fn route_trunk(
        &self,
        grid: &mut Grid,
        pads: &mut Vec<RPad>,
        dsu: &mut Dsu,
        state: &mut RouteState,
        ci: usize,
        pc: &PairConfig,
        pn: usize,
        nn: usize,
        ra: &str,
        rb: &str,
        gap_of: &[f64],
    ) -> PairReport {
        let pitch = self.config.grid;
        let mut rep = PairReport { p: pc.p.clone(), n: pc.n.clone(), skew_budget_mm: pc.skew_budget_mm, ..Default::default() };
        // Endpoint pads: the closest P/N pad couple on each component
        let couple = |r: &str| -> Option<(u32, u32)> {
            let ps: Vec<&RPad> = pads.iter().filter(|p| p.reference == r && p.net == Some(pn) && !p.virtual_pad).collect();
            let ns: Vec<&RPad> = pads.iter().filter(|p| p.reference == r && p.net == Some(nn) && !p.virtual_pad).collect();
            let mut best: Option<(f64, u32, u32)> = None;
            for p in &ps {
                for n in &ns {
                    let d = (p.x - n.x).hypot(p.y - n.y);
                    if best.map_or(true, |b| d < b.0) {
                        best = Some((d, p.id, n.id));
                    }
                }
            }
            best.map(|b| (b.1, b.2))
        };
        let (Some((pa, na)), Some((pb, nb))) = (couple(ra), couple(rb)) else {
            rep.cause = format!("no P/N pad couple on {} or {}", ra, rb);
            return rep;
        };
        let w = pc.width_mm;
        let o = pc.pitch_mm / 2.0;
        // Terminal offset in cells: the tails arrive at ±k cells and must
        // themselves clear each other by the design clearance.
        let k = (((w + self.config.clearance) / (2.0 * pitch)).ceil() as i32).max(1);
        let w_eff = pc.pitch_mm + w;
        let body = self.body_cells(w_eff).max(k);
        // A corner closer than the corridor halo to either terminal lets the
        // post-corner halo reach back into the tails' approach cells
        // (live-verified: BLOCKed approach, tail unroutable).
        let straight_len = (self.halo_cells(w_eff) + 2).max(3);
        let (tp, tn) = (pn as u32 + 1, nn as u32 + 1);
        let cellof = |p: &RPad| ((p.x / pitch), (p.y / pitch));
        let (ca_p, ca_n, cb_p, cb_n) = (cellof(&pads[pa as usize]), cellof(&pads[na as usize]), cellof(&pads[pb as usize]), cellof(&pads[nb as usize]));
        let start_mid = ((ca_p.0 + ca_n.0) / 2.0, (ca_p.1 + ca_n.1) / 2.0);
        let goal_mid = ((cb_p.0 + cb_n.0) / 2.0, (cb_p.1 + cb_n.1) / 2.0);
        let start_vec = (ca_p.0 - ca_n.0, ca_p.1 - ca_n.1);
        let goal_vec = (cb_p.0 - cb_n.0, cb_p.1 - cb_n.1);
        // The corridor leaves component A away from A's body and arrives
        // at B heading into B's body (never a U-turn back over the pads).
        let fa = (pads[pa as usize].fp_cx / pitch, pads[pa as usize].fp_cy / pitch);
        let fb = (pads[pb as usize].fp_cx / pitch, pads[pb as usize].fp_cy / pitch);
        let start_away = (start_mid.0 - fa.0, start_mid.1 - fa.1);
        let goal_toward = (fb.0 - goal_mid.0, fb.1 - goal_mid.1);
        let radius = ((self.config.xt_exempt_band_mm.max(self.config.escape_band_mm) + 3.0) / pitch).ceil() as i32;
        // Virtual pads for this trunk: 6 fresh entries
        let base = pads.len() as u32;
        for (i, (net, name)) in [(pn, "P-start"), (pn, "P-end"), (pn, "P-body"), (nn, "N-start"), (nn, "N-end"), (nn, "N-body")].iter().enumerate() {
            pads.push(RPad {
                id: base + i as u32,
                reference: format!("~{}/{}", pc.p, pc.n),
                number: name.to_string(),
                net: Some(*net),
                x: 0.0,
                y: 0.0,
                hx: 0.0,
                hy: 0.0,
                through: false,
                npth: false,
                round: false,
                drill: 0.0,
                layers: vec![0],
                fine_pitch: false,
                fp_cx: 0.0,
                fp_cy: 0.0,
                virtual_pad: true,
            });
        }
        dsu.grow(pads.len());
        let vp = [base, base + 1, base + 2, base + 3, base + 4, base + 5];
        let mut forbid: std::collections::HashSet<Cell> = std::collections::HashSet::new();
        let layers: Vec<u8> = self.config.pair_layers.iter().filter_map(|l| self.config.signal_layers.iter().position(|s| s == l).map(|i| i as u8)).collect();
        let layers = if layers.is_empty() { vec![0u8] } else { layers };
        let mut last_cause = String::from("no corridor on any allowed layer");
        for attempt in 0..6 {
            let mut found: Option<(u8, i32, Vec<Cell>, bool)> = None;
            // Matching handedness first; when the pads' P/N order differs at
            // the two ends, a crossover (far tails swap, one through a via).
            'search: for crossover in [false, true] {
                for &layer in &layers {
                    let mut best: Option<(f64, i32, Vec<Cell>)> = None;
                    for side in [1i32, -1] {
                        let goal_side = if crossover { -side } else { side };
                        if let Some((c, cost)) = self.search_corridor(grid, layer, body, k, straight_len, tp, tn, start_mid, start_vec, start_away, goal_mid, goal_vec, goal_toward, side, goal_side, &forbid, radius, gap_of[pn]) {
                            if best.as_ref().map_or(true, |b| cost < b.0) {
                                best = Some((cost, side, c));
                            }
                        }
                    }
                    if let Some((_, side, c)) = best {
                        found = Some((layer, side, c, crossover));
                        break 'search;
                    }
                }
            }
            let Some((layer, side, corridor, crossover)) = found else {
                break;
            };
            rep.crossover = crossover;
            // Trial on a grid clone: stamp corridor, route tails, keep on success
            let mut g2 = grid.clone();
            let mut d2 = dsu.clone();
            let (p_track, n_track, p_cells, n_cells, corners, _dirs) = self.build_tracks(&corridor, side, o, k, w, pitch);
            for (vi, l) in vp.iter().zip([layer; 6]) {
                pads[*vi as usize].layers = vec![l];
            }
            // Terminal cells = the exact polyline endpoints (the rasterizer's
            // cell order is not the polyline order — live-verified: a tail
            // aimed at a cell inside the corridor)
            let (tps, tpe, tns, tne) = (
                Self::cell_at(p_track[0], layer),
                Self::cell_at(*p_track.last().unwrap(), layer),
                Self::cell_at(n_track[0], layer),
                Self::cell_at(*n_track.last().unwrap(), layer),
            );
            for (vi, c) in [(vp[0], tps), (vp[1], tpe), (vp[3], tns), (vp[4], tne)] {
                pads[vi as usize].x = c.x as f64 * pitch;
                pads[vi as usize].y = c.y as f64 * pitch;
            }
            self.stamp_pair(&mut g2, &corridor, side, layer, &p_cells, &n_cells, [tps, tpe, tns, tne], pn, nn, w, w_eff, vp, gap_of[pn]);
            // Tails
            let mut tails: Vec<RoutedEdge> = Vec::new();
            let mut ok = true;
            let mut failed_term: Option<Cell> = None;
            for (real, virt, term_cell) in [(pa, vp[0], tps), (na, vp[3], tns), (pb, vp[1], tpe), (nb, vp[4], tne)] {
                let width = if pads[real as usize].fine_pitch { self.config.trace_width } else { w };
                match self.route_edge_opt(&mut g2, pads, &mut d2, real, virt, pads[real as usize].net.unwrap(), width, true) {
                    Some(cells) => {
                        self.stamp_path(&mut g2, &cells, pads[real as usize].net.unwrap(), width, real);
                        d2.union(real, virt);
                        tails.push(RoutedEdge { edge_index: usize::MAX, cells, width, net: pads[real as usize].net.unwrap(), from_pad: real });
                    }
                    None => {
                        ok = false;
                        failed_term = Some(term_cell);
                        if std::env::var("SOMEDA_ROUTE_DEBUG").map(|v| v == "2").unwrap_or(false) {
                            eprintln!("route-debug: PAIR TAIL FAILED {}:{} → terminal ({:.3},{:.3}) layer {}", pads[real as usize].reference, pads[real as usize].number, term_cell.x as f64 * pitch, term_cell.y as f64 * pitch, layer);
                            eprintln!("route-debug:   corridor head {:?} side {} k {} o {:.3}cells", corridor.iter().take(5).map(|c| (c.x, c.y)).collect::<Vec<_>>(), side, k, o / pitch);
                            eprintln!("route-debug:   p_track head {:?} p_cells head {:?}", p_track.iter().take(4).collect::<Vec<_>>(), p_cells.iter().take(6).map(|c| (c.x, c.y)).collect::<Vec<_>>());
                            eprintln!("route-debug:   n_track head {:?} n_cells head {:?}", n_track.iter().take(4).collect::<Vec<_>>(), n_cells.iter().take(6).map(|c| (c.x, c.y)).collect::<Vec<_>>());
                            eprintln!("route-debug:   terminals P {:?}/{:?} N {:?}/{:?}", (tps.x, tps.y), (tpe.x, tpe.y), (tns.x, tns.y), (tne.x, tne.y));
                            self.dump_occupancy(&g2, term_cell, pads[real as usize].net.unwrap() as u32 + 1, 12);
                            let pr = &pads[real as usize];
                            self.dump_occupancy(&g2, Cell { x: (pr.x / pitch).round() as i32, y: (pr.y / pitch).round() as i32, layer: pr.layers[0] }, pr.net.unwrap() as u32 + 1, 12);
                        }
                        break;
                    }
                }
            }
            if !ok {
                last_cause = format!("tail to a corridor terminal could not be routed (attempt {})", attempt + 1);
                if let Some(c) = failed_term {
                    // forbid corridor ends near this terminal and retry
                    for dy in -3..=3 {
                        for dx in -3..=3 {
                            forbid.insert(Cell { x: c.x + dx, y: c.y + dy, layer: c.layer });
                        }
                    }
                }
                continue;
            }
            // Lengths and skew
            let tail_len = |t: &RoutedEdge| (t.cells.len().saturating_sub(1)) as f64 * pitch;
            let poly_len = |t: &[(f64, f64)]| t.windows(2).map(|s| ((s[1].0 - s[0].0).powi(2) + (s[1].1 - s[0].1).powi(2)).sqrt() * pitch).sum::<f64>();
            let p_len0 = poly_len(&p_track) + tail_len(&tails[0]) + tail_len(&tails[2]);
            let n_len0 = poly_len(&n_track) + tail_len(&tails[1]) + tail_len(&tails[3]);
            let mut p_track = p_track;
            let mut n_track = n_track;
            let mut skew = p_len0 - n_len0;
            let mut bumps = 0usize;
            // Coarse compensation first: rectangular detours on the shorter
            // net's tails (0.254 mm steps), re-stamped on a fresh clone.
            if skew.abs() > pc.skew_budget_mm {
                let shorter_net = if skew > 0.0 { nn } else { pn };
                let (shorter_tag, mate_tag) = if skew > 0.0 { (tn, tp) } else { (tp, tn) };
                let mut need = skew.abs() - pc.skew_budget_mm * 0.5;
                let mut changed = false;
                for t in tails.iter_mut().filter(|t| t.net == shorter_net) {
                    while need > pitch * 0.5 {
                        let before = t.cells.len();
                        if !self.tail_detour(&g2, &mut t.cells, need, shorter_tag, mate_tag) {
                            break;
                        }
                        let added = (t.cells.len() - before) as f64 * pitch;
                        need -= added;
                        changed = true;
                    }
                }
                if changed {
                    let mut g3 = grid.clone();
                    self.stamp_pair(&mut g3, &corridor, side, layer, &p_cells, &n_cells, [tps, tpe, tns, tne], pn, nn, w, w_eff, vp, gap_of[pn]);
                    for t in &tails {
                        self.stamp_path(&mut g3, &t.cells, t.net, t.width, t.from_pad);
                    }
                    g2 = g3;
                    let p_len1 = poly_len(&p_track) + tail_len(&tails[0]) + tail_len(&tails[2]);
                    let n_len1 = poly_len(&n_track) + tail_len(&tails[1]) + tail_len(&tails[3]);
                    skew = p_len1 - n_len1;
                }
            }
            if skew.abs() > pc.skew_budget_mm {
                // Lengthen the shorter track with trapezoid bumps (45° legs)
                let (shorter, tag, mate, side_sign) = if skew > 0.0 { (&mut n_track, tn, tp, -side) } else { (&mut p_track, tp, tn, side) };
                let need = skew.abs();
                let (added, nb_) = self.add_bumps(&mut g2, shorter, layer, need, w, tag, mate, side_sign, pitch, if skew > 0.0 { vp[5] } else { vp[2] });
                bumps = nb_;
                if skew > 0.0 { skew -= added } else { skew += added }
            }
            // Commit
            *grid = g2;
            *dsu = d2;
            dsu.union(vp[0], vp[1]);
            dsu.union(vp[0], vp[2]);
            dsu.union(vp[3], vp[4]);
            dsu.union(vp[3], vp[5]);
            dsu.union(pa, vp[0]);
            dsu.union(pb, vp[1]);
            dsu.union(na, vp[3]);
            dsu.union(nb, vp[4]);
            let coupled = (corridor.len().saturating_sub(1)) as f64 * pitch;
            let p_len0 = poly_len(&p_track) + tail_len(&tails[0]) + tail_len(&tails[2]) - if skew < 0.0 { 0.0 } else { 0.0 };
            let n_len0 = poly_len(&n_track) + tail_len(&tails[1]) + tail_len(&tails[3]);
            let _ = (p_len0, n_len0);
            rep.routed = true;
            rep.layer = self.config.signal_layers[layer as usize].clone();
            rep.coupled_mm = coupled;
            rep.tail_max_mm = tails.iter().map(|t| tail_len(t)).fold(0.0, f64::max);
            rep.p_len_mm = if skew >= 0.0 { n_len0 + skew } else { p_len0 };
            rep.n_len_mm = if skew >= 0.0 { n_len0 } else { p_len0 - skew };
            rep.skew_mm = skew;
            rep.corners = corners;
            rep.bumps = bumps;
            rep.cause = if skew.abs() > pc.skew_budget_mm { format!("skew {:.3}mm exceeds budget {:.3}mm (no room for compensation bumps)", skew, pc.skew_budget_mm) } else { String::new() };
            state.pairs.push(PairRoute { cfg: ci, p_net: pn, n_net: nn, layer, corridor, side, p_track, n_track, tails, vpads: vp, report: rep.clone() });
            return rep;
        }
        rep.cause = last_cause;
        rep
    }

    /// Direction-aware A* for the pair centreline on one layer.
    #[allow(clippy::too_many_arguments)]
    fn search_corridor(
        &self,
        grid: &Grid,
        layer: u8,
        body: i32,
        k: i32,
        straight_len: i32,
        tp: u32,
        tn: u32,
        start_mid: (f64, f64),
        start_vec: (f64, f64),
        start_away: (f64, f64),
        goal_mid: (f64, f64),
        goal_vec: (f64, f64),
        goal_toward: (f64, f64),
        side: i32,
        goal_side: i32,
        forbid: &std::collections::HashSet<Cell>,
        radius: i32,
        gap: f64,
    ) -> Option<(Vec<Cell>, f64)> {
        let leaves_a = |d: u8| -> bool {
            let (dx, dy) = DIRS[d as usize];
            dx as f64 * start_away.0 + dy as f64 * start_away.1 >= -1e-9
        };
        let enters_b = |d: u8| -> bool {
            let (dx, dy) = DIRS[d as usize];
            dx as f64 * goal_toward.0 + dy as f64 * goal_toward.1 >= -1e-9
        };
        let turn_penalty = (0.5 / self.config.grid).max(1.0); // ≈0.5 mm per corner
        let near = self.near_radius_index(gap);
        let free_cell = |c: Cell| -> bool {
            if forbid.contains(&c) {
                return false;
            }
            if c.x < 0 || c.y < 0 || (c.x as usize) >= grid.w || (c.y as usize) >= grid.h {
                return false;
            }
            for dy in -body..=body {
                for dx in -body..=body {
                    let q = Cell { x: c.x + dx, y: c.y + dy, layer: c.layer };
                    let Some(i) = grid.idx(q.x, q.y, q.layer) else { return false };
                    let o = grid.owner[i];
                    if o != FREE && o != tp && o != tn {
                        return false;
                    }
                    let lane = grid.lane[q.y as usize * grid.w + q.x as usize];
                    if lane != 0 && lane != tp && lane != tn {
                        return false;
                    }
                    let x = grid.xt[i];
                    if x != FREE && x != tp && x != tn {
                        return false;
                    }
                    if !grid.exempt[i] {
                        if let Some(m) = near {
                            let v = if m == 0 { grid.near3[i] } else { grid.near5[i] };
                            if v != FREE && v != tp && v != tn {
                                return false;
                            }
                        }
                    }
                }
            }
            true
        };
        // The approach cells behind a start (ahead of a goal) at ±k must be
        // free for the tails: centre and both terminals' approach cells.
        let approach_free = |c: Cell, d: u8, behind: bool| -> bool {
            let (dx, dy) = DIRS[d as usize];
            let (px, py) = perp(d);
            let s = if behind { -1 } else { 1 };
            for j in [-k, 0, k] {
                for step in 1..=straight_len {
                    let q = Cell { x: c.x + s * step * dx + j * px, y: c.y + s * step * dy + j * py, layer: c.layer };
                    let Some(i) = grid.idx(q.x, q.y, q.layer) else { return false };
                    let o = grid.owner[i];
                    if o != FREE && o != tp && o != tn {
                        return false;
                    }
                }
            }
            true
        };
        let manh = |c: &Cell, m: (f64, f64)| (c.x as f64 - m.0).abs() + (c.y as f64 - m.1).abs();
        // The goal zone is as wide as the start zone: the lanes + escape
        // band of a fine-pitch connector already span ~4 mm from its pads.
        let goal_r = radius as f64;
        // Admissible everywhere: from any cell the corridor must still reach
        // a goal cell and the far tails cost 2× that cell's distance to the
        // pads, which is never less than the straight distance from here.
        let h = |c: &Cell| manh(c, goal_mid);
        let mut open = BinaryHeap::new();
        let mut came: std::collections::HashMap<DState, DState> = std::collections::HashMap::new();
        let mut g_score: std::collections::HashMap<DState, f64> = std::collections::HashMap::new();
        let (sx, sy) = (start_mid.0.round() as i32, start_mid.1.round() as i32);
        for y in (sy - radius)..=(sy + radius) {
            for x in (sx - radius)..=(sx + radius) {
                let c = Cell { x, y, layer };
                if manh(&c, start_mid) > radius as f64 || !free_cell(c) {
                    continue;
                }
                for d in 0..4u8 {
                    if side_of(d, start_vec) != side || !leaves_a(d) || !approach_free(c, d, true) {
                        continue;
                    }
                    // The corridor leaves straight for `straight_len` cells
                    // (a corner near the start pokes a track into the other
                    // side and its halo into the approach cells — both
                    // live-verified). Pre-extend the start by straight steps.
                    let (dx, dy) = DIRS[d as usize];
                    let chain: Vec<Cell> = (0..straight_len).map(|i| Cell { x: c.x + i * dx, y: c.y + i * dy, layer }).collect();
                    if chain.iter().skip(1).any(|q| !free_cell(*q)) {
                        continue;
                    }
                    // Symmetric tails: prefer starts on the pads' bisector
                    let along = ((c.x as f64 - start_mid.0) * start_vec.0 + (c.y as f64 - start_mid.1) * start_vec.1).abs() / (start_vec.0.hypot(start_vec.1)).max(1e-9);
                    let g0 = 2.0 * manh(&c, start_mid) + 2.0 * along;
                    let last = DState { cell: *chain.last().unwrap(), dir: d };
                    let g_last = g0 + (straight_len - 1) as f64;
                    if g_score.get(&last).map_or(true, |g| g_last < *g) {
                        let mut prev: Option<DState> = None;
                        for (i, q) in chain.iter().enumerate() {
                            let st = DState { cell: *q, dir: d };
                            g_score.insert(st, g0 + i as f64);
                            if let Some(p) = prev {
                                came.insert(st, p);
                            }
                            prev = Some(st);
                        }
                        open.push(Node2 { st: last, f: g_last + h(&last.cell), g: g_last, done: false });
                    }
                }
            }
        }
        let debug = std::env::var("SOMEDA_ROUTE_DEBUG").is_ok();
        let n_starts = open.len();
        if open.is_empty() {
            if debug {
                eprintln!("route-debug: CORRIDOR layer {} side {}/{}: no valid start cell within {} cells of ({:.1},{:.1})", layer, side, goal_side, radius, start_mid.0, start_mid.1);
            }
            return None;
        }
        let (mut near_goal, mut bad_side, mut bad_enter, mut bad_approach, mut bad_straight) = (0usize, 0usize, 0usize, 0usize, 0usize);
        let mut expansions = 0usize;
        while let Some(node) = open.pop() {
            expansions += 1;
            if expansions > self.config.max_expansions {
                if debug {
                    eprintln!("route-debug: CORRIDOR layer {} side {}: budget after {} expansions", layer, side, expansions);
                }
                return None;
            }
            let st = node.st;
            if node.done {
                let mut path = vec![st];
                let mut cur = st;
                while let Some(&prev) = came.get(&cur) {
                    path.push(prev);
                    cur = prev;
                }
                path.reverse();
                let cells: Vec<Cell> = path.iter().map(|s| s.cell).collect();
                return Some((cells, node.f));
            }
            let g_here = *g_score.get(&st).unwrap_or(&f64::INFINITY);
            if node.g > g_here + 1e-9 {
                continue;
            }
            // Goal: inside the goal disc, arriving with the right handedness,
            // with free approach cells ahead, and a straight run of ≥ 2k
            // cells behind (the terminal jog needs it).
            let straight_in = {
                let mut cur = st;
                let mut n = 1;
                while n < straight_len {
                    match came.get(&cur) {
                        Some(p) if p.dir == st.dir => { cur = *p; n += 1; }
                        _ => break,
                    }
                }
                n >= straight_len
            };
            if manh(&st.cell, goal_mid) <= goal_r {
                near_goal += 1;
                if side_of(st.dir, goal_vec) != goal_side { bad_side += 1; }
                else if !enters_b(st.dir) { bad_enter += 1; }
                else if !approach_free(st.cell, st.dir, false) { bad_approach += 1; }
                else if !straight_in { bad_straight += 1; }
            }
            if manh(&st.cell, goal_mid) <= goal_r && side_of(st.dir, goal_vec) == goal_side && enters_b(st.dir) && approach_free(st.cell, st.dir, false) && g_here > 0.0 && straight_in {
                // Goal-eligible: costed with its far tails (2× the distance
                // from here to the pads), accepted only when it is the
                // cheapest thing left in the heap.
                open.push(Node2 { st, f: g_here + 2.0 * manh(&st.cell, goal_mid), g: g_here, done: true });
            }
            for nd in 0..4u8 {
                if (nd + 2) % 4 == st.dir {
                    continue; // no U-turn
                }
                let (dx, dy) = DIRS[nd as usize];
                let nc = Cell { x: st.cell.x + dx, y: st.cell.y + dy, layer };
                if !free_cell(nc) {
                    continue;
                }
                let turn = if nd != st.dir { turn_penalty } else { 0.0 };
                let extra = grid.idx(nc.x, nc.y, layer).map(|i| {
                    let iso = grid.cost[i] as f64 * 0.25;
                    let sideways = match grid.band_axis[i] {
                        1 => dy != 0,
                        2 => dx != 0,
                        _ => false,
                    };
                    iso + if sideways { self.config.band_side_cost } else { 0.0 }
                }).unwrap_or(0.0);
                let ns = DState { cell: nc, dir: nd };
                let tentative = g_here + 1.0 + turn + extra;
                if tentative < *g_score.get(&ns).unwrap_or(&f64::INFINITY) {
                    came.insert(ns, st);
                    g_score.insert(ns, tentative);
                    open.push(Node2 { st: ns, f: tentative + h(&nc), g: tentative, done: false });
                }
            }
        }
        if debug {
            eprintln!(
                "route-debug: CORRIDOR layer {} side {}/{}: exhausted after {} expansions from {} starts; goal-zone visits {} (wrong side {}, not entering {}, approach blocked {}, not straight {})",
                layer, side, goal_side, expansions, n_starts, near_goal, bad_side, bad_enter, bad_approach, bad_straight
            );
        }
        None
    }

    /// Offset the centreline into the two track polylines (cell units)
    /// and the copper cells each occupies. Returns (p_track, n_track,
    /// p_cells, n_cells, corners, dirs).
    fn build_tracks(&self, corridor: &[Cell], side: i32, o_mm: f64, k: i32, w_mm: f64, pitch: f64) -> (Vec<(f64, f64)>, Vec<(f64, f64)>, Vec<Cell>, Vec<Cell>, usize, Vec<u8>) {
        // Directions per step
        let dirs: Vec<u8> = corridor.windows(2).map(|s| {
            let d = (s[1].x - s[0].x, s[1].y - s[0].y);
            DIRS.iter().position(|&x| x == d).unwrap_or(0) as u8
        }).collect();
        let o = o_mm / pitch; // offset in cells (fractional)
        let layer = corridor[0].layer;
        let track = |sigma: i32| -> Vec<(f64, f64)> {
            let s = sigma as f64;
            let mut pts: Vec<(f64, f64)> = Vec::new();
            let d0 = dirs[0];
            let p0 = perp(d0);
            let c0 = corridor[0];
            // terminal at ±k cells, then jog to ±o
            pts.push((c0.x as f64 + s * k as f64 * p0.0 as f64, c0.y as f64 + s * k as f64 * p0.1 as f64));
            let jog = (k as f64 - o).abs();
            if jog > 1e-6 {
                let (dx, dy) = DIRS[d0 as usize];
                pts.push((c0.x as f64 + dx as f64 * jog + s * o * p0.0 as f64, c0.y as f64 + dy as f64 * jog + s * o * p0.1 as f64));
            }
            for i in 1..corridor.len() - 1 {
                let (din, dout) = (dirs[i - 1], dirs[i]);
                if din != dout {
                    let (pi, po) = (perp(din), perp(dout));
                    let c = corridor[i];
                    pts.push((c.x as f64 + s * o * (pi.0 + po.0) as f64, c.y as f64 + s * o * (pi.1 + po.1) as f64));
                }
            }
            let dl = *dirs.last().unwrap();
            let pl = perp(dl);
            let cl = *corridor.last().unwrap();
            if jog > 1e-6 {
                let (dx, dy) = DIRS[dl as usize];
                pts.push((cl.x as f64 - dx as f64 * jog + s * o * pl.0 as f64, cl.y as f64 - dy as f64 * jog + s * o * pl.1 as f64));
            }
            pts.push((cl.x as f64 + s * k as f64 * pl.0 as f64, cl.y as f64 + s * k as f64 * pl.1 as f64));
            pts
        };
        let p_track = track(side);
        let n_track = track(-side);
        let corners = dirs.windows(2).filter(|s| s[0] != s[1]).count();
        let p_cells = self.rasterize(&p_track, layer, w_mm, pitch);
        let n_cells = self.rasterize(&n_track, layer, w_mm, pitch);
        (p_track, n_track, p_cells, n_cells, corners, dirs)
    }

    /// Cells whose centres lie within w/2 of a polyline (cell units),
    /// ordered along the polyline, first = start terminal, last = end.
    fn rasterize(&self, poly: &[(f64, f64)], layer: u8, w_mm: f64, pitch: f64) -> Vec<Cell> {
        let r = (w_mm / 2.0) / pitch + 1e-6;
        let mut out: Vec<Cell> = Vec::new();
        let mut push = |c: Cell| {
            if !out.contains(&c) {
                out.push(c);
            }
        };
        for s in poly.windows(2) {
            let (ax, ay, bx, by) = (s[0].0, s[0].1, s[1].0, s[1].1);
            let len = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
            let steps = (len / 0.25).ceil().max(1.0) as i32;
            for i in 0..=steps {
                let t = i as f64 / steps as f64;
                let (x, y) = (ax + (bx - ax) * t, ay + (by - ay) * t);
                for cy in (y - r).floor() as i32..=(y + r).ceil() as i32 {
                    for cx in (x - r).floor() as i32..=(x + r).ceil() as i32 {
                        // distance from cell centre to the segment
                        let (vx, vy) = (bx - ax, by - ay);
                        let tt = if len < 1e-9 { 0.0 } else { (((cx as f64 - ax) * vx + (cy as f64 - ay) * vy) / (len * len)).clamp(0.0, 1.0) };
                        let (px, py) = (ax + vx * tt, ay + vy * tt);
                        let d = ((cx as f64 - px).powi(2) + (cy as f64 - py).powi(2)).sqrt();
                        if d <= r {
                            push(Cell { x: cx, y: cy, layer });
                        }
                    }
                }
            }
        }
        if out.is_empty() {
            out.push(Cell { x: poly[0].0.round() as i32, y: poly[0].1.round() as i32, layer });
        }
        out
    }

    /// Stamp a pair's corridor: halos split by side (P's tag on P's side,
    /// N's on N's, centreline BLOCK), copper of each track with its virtual
    /// pad id (terminals first/last cells get the start/end ids, the rest
    /// the body id), crosstalk halo for both nets, near maps.
    #[allow(clippy::too_many_arguments)]
    /// Cell under a polyline point (cell units).
    fn cell_at(p: (f64, f64), layer: u8) -> Cell {
        Cell { x: p.0.round() as i32, y: p.1.round() as i32, layer }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn stamp_pair(&self, grid: &mut Grid, corridor: &[Cell], side: i32, layer: u8, p_cells: &[Cell], n_cells: &[Cell], terms: [Cell; 4], pn: usize, nn: usize, w: f64, w_eff: f64, vp: [u32; 6], gap: f64) {
        let (tp, tn) = (pn as u32 + 1, nn as u32 + 1);
        let halo = self.halo_cells(w_eff);
        let dirs: Vec<u8> = corridor.windows(2).map(|s| {
            let d = (s[1].x - s[0].x, s[1].y - s[0].y);
            DIRS.iter().position(|&x| x == d).unwrap_or(0) as u8
        }).collect();
        for (i, c) in corridor.iter().enumerate() {
            let d = dirs[i.min(dirs.len() - 1)];
            let p = perp(d);
            for dy in -halo..=halo {
                for dx in -halo..=halo {
                    let sv = dx * p.0 + dy * p.1;
                    let tag = if sv == 0 { BLOCK } else if (sv > 0) == (side > 0) { tp } else { tn };
                    if tag == BLOCK {
                        grid.claim_halo(c.x + dx, c.y + dy, layer, tp);
                        grid.claim_halo(c.x + dx, c.y + dy, layer, tn);
                    } else {
                        grid.claim_halo(c.x + dx, c.y + dy, layer, tag);
                    }
                    if let Some(ix) = grid.idx(c.x + dx, c.y + dy, layer) {
                        grid.corridor[ix] = true;
                    }
                }
            }
        }
        // Tails may only touch the corridor from behind its start and
        // ahead of its end: those approach cells stay open.
        for (c, d, back) in [(corridor[0], dirs[0], true), (*corridor.last().unwrap(), *dirs.last().unwrap(), false)] {
            let (dx, dy) = DIRS[d as usize];
            let p = perp(d);
            let s = if back { -1 } else { 1 };
            for step in 0..=(halo + 2) {
                for j in -(halo + 1)..=(halo + 1) {
                    let q = (c.x + s * step * dx + j * p.0, c.y + s * step * dy + j * p.1);
                    if let Some(ix) = grid.idx(q.0, q.1, layer) {
                        grid.corridor[ix] = false;
                    }
                }
            }
        }
        // Copper: terminal cells carry the start/end virtual pad ids (the
        // tails' goals), everything else the body id
        for c in p_cells {
            let id = if *c == terms[0] { vp[0] } else if *c == terms[1] { vp[1] } else { vp[2] };
            grid.force(c.x, c.y, layer, tp, id);
        }
        for c in n_cells {
            let id = if *c == terms[2] { vp[3] } else if *c == terms[3] { vp[4] } else { vp[5] };
            grid.force(c.x, c.y, layer, tn, id);
        }
        for (i, t) in terms.iter().enumerate() {
            let (tag, id) = if i < 2 { (tp, vp[i]) } else { (tn, vp[i + 1]) };
            grid.force(t.x, t.y, layer, tag, id);
        }
        // Crosstalk halo of the pair (both nets, mate-aware) + near maps.
        // The rule is measured from each TRACK, which sits o cells off the
        // centreline: grow the corridor radius by that offset.
        let (_, xr0) = self.xt_radius(gap, w);
        let xr = if xr0 > 0 { xr0 + ((w_eff - w) / 2.0 / self.config.grid).ceil() as i32 } else { 0 };
        for c in corridor {
            for dy in -xr..=xr {
                for dx in -xr..=xr {
                    if let Some(i) = grid.idx(c.x + dx, c.y + dy, layer) {
                        if !grid.exempt[i] {
                            Grid::claim_mate(&mut grid.xt, i, tp, tn);
                        }
                    }
                }
            }
        }
        for (cells, tag, mate) in [(p_cells, tp, tn), (n_cells, tn, tp)] {
            self.stamp_near(grid, cells, w, tag, mate);
        }
        let _ = w;
    }

    /// Lengthen `track` by `need` mm with trapezoid bumps (45° legs, depth
    /// ≤ 0.4 mm each) on its straight runs, away from the mate. Returns
    /// (length added, bumps placed). Each bump's copper is stamped.
    #[allow(clippy::too_many_arguments)]
    fn add_bumps(&self, grid: &mut Grid, track: &mut Vec<(f64, f64)>, layer: u8, need_mm: f64, w: f64, tag: u32, mate: u32, side_sign: i32, pitch: f64, body_id: u32) -> (f64, usize) {
        let a_max = 0.3 / pitch; // cells (fits inside the pair's own 5W halo)
        let gain = 2.0 * (std::f64::consts::SQRT_2 - 1.0); // added length per unit depth
        let mut remaining = need_mm / pitch;
        let mut bumps = 0usize;
        let halo = self.halo_cells(w);
        let mut i = 0usize;
        while i + 1 < track.len() && remaining > 1e-6 {
            let (a, b) = (track[i], track[i + 1]);
            let (vx, vy) = (b.0 - a.0, b.1 - a.1);
            let len = (vx * vx + vy * vy).sqrt();
            let axis_aligned = vx.abs() < 1e-9 || vy.abs() < 1e-9;
            if std::env::var("SOMEDA_ROUTE_DEBUG").map(|v| v == "2").unwrap_or(false) {
                eprintln!("route-debug: BUMP? run {} len {:.1} axis {} remaining {:.1}", i, len, axis_aligned, remaining);
            }
            if !axis_aligned || len < 6.0 {
                i += 1;
                continue;
            }
            let depth = (remaining / gain).min(a_max);
            let foot = 2.0 * depth; // along-track footprint of the bump
            if foot + 2.0 > len {
                i += 1;
                continue;
            }
            // Outward normal: away from the mate = sign given by caller
            let (ux, uy) = (vx / len, vy / len);
            let (nx, ny) = (-uy * side_sign as f64, ux * side_sign as f64);
            // Start one cell into the run so the remainder stays usable
            let s0 = 1.0;
            let p1 = (a.0 + ux * s0, a.1 + uy * s0);
            let p2 = (p1.0 + ux * depth + nx * depth, p1.1 + uy * depth + ny * depth);
            let p3 = (p2.0 + ux * depth - nx * depth, p2.1 + uy * depth - ny * depth);
            // Every cell the bump copper + halo touches must be free or ours
            let cells = self.rasterize(&[p1, p2, p3], layer, w, pitch);
            // Every cell the bump copper + halo touches on the OUTWARD side
            // must be free or ours (inward lies the pair's own gap/mate).
            let mut ok = true;
            'chk: for c in &cells {
                for dy in -halo..=halo {
                    for dx in -halo..=halo {
                        let q = Cell { x: c.x + dx, y: c.y + dy, layer };
                        let inward = (q.x as f64 - a.0) * nx + (q.y as f64 - a.1) * ny < -1e-9;
                        if inward {
                            continue;
                        }
                        let o = grid.owner_at(q);
                        if o != FREE && o != tag && o != mate {
                            ok = false;
                            break 'chk;
                        }
                    }
                }
            }
            if std::env::var("SOMEDA_ROUTE_DEBUG").map(|v| v == "2").unwrap_or(false) {
                eprintln!("route-debug: BUMP run {} len {:.1} depth {:.2} cells {} ok {}", i, len, depth, cells.len(), ok);
            }
            if !ok {
                i += 1;
                continue;
            }
            for c in &cells {
                for dy in -halo..=halo {
                    for dx in -halo..=halo {
                        grid.claim(c.x + dx, c.y + dy, layer, tag);
                    }
                }
            }
            for c in &cells {
                grid.force(c.x, c.y, layer, tag, body_id);
            }
            self.stamp_near(grid, &cells, w, tag, mate);
            track.splice(i + 1..i + 1, [p1, p2, p3]);
            remaining -= depth * gain;
            bumps += 1;
            i += 3; // continue on the remainder of this run (p3 → b)
        }
        ((need_mm / pitch - remaining) * pitch, bumps)
    }

    /// Insert one rectangular detour into a tail's cell path: out `k`
    /// cells sideways, along the run, back — adding 2k cells. Picks the
    /// longest straight run with free cells on one side. Returns false
    /// when no run can take a detour.
    fn tail_detour(&self, grid: &Grid, cells: &mut Vec<Cell>, need_mm: f64, tag: u32, mate: u32) -> bool {
        let pitch = self.config.grid;
        if cells.len() < 4 {
            return false;
        }
        let want_k = ((need_mm / (2.0 * pitch)).round() as i32).clamp(1, 4);
        let ok = |c: Cell| -> bool {
            match grid.idx(c.x, c.y, c.layer) {
                Some(i) => {
                    let o = grid.owner[i];
                    let lane = grid.lane[c.y as usize * grid.w + c.x as usize];
                    let x = grid.xt[i];
                    (o == FREE || o == tag) && (lane == 0 || lane == tag) && (x == FREE || x == tag || x == mate) && !grid.corridor[i]
                }
                None => false,
            }
        };
        // straight runs: (start index, end index inclusive, dir)
        let mut runs: Vec<(usize, usize, (i32, i32))> = Vec::new();
        let mut i = 0;
        while i + 1 < cells.len() {
            if cells[i].layer != cells[i + 1].layer {
                i += 1;
                continue;
            }
            let d = (cells[i + 1].x - cells[i].x, cells[i + 1].y - cells[i].y);
            let mut j = i + 1;
            while j + 1 < cells.len() && cells[j + 1].layer == cells[j].layer && (cells[j + 1].x - cells[j].x, cells[j + 1].y - cells[j].y) == d {
                j += 1;
            }
            if j - i >= 3 {
                runs.push((i, j, d));
            }
            i = j;
        }
        runs.sort_by(|a, b| (b.1 - b.0).cmp(&(a.1 - a.0)));
        for (i0, j0, d) in runs {
            // keep one cell at each end of the run untouched
            let (i, j) = (i0 + 1, j0 - 1);
            if j <= i + 1 {
                continue;
            }
            let layer = cells[i].layer;
            for sgn in [1i32, -1] {
                let p = (-d.1 * sgn, d.0 * sgn);
                let mut k = want_k;
                'try_k: while k >= 1 {
                    let mut detour: Vec<Cell> = Vec::new();
                    for t in 1..=k {
                        detour.push(Cell { x: cells[i].x + t * p.0, y: cells[i].y + t * p.1, layer });
                    }
                    for idx in (i + 1)..=j {
                        detour.push(Cell { x: cells[idx].x + k * p.0, y: cells[idx].y + k * p.1, layer });
                    }
                    for t in (1..k).rev() {
                        detour.push(Cell { x: cells[j].x + t * p.0, y: cells[j].y + t * p.1, layer });
                    }
                    // every detour cell and its clearance halo must be free or ours
                    let halo = self.halo_cells(self.config.trace_width);
                    for c in &detour {
                        for dy in -halo..=halo {
                            for dx in -halo..=halo {
                                let q = Cell { x: c.x + dx, y: c.y + dy, layer };
                                // the run's own cells are ours
                                if !ok(q) {
                                    k -= 1;
                                    continue 'try_k;
                                }
                            }
                        }
                    }
                    let mut new_cells: Vec<Cell> = cells[..=i].to_vec();
                    new_cells.extend(detour);
                    new_cells.extend_from_slice(&cells[j..]);
                    *cells = new_cells;
                    return true;
                }
            }
        }
        false
    }

    /// Re-stamp routed pairs into a rebuilt grid (rip-up replay).
    pub(super) fn replay_pairs(&self, grid: &mut Grid, dsu: &mut Dsu, pads: &[RPad], pairs: &[PairRoute], gap_of: &[f64]) {
        let pitch = self.config.grid;
        for pr in pairs {
            let pc = &self.config.pairs[pr.cfg];
            let w = pc.width_mm;
            let w_eff = pc.pitch_mm + w;
            let p_cells = self.rasterize(&pr.p_track, pr.layer, w, pitch);
            let n_cells = self.rasterize(&pr.n_track, pr.layer, w, pitch);
            let terms = [
                Self::cell_at(pr.p_track[0], pr.layer),
                Self::cell_at(*pr.p_track.last().unwrap(), pr.layer),
                Self::cell_at(pr.n_track[0], pr.layer),
                Self::cell_at(*pr.n_track.last().unwrap(), pr.layer),
            ];
            self.stamp_pair(grid, &pr.corridor, pr.side, pr.layer, &p_cells, &n_cells, terms, pr.p_net, pr.n_net, w, w_eff, pr.vpads, gap_of[pr.p_net]);
            for t in &pr.tails {
                self.stamp_path(grid, &t.cells, t.net, t.width, t.from_pad);
            }
            let vp = pr.vpads;
            dsu.union(vp[0], vp[1]);
            dsu.union(vp[0], vp[2]);
            dsu.union(vp[3], vp[4]);
            dsu.union(vp[3], vp[5]);
            for t in &pr.tails {
                let virt = if t.net == pr.p_net { vp[0] } else { vp[3] };
                dsu.union(t.from_pad, virt);
            }
        }
        let _ = pads;
    }

    /// Emit a routed pair: the two track polylines as segments plus the
    /// tails as ordinary paths.
    pub(super) fn emit_pair(&self, result: &mut RouteResult, pr: &PairRoute, net_name: &dyn Fn(usize) -> String, ox: f64, oy: f64) {
        let pitch = self.config.grid;
        let pc = &self.config.pairs[pr.cfg];
        let layer = self.config.signal_layers[pr.layer as usize].clone();
        for (track, net) in [(&pr.p_track, pr.p_net), (&pr.n_track, pr.n_net)] {
            let name = net_name(net);
            for s in track.windows(2) {
                if (s[0].0 - s[1].0).abs() < 1e-9 && (s[0].1 - s[1].1).abs() < 1e-9 {
                    continue;
                }
                result.segments.push(TrackSegment {
                    layer: layer.clone(),
                    net: (net, name.clone()),
                    x1: s[0].0 * pitch + ox,
                    y1: s[0].1 * pitch + oy,
                    x2: s[1].0 * pitch + ox,
                    y2: s[1].1 * pitch + oy,
                    width: pc.width_mm,
                });
            }
        }
        for t in &pr.tails {
            self.emit_path(result, &t.cells, (t.net, net_name(t.net)), t.width, ox, oy);
        }
    }
}

/// A* node with a direction-aware state.
#[derive(Debug, Clone, Copy)]
struct Node2 {
    st: DState,
    f: f64,
    g: f64,
    /// A goal-eligible state, costed with its far tails; accepted when popped.
    done: bool,
}
impl PartialEq for Node2 {
    fn eq(&self, o: &Self) -> bool {
        self.f == o.f
    }
}
impl Eq for Node2 {}
impl PartialOrd for Node2 {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Node2 {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        o.f.partial_cmp(&self.f).unwrap_or(std::cmp::Ordering::Equal)
    }
}
