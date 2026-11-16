//! Painting of the arrangement canvas.
//!
//! Split from the widget because the picture is a pure function of the state: it
//! reads the layout and the reachability and turns them into strokes, and touches
//! no widget at all. That is also what lets it be rendered off screen and checked
//! pixel by pixel in `arrangement::render`.

use gtk::cairo;
use lan_mouse_ipc::{EdgeRange, Position};

use super::layout::Screen;
use super::paint::{self, Palette, Text};
use super::{Drag, GAP, Machine, State, positions};

/// Paint the canvas.
pub(super) fn draw(cr: &cairo::Context, width: f64, height: f64, state: &State, palette: &Palette) {
    paint::set(cr, palette.foreground, 0.04);
    let _ = cr.paint();
    if state.show_grid {
        draw_grid(cr, state, palette);
    }

    let local = state.geometry.local;

    /* links and walls, underneath everything else. Both come out of
     * `reachable_stretches`, so a bridge and the wall next to it cannot disagree
     * about where the crossing stops. */
    let reachable = reachable_stretches(state, local);
    for (machine, piece) in &reachable {
        let Some(rect) = state.rect_of(machine.handle) else {
            continue;
        };
        draw_link(cr, state, machine, *piece, rect, palette);
    }
    for position in positions() {
        /* The walls are the complement of what is reachable, per edge: a stretch
         * nobody claims and a stretch a blocked corner eats look the same to the
         * pointer, so they are drawn the same way. An edge with nothing on it is
         * left blank - a wall there would claim the user attached a machine. */
        let claimed = state
            .machines
            .iter()
            .any(|machine| machine.position == position && machine.active);
        if !claimed {
            continue;
        }
        let pieces = reachable
            .iter()
            .filter(|(machine, _)| machine.position == position)
            .map(|(_, piece)| *piece)
            .collect::<Vec<_>>();
        for gap in EdgeRange::gaps(&pieces) {
            draw_wall(cr, state, local, position, gap, palette);
        }
    }

    /* the stretch of edge a dragged card is about to claim */
    if let Some(drag) = state.drag.as_ref() {
        draw_snap_preview(cr, state, drag, palette);
    }

    /* Every screen of every machine, in the one coordinate space they actually
     * share.
     *
     * A machine is drawn as the monitors it has, not as one box, for the same
     * reason the ranges are per monitor: a box around two screens of unequal
     * height covers area no screen does, so a border there would claim a screen
     * that does not exist. Each machine gets its own border colour, so a desk
     * where one machine's screens are separated by another's still reads as two
     * machines rather than three.
     */
    let monitors = state.local_monitors();
    let local_colour = paint::machine_color(0, palette);
    for monitor in &monitors {
        let canvas = state.to_canvas(monitor.rect);
        paint::fill_card(cr, canvas, palette);
        paint::set(cr, local_colour, 0.9);
        cr.set_line_width(2.0);
        let _ = cr.stroke();
        draw_resolution(cr, canvas, monitor.rect, palette);
    }
    let canvas_local = state.to_canvas(local);
    /* The label goes on the largest monitor, not on the bounding box: with a
     * rotated monitor in the desk the box's centre can be a place where there is
     * no screen at all, and the name would float over nothing. */
    let labelled = monitors
        .iter()
        .max_by(|a, b| (a.rect.w * a.rect.h).total_cmp(&(b.rect.w * b.rect.h)))
        .map(|monitor| state.to_canvas(monitor.rect))
        .unwrap_or(canvas_local);
    /* the name on every screen, the full card on the largest */
    for monitor in &monitors {
        let canvas = state.to_canvas(monitor.rect);
        if canvas != labelled {
            draw_screen_name(cr, canvas, &state.local_hostname, local_colour, palette);
        }
    }
    draw_card(
        cr,
        labelled,
        &state.local_hostname,
        if state.local_displays.is_empty() {
            "this screen · size unknown".to_owned()
        } else {
            format!("this machine · {} screens", monitors.len().max(1))
        },
        None,
        palette,
        Some(local_colour),
    );

    /* the peers, the dragged one last so it is on top */
    let mut order = state
        .machines
        .iter()
        .filter(|m| m.active)
        .map(|m| m.handle)
        .collect::<Vec<_>>();
    if let Some(drag) = state.drag.as_ref() {
        order.retain(|handle| *handle != drag.handle);
        order.push(drag.handle);
    }
    let squeezed = state.unreachable();
    for handle in order {
        let Some(machine) = state.machine(handle) else {
            continue;
        };
        let Some(rect) = state.rect_of(handle) else {
            continue;
        };
        let dragging = state.drag.as_ref().is_some_and(|d| d.handle == handle);
        let hovered = state.hover == Some(handle);
        let blocked = squeezed.contains(&handle);
        let canvas = state.to_canvas(rect);
        if dragging {
            paint::set(cr, (0.0, 0.0, 0.0), 0.22);
            let shadow = Screen::new(canvas.x + 2.0, canvas.y + 4.0, canvas.w, canvas.h);
            paint::rounded_rect(cr, shadow, 10.0);
            let _ = cr.fill();
        }
        /* A peer that reported its monitors is drawn as those monitors, the same
         * way the local machine is. A peer that reported nothing is one box - that
         * is all that is known about it. */
        let colour = paint::machine_color(state.machine_index(handle) + 1, palette);
        let screens = state.screens_of(handle);
        for screen in &screens {
            let card = state.to_canvas(*screen);
            paint::fill_card(cr, card, palette);
            paint::set(
                cr,
                if blocked { palette.danger } else { colour },
                if hovered || dragging { 0.95 } else { 0.7 },
            );
            cr.set_line_width(if hovered || dragging { 2.0 } else { 1.5 });
            let _ = cr.stroke();
            draw_resolution(cr, card, *screen, palette);
        }

        let name_at = screens
            .iter()
            .max_by(|a, b| (a.w * a.h).total_cmp(&(b.w * b.h)))
            .map(|screen| state.to_canvas(*screen))
            .unwrap_or(canvas);
        for screen in &screens {
            let canvas = state.to_canvas(*screen);
            if canvas != name_at {
                draw_screen_name(cr, canvas, &machine.hostname, colour, palette);
            }
        }
        draw_card(
            cr,
            name_at,
            &machine.hostname,
            format!("{} · {}", edge_label(machine.position), machine.edge),
            machine
                .peer_size
                .map(Detail::Size)
                .or(Some(Detail::Range(machine.peer_edge))),
            palette,
            machine.active.then_some(colour),
        );
        if blocked {
            paint::set(cr, palette.danger, 0.95);
            let text = Text::new(cr, "unreachable", 10.5, true, (name_at.w - 16.0).max(1.0));
            text.draw(cr, name_at.x + name_at.w / 2.0, name_at.bottom() - 12.0);
        }
    }

    /* The handles, on top of what they adjust - and only for the machine the
     * pointer is on.
     *
     * Drawn for every machine they are eight identical blue squares with nothing
     * saying what they belong to or what they do, which reads as decoration or as
     * an error. Showing them for one machine at a time makes them part of that
     * machine's card. */
    let focused = state.drag.as_ref().map(|drag| drag.handle).or(state.hover);
    for handle in state
        .geometry
        .handles
        .iter()
        .filter(|handle| Some(handle.handle) == focused)
    {
        let canvas = state.to_canvas(handle.rect);
        paint::set(cr, palette.accent, 0.95);
        paint::rounded_rect(cr, canvas, 3.0);
        let _ = cr.fill_preserve();
        paint::set(cr, palette.foreground, 0.4);
        cr.set_line_width(1.0);
        let _ = cr.stroke();
    }

    if state.machines.iter().all(|m| !m.active) {
        paint::set(cr, palette.foreground, 0.55);
        let text = Text::new(
            cr,
            "No machine is enabled - enable one on the Devices page",
            13.0,
            false,
            width - 40.0,
        );
        text.draw(cr, width / 2.0, height - 24.0);
    }
}

