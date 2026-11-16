//! The geometry of the arrangement, computed in screen units.
//!
//! Everything here is derived from the same two numbers the daemon routes by -
//! the segment of the local edge a client is reached through and the segment of
//! the client's facing edge its cursor lands in - so what the canvas shows *is*
//! the routing rather than a picture next to it. A stretch of edge with no
//! client on it is drawn as a wall because a crossing there reaches nobody.
//!
//! Screen units are pixels of the *local* screen: the local rectangle is
//! `screen_width x screen_height` and every peer is placed in the same space,
//! which is what lets two screens of different sizes be compared by eye.

use lan_mouse_ipc::{ClientHandle, EdgeRange, Position};

/// A rectangle in screen units.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Screen {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Screen {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// The length of `position`'s edge: `Left`/`Right` edges are measured
    /// vertically, `Top`/`Bottom` edges horizontally.
    pub fn extent_along(&self, position: Position) -> f64 {
        match position {
            Position::Left | Position::Right => self.h,
            Position::Top | Position::Bottom => self.w,
        }
    }

    /// Where the low end of `position` is - the top for a left/right edge, the
    /// left end for a top/bottom edge.
    pub fn edge_start(&self, position: Position) -> f64 {
        match position {
            Position::Left | Position::Right => self.y,
            Position::Top | Position::Bottom => self.x,
        }
    }

    /// The rectangle's position along the crossing axis of `position`.
    pub fn touching_start(&self, position: Position) -> f64 {
        self.edge_start(position)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.right() && y >= self.y && y <= self.bottom()
    }

    /// The bounding box of two rectangles.
    pub fn merged(self, other: Screen) -> Screen {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Screen::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }
}

/// The part of a peer's configuration the geometry depends on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub position: Position,
    /// the segment of the local edge the peer is reached through
    pub edge: EdgeRange,
    /// the segment of the peer's facing edge the cursor lands in
    pub peer_edge: EdgeRange,
    /// the peer's size in screen units
    pub size: (f64, f64),
}

/// The size to draw a peer at, in screen units.
///
/// A peer that reported its resolution is drawn with it - that is its real size,
/// and the whole point of the canvas is that a laptop and a 4K panel do not look
/// the same.
///
/// A peer that did not report one is sized so the two segments coincide: the
/// stretch of its edge the cursor enters through is as long as the stretch of our
/// edge it left through, so `our 0-50% -> its 0-100%` means its edge is half as
/// long as ours. That is the only size the data supports, and it is the size that
/// makes the picture consistent with the routing. The other dimension keeps the
/// local screen's aspect ratio, since nothing says otherwise.
pub fn peer_size(
    local: Screen,
    position: Position,
    edge: EdgeRange,
    peer_edge: EdgeRange,
    reported: Option<(f64, f64)>,
) -> (f64, f64) {
    if let Some(size) = reported.filter(|(w, h)| *w > 0.0 && *h > 0.0) {
        return size;
    }
    let edge_len = local.extent_along(position);
    let theirs = peer_edge.len() as f64;
    let touching = if theirs > 0.0 {
        edge_len * edge.len() as f64 / theirs
    } else {
        edge_len
    };
    let aspect = if local.h > 0.0 {
        local.w / local.h
    } else {
        16.0 / 9.0
    };
    match position {
        Position::Left | Position::Right => (touching * aspect, touching),
        Position::Top | Position::Bottom => (touching, touching / aspect),
    }
}

/// Place a peer around `local`.
///
/// The peer's touching edge is slid along itself until the start of its
/// `peer_edge` segment faces the start of the local `edge` segment. When the
/// peer's real size disagrees with that mapping - `our 0-50% -> its 0-100%` on two
/// identical screens - it sticks out past the local edge, and that is the honest
/// picture: the cursor is stretched across a longer edge than the one it left.
pub fn place(local: Screen, monitors: &[Screen], placement: &Placement) -> Screen {
    let position = placement.position;
    let edge_len = local.extent_along(position);
    let theirs = placement.size_along(position);
    let low = local.edge_start(position);
    let segment_lo = low + placement.edge.start() as f64 / 1000.0 * edge_len;
    let touching_lo = segment_lo - placement.peer_edge.start() as f64 / 1000.0 * theirs;
    /* Anchor to the monitor this link actually touches, not to the bounding box.
     *
     * `local` is the box around every monitor, and on a desk with unequal heights
     * its top edge can be a monitor the link does not touch at all: here the box's
     * top is the rotated panel's, while the link covers the stretch belonging to
     * the upright one, whose top is 1120 px lower. Anchoring to the box drew the
     * peer floating far above the screen it belongs to. The barrier is created per
     * monitor (`segment_barrier` uses that monitor's own edge), so this is also the
     * only anchor that makes the picture agree with the routing. */
    let anchor = anchor_monitor(local, monitors, placement);
    match position {
        Position::Left => Screen::new(
            anchor.x - GAP - placement.size.0,
            touching_lo,
            placement.size.0,
            placement.size.1,
        ),
        Position::Right => Screen::new(
            anchor.right() + GAP,
            touching_lo,
            placement.size.0,
            placement.size.1,
        ),
        Position::Top => Screen::new(
            touching_lo,
            anchor.y - GAP - placement.size.1,
            placement.size.0,
            placement.size.1,
        ),
        Position::Bottom => Screen::new(
            touching_lo,
            anchor.bottom() + GAP,
            placement.size.0,
            placement.size.1,
        ),
    }
}

