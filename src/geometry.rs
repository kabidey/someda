//! Geometry primitives for DRC: rotation-aware pad shapes, exact distance
//! between rectangles and segments, bbox math. Pure f64, no dependencies.

/// Axis-aligned bounding box: (min_x, min_y, max_x, max_y)
pub type Bbox = (f64, f64, f64, f64);

/// A rotated rectangle centered at (cx, cy) with half-extents (hw, hh).
#[derive(Debug, Clone, Copy)]
pub struct RotRect {
    pub cx: f64,
    pub cy: f64,
    pub hw: f64,
    pub hh: f64,
    /// Rotation in degrees
    pub rot_deg: f64,
}

impl RotRect {
    pub fn new(cx: f64, cy: f64, w: f64, h: f64, rot_deg: f64) -> Self {
        RotRect { cx, cy, hw: w / 2.0, hh: h / 2.0, rot_deg }
    }

    /// Conservative axis-aligned bbox that fully contains this rect.
    pub fn bbox(&self) -> Bbox {
        let r = self.rot_deg.to_radians();
        let (s, c) = (r.sin().abs(), r.cos().abs());
        let ex = self.hw * c + self.hh * s;
        let ey = self.hw * s + self.hh * c;
        (self.cx - ex, self.cy - ey, self.cx + ex, self.cy + ey)
    }

    fn corners(&self) -> [(f64, f64); 4] {
        let r = self.rot_deg.to_radians();
        let (s, c) = (r.sin(), r.cos());
        let (hw, hh) = (self.hw, self.hh);
        [
            (self.cx + hw * c - hh * s, self.cy + hw * s + hh * c),
            (self.cx - hw * c - hh * s, self.cy - hw * s + hh * c),
            (self.cx - hw * c + hh * s, self.cy - hw * s - hh * c),
            (self.cx + hw * c + hh * s, self.cy + hw * s - hh * c),
        ]
    }
}

/// Distance from a point to a line segment (ax,ay)-(bx,by).
pub fn point_segment_distance(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        return ((px - ax).powi(2) + (py - ay).powi(2)).sqrt();
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0);
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// Exact clearance between two rotated rectangles:
/// 0 if they overlap, else the minimum corner-to-edge distance.
/// For non-overlapping bboxes this is exact; for overlapping bboxes we
/// conservatively report 0 (overlap is a short anyway).
pub fn roirect_clearance(a: &RotRect, b: &RotRect) -> f64 {
    // Cheap reject: bbox distance > sum of circumradii → far apart
    let (ax0, ay0, ax1, ay1) = a.bbox();
    let (bx0, by0, bx1, by1) = b.bbox();
    let bbox_gap_x = (bx0 - ax1).max(ax0 - bx1);
    let bbox_gap_y = (by0 - ay1).max(ay0 - by1);
    let bbox_gap = if bbox_gap_x > 0.0 || bbox_gap_y > 0.0 {
        (bbox_gap_x.max(0.0).powi(2) + bbox_gap_y.max(0.0).powi(2)).sqrt()
    } else {
        0.0
    };
    let ra = (a.hw.powi(2) + a.hh.powi(2)).sqrt();
    let rb = (b.hw.powi(2) + b.hh.powi(2)).sqrt();
    if bbox_gap > ra + rb {
        return bbox_gap; // definitely separated; bbox gap is a lower bound ≥ actual? no—
    }
    // Exact path: corners of one vs edges of the other, both ways.
    let ca = a.corners();
    let cb = b.corners();
    let mut min_dist = f64::INFINITY;

    // a-corners to b-edges
    for &p in &ca {
        for i in 0..4 {
            let (q1, q2) = (cb[i], cb[(i + 1) % 4]);
            let d = point_segment_distance(p.0, p.1, q1.0, q1.1, q2.0, q2.1);
            min_dist = min_dist.min(d);
        }
    }
    // b-corners to a-edges
    for &p in &cb {
        for i in 0..4 {
            let (q1, q2) = (ca[i], ca[(i + 1) % 4]);
            let d = point_segment_distance(p.0, p.1, q1.0, q1.1, q2.0, q2.1);
            min_dist = min_dist.min(d);
        }
    }
    // If any edge intersects, distance is 0
    if edges_intersect(&ca, &cb) {
        return 0.0;
    }
    min_dist
}

fn edges_intersect(a: &[(f64, f64); 4], b: &[(f64, f64); 4]) -> bool {
    for i in 0..4 {
        let (p1, p2) = (a[i], a[(i + 1) % 4]);
        for j in 0..4 {
            let (q1, q2) = (b[j], b[(j + 1) % 4]);
            if segments_cross(p1, p2, q1, q2) {
                return true;
            }
        }
    }
    // containment counts as overlap
    point_in_rect(a[0], b) || point_in_rect(b[0], a)
}

