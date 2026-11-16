//! Screen geometry: which outer edge a point touches, and where to enter.
//!
//! Positions along an edge are fractions (0..=1) of that side's span, so they
//! map correctly between machines with different resolutions and DPI scales.

use desklink_proto::{Edge, Monitor};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn contains(&self, p: (i32, i32)) -> bool {
        p.0 >= self.x && p.0 < self.right() && p.1 >= self.y && p.1 < self.bottom()
    }
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// Where a director parks its own cursor while its input goes to a peer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Park {
    #[default]
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
    Center,
}

/// Inset from the edge when placing the cursor on entry, so the very next
/// motion back towards the edge (and only that) returns it.
const ENTRY_INSET: i32 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct Screens {
    pub rects: Vec<Rect>,
    primary: usize,
}

impl Screens {
    pub fn new(monitors: &[Monitor]) -> Self {
        let rects: Vec<Rect> = monitors
            .iter()
            .map(|m| Rect { x: m.x, y: m.y, w: m.width, h: m.height })
            .collect();
        let primary = monitors.iter().position(|m| m.primary).unwrap_or(0);
        Screens { rects, primary }
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub fn primary_center(&self) -> (i32, i32) {
        self.rects.get(self.primary).map(Rect::center).unwrap_or((0, 0))
    }

    /// Where the director parks its cursor while input goes remote, on the
    /// primary monitor.
    pub fn park_point(&self, park: Park) -> (i32, i32) {
        let Some(r) = self.rects.get(self.primary) else {
            return (0, 0);
        };
        let (l, t, rt, b) = (r.x, r.y, r.right() - 1, r.bottom() - 1);
        match park {
            Park::BottomRight => (rt, b),
            Park::BottomLeft => (l, b),
            Park::TopRight => (rt, t),
            Park::TopLeft => (l, t),
            Park::Center => r.center(),
        }
    }

    pub fn monitor_at(&self, p: (i32, i32)) -> Option<&Rect> {
        self.rects.iter().find(|r| r.contains(p))
    }

    fn nearest(&self, p: (i32, i32)) -> Option<&Rect> {
        self.monitor_at(p).or_else(|| {
            self.rects.iter().min_by_key(|r| {
                let dx = (r.x - p.0).max(p.0 - (r.right() - 1)).max(0) as i64;
                let dy = (r.y - p.1).max(p.1 - (r.bottom() - 1)).max(0) as i64;
                dx * dx + dy * dy
            })
        })
    }

    fn covered(&self, p: (i32, i32)) -> bool {
        self.monitor_at(p).is_some()
    }

    /// The pixel just beyond `edge` of `r` at coordinate `along`.
    fn beyond(r: &Rect, edge: Edge, along: i32) -> (i32, i32) {
        match edge {
            Edge::Left => (r.x - 1, along),
            Edge::Right => (r.right(), along),
            Edge::Top => (along, r.y - 1),
            Edge::Bottom => (along, r.bottom()),
        }
    }

    /// Span (start, length) of the outermost monitors on `edge`, along it.
    fn side_span(&self, edge: Edge) -> Option<(i32, i32)> {
        let outer = self.outermost(edge);
        let (mut lo, mut hi) = (i32::MAX, i32::MIN);
        for r in outer {
            let (a, b) = match edge {
                Edge::Left | Edge::Right => (r.y, r.bottom()),
                Edge::Top | Edge::Bottom => (r.x, r.right()),
            };
            lo = lo.min(a);
            hi = hi.max(b);
        }
        (lo < hi).then_some((lo, hi - lo))
    }

    fn outermost(&self, edge: Edge) -> impl Iterator<Item = &Rect> {
        let key = move |r: &Rect| match edge {
            Edge::Left => -r.x,
            Edge::Right => r.right(),
            Edge::Top => -r.y,
            Edge::Bottom => r.bottom(),
        };
        let best = self.rects.iter().map(key).max().unwrap_or(0);
        self.rects.iter().filter(move |r| key(r) == best)
    }

    fn fraction(&self, edge: Edge, along: i32) -> f32 {
        match self.side_span(edge) {
            Some((start, len)) if len > 1 => {
                ((along - start) as f32 / (len - 1) as f32).clamp(0.0, 1.0)
            }
            _ => 0.5,
        }
    }

    /// The (clipped) cursor point `p` touches an outer edge: the edge, and
    /// `p` (on this desktop's border).
    pub fn touched(&self, p: (i32, i32)) -> Option<(Edge, (i32, i32))> {
        let r = self.monitor_at(p)?;
        let checks = [
            (Edge::Left, p.0 <= r.x, p.1),
            (Edge::Right, p.0 >= r.right() - 1, p.1),
            (Edge::Top, p.1 <= r.y, p.0),
            (Edge::Bottom, p.1 >= r.bottom() - 1, p.0),
        ];
        checks
            .into_iter()
            .find(|&(edge, at, along)| at && !self.covered(Self::beyond(r, edge, along)))
            .map(|(edge, _, _)| (edge, p))
    }

    /// Moving from `from` to `to`, does the cursor leave the desktop? The
    /// crossed edge and the last point on this desktop before it.
    pub fn exit(&self, from: (i32, i32), to: (i32, i32)) -> Option<(Edge, (i32, i32))> {
        if self.covered(to) {
            return None;
        }
        let r = self.nearest(from)?;
        let along_v = to.1.clamp(r.y, r.bottom() - 1);
        let along_h = to.0.clamp(r.x, r.right() - 1);
        let (edge, at) = if to.0 < r.x {
            (Edge::Left, (r.x, along_v))
        } else if to.0 >= r.right() {
            (Edge::Right, (r.right() - 1, along_v))
        } else if to.1 < r.y {
            (Edge::Top, (along_h, r.y))
        } else {
            (Edge::Bottom, (along_h, r.bottom() - 1))
        };
        // Moving into a gap between monitors isn't leaving the desktop.
        if self.covered(Self::beyond(r, edge, along(edge, at))) {
            return None;
        }
        Some((edge, at))
    }

    /// Is border point `at` on `edge` within `size` px of a corner of the
    /// desktop (a monitor's end where the perpendicular side is an outer
    /// edge too)? Inner corners, where another monitor continues, don't count.
    pub fn near_corner(&self, edge: Edge, at: (i32, i32), size: i32) -> bool {
        let Some(r) = self.nearest(at) else { return false };
        let outer = |p: (i32, i32)| !self.covered(p);
        match edge {
            Edge::Left | Edge::Right => {
                (at.1 - r.y < size && outer((at.0, r.y - 1))) || (r.bottom() - 1 - at.1 < size && outer((at.0, r.bottom())))
            }
            Edge::Top | Edge::Bottom => {
                (at.0 - r.x < size && outer((r.x - 1, at.1))) || (r.right() - 1 - at.0 < size && outer((r.right(), at.1)))
            }
        }
    }

    /// Position (0..=1) of border point `p` along this desktop's `edge`.
    pub fn fraction_at(&self, edge: Edge, p: (i32, i32)) -> f32 {
        self.fraction(edge, along(edge, p))
    }

    /// Director side: the edge touched and the position along that side.
    pub fn touched_edge(&self, p: (i32, i32)) -> Option<(Edge, f32)> {
        self.touched(p).map(|(e, at)| (e, self.fraction_at(e, at)))
    }

    /// Client side: the edge left and the position along it.
    pub fn exit_edge(&self, from: (i32, i32), to: (i32, i32)) -> Option<(Edge, f32)> {
        self.exit(from, to).map(|(e, at)| (e, self.fraction_at(e, at)))
    }

    /// Where the cursor enters this desktop through `edge` at `pos`.
    pub fn entry_point(&self, edge: Edge, pos: f32) -> (i32, i32) {
        let Some((start, len)) = self.side_span(edge) else {
            return self.primary_center();
        };
        let along = start + (pos.clamp(0.0, 1.0) * (len - 1) as f32).round() as i32;
        // The outermost monitor covering `along` (spans can have gaps).
        let r = self
            .outermost(edge)
            .find(|r| match edge {
                Edge::Left | Edge::Right => (r.y..r.bottom()).contains(&along),
                Edge::Top | Edge::Bottom => (r.x..r.right()).contains(&along),
            })
            .or_else(|| self.outermost(edge).next())
            .copied()
            .unwrap();
        match edge {
            Edge::Left => (r.x + ENTRY_INSET, along.clamp(r.y, r.bottom() - 1)),
            Edge::Right => (r.right() - 1 - ENTRY_INSET, along.clamp(r.y, r.bottom() - 1)),
            Edge::Top => (along.clamp(r.x, r.right() - 1), r.y + ENTRY_INSET),
            Edge::Bottom => (along.clamp(r.x, r.right() - 1), r.bottom() - 1 - ENTRY_INSET),
        }
    }

    /// Clamp a point onto the desktop (nearest monitor).
    pub fn clamp(&self, p: (i32, i32)) -> (i32, i32) {
        match self.nearest(p) {
            Some(r) => (p.0.clamp(r.x, r.right() - 1), p.1.clamp(r.y, r.bottom() - 1)),
            None => p,
        }
    }
}


/// The coordinate of `p` that varies along `edge`.
fn along(edge: Edge, p: (i32, i32)) -> i32 {
    match edge {
        Edge::Left | Edge::Right => p.1,
        Edge::Top | Edge::Bottom => p.0,
    }
}

/// Largest gap between two PCs' screens on the desk that the cursor jumps.
pub const DESK_GAP: i32 = 64;

/// Every PC's screens placed on one desk: `pcs[i]` is PC i's screens and the
/// offset from its own coordinates to desk coordinates (`None`: not placed).
/// From PC `from` at its border point `p` crossing `edge`: the nearest screen
/// of another PC right beyond that point. Returns that PC and where the
/// cursor enters it, in its own coordinates.
pub fn across(pcs: &[Option<(&Screens, (i32, i32))>], from: usize, p: (i32, i32), edge: Edge) -> Option<(usize, (i32, i32))> {
    let (_, off) = pcs.get(from)?.as_ref()?;
    let d = (p.0 + off.0, p.1 + off.1);
    // (distance, pc, entry point on the desk)
    let mut best: Option<(i32, usize, (i32, i32))> = None;
    for (i, pc) in pcs.iter().enumerate() {
        let Some((screens, o)) = pc.as_ref().filter(|_| i != from) else { continue };
        for r in &screens.rects {
            let r = Rect { x: r.x + o.0, y: r.y + o.1, w: r.w, h: r.h };
            let (dist, facing, entry) = match edge {
                Edge::Right => (r.x - (d.0 + 1), (r.y..r.bottom()).contains(&d.1), (r.x + ENTRY_INSET, d.1)),
                Edge::Left => (d.0 - r.right(), (r.y..r.bottom()).contains(&d.1), (r.right() - 1 - ENTRY_INSET, d.1)),
                Edge::Bottom => (r.y - (d.1 + 1), (r.x..r.right()).contains(&d.0), (d.0, r.y + ENTRY_INSET)),
                Edge::Top => (d.1 - r.bottom(), (r.x..r.right()).contains(&d.0), (d.0, r.bottom() - 1 - ENTRY_INSET)),
            };
            // Touching, a small gap, or a pixel or two of overlap.
            if facing && (-2..=DESK_GAP).contains(&dist) && best.is_none_or(|b| dist < b.0) {
                best = Some((dist, i, entry));
            }
        }
    }
    let (_, i, e) = best?;
    let (_, o) = pcs[i].as_ref()?;
    Some((i, (e.0 - o.0, e.1 - o.1)))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, y: i32, w: i32, h: i32, primary: bool) -> Monitor {
        Monitor { x, y, width: w, height: h, scale_percent: 100, primary, name: String::new() }
    }