/// The monitor a link is attached to: the one whose edge it covers most.
///
/// Falls back to the bounding box when nothing overlaps, so a placement is always
/// drawable - a link that covers no monitor is a configuration mistake the daemon
/// reports, not a reason to leave a card at the origin.
pub fn anchor_monitor(local: Screen, monitors: &[Screen], placement: &Placement) -> Screen {
    let position = placement.position;
    let edge_len = local.extent_along(position);
    if edge_len <= 0.0 || monitors.is_empty() {
        return local;
    }
    let low = local.edge_start(position);
    let segment_lo = low + placement.edge.start() as f64 / 1000.0 * edge_len;
    let segment_hi = low + placement.edge.end() as f64 / 1000.0 * edge_len;
    monitors
        .iter()
        .map(|monitor| {
            let (lo, hi) = match position {
                Position::Left | Position::Right => (monitor.y, monitor.bottom()),
                Position::Top | Position::Bottom => (monitor.x, monitor.right()),
            };
            let overlap = segment_hi.min(hi) - segment_lo.max(lo);
            (*monitor, overlap.max(0.0))
        })
        /* the largest overlap; on a tie the earlier monitor wins, so the anchor
         * does not change while a card is dragged across two equal ones */
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .filter(|(_, overlap)| *overlap > 0.0)
        .map(|(monitor, _)| monitor)
        .unwrap_or(local)
}

impl Placement {
    /// The length of the peer's facing edge, in screen units.
    fn size_along(&self, position: Position) -> f64 {
        match position {
            Position::Left | Position::Right => self.size.1,
            Position::Top | Position::Bottom => self.size.0,
        }
    }
}

/// Where a dragged rectangle ends up: the edge it is closest to, and the two
/// ranges its position implies.
///
/// This is the inverse of [`place`], and it is what makes dragging meaningful:
/// the user positions a screen, and the overlap of the two facing edges becomes
/// the routing. A drop that does not overlap the local edge is pulled back until
/// it does, because a peer nothing can reach is not a layout, it is a bug.
pub fn snap(local: Screen, monitors: &[Screen], candidate: Screen) -> Placement {
    let position = nearest_edge(monitors, local, candidate);
    let edge_len = local.extent_along(position);
    let low = local.edge_start(position);
    let touching_lo = candidate.touching_start(position);

    /* The overlap is measured against the monitor the card was dropped on, not
     * against the whole bounding box: on a desk with unequal heights the box's
     * edge includes stretches no monitor has, and using it would claim a link
     * covering area the pointer cannot even be in. */
    let anchor = nearest_monitor(monitors, local, candidate);
    let (anchor_lo, anchor_hi) = match position {
        Position::Left | Position::Right => (anchor.y, anchor.bottom()),
        Position::Top | Position::Bottom => (anchor.x, anchor.right()),
    };
    let theirs = match position {
        Position::Left | Position::Right => candidate.h,
        Position::Top | Position::Bottom => candidate.w,
    };

    /* the part of the peer's edge that actually faces the monitor's edge */
    const MIN_PERMILLE: f64 = 2.0;
    let anchor_len = anchor_hi - anchor_lo;
    let min_len = (anchor_len * MIN_PERMILLE / 1000.0).max(1.0);
    let mut overlap_lo = touching_lo.max(anchor_lo);
    let mut overlap_hi = (touching_lo + theirs).min(anchor_hi);
    if anchor_len <= 0.0 || overlap_hi <= overlap_lo {
        /* dropped clear off the monitor: centre it on wherever it landed so the
         * peer stays reachable instead of silently vanishing from the layout */
        let centre = (touching_lo + theirs / 2.0).clamp(anchor_lo, anchor_hi.max(anchor_lo + 1.0));
        overlap_lo = (centre - min_len / 2.0).max(anchor_lo);
        overlap_hi = (overlap_lo + min_len).min(anchor_hi.max(anchor_lo + 1.0));
    }

    let range = |lo: f64, hi: f64, origin: f64, total: f64| -> EdgeRange {
        if total <= 0.0 {
            return EdgeRange::FULL;
        }
        let start = (lo - origin) / total;
        let end = (hi - origin) / total;
        EdgeRange::from_fractions(start, end)
            // two positions a few pixels apart can round to the same permille;
            // widen by one step rather than dropping the peer's range entirely
            .or_else(|_| EdgeRange::from_fractions((start - 0.001).max(0.0), end.clamp(0.001, 1.0)))
            .unwrap_or(EdgeRange::FULL)
    };

    Placement {
        position,
        edge: range(overlap_lo, overlap_hi, low, edge_len),
        peer_edge: range(overlap_lo, overlap_hi, touching_lo, theirs),
        size: (candidate.w, candidate.h),
    }
}

