use windows::Win32::Foundation::RECT;

use crate::Position;

fn is_within_dp_region(point: (i32, i32), display: &RECT) -> bool {
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
    .iter()
    .all(|&pos| is_within_dp_boundary(point, display, pos))
}

fn is_within_dp_boundary(point: (i32, i32), display: &RECT, pos: Position) -> bool {
    let (x, y) = point;
    match pos {
        Position::Left => display.left <= x,
        Position::Right => display.right > x,
        Position::Top => display.top <= y,
        Position::Bottom => display.bottom > y,
    }
}

/// Where along `pos` a cursor at `point` crosses the edge, as a fraction of the
/// whole reachable area.
///
/// The fraction is measured over every display rather than over the one the
/// pointer left: a client owns a segment of the *edge*, and the edge of a
/// multi-display layout is as tall or as wide as the whole layout. Measuring per
/// display would make the same stretch mean different things to the client that
/// routes and to the peer that places the cursor.
pub(crate) fn crossing_fraction(displays: &[RECT], pos: Position, point: (i32, i32)) -> f64 {
    let Some((x0, y0, x1, y1)) = layout_bounds(displays) else {
        return 0.5;
    };
    match pos {
        Position::Left | Position::Right => {
            let height = (y1 - y0) as f64;
            if height <= 0.0 {
                return 0.5;
            }
            ((point.1 - y0) as f64 / height).clamp(0.0, 1.0)
        }
        Position::Top | Position::Bottom => {
            let width = (x1 - x0) as f64;
            if width <= 0.0 {
                return 0.5;
            }
            ((point.0 - x0) as f64 / width).clamp(0.0, 1.0)
        }
    }
}

/// The bounding box of every display, `(left, top, right, bottom)`, exclusive on
/// the far side, so its extent is `right - left`.
pub(crate) fn layout_bounds(displays: &[RECT]) -> Option<(i32, i32, i32, i32)> {
    let first = displays.first()?;
    let (mut x0, mut y0, mut x1, mut y1) = (first.left, first.top, first.right, first.bottom);
    for display in displays {
        x0 = x0.min(display.left);
        y0 = y0.min(display.top);
        x1 = x1.max(display.right);
        y1 = y1.max(display.bottom);
    }
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
}

/// the display that contains the given point, if any
pub(crate) fn display_at(point: (i32, i32), displays: &[RECT]) -> Option<&RECT> {
    displays.iter().find(|&d| is_within_dp_region(point, d))
}

fn in_display_region(point: (i32, i32), displays: &[RECT]) -> bool {
    displays.iter().any(|d| is_within_dp_region(point, d))
}

fn moved_across_boundary(
    prev_pos: (i32, i32),
    curr_pos: (i32, i32),
    displays: &[RECT],
    pos: Position,
) -> bool {
    /* The cursor has to leave the edge of the display it currently sits on and
     * must not land on any other display. Testing every display instead makes
     * a barrier never fire when some other (e.g. larger, vertically offset)
     * display still lies beyond the crossing point.
     */
    let Some(display) = display_at(prev_pos, displays) else {
        return false;
    };
    !is_within_dp_boundary(curr_pos, display, pos) && !in_display_region(curr_pos, displays)
}

pub(crate) fn entered_barrier(
    prev_pos: (i32, i32),
    curr_pos: (i32, i32),
    displays: &[RECT],
) -> Option<Position> {
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
    .into_iter()
    .find(|&pos| moved_across_boundary(prev_pos, curr_pos, displays, pos))
}

///
/// clamp point to display bounds
///
/// # Arguments
///
/// * `prev_point`: coordinates, the cursor was before entering, within bounds of a display
/// * `entry_point`: point to clamp
///
/// returns: (i32, i32), the corrected entry point
///
pub(crate) fn clamp_to_display_bounds(
    display_regions: &[RECT],
    prev_point: (i32, i32),
    point: (i32, i32),
) -> (i32, i32) {
    /* find display where movement came from */
    let Some(display) = display_at(prev_point, display_regions) else {
        /* the display layout changed underneath us: leave the point untouched */
        return point;
    };

    /* clamp to bounds (inclusive) */
    let (x, y) = point;
    let (min_x, max_x) = (display.left, display.right - 1);
    let (min_y, max_y) = (display.top, display.bottom - 1);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}