    /// The user's director: 4K primary, 1920x1200 to its right.
    fn director() -> Screens {
        Screens::new(&[mon(0, 0, 3840, 2160, true), mon(3840, 0, 1920, 1200, false)])
    }

    #[test]
    fn left_edge_of_director() {
        let s = director();
        assert_eq!(s.touched_edge((0, 0)), Some((Edge::Left, 0.0)));
        assert_eq!(s.touched_edge((0, 2159)), Some((Edge::Left, 1.0)));
        let (e, pos) = s.touched_edge((0, 1080)).unwrap();
        assert_eq!(e, Edge::Left);
        assert!((pos - 0.5).abs() < 0.001);
    }

    #[test]
    fn inner_boundary_is_not_an_edge() {
        let s = director();
        // Right side of the 4K where the second monitor continues.
        assert_eq!(s.touched_edge((3839, 500)), None);
        // Right side of the 4K below the second monitor is an outer edge.
        assert_eq!(s.touched_edge((3839, 1500)).map(|e| e.0), Some(Edge::Right));
        assert_eq!(s.touched_edge((1000, 1000)), None);
    }

    #[test]
    fn entry_maps_fraction_across_resolutions() {
        // Client: single 1080p. Entering from its right edge at the middle.
        let client = Screens::new(&[mon(0, 0, 1920, 1080, true)]);
        let (x, y) = client.entry_point(Edge::Right, 0.5);
        assert_eq!(x, 1918);
        assert!((538..=541).contains(&y));
        // And back on the director's left edge at the same fraction.
        let (x, y) = director().entry_point(Edge::Left, 0.5);
        assert_eq!(x, 1);
        assert!((1078..=1081).contains(&y));
    }

