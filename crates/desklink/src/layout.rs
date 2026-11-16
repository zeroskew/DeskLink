//! The Layout tab's model: this PC's screens and the other PCs placed around
//! them on one desk (physical pixels, in this PC's screen coordinates).

use desklink_config::{Peer, Side, UNKNOWN_SCREEN};
use desklink_proto::Monitor;

/// x, y, w, h.
pub type Rect = (i32, i32, i32, i32);


#[derive(Clone, Debug, PartialEq)]
pub struct PlacedPeer {
    pub name: String,
    pub side: Side,
    /// Desk offset of its screen coordinates (`None`: placed by side).
    pub position: Option<(i32, i32)>,
    pub monitors: Vec<Monitor>,
    /// In the list only, not on the layout yet.
    pub unplaced: bool,
}

impl PlacedPeer {
    pub fn from_config(p: &Peer) -> PlacedPeer {
        PlacedPeer { name: p.name.clone(), side: p.side, position: p.position.map(|[x, y]| (x, y)), monitors: p.monitors.clone(), unplaced: p.unplaced }
    }

    /// Its screens in its own coordinates (a placeholder if unknown).
    fn own_rects(&self) -> Vec<Rect> {
        if self.monitors.is_empty() {
            return vec![(0, 0, UNKNOWN_SCREEN.0, UNKNOWN_SCREEN.1)];
        }
        self.monitors.iter().map(|m| (m.x, m.y, m.width, m.height)).collect()
    }
}

/// (x0, y0, x1, y1) around `rects`.
fn bounds(rects: &[Rect]) -> (i32, i32, i32, i32) {
    rects.iter().fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |b, r| {
        (b.0.min(r.0), b.1.min(r.1), b.2.max(r.0 + r.2), b.3.max(r.1 + r.3))
    })
}

fn moved(rects: &[Rect], by: (i32, i32)) -> Vec<Rect> {
    rects.iter().map(|r| (r.0 + by.0, r.1 + by.1, r.2, r.3)).collect()
}

fn overlaps(a: &Rect, b: &Rect) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

#[derive(Clone, Debug)]
pub struct Desk {
    pub local: Vec<Rect>,
    pub peers: Vec<PlacedPeer>,
}

impl Desk {
    pub fn new(local: &[Monitor], peers: Vec<PlacedPeer>) -> Desk {
        let mut local: Vec<Rect> = local.iter().map(|m| (m.x, m.y, m.width, m.height)).collect();
        if local.is_empty() {
            local.push((0, 0, UNKNOWN_SCREEN.0, UNKNOWN_SCREEN.1));
        }
        Desk { local, peers }
    }

    /// Desk offset of peer `i`: its position, or next to this PC on its side,
    /// centred (further out for a second PC on the same side).
    pub fn offset(&self, i: usize) -> (i32, i32) {
        let peer = &self.peers[i];
        if let Some(p) = peer.position {
            return p;
        }
        let (bx0, by0, bx1, by1) = bounds(&peer.own_rects());
        let (lx0, ly0, lx1, ly1) = bounds(&self.local);
        let k = self.peers[..i].iter().filter(|p| !p.unplaced && p.position.is_none() && p.side == peer.side).count() as i32;
        let (w, h) = (bx1 - bx0, by1 - by0);
        let (cx, cy) = ((lx0 + lx1) / 2, (ly0 + ly1) / 2);
        match peer.side {
            Side::Left => (lx0 - bx1 - k * w, cy - (by0 + by1) / 2),
            Side::Right => (lx1 - bx0 + k * w, cy - (by0 + by1) / 2),
            Side::Top => (cx - (bx0 + bx1) / 2, ly0 - by1 - k * h),
            Side::Bottom => (cx - (bx0 + bx1) / 2, ly1 - by0 + k * h),
        }
    }

    /// Peer `i`'s screens on the desk.
    pub fn peer_rects(&self, i: usize) -> Vec<Rect> {
        if self.peers[i].unplaced {
            return Vec::new(); // in the list only
        }
        moved(&self.peers[i].own_rects(), self.offset(i))
    }

    /// Where a PC of unknown size would go on `side` (an "add a PC" spot).
    pub fn free_spot(&self, side: Side) -> Rect {
        let probe = Desk {
            local: self.local.clone(),
            peers: vec![PlacedPeer { name: String::new(), side, position: None, monitors: Vec::new(), unplaced: false }],
        };
        probe.peer_rects(0)[0]
    }

    /// The side of this PC that `rects` are nearest to.
    pub fn side_of(&self, rects: &[Rect]) -> Side {
        let l = bounds(&self.local);
        let p = bounds(rects);
        let dx = ((p.0 + p.2) - (l.0 + l.2)) as f32 / ((l.2 - l.0) + (p.2 - p.0)) as f32;
        let dy = ((p.1 + p.3) - (l.1 + l.3)) as f32 / ((l.3 - l.1) + (p.3 - p.1)) as f32;
        if dx.abs() >= dy.abs() {
            if dx < 0.0 { Side::Left } else { Side::Right }
        } else if dy < 0.0 {
            Side::Top
        } else {
            Side::Bottom
        }
    }

    /// Peer `i` dropped with its centre at `centre` (desk coordinates).
    pub fn drop_at(&mut self, i: usize, centre: (i32, i32)) -> bool {
        let (x0, y0, x1, y1) = bounds(&self.peers[i].own_rects());
        self.place(i, (centre.0 - (x0 + x1) / 2, centre.1 - (y0 + y1) / 2))
    }