/// Which ends of `position`'s edge a blocked corner eats, in permille.
///
/// The daemon refuses a crossing whose point is within `corner_size` pixels of a
/// corner the user blocked, so the canvas has to show that part of the edge as
/// unreachable too - otherwise the picture promises a crossing the pointer will
/// not make. The names are the corners of the local screen, so each edge has two
/// of them and the low end of a left/right edge is its top.
///
/// `pub(super)` because the widget's tests check it directly: whether a machine is
/// reachable is the question the canvas answers, and it is answered here so the
/// picture cannot disagree with itself.
pub(super) fn blocked_ends(
    position: Position,
    corners: &[lan_mouse_ipc::SwitchCorner],
    edge_len: f64,
    corner_size: u32,
) -> (u16, u16) {
    use lan_mouse_ipc::SwitchCorner as Corner;
    let blocked = |corner: Corner| -> u16 {
        if !corners.contains(&corner) || edge_len <= 0.0 || corner_size == 0 {
            return 0;
        }
        /* permille of the edge, rounded *up*: the daemon blocks the whole
         * `corner_size`, so rounding down would leave a sliver the canvas shows as
         * a bridge and the pointer refuses to cross. Capped at the whole edge,
         * which a corner size larger than the screen means. */
        ((corner_size as f64 / edge_len) * 1000.0)
            .ceil()
            .clamp(0.0, 1000.0) as u16
    };
    match position {
        Position::Top => (blocked(Corner::TopLeft), blocked(Corner::TopRight)),
        Position::Bottom => (blocked(Corner::BottomLeft), blocked(Corner::BottomRight)),
        Position::Left => (blocked(Corner::TopLeft), blocked(Corner::BottomLeft)),
        Position::Right => (blocked(Corner::TopRight), blocked(Corner::BottomRight)),
    }
}