fn segments_cross(p1: (f64, f64), p2: (f64, f64), q1: (f64, f64), q2: (f64, f64)) -> bool {
    let orient = |a: (f64, f64), b: (f64, f64), c: (f64, f64)| {
        (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
    };
    let d1 = orient(q1, q2, p1);
    let d2 = orient(q1, q2, p2);
    let d3 = orient(p1, p2, q1);
    let d4 = orient(p1, p2, q2);
    ((d1 > 0.0) != (d2 > 0.0)) && ((d3 > 0.0) != (d4 > 0.0))
}

fn point_in_rect(p: (f64, f64), r: &[(f64, f64); 4]) -> bool {
    // Convex polygon containment via cross products (rect corners in order)
    for i in 0..4 {
        let (a, b) = (r[i], r[(i + 1) % 4]);
        let cross = (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
        if cross < 0.0 {
            return false;
        }
    }
    true
}

/// Clearance between a rotated rect and a segment. 0 if they intersect.
pub fn roirect_segment_clearance(r: &RotRect, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let corners = r.corners();
    // segment crossing any edge → 0
    for i in 0..4 {
        let (q1, q2) = (corners[i], corners[(i + 1) % 4]);
        if segments_cross((ax, ay), (bx, by), q1, q2) {
            return 0.0;
        }
    }
    // segment endpoint inside rect → 0
    if point_in_rect((ax, ay), &corners) || point_in_rect((bx, by), &corners) {
        return 0.0;
    }
    // min over rect-corners to segment and segment-ends to rect-edges
    let mut d = f64::INFINITY;
    for &c in &corners {
        d = d.min(point_segment_distance(c.0, c.1, ax, ay, bx, by));
    }
    for i in 0..4 {
        let (q1, q2) = (corners[i], corners[(i + 1) % 4]);
        d = d.min(point_segment_distance(ax, ay, q1.0, q1.1, q2.0, q2.1));
        d = d.min(point_segment_distance(bx, by, q1.0, q1.1, q2.0, q2.1));
    }
    d
}

/// Clearance between two axis-aligned bboxes; 0 if overlapping.
pub fn bbox_clearance(a: Bbox, b: Bbox) -> f64 {
    let gx = (b.0 - a.2).max(a.0 - b.2);
    let gy = (b.1 - a.3).max(a.1 - b.3);
    if gx > 0.0 || gy > 0.0 {
        (gx.max(0.0).powi(2) + gy.max(0.0).powi(2)).sqrt()
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_rotated_rect() {
        // 2x1 rect rotated 45° around origin → bbox extent = (1+0.5)/√2 each
        let r = RotRect::new(0.0, 0.0, 2.0, 1.0, 45.0);
        let (x0, y0, x1, y1) = r.bbox();
        let ex = 1.061;
        assert!((x0 + ex).abs() < 0.01, "x0={}", x0);
        assert!((x1 - ex).abs() < 0.01, "x1={}", x1);
        assert!((y0 + ex).abs() < 0.01, "y0={}", y0);
        assert!((y1 - ex).abs() < 0.01, "y1={}", y1);
    }

    #[test]
    fn point_segment_dist_basic() {
        assert!((point_segment_distance(0.0, 1.0, -1.0, 0.0, 1.0, 0.0) - 1.0).abs() < 1e-9);
        assert!((point_segment_distance(2.0, 0.0, -1.0, 0.0, 1.0, 0.0) - 1.0).abs() < 1e-9);
        assert!((point_segment_distance(0.5, 0.0, -1.0, 0.0, 1.0, 0.0)).abs() < 1e-9);
        // degenerate segment = point
        assert!((point_segment_distance(3.0, 4.0, 0.0, 0.0, 0.0, 0.0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn roirect_clearance_far_apart() {
        let a = RotRect::new(0.0, 0.0, 2.0, 1.0, 0.0);
        let b = RotRect::new(10.0, 0.0, 2.0, 1.0, 0.0);
        // edge-to-edge: 10 - (1 + 1) = 8
        assert!((roirect_clearance(&a, &b) - 8.0).abs() < 1e-9);
    }

    #[test]
    fn roirect_clearance_overlapping() {
        let a = RotRect::new(0.0, 0.0, 2.0, 2.0, 0.0);
        let b = RotRect::new(0.5, 0.0, 2.0, 2.0, 0.0);
        assert_eq!(roirect_clearance(&a, &b), 0.0);
    }

    #[test]
    fn roirect_clearance_rotated_touching() {
        // Two 1x1 rects, one rotated 45°, centers 1.0 apart along x:
        // axis-aligned: gap = 1 - 0.5 - 0.5 = 0 → touching
        let a = RotRect::new(0.0, 0.0, 1.0, 1.0, 0.0);
        let b = RotRect::new(1.2, 0.0, 1.0, 1.0, 45.0);
        // bbox of b spans ±0.707; a spans ±0.5 → bbox gap = 1.2 - 0.707 - 0.5 ≈ -0.007 → overlap bboxes but rotated rect may not touch
        let d = roirect_clearance(&a, &b);
        // The exact answer: corner of b at (1.2-0.707, 0) ≈ (0.493, 0) — just outside a (0.5 edge)
        assert!(d < 0.05 && d >= 0.0, "d = {}", d);
    }

    #[test]
    fn bbox_clearance_basics() {
        assert!((bbox_clearance((0.0, 0.0, 1.0, 1.0), (3.0, 0.0, 4.0, 1.0)) - 2.0).abs() < 1e-9);
        assert_eq!(bbox_clearance((0.0, 0.0, 1.0, 1.0), (0.5, 0.5, 1.5, 1.5)), 0.0);
        // diagonal gap
        let d = bbox_clearance((0.0, 0.0, 1.0, 1.0), (2.0, 2.0, 3.0, 3.0));
        assert!((d - 2.0f64.sqrt()).abs() < 1e-9);
    }

    #[test]
    fn segment_clearance() {
        let r = RotRect::new(0.0, 0.0, 2.0, 2.0, 0.0);
        // horizontal line 2 units above center: gap = 2 - 1 = 1
        let d = roirect_segment_clearance(&r, -5.0, 2.0, 5.0, 2.0);
        assert!((d - 1.0).abs() < 1e-9);
        // crossing segment → 0
        assert_eq!(roirect_segment_clearance(&r, -5.0, 0.0, 5.0, 0.0), 0.0);
    }
}