    /// Put peer `i` where it was dropped (`offset`): attached to the nearest
    /// free side of the nearest screen, centred on that edge (a PC touching
    /// nothing could never be reached). False (nothing changes) only if no
    /// side is free.
    pub fn place(&mut self, i: usize, offset: (i32, i32)) -> bool {
        let own = self.peers[i].own_rects();
        let fixed: Vec<Rect> = self
            .local
            .iter()
            .copied()
            .chain((0..self.peers.len()).filter(|&j| j != i).flat_map(|j| self.peer_rects(j)))
            .collect();
        let Some(at) = nearest_centred(&own, &fixed, offset) else { return false };
        let placed = moved(&own, at);
        let side = self.side_of(&placed);
        let peer = &mut self.peers[i];
        peer.position = Some(at);
        peer.side = side;
        peer.unplaced = false;
        true
    }
}

/// Does any of `moving` share an edge (some length of it) with `fixed`?
#[cfg(test)]
fn touches(moving: &[Rect], fixed: &[Rect]) -> bool {
    moving.iter().any(|m| {
        fixed.iter().any(|f| {
            let beside = (m.0 + m.2 == f.0 || f.0 + f.2 == m.0) && m.1 < f.1 + f.3 && f.1 < m.1 + m.3;
            let stacked = (m.1 + m.3 == f.1 || f.1 + f.3 == m.1) && m.0 < f.0 + f.2 && f.0 < m.0 + m.2;
            beside || stacked
        })
    })
}

/// The offset nearest `want` at which `own` sits against a side of one of
/// `fixed`, centred on it, without covering any.
fn nearest_centred(own: &[Rect], fixed: &[Rect], want: (i32, i32)) -> Option<(i32, i32)> {
    let (bx0, by0, bx1, by1) = bounds(own);
    let (w, h) = (bx1 - bx0, by1 - by0);
    let mut best: Option<((i32, i32), i64)> = None;
    for f in fixed {
        let (cx, cy) = (f.0 + f.2 / 2 - w / 2, f.1 + f.3 / 2 - h / 2);
        // Top-left corner of the group on each side of `f`.
        for (x, y) in [(f.0 - w, cy), (f.0 + f.2, cy), (cx, f.1 - h), (cx, f.1 + f.3)] {
            let at = (x - bx0, y - by0);
            if moved(own, at).iter().any(|m| fixed.iter().any(|g| overlaps(m, g))) {
                continue;
            }
            let d = (at.0 - want.0) as i64 * (at.0 - want.0) as i64 + (at.1 - want.1) as i64 * (at.1 - want.1) as i64;
            if best.is_none_or(|b| d < b.1) {
                best = Some((at, d));
            }
        }
    }
    best.map(|b| b.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, y: i32, w: i32, h: i32) -> Monitor {
        Monitor { x, y, width: w, height: h, scale_percent: 100, primary: x == 0 && y == 0, name: String::new() }
    }

    fn desk() -> Desk {
        // The user's 4K plus 1920x1200 to its right; flash (1080p) left.
        Desk::new(
            &[mon(0, 0, 3840, 2160), mon(3840, 0, 1920, 1200)],
            vec![PlacedPeer { name: "flash".into(), side: Side::Left, position: None, monitors: vec![mon(0, 0, 1920, 1080)], unplaced: false }],
        )
    }

    #[test]
    fn unplaced_peer_sits_centred_on_its_side() {
        let d = desk();
        assert_eq!(d.peer_rects(0), vec![(-1920, 540, 1920, 1080)]);
        assert_eq!(d.free_spot(Side::Top), (1920, -1080, 1920, 1080));
    }

    #[test]
    fn dropped_peer_attaches_centred_on_the_nearest_edge() {
        let mut d = desk();
        // Dropped a little off, near the 4K's top-left: centred on its left edge.
        assert!(d.place(0, (-1940, 15)));
        assert_eq!(d.peers[0].position, Some((-1920, 540)));
        assert_eq!(d.peers[0].side, Side::Left);
        // Below the 1920x1200: centred under it.
        assert!(d.place(0, (3850, 1230)));
        assert_eq!(d.peers[0].position, Some((3840, 1200)));
    }

    #[test]
    fn dropped_on_top_moves_to_the_nearest_free_edge() {
        let mut d = desk();
        // On the 4K, near its top: centred above it.
        assert!(d.place(0, (100, 100)));
        assert_eq!(d.peers[0].position, Some((960, -1080)));
        assert_eq!(d.peers[0].side, Side::Top);
    }


    #[test]
    fn an_unplaced_pc_is_off_the_layout_until_dropped() {
        let mut d = desk();
        d.peers[0].unplaced = true;
        assert!(d.peer_rects(0).is_empty());
        assert!(d.place(0, (-1940, 15)));
        assert!(!d.peers[0].unplaced);
        assert_eq!(d.peers[0].position, Some((-1920, 540)));
    }
    #[test]
    fn dropped_in_empty_space_attaches_to_the_nearest_screen() {
        let mut d = desk();
        // Far down-left, touching nothing: centred on the 4K's left edge.
        assert!(d.place(0, (-3000, 2600)));
        assert_eq!(d.peers[0].position, Some((-1920, 540)));
        assert!(touches(&d.peer_rects(0), &d.local));
    }
}