/// The edge of the local screen a rectangle is being dragged towards.
///
/// Decided by which side of the local centre the candidate's centre is on, and
/// by how far: the dominant axis wins, so a screen dragged to the right of this
/// one goes to the right edge even when it also hangs below it. Measuring the
/// gap to each edge instead would pick the edge the candidate overlaps *most*,
/// which flips to the opposite side the moment a big screen is dropped near the
/// middle.
pub fn nearest_edge(monitors: &[Screen], local: Screen, candidate: Screen) -> Position {
    /* Measured against the monitor the card is being dropped on. Against the
     * bounding box, a desk with a rotated panel puts the box's centre in empty
     * space, and a card dragged up-and-left from it has a larger horizontal offset
     * than a vertical one - so "above the left-hand screen" was read as "left". */
    let reference = nearest_monitor(monitors, local, candidate);
    let (lx, ly) = reference.center();
    let (cx, cy) = candidate.center();
    let (dx, dy) = (cx - lx, cy - ly);
    if dx.abs() >= dy.abs() {
        if dx < 0.0 {
            Position::Left
        } else {
            Position::Right
        }
    } else if dy < 0.0 {
        Position::Top
    } else {
        Position::Bottom
    }
}

/// The monitor a rectangle is closest to, by the gap between the two.
///
/// Falls back to the bounding box when the caller knows no monitors, so every
/// placement stays well defined on a backend that reports no geometry.
pub fn nearest_monitor(monitors: &[Screen], local: Screen, candidate: Screen) -> Screen {
    if monitors.is_empty() {
        return local;
    }
    /* Overlap first: a card dropped on top of a monitor obviously belongs to it,
     * and the largest overlap is also the least surprising choice when it covers
     * two. */
    let overlap = |monitor: &Screen| {
        let dx = monitor.right().min(candidate.right()) - monitor.x.max(candidate.x);
        let dy = monitor.bottom().min(candidate.bottom()) - monitor.y.max(candidate.y);
        dx.max(0.0) * dy.max(0.0)
    };
    if let Some(monitor) = monitors
        .iter()
        .filter(|monitor| overlap(monitor) > 0.0)
        .max_by(|a, b| overlap(a).total_cmp(&overlap(b)))
    {
        return *monitor;
    }
    /* Otherwise the closest by the distance between the centres.
     *
     * Not the gap between the edges: a card that reaches exactly the far edge of
     * one monitor also touches the near edge of the next one, so the two gaps are
     * both zero and the wrong monitor wins. The centre distance has no such tie
     * unless the card is exactly between them, which a drop never is. */
    let (cx, cy) = candidate.center();
    monitors
        .iter()
        .min_by(|a, b| {
            let distance = |monitor: &Screen| {
                let (mx, my) = monitor.center();
                (mx - cx).powi(2) + (my - cy).powi(2)
            };
            distance(a).total_cmp(&distance(b))
        })
        .copied()
        .unwrap_or(local)
}

/// Split the whole edge into `count` equal, adjacent ranges.
///
/// "Auto arrange": every peer on an edge gets the same stretch, with neither a
/// gap to fall through nor an overlap to argue about.
pub fn auto_arrange(count: usize) -> Vec<(EdgeRange, EdgeRange)> {
    let count = count.max(1) as u16;
    (0..count)
        .map(|index| {
            let edge = EdgeRange::nth_of(count, index);
            (edge, EdgeRange::FULL)
        })
        .collect()
}

/// The space left between a peer and the local screen, in screen units.
///
/// Generous, because the link is drawn in it and because two screens touching
/// exactly would look like one.
pub const GAP: f64 = 60.0;

/// The gap between two lanes of peers on the same edge, in layout units.
pub const GUTTER: f64 = 40.0;