/// The parts of the local edge a crossing actually goes through, per peer.
///
/// Two things stop a crossing: no peer is configured for the stretch, and a blocked
/// corner eats the end of an edge. The daemon refuses a crossing at a blocked corner
/// the same way it refuses one that matches no barrier, so both are walls here and
/// a bridge is only drawn over what is left of a peer's claim once the corners have
/// taken their share.
pub(super) fn reachable_stretches(state: &State, local: Screen) -> Vec<(&Machine, EdgeRange)> {
    let mut out = Vec::new();
    for position in positions() {
        let edge_len = local.extent_along(position);
        let (low, high) = blocked_ends(
            position,
            &state.switch_options.corners,
            edge_len,
            state.switch_options.corner_size,
        );
        let mut blockers = Vec::new();
        if low > 0 {
            blockers.push(EdgeRange::new(0, low).expect("blocked corner"));
        }
        if high > 0 {
            blockers.push(EdgeRange::new(1000 - high, 1000).expect("blocked corner"));
        }
        for machine in state
            .machines
            .iter()
            .filter(|machine| machine.position == position && machine.active)
        {
            /* A peer whose whole claim is a blocked corner keeps no reachable
             * stretch: the canvas shows no bridge and the wall fills the gap, which
             * is the honest picture of a machine nothing gets to. */
            out.extend(
                machine
                    .edge
                    .subtract(&blockers)
                    .into_iter()
                    .map(|piece| (machine, piece)),
            );
        }
    }
    out
}

fn draw_grid(cr: &cairo::Context, state: &State, palette: &Palette) {
    let step = 24.0;
    let mut y = step;
    while y < state.canvas.1 {
        let mut x = step;
        while x < state.canvas.0 {
            cr.new_sub_path();
            cr.arc(x, y, 1.2, 0.0, std::f64::consts::PI * 2.0);
            paint::set(cr, palette.foreground, 0.12);
            let _ = cr.fill();
            x += step;
        }
        y += step;
    }
}