    #[test]
    fn exit_only_when_leaving_desktop() {
        let client = Screens::new(&[mon(0, 0, 1920, 1080, true)]);
        assert_eq!(client.exit_edge((1900, 500), (1910, 505)), None);
        let (e, pos) = client.exit_edge((1915, 540), (1925, 540)).unwrap();
        assert_eq!(e, Edge::Right);
        assert!((pos - 0.5).abs() < 0.01);
        // Moving between the director's two monitors is not an exit.
        assert_eq!(director().exit_edge((3830, 500), (3850, 500)), None);
    }


    #[test]
    fn crossing_points_are_exact() {
        let s = director();
        assert_eq!(s.touched((0, 700)), Some((Edge::Left, (0, 700))));
        assert_eq!(s.exit((5, 700), (-30, 710)), Some((Edge::Left, (0, 710))));
        assert_eq!(s.exit((5000, 1190), (5000, 1230)), Some((Edge::Bottom, (5000, 1199))));
    }


    #[test]
    fn corners_are_only_outer_corners() {
        let s = director();
        // The 4K's left edge: near its top and bottom ends.
        assert!(s.near_corner(Edge::Left, (0, 5), 15));
        assert!(s.near_corner(Edge::Left, (0, 2150), 15));
        assert!(!s.near_corner(Edge::Left, (0, 1080), 15));
        // The 4K's top edge near its right end: the 1920x1200 continues
        // there, so that is no corner of the desktop.
        assert!(!s.near_corner(Edge::Top, (3835, 0), 15));
        // The 1920x1200's bottom edge near its left end: the 4K continues
        // below, an inner corner too; near its right end, a real corner.
        assert!(!s.near_corner(Edge::Bottom, (3845, 1199), 15));
        assert!(s.near_corner(Edge::Bottom, (5755, 1199), 15));
    }
    #[test]
    fn desk_routes_between_placed_pcs() {
        let d = director();
        // A 1920x1080 PC left of the 4K, its top 540 px down; another below
        // the 1920x1200 monitor.
        let left = Screens::new(&[mon(0, 0, 1920, 1080, true)]);
        let below = Screens::new(&[mon(0, 0, 1920, 1080, true)]);
        let pcs = [Some((&d, (0, 0))), Some((&left, (-1920, 540))), Some((&below, (3840, 1200)))];
        // 4K left edge at y=1000 lands in the left PC at its y=460.
        assert_eq!(across(&pcs, 0, (0, 1000), Edge::Left), Some((1, (1918, 460))));
        // Above or below the left PC: nothing there.
        assert_eq!(across(&pcs, 0, (0, 200), Edge::Left), None);
        assert_eq!(across(&pcs, 0, (0, 1700), Edge::Left), None);
        // Back from the left PC's right edge.
        assert_eq!(across(&pcs, 1, (1919, 460), Edge::Right), Some((0, (1, 1000))));
        // The PC below the 1920x1200 monitor; going left from it lands on the
        // left: it touches the lower part of the 4K (taller than its neighbour).
        assert_eq!(across(&pcs, 0, (4000, 1199), Edge::Bottom), Some((2, (160, 1))));
        assert_eq!(across(&pcs, 2, (0, 500), Edge::Left), Some((0, (3838, 1700))));
        // Unplaced PCs are skipped.
        let pcs = [Some((&d, (0, 0))), None, None];
        assert_eq!(across(&pcs, 0, (0, 1000), Edge::Left), None);
    }
    #[test]
    fn negative_coordinates() {
        // Secondary monitor to the left of primary has negative x.
        let s = Screens::new(&[mon(-1920, 0, 1920, 1080, false), mon(0, 0, 2560, 1440, true)]);
        assert_eq!(s.touched_edge((-1920, 10)).map(|e| e.0), Some(Edge::Left));
        assert_eq!(s.touched_edge((0, 10)), None);
        assert_eq!(s.entry_point(Edge::Left, 0.0), (-1919, 0));
    }
}