/// Place every peer, keeping the ones that would otherwise hide each other.
///
/// The position *along* the shared edge is what the daemon routes by, so it is
/// never changed here. The distance *from* the edge is not part of the routing,
/// and it is what makes a collision readable: peers whose cards overlap along the
/// edge are put in successive lanes, each one aligned with its own stretch of the
/// local edge and none hidden behind another.
///
/// The collision is real, not cosmetic. A peer sized from its own resolution
/// cannot in general fit the segment it was configured for: `our 0-55% -> its
/// 0-100%` on two identical screens means its card is twice as long as the stretch
/// it hangs on, so it reaches past the peer configured for `55-100%`.
pub fn place_all(
    local: Screen,
    monitors: &[Screen],
    placements: &[(ClientHandle, Placement)],
) -> Vec<(ClientHandle, Screen)> {
    let mut out = Vec::with_capacity(placements.len());
    for position in [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ] {
        let group = placements
            .iter()
            .filter(|(_, placement)| placement.position == position)
            .collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }
        let rects = group
            .iter()
            .map(|(_, placement)| place(local, monitors, placement))
            .collect::<Vec<_>>();
        let lane_of = lanes(position, &rects);
        let offsets = lane_offsets(position, &rects, &lane_of);
        for ((handle, placement), (rect, lane)) in group
            .iter()
            .map(|(handle, placement)| (*handle, *placement))
            .zip(
                rects
                    .iter()
                    .zip(&lane_of)
                    .map(|(rect, lane)| (*rect, *lane)),
            )
        {
            /* each card stacks outward from *its own* monitor, so two cards above
             * two monitors of different heights end up at the heights of those
             * monitors rather than both at the height of the box */
            let anchor = anchor_monitor(local, monitors, &placement);
            out.push((handle, shift(anchor, rect, position, offsets[lane])));
        }
    }
    out
}

/// Which lane each rectangle goes in: the first lane it does not overlap.
///
/// Greedy over the spans in ascending order, which is the standard interval
/// partitioning - it uses as few lanes as there are cards covering any one point
/// of the edge, and it is stable while a card is dragged, so a card does not jump
/// lanes under the pointer.
fn lanes(position: Position, rects: &[Screen]) -> Vec<usize> {
    let span = |rect: Screen| match position {
        Position::Left | Position::Right => (rect.y, rect.bottom()),
        Position::Top | Position::Bottom => (rect.x, rect.right()),
    };
    let mut order = (0..rects.len()).collect::<Vec<_>>();
    order.sort_by(|a, b| span(rects[*a]).0.total_cmp(&span(rects[*b]).0));
    let mut lane_of = vec![0usize; rects.len()];
    // the far end of the last card in each lane
    let mut ends: Vec<f64> = Vec::new();
    for index in order {
        let (start, stop) = span(rects[index]);
        match ends.iter().position(|end| start >= *end) {
            Some(lane) => ends[lane] = stop.max(ends[lane]),
            None => {
                ends.push(stop);
                lane_of[index] = ends.len() - 1;
            }
        }
    }
    lane_of
}

/// The distance of each lane from the local edge.
///
/// A lane is as deep as its widest card, so nothing in it is clipped by the lane
/// before it.
fn lane_offsets(position: Position, rects: &[Screen], lane_of: &[usize]) -> Vec<f64> {
    let depth = |rect: Screen| match position {
        Position::Left | Position::Right => rect.w,
        Position::Top | Position::Bottom => rect.h,
    };
    let mut depths: Vec<f64> = Vec::new();
    for (rect, lane) in rects.iter().zip(lane_of) {
        if *lane >= depths.len() {
            depths.resize(*lane + 1, 0.0);
        }
        depths[*lane] = depths[*lane].max(depth(*rect));
    }
    let mut offsets = Vec::with_capacity(depths.len());
    let mut offset = GAP;
    for depth in depths {
        offsets.push(offset);
        offset += depth + GUTTER;
    }
    offsets
}