/// The bridge between a peer and the local screen, drawn over the overlap only.
///
/// A line across the whole edge would claim the peer is reachable from anywhere
/// along it, which is exactly the claim the canvas used to make and the daemon did
/// not honour.
fn draw_link(
    cr: &cairo::Context,
    state: &State,
    machine: &Machine,
    piece: EdgeRange,
    peer: Screen,
    palette: &Palette,
) {
    /* The band spans the gap between the peer's card and the monitor it is
     * attached to - not the bounding box.
     *
     * `piece` is measured against the monitor the link names, and on a desk with
     * unequal heights the box's edge is a different line from that monitor's: the
     * box top belongs to the rotated panel while the link sits on the upright one,
     * 1120 px lower. Anchoring to the box made the height of this band negative, so
     * the link disappeared and the two screens stopped looking connected. */
    let anchor = state.anchor_of(machine);
    let edge_len = anchor.extent_along(machine.position);
    if edge_len <= 0.0 {
        return;
    }
    let low = anchor.edge_start(machine.position);
    let start = low + piece.start() as f64 / 1000.0 * edge_len;
    let stop = low + piece.end() as f64 / 1000.0 * edge_len;
    let band = match machine.position {
        Position::Left => Screen::new(peer.right(), start, anchor.x - peer.right(), stop - start),
        Position::Right => {
            Screen::new(anchor.right(), start, peer.x - anchor.right(), stop - start)
        }
        Position::Top => Screen::new(start, peer.bottom(), stop - start, anchor.y - peer.bottom()),
        Position::Bottom => Screen::new(
            start,
            anchor.bottom(),
            stop - start,
            peer.y - anchor.bottom(),
        ),
    };
    if band.w <= 0.0 || band.h <= 0.0 {
        /* the peer is inside the anchor's own row, which a drag does momentarily;
         * a negative rectangle is not a link */
        return;
    }
    let canvas = state.to_canvas(band);
    paint::set(cr, palette.success, 0.5);
    cr.rectangle(canvas.x, canvas.y, canvas.w.max(1.0), canvas.h.max(1.0));
    let _ = cr.fill();
}

/// A stretch of edge that reaches nothing, drawn as a hatched wall.
fn draw_wall(
    cr: &cairo::Context,
    state: &State,
    local: Screen,
    position: Position,
    gap: EdgeRange,
    palette: &Palette,
) {
    let edge_len = local.extent_along(position);
    let low = local.edge_start(position);
    let start = low + gap.start() as f64 / 1000.0 * edge_len;
    let stop = low + gap.end() as f64 / 1000.0 * edge_len;
    /* A wall is the complement of what is reachable along the whole edge, so it is
     * drawn against the box: it spans several monitors, and no single one of them
     * owns it. */
    let band = match position {
        Position::Left => Screen::new(local.x - GAP, start, GAP, stop - start),
        Position::Right => Screen::new(local.right(), start, GAP, stop - start),
        Position::Top => Screen::new(start, local.y - GAP, stop - start, GAP),
        Position::Bottom => Screen::new(start, local.bottom(), stop - start, GAP),
    };
    let canvas = state.to_canvas(band);
    paint::set(cr, palette.warning, 0.55);
    cr.set_line_width(1.5);
    /* hatched, so it reads as blocked rather than as an empty link */
    let step = 8.0f64.max(canvas.h / 6.0);
    let mut x = canvas.x;
    while x < canvas.right() {
        cr.move_to(x, canvas.y);
        cr.line_to((x + step).min(canvas.right()), canvas.bottom());
        let _ = cr.stroke();
        x += step * 2.0;
    }
}

/// Highlight the stretch of edge a dragged card is about to claim.
fn draw_snap_preview(cr: &cairo::Context, state: &State, drag: &Drag, palette: &Palette) {
    let local = state.geometry.local;
    let placement = &drag.placement;
    let edge_len = local.extent_along(placement.position);
    if edge_len <= 0.0 {
        return;
    }
    let low = local.edge_start(placement.position);
    let start = low + placement.edge.start() as f64 / 1000.0 * edge_len;
    let stop = low + placement.edge.end() as f64 / 1000.0 * edge_len;
    let band = match placement.position {
        Position::Left => Screen::new(local.x - 5.0, start, 5.0, stop - start),
        Position::Right => Screen::new(local.right(), start, 5.0, stop - start),
        Position::Top => Screen::new(start, local.y - 5.0, stop - start, 5.0),
        Position::Bottom => Screen::new(start, local.bottom(), stop - start, 5.0),
    };
    let canvas = state.to_canvas(band);
    paint::set(cr, palette.accent, 0.95);
    cr.rectangle(canvas.x, canvas.y, canvas.w.max(2.0), canvas.h.max(2.0));
    let _ = cr.fill();
}

/// Draw a card's contents.
fn draw_card(
    cr: &cairo::Context,
    rect: Screen,
    name: &str,
    subtitle: String,
    detail: Option<Detail>,
    palette: &Palette,
    /* the machine's colour while it can be reached, `None` when it cannot */
    machine: Option<(f64, f64, f64)>,
) {
    let inner = (rect.w - 16.0).max(1.0);
    paint::set(cr, palette.foreground, 0.95);
    let title = Text::new(cr, name, 14.0, true, inner);
    title.draw(cr, rect.x + rect.w / 2.0, rect.y + rect.h / 2.0 - 8.0);

    /* the subtitle is in the machine's own colour, so the second line already
     * says which machine this card belongs to before the border is noticed */
    paint::set(cr, machine.unwrap_or(palette.foreground), 0.95);
    let sub = Text::new(cr, &subtitle, 11.5, false, inner);
    sub.draw(cr, rect.x + rect.w / 2.0, rect.y + rect.h / 2.0 + 12.0);

    if let Some(detail) = detail {
        paint::set(cr, palette.foreground, 0.6);
        let text = match detail {
            Detail::Size(size) => paint::format_size(size),
            Detail::Range(range) => format!("lands in its {range}"),
        };
        let line = Text::new(cr, &text, 10.5, false, inner);
        line.draw(cr, rect.x + rect.w / 2.0, rect.y + rect.h / 2.0 + 28.0);
    }
}

/// The machine a screen belongs to, in the top-left of that screen.
///
/// Every screen, not only the largest one: a machine with two monitors otherwise
/// has one labelled rectangle and one anonymous one, and "which of these is my
/// second screen" is exactly what a per-monitor picture has to answer. Small and
/// dim, so it never competes with the full card on the largest screen.
fn draw_screen_name(
    cr: &cairo::Context,
    canvas: Screen,
    name: &str,
    colour: (f64, f64, f64),
    palette: &Palette,
) {
    let _ = palette;
    if canvas.w < 96.0 || canvas.h < 56.0 {
        return;
    }
    paint::set(cr, colour, 0.75);
    let text = Text::new(cr, name, 11.0, true, (canvas.w - 16.0).max(1.0));
    text.draw(cr, canvas.x + canvas.w / 2.0, canvas.y + 16.0);
}

/// The pixel size of one screen, in its bottom-left corner.
///
/// Every screen gets one, not only the largest: a desk is bought and arranged by
/// its resolutions, and "which of these is the 1440p one" is the question the
/// canvas exists to answer. Drawn small and dim so it never competes with the
/// machine name.
fn draw_resolution(cr: &cairo::Context, canvas: Screen, layout: Screen, palette: &Palette) {
    if canvas.w < 64.0 || canvas.h < 40.0 {
        /* unreadable at this size, and the label would be clipped to nothing */
        return;
    }
    paint::set(cr, palette.foreground, 0.45);
    let text = format!("{}×{}", layout.w as i64, layout.h as i64);
    let line = Text::new(cr, &text, 10.0, false, (canvas.w - 12.0).max(1.0));
    line.draw(cr, canvas.x + canvas.w / 2.0, canvas.bottom() - 10.0);
}

/// The third line of a card: what the peer's screen is, or where its cursor lands.
enum Detail {
    Size((f64, f64)),
    Range(EdgeRange),
}

impl From<EdgeRange> for Detail {
    fn from(range: EdgeRange) -> Self {
        Detail::Range(range)
    }
}

fn edge_label(position: Position) -> &'static str {
    match position {
        Position::Left => "left",
        Position::Right => "right",
        Position::Top => "top",
        Position::Bottom => "bottom",
    }
}