/// Move a rectangle to `distance` from the local edge, keeping where it sits along
/// the edge.
fn shift(local: Screen, rect: Screen, position: Position, distance: f64) -> Screen {
    match position {
        Position::Left => Screen::new(local.x - distance - rect.w, rect.y, rect.w, rect.h),
        Position::Right => Screen::new(local.right() + distance, rect.y, rect.w, rect.h),
        Position::Top => Screen::new(rect.x, local.y - distance - rect.h, rect.w, rect.h),
        Position::Bottom => Screen::new(rect.x, local.bottom() + distance, rect.w, rect.h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `place` for a desk with one monitor: the tests that only care about the
    /// geometry of the link do not need to pass the monitor list.
    fn place_one(local: Screen, placement: &Placement) -> Screen {
        place(local, &[local], placement)
    }

    /// `snap` for a desk with one monitor.
    fn snap_one(local: Screen, candidate: Screen) -> Placement {
        snap(local, &[local], candidate)
    }

    /// A 1920x1080 local screen.
    fn local() -> Screen {
        Screen::new(0.0, 0.0, 1920.0, 1080.0)
    }

    fn range(start: f64, end: f64) -> EdgeRange {
        EdgeRange::from_fractions(start, end).expect("valid range")
    }

    fn placement(
        position: Position,
        edge: (f64, f64),
        peer_edge: (f64, f64),
        size: (f64, f64),
    ) -> Placement {
        Placement {
            position,
            edge: range(edge.0, edge.1),
            peer_edge: range(peer_edge.0, peer_edge.1),
            size,
        }
    }

    #[test]
    fn a_full_edge_peer_touches_the_edge_and_is_the_same_size() {
        let screen = place_one(
            local(),
            &placement(Position::Right, (0.0, 1.0), (0.0, 1.0), (1920.0, 1080.0)),
        );
        assert_eq!(screen.y, 0.0);
        assert_eq!(screen.h, 1080.0);
        assert_eq!(screen.x, local().right() + GAP);
    }

    #[test]
    fn two_peers_on_one_edge_do_not_overlap_and_cover_it_exactly() {
        let first = place_one(
            local(),
            &placement(Position::Right, (0.0, 0.5), (0.0, 1.0), (1920.0, 540.0)),
        );
        let second = place_one(
            local(),
            &placement(Position::Right, (0.5, 1.0), (0.0, 1.0), (1920.0, 540.0)),
        );
        assert_eq!(first.y, 0.0);
        assert_eq!(second.y, 540.0);
        assert_eq!(second.bottom(), 1080.0);
        assert!(
            first.bottom() <= second.y + f64::EPSILON,
            "{first:?} {second:?}"
        );
    }

    #[test]
    fn the_peer_is_slid_until_its_entry_segment_faces_ours() {
        // our top half maps onto the peer's bottom half: the peer has to sit
        // shifted up so those two segments face each other
        let screen = place_one(
            local(),
            &placement(Position::Right, (0.0, 0.5), (0.5, 1.0), (1920.0, 1080.0)),
        );
        assert_eq!(screen.y, -540.0);
        // the middle of its edge is level with the top of ours
        assert_eq!(screen.y + 0.5 * screen.h, 0.0);
    }

    #[test]
    fn an_unreported_peer_is_sized_so_the_two_segments_coincide() {
        // our 0-50% -> its 0-100%: its whole edge is as long as our half edge
        let size = peer_size(
            local(),
            Position::Right,
            range(0.0, 0.5),
            range(0.0, 1.0),
            None,
        );
        assert_eq!(size.1, 540.0);
        // our whole edge -> its 0-50%: its edge is twice as long as ours
        let size = peer_size(
            local(),
            Position::Right,
            EdgeRange::FULL,
            range(0.0, 0.5),
            None,
        );
        assert_eq!(size.1, 2160.0);
    }

    #[test]
    fn a_reported_resolution_is_used_verbatim() {
        let size = peer_size(
            local(),
            Position::Right,
            EdgeRange::FULL,
            EdgeRange::FULL,
            Some((1366.0, 768.0)),
        );
        assert_eq!(size, (1366.0, 768.0));
        // a zero or missing report falls back to the derived size
        assert_eq!(
            peer_size(
                local(),
                Position::Right,
                EdgeRange::FULL,
                EdgeRange::FULL,
                Some((0.0, 0.0))
            ),
            (1920.0, 1080.0)
        );
    }

    #[test]
    fn a_top_edge_peer_is_wide_rather_than_tall() {
        let size = peer_size(
            local(),
            Position::Top,
            EdgeRange::FULL,
            EdgeRange::FULL,
            None,
        );
        assert_eq!(size, (1920.0, 1080.0));
        let screen = place_one(
            local(),
            &placement(Position::Top, (0.0, 1.0), (0.0, 1.0), size),
        );
        assert_eq!(screen.x, 0.0);
        assert_eq!(screen.y, -1080.0 - GAP);
    }

    #[test]
    fn dragging_a_peer_onto_an_edge_yields_that_edge_and_the_overlap() {
        let candidate = Screen::new(local().right() + 70.0, 0.0, 1920.0, 1080.0);
        let placement = snap_one(local(), candidate);
        assert_eq!(placement.position, Position::Right);
        assert_eq!(placement.edge, EdgeRange::FULL);
        assert_eq!(placement.peer_edge, EdgeRange::FULL);
    }

    #[test]
    fn dragging_a_peer_partly_past_the_edge_shortens_both_ranges() {
        // a 1080 tall peer shifted down by 540: only its top half faces our edge
        let candidate = Screen::new(local().right() + 70.0, 540.0, 1920.0, 1080.0);
        let placement = snap_one(local(), candidate);
        assert_eq!(placement.position, Position::Right);
        assert_eq!(placement.edge.fractions(), [0.5, 1.0]);
        assert_eq!(placement.peer_edge.fractions(), [0.0, 0.5]);
    }

    #[test]
    fn a_peer_bigger_than_the_edge_is_entered_through_its_middle() {
        // a 2160 tall peer centred on a 1080 edge: the whole local edge is
        // covered, but only the middle half of the peer's edge is reachable
        let candidate = Screen::new(local().right() + 70.0, -540.0, 1920.0, 2160.0);
        let placement = snap_one(local(), candidate);
        assert_eq!(placement.edge, EdgeRange::FULL);
        assert_eq!(placement.peer_edge.fractions(), [0.25, 0.75]);
    }

    #[test]
    fn the_nearest_edge_wins() {
        assert_eq!(
            snap_one(local(), Screen::new(-2100.0, 0.0, 1920.0, 1080.0)).position,
            Position::Left
        );
        assert_eq!(
            snap_one(local(), Screen::new(0.0, -1200.0, 1920.0, 1080.0)).position,
            Position::Top
        );
        assert_eq!(
            snap_one(local(), Screen::new(0.0, 1200.0, 1920.0, 1080.0)).position,
            Position::Bottom
        );
    }

    #[test]
    fn a_drop_far_off_the_edge_is_pulled_back_until_it_is_reachable() {
        // dropped well past the bottom of the screen: nothing faces the edge
        let candidate = Screen::new(local().right() + 70.0, 3000.0, 1920.0, 1080.0);
        let placement = snap_one(local(), candidate);
        assert!(!placement.edge.is_full());
        assert!(placement.edge.end() <= 1000);
        assert!(placement.edge.contains(0.999), "{}", placement.edge);
        // and it is routable: the range is not empty
        assert!(placement.peer_edge.len() > 0);
    }

    #[test]
    fn dragging_a_screen_back_to_where_it_was_keeps_its_configuration() {
        let edge = range(0.2, 0.7);
        let peer_edge = EdgeRange::FULL;
        let size = peer_size(local(), Position::Right, edge, peer_edge, None);
        let original = Placement {
            position: Position::Right,
            edge,
            peer_edge,
            size,
        };
        let back = snap_one(local(), place_one(local(), &original));
        assert_eq!(back, original);
    }

    #[test]
    fn dragging_redefines_both_ranges_from_the_overlap() {
        /* A peer whose whole edge faces more than the segment it was configured
         * for cannot round-trip: dragging it means "the overlap is now the link",
         * which is the only reading that keeps picture and routing in agreement.
         */
        let edge = range(0.2, 0.7);
        let peer_edge = range(0.1, 0.9);
        let size = peer_size(local(), Position::Right, edge, peer_edge, None);
        let placed = place_one(
            local(),
            &Placement {
                position: Position::Right,
                edge,
                peer_edge,
                size,
            },
        );
        let back = snap_one(local(), placed);
        assert_eq!(back.position, Position::Right);
        assert_eq!(back.peer_edge, EdgeRange::FULL);
        assert!(back.edge.start() < back.edge.end());
    }

    #[test]
    fn a_peer_whose_real_size_disagrees_is_drawn_sticking_out() {
        // our whole edge -> its whole edge, but the peer is twice as tall: it
        // cannot fit, so it hangs past the bottom and part of its edge is dead
        let tall = placement(Position::Right, (0.0, 1.0), (0.0, 1.0), (1920.0, 2160.0));
        let screen = place_one(local(), &tall);
        assert_eq!(screen.y, 0.0);
        assert!(screen.bottom() > local().bottom());
        // and reading that geometry back gives the same ranges
        let back = snap_one(local(), screen);
        assert_eq!(back.edge, EdgeRange::FULL);
        assert_eq!(back.peer_edge, range(0.0, 0.5));
    }

    #[test]
    fn auto_arrange_splits_the_edge_without_gaps_or_overlaps() {
        for count in 1..=4usize {
            let ranges = auto_arrange(count);
            assert_eq!(ranges.len(), count);
            let covered = ranges.iter().map(|(edge, _)| *edge).collect::<Vec<_>>();
            assert!(
                EdgeRange::gaps(&covered).is_empty(),
                "{count} peers leave {:?}",
                EdgeRange::gaps(&covered)
            );
            for pair in covered.windows(2) {
                assert!(!pair[0].overlaps(pair[1]), "{count}: {:?} overlap", pair);
            }
        }
    }

    #[test]
    fn peers_that_do_not_overlap_along_the_edge_share_one_lane() {
        // the ordinary case: two peers, each sized to the half of the edge it hangs on
        let placements = vec![
            (
                0,
                placement(Position::Right, (0.0, 0.5), (0.0, 1.0), (1920.0, 540.0)),
            ),
            (
                1,
                placement(Position::Right, (0.5, 1.0), (0.0, 1.0), (1920.0, 540.0)),
            ),
        ];
        let placed = place_all(local(), &[local()], &placements);
        let first = placed[0].1;
        let second = placed[1].1;
        // same distance from the edge, stacked along it
        assert_eq!(first.x, second.x);
        assert!(first.bottom() <= second.y + 1.0, "{first:?} {second:?}");
    }

    #[test]
    fn a_peer_that_hangs_past_its_segment_is_put_in_another_lane() {
        /* `our 0-55% -> its 0-100%` on a screen as tall as ours: the card is twice
         * as long as the stretch it hangs on, so it reaches past the peer
         * configured for the rest of the edge. They must not be drawn on top of
         * each other, and neither may be moved along the edge - that is the
         * routing. */
        let placements = vec![
            (
                0,
                placement(Position::Right, (0.0, 0.55), (0.0, 1.0), (1920.0, 1080.0)),
            ),
            (
                1,
                placement(Position::Right, (0.55, 1.0), (0.0, 1.0), (1920.0, 1080.0)),
            ),
        ];
        let placed = place_all(local(), &[local()], &placements);
        let (first, second) = (placed[0].1, placed[1].1);
        assert_ne!(first.x, second.x, "the cards overlap: {first:?} {second:?}");
        // aligned with their own stretch of the edge, untouched
        assert_eq!(first.y, 0.0);
        assert_eq!(second.y, 0.55 * 1080.0);
        // and the further lane is further out
        assert!(second.x > first.x, "{first:?} {second:?}");
    }

    #[test]
    fn peers_on_different_edges_are_laid_out_independently() {
        let placements = vec![
            (
                0,
                placement(Position::Right, (0.0, 1.0), (0.0, 1.0), (1920.0, 1080.0)),
            ),
            (
                1,
                placement(Position::Top, (0.0, 1.0), (0.0, 1.0), (1920.0, 1080.0)),
            ),
        ];
        let placed = place_all(local(), &[local()], &placements);
        assert!(placed[0].1.x > local().right());
        assert!(placed[1].1.y < 0.0);
        assert_eq!(placed[0].1.x, local().right() + GAP);
        assert_eq!(placed[1].1.y, -1080.0 - GAP);
    }

    #[test]
    fn a_lane_is_deep_enough_for_the_widest_card_in_it() {
        // three peers all claiming the whole edge: three lanes, each clear of the last
        let placements = (0..3u64)
            .map(|handle| {
                (
                    handle,
                    placement(Position::Right, (0.0, 1.0), (0.0, 1.0), (1920.0, 1080.0)),
                )
            })
            .collect::<Vec<_>>();
        let placed = place_all(local(), &[local()], &placements);
        let mut xs = placed.iter().map(|(_, rect)| rect.x).collect::<Vec<_>>();
        xs.sort_by(|a, b| a.total_cmp(b));
        xs.dedup_by(|a, b| (*a - *b).abs() < 1.0);
        assert_eq!(xs.len(), 3, "the cards are not in separate lanes: {xs:?}");
        // successive lanes leave room for the card in the one before them
        assert!(xs[1] - xs[0] >= 1920.0, "{xs:?}");
    }

    /// The desk this whole file exists for: a 2560x1440 panel turned 270 degrees
    /// at (2560,0) beside an upright 2560x1440 at (0,1120).
    ///
    /// The bounding box is 4000x2560, and its top edge belongs to the *rotated*
    /// panel, while the upright one's top is 1120 px lower. Putting a peer "above
    /// the left-hand screen" therefore cannot be drawn against the box.
    fn two_monitor_desk() -> (Screen, Vec<Screen>) {
        let bounds = Screen::new(0.0, 0.0, 4000.0, 2560.0);
        let monitors = vec![
            // DP-3, upright, bottom left
            Screen::new(0.0, 1120.0, 2560.0, 1440.0),
            // DP-2, rotated, right
            Screen::new(2560.0, 0.0, 1440.0, 2560.0),
        ];
        (bounds, monitors)
    }

    #[test]
    fn a_peer_above_the_lower_monitor_is_drawn_above_that_monitor() {
        let (bounds, monitors) = two_monitor_desk();
        /* a link covering the left 64% of the top edge: that is DP-3's width as a
         * fraction of the bounding box */
        let placement = placement(Position::Top, (0.0, 0.64), (0.0, 1.0), (2560.0, 1440.0));
        let card = place(bounds, &monitors, &placement);
        /* just above DP-3's top (1120), not above the bounding box's (0) */
        assert_eq!(card.bottom(), 1120.0 - GAP, "{card:?}");
        assert_eq!(card.x, 0.0, "{card:?}");
    }

    #[test]
    fn a_peer_above_the_rotated_monitor_is_drawn_at_the_box_top() {
        let (bounds, monitors) = two_monitor_desk();
        /* the right 36% of the top edge is DP-2, which does reach the box top */
        let placement = placement(Position::Top, (0.64, 1.0), (0.0, 1.0), (1440.0, 2560.0));
        let card = place(bounds, &monitors, &placement);
        assert_eq!(card.bottom(), 0.0 - GAP, "{card:?}");
        assert!(card.x >= 2560.0, "{card:?}");
    }

    #[test]
    fn dropping_a_card_up_and_left_of_the_lower_monitor_means_top_not_left() {
        /* This is the case that read as "left": the card is dragged above DP-3,
         * which is up *and* to the left of the bounding box's centre (2000,1280) -
         * a point in empty space. Measured from the box the horizontal offset wins
         * and the peer is attached to the wrong edge. */
        let (bounds, monitors) = two_monitor_desk();
        let candidate = Screen::new(0.0, -400.0, 2560.0, 1440.0);
        let placement = snap(bounds, &monitors, candidate);
        assert_eq!(placement.position, Position::Top);
        /* and it covers DP-3, not the whole box */
        assert_eq!(placement.edge.fractions(), [0.0, 0.64]);
    }

    #[test]
    fn dropping_a_card_above_the_rotated_monitor_covers_only_that_one() {
        let (bounds, monitors) = two_monitor_desk();
        let candidate = Screen::new(2600.0, -1600.0, 1440.0, 2560.0);
        let placement = snap(bounds, &monitors, candidate);
        assert_eq!(placement.position, Position::Top);
        let [start, end] = placement.edge.fractions();
        assert!(start >= 0.63, "{start}");
        assert!((end - 1.0).abs() < 0.01, "{end}");
    }

    #[test]
    fn a_card_beside_the_lower_monitor_covers_only_its_stretch() {
        let (bounds, monitors) = two_monitor_desk();
        /* to the left of DP-3, which spans y 1120..2560 of a 2560 tall box */
        let candidate = Screen::new(-2700.0, 1200.0, 2560.0, 1440.0);
        let placement = snap(bounds, &monitors, candidate);
        assert_eq!(placement.position, Position::Left);
        /* the card starts 80 px below DP-3's top edge, so it covers
         * (2560-1200)/2560 of it */
        let [start, end] = placement.edge.fractions();
        assert!((start - 1200.0 / 2560.0).abs() < 0.01, "{start}");
        assert!((end - 1.0).abs() < 0.01, "{end}");
    }

    #[test]
    fn the_anchor_is_the_monitor_with_the_largest_overlap() {
        let (bounds, monitors) = two_monitor_desk();
        let on_lower = placement(Position::Bottom, (0.0, 0.64), (0.0, 1.0), (2560.0, 1440.0));
        assert_eq!(anchor_monitor(bounds, &monitors, &on_lower), monitors[0]);
        let on_rotated = placement(Position::Bottom, (0.64, 1.0), (0.0, 1.0), (1440.0, 2560.0));
        assert_eq!(anchor_monitor(bounds, &monitors, &on_rotated), monitors[1]);
        /* every stretch of this desk belongs to a monitor; a desk with a gap
         * between its monitors is what the fallback is for */
        let spare = Screen::new(0.0, 0.0, 100.0, 100.0);
        let only_left = vec![Screen::new(0.0, 1120.0, 2560.0, 1440.0)];
        let over_gap = placement(Position::Top, (0.9, 1.0), (0.0, 1.0), (10.0, 10.0));
        assert_eq!(anchor_monitor(bounds, &only_left, &over_gap), bounds);
        let _ = spare;
    }

    #[test]
    fn a_desk_with_no_reported_monitors_still_places_and_snaps() {
        let (bounds, _) = two_monitor_desk();
        let placement = placement(Position::Top, (0.0, 1.0), (0.0, 1.0), (1920.0, 1080.0));
        let card = place(bounds, &[], &placement);
        assert_eq!(card.bottom(), -GAP);
        let snapped = snap(bounds, &[], Screen::new(0.0, -1200.0, 1920.0, 1080.0));
        assert_eq!(snapped.position, Position::Top);
    }

    #[test]
    fn a_zero_sized_local_screen_does_not_produce_an_invalid_range() {
        let local = Screen::new(0.0, 0.0, 0.0, 0.0);
        let placement = snap_one(local, Screen::new(10.0, 10.0, 100.0, 100.0));
        assert!(placement.edge.fractions()[0].is_finite());
        assert!(placement.edge.start() < placement.edge.end());
        assert_eq!(
            peer_size(
                local,
                Position::Right,
                EdgeRange::FULL,
                EdgeRange::FULL,
                None
            )
            .1,
            0.0
        );
    }

    #[test]
    fn a_peer_dropped_on_the_local_screen_itself_still_gets_an_edge() {
        // a sloppy drop right on top of the local screen must not lose the peer
        let placement = snap_one(local(), local());
        assert!(matches!(
            placement.position,
            Position::Left | Position::Right | Position::Top | Position::Bottom
        ));
        assert!(placement.edge.start() < placement.edge.end());
    }
}
