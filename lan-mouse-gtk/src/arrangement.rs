//! Visual arrangement of the screens lan-mouse can cross to.
//!
//! The daemon models a peer as sitting on exactly one edge of the local
//! screen ([`Position`]), so the editor draws this screen in the middle and the
//! peers around it, the way a commercial KVM lays its machines out: real screen
//! rectangles, a link between the two facing edges, and drag and drop to move a
//! peer to another edge. Peers that share an edge are stacked along it, which
//! is also how `position_map` handles them (the first peer in the list that
//! owns the edge takes the pointer).
//!
//! Everything is painted with cairo on a single [`gtk::DrawingArea`]: the grid
//! has to line up with the links and the cards, which is much easier to keep
//! consistent in one layout pass than with a nest of styled boxes.

use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;

use gtk::{cairo, prelude::*};
use lan_mouse_ipc::{ClientHandle, Position};

/// One peer that can be dragged around the local screen.
pub(super) struct Machine {
    pub handle: ClientHandle,
    pub hostname: String,
    pub position: Position,
    pub active: bool,
}

type OnMove = Rc<dyn Fn(ClientHandle, Position)>;

const CARD_WIDTH: f64 = 190.0;
/// 16:9, so the cards read as screens rather than as buttons.
const CARD_HEIGHT: f64 = 107.0;
const CARD_RADIUS: f64 = 8.0;
/// Space between the local screen and the peers, wide enough for the link.
const GAP: f64 = 26.0;
const SPACING: f64 = 14.0;
const GRID_STEP: f64 = 24.0;
const MAX_SCALE: f64 = 1.2;
const CANVAS_HEIGHT: i32 = 440;

#[derive(Clone, Copy, Debug, Default)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn center(self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    fn union(self, other: Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.w).max(other.x + other.w);
        let bottom = (self.y + self.h).max(other.y + other.h);
        Rect {
            x,
            y,
            w: right - x,
            h: bottom - y,
        }
    }

    fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    /// The point on the edge `position` that lines up with the centre of
    /// `other`, so bridges between several peers do not cross.
    fn point_at(self, position: Position, other: Rect) -> (f64, f64) {
        let (ox, oy) = other.center();
        let clamp = |value: f64| value.clamp(0.08, 0.92);
        match position {
            Position::Left => (
                self.x,
                self.y + self.h * clamp((oy - self.y) / self.h.max(1.0)),
            ),
            Position::Right => (
                self.x + self.w,
                self.y + self.h * clamp((oy - self.y) / self.h.max(1.0)),
            ),
            Position::Top => (
                self.x + self.w * clamp((ox - self.x) / self.w.max(1.0)),
                self.y,
            ),
            Position::Bottom => (
                self.x + self.w * clamp((ox - self.x) / self.w.max(1.0)),
                self.y + self.h,
            ),
        }
    }

    /// Midpoint of the edge that faces `position`.
    fn edge_mid(self, position: Position) -> (f64, f64) {
        let (cx, cy) = self.center();
        match position {
            Position::Left => (self.x, cy),
            Position::Right => (self.x + self.w, cy),
            Position::Top => (cx, self.y),
            Position::Bottom => (cx, self.y + self.h),
        }
    }
}

/// Colours resolved once per draw call.
struct Palette {
    foreground: (f64, f64, f64),
    accent: (f64, f64, f64),
    success: (f64, f64, f64),
}

/// A peer card that is currently being dragged.
struct Drag {
    handle: ClientHandle,
    /// Top left corner of the card when the drag started.
    origin: (f64, f64),
    /// Top left corner under the pointer, updated while dragging.
    corner: (f64, f64),
    /// Edge the card snaps to when the drag ends.
    target: Position,
}

/// State shared by the draw function and the gestures.
struct State {
    local_hostname: String,
    machines: Vec<Machine>,
    on_move: OnMove,
    /// Peer rectangles from the last layout pass, for hit testing.
    layout: Vec<(ClientHandle, Rect)>,
    local: Rect,
    drag: Option<Drag>,
    hover: Option<ClientHandle>,
    /// Scale applied to the whole layout so it always fits the canvas.
    scale: f64,
    /// Translation of the scaled layout inside the canvas.
    origin: (f64, f64),
}

impl State {
    fn machine(&self, handle: ClientHandle) -> Option<&Machine> {
        self.machines
            .iter()
            .find(|machine| machine.handle == handle)
    }

    fn machine_mut(&mut self, handle: ClientHandle) -> Option<&mut Machine> {
        self.machines
            .iter_mut()
            .find(|machine| machine.handle == handle)
    }

    /// Lay the cards out for a canvas of `width` x `height`.
    ///
    /// The local screen sits in the middle and every peer is attached to the
    /// edge it is configured on, stacked along that edge so several peers on
    /// the same edge stay apart.
    fn relayout(&mut self, width: f64, height: f64) {
        let local = Rect {
            x: (width - CARD_WIDTH) / 2.0,
            y: (height - CARD_HEIGHT) / 2.0,
            w: CARD_WIDTH,
            h: CARD_HEIGHT,
        };
        self.local = local;

        let (cx, cy) = local.center();
        let mut layout = Vec::with_capacity(self.machines.len());
        for position in [
            Position::Left,
            Position::Right,
            Position::Top,
            Position::Bottom,
        ] {
            let peers = self
                .machines
                .iter()
                .filter(|machine| machine.position == position)
                .map(|machine| machine.handle)
                .collect::<Vec<_>>();
            let count = peers.len() as f64;
            for (index, handle) in peers.into_iter().enumerate() {
                let index = index as f64;
                let rect = match position {
                    Position::Left | Position::Right => {
                        let step = CARD_HEIGHT + SPACING;
                        Rect {
                            x: if position == Position::Left {
                                local.x - GAP - CARD_WIDTH
                            } else {
                                local.x + CARD_WIDTH + GAP
                            },
                            y: cy - (count * step - SPACING) / 2.0 + index * step,
                            w: CARD_WIDTH,
                            h: CARD_HEIGHT,
                        }
                    }
                    Position::Top | Position::Bottom => {
                        let step = CARD_WIDTH + SPACING;
                        Rect {
                            x: cx - (count * step - SPACING) / 2.0 + index * step,
                            y: if position == Position::Top {
                                local.y - GAP - CARD_HEIGHT
                            } else {
                                local.y + CARD_HEIGHT + GAP
                            },
                            w: CARD_WIDTH,
                            h: CARD_HEIGHT,
                        }
                    }
                };
                layout.push((handle, rect));
            }
        }
        self.layout = layout;
        self.fit(width, height);
    }

    /// Scale and centre the laid out cards so they always fit the canvas, no
    /// matter how many peers sit on an edge.
    fn fit(&mut self, width: f64, height: f64) {
        let mut bounds: Option<Rect> = None;
        for rect in std::iter::once(self.local).chain(self.layout.iter().map(|(_, r)| *r)) {
            bounds = Some(match bounds {
                None => rect,
                Some(current) => current.union(rect),
            });
        }
        let Some(bounds) = bounds else {
            self.scale = 1.0;
            self.origin = (0.0, 0.0);
            return;
        };
        /* Leave room for the canvas border and a little breathing space. */
        let padding = 28.0;
        let available_w = (width - 2.0 * padding).max(80.0);
        let available_h = (height - 2.0 * padding).max(60.0);
        let scale = (available_w / bounds.w)
            .min(available_h / bounds.h)
            .clamp(0.25, MAX_SCALE);
        self.scale = scale;
        self.origin = (
            (width - bounds.w * scale) / 2.0 - bounds.x * scale,
            (height - bounds.h * scale) / 2.0 - bounds.y * scale,
        );
    }

    /// Canvas coordinates to layout coordinates.
    fn to_layout(&self, x: f64, y: f64) -> (f64, f64) {
        (
            (x - self.origin.0) / self.scale,
            (y - self.origin.1) / self.scale,
        )
    }

    fn rect_of(&self, handle: ClientHandle) -> Option<Rect> {
        if let Some(drag) = self.drag.as_ref().filter(|drag| drag.handle == handle) {
            return Some(Rect {
                x: drag.corner.0,
                y: drag.corner.1,
                w: CARD_WIDTH,
                h: CARD_HEIGHT,
            });
        }
        self.layout
            .iter()
            .find(|(other, _)| *other == handle)
            .map(|(_, rect)| *rect)
    }

    fn hit(&self, x: f64, y: f64) -> Option<ClientHandle> {
        let (x, y) = self.to_layout(x, y);
        self.layout
            .iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(handle, _)| *handle)
    }

    /// The edge of the local screen a card is closest to.
    fn nearest_edge(&self, rect: Rect) -> Position {
        let (cx, cy) = rect.center();
        let (lx, ly) = self.local.center();
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
}

/// Replace the content of `container` with the visual arrangement.
pub(super) fn rebuild(
    container: &gtk::Box,
    local_hostname: &str,
    machines: &[Machine],
    on_move: impl Fn(ClientHandle, Position) + 'static,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let state = Rc::new(RefCell::new(State {
        local_hostname: local_hostname.to_string(),
        machines: machines
            .iter()
            .map(|machine| Machine {
                handle: machine.handle,
                hostname: machine.hostname.clone(),
                position: machine.position,
                active: machine.active,
            })
            .collect(),
        on_move: Rc::new(on_move),
        layout: Vec::new(),
        local: Rect::default(),
        drag: None,
        hover: None,
        scale: 1.0,
        origin: (0.0, 0.0),
    }));

    let area = gtk::DrawingArea::builder()
        .content_height(CANVAS_HEIGHT)
        .hexpand(true)
        .vexpand(true)
        .build();
    area.set_cursor_from_name(Some("default"));
    area.set_tooltip_text(Some(
        "Drag a screen to the edge it should be reached through, use its menu to pick the edge",
    ));

    {
        let state = Rc::clone(&state);
        area.set_draw_func(move |area, cr, width, height| {
            let mut state = state.borrow_mut();
            state.relayout(width as f64, height as f64);
            draw(area, cr, width as f64, height as f64, &state);
        });
    }

    let motion = gtk::EventControllerMotion::new();
    {
        let state = Rc::clone(&state);
        let weak_area = area.downgrade();
        motion.connect_motion(move |_motion, x, y| {
            let mut state = state.borrow_mut();
            let hover = state.hit(x, y);
            if state.hover != hover {
                state.hover = hover;
                if let Some(area) = weak_area.upgrade() {
                    area.set_cursor_from_name(Some(if hover.is_some() {
                        "grab"
                    } else {
                        "default"
                    }));
                    area.queue_draw();
                }
            }
        });
    }
    {
        let state = Rc::clone(&state);
        let weak_area = area.downgrade();
        motion.connect_leave(move |_motion| {
            let mut state = state.borrow_mut();
            if state.hover.take().is_some() {
                if let Some(area) = weak_area.upgrade() {
                    area.set_cursor_from_name(Some("default"));
                    area.queue_draw();
                }
            }
        });
    }
    area.add_controller(motion);

    let gesture = gtk::GestureDrag::new();
    {
        let state = Rc::clone(&state);
        let weak_area = area.downgrade();
        gesture.connect_drag_begin(move |_gesture, x, y| {
            let mut state = state.borrow_mut();
            let Some(handle) = state.hit(x, y) else {
                return;
            };
            let Some(rect) = state.rect_of(handle) else {
                return;
            };
            state.drag = Some(Drag {
                handle,
                origin: (rect.x, rect.y),
                corner: (rect.x, rect.y),
                target: state.nearest_edge(rect),
            });
            state.hover = None;
            if let Some(area) = weak_area.upgrade() {
                area.set_cursor_from_name(Some("grabbing"));
                area.queue_draw();
            }
        });
    }
    {
        let state = Rc::clone(&state);
        let weak_area = area.downgrade();
        gesture.connect_drag_update(move |_gesture, offset_x, offset_y| {
            let mut state = state.borrow_mut();
            let scale = state.scale;
            let Some(drag) = state.drag.as_mut() else {
                return;
            };
            drag.corner = (
                drag.origin.0 + offset_x / scale,
                drag.origin.1 + offset_y / scale,
            );
            let candidate = Rect {
                x: drag.corner.0,
                y: drag.corner.1,
                w: CARD_WIDTH,
                h: CARD_HEIGHT,
            };
            let target = state.nearest_edge(candidate);
            if let Some(drag) = state.drag.as_mut() {
                drag.target = target;
            }
            if let Some(area) = weak_area.upgrade() {
                area.queue_draw();
            }
        });
    }
    {
        let state = Rc::clone(&state);
        let weak_area = area.downgrade();
        gesture.connect_drag_end(move |_gesture, _offset_x, _offset_y| {
            let mut state = state.borrow_mut();
            let Some(drag) = state.drag.take() else {
                return;
            };
            let current = state.machine(drag.handle).map(|machine| machine.position);
            if current != Some(drag.target) {
                (state.on_move)(drag.handle, drag.target);
                /* Move the card right away so it does not jump back while the
                 * daemon is still answering; `rebuild` redraws the final
                 * layout from the client store either way. */
                if let Some(machine) = state.machine_mut(drag.handle) {
                    machine.position = drag.target;
                }
            }
            if let Some(area) = weak_area.upgrade() {
                area.set_cursor_from_name(Some("default"));
                area.queue_draw();
            }
        });
    }
    area.add_controller(gesture);

    /* The menu behind the three dots: pick the edge by hand when dragging is not
     * precise enough. */
    let menu_handle = Rc::new(RefCell::new(None::<ClientHandle>));
    let popover = gtk::Popover::builder().has_arrow(true).build();
    popover.set_parent(&area);
    {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        for (label, position) in [
            ("Move to the left", Position::Left),
            ("Move to the right", Position::Right),
            ("Move to the top", Position::Top),
            ("Move to the bottom", Position::Bottom),
        ] {
            let button = gtk::Button::with_label(label);
            button.add_css_class("flat");
            button.set_halign(gtk::Align::Fill);
            let state = Rc::clone(&state);
            let menu_handle = Rc::clone(&menu_handle);
            let popover = popover.clone();
            button.connect_clicked(move |_button| {
                if let Some(handle) = *menu_handle.borrow() {
                    let mut state = state.borrow_mut();
                    if state.machine(handle).map(|machine| machine.position) != Some(position) {
                        (state.on_move)(handle, position);
                        if let Some(machine) = state.machine_mut(handle) {
                            machine.position = position;
                        }
                    }
                }
                popover.popdown();
            });
            list.append(&button);
        }
        popover.set_child(Some(&list));
    }
    {
        /* The popover is parented to the drawing area, so it has to be detached
         * again before the area is rebuilt, otherwise GTK refuses to finalise
         * the widget (and warns on every refresh). */
        let popover = popover.clone();
        area.connect_unrealize(move |_area| popover.unparent());
    }

    /* Capture phase: the dots are claimed before the drag gesture sees the
     * press, a press anywhere else on a card falls through to the drag. */
    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let state = Rc::clone(&state);
        let menu_handle = Rc::clone(&menu_handle);
        let popover = popover.clone();
        click.connect_pressed(move |click, _n, x, y| {
            /* Only the dots in the card corner open the menu, a press anywhere
             * else on a card is part of a drag. The borrow is released before the
             * popover is shown: popping it up moves the pointer focus and runs
             * the motion handlers, which borrow the same state. */
            let Some((handle, bounds)) = ({
                let state = state.borrow();
                let handle = state.hit(x, y);
                let rect = handle.and_then(|handle| state.rect_of(handle));
                match (handle, rect) {
                    (Some(handle), Some(rect)) => {
                        let (lx, ly) = state.to_layout(x, y);
                        let dots = Rect {
                            x: rect.x + rect.w - 34.0,
                            y: rect.y + 4.0,
                            w: 30.0,
                            h: 24.0,
                        };
                        dots.contains(lx, ly).then(|| {
                            (
                                handle,
                                gtk::gdk::Rectangle::new(
                                    (rect.x * state.scale + state.origin.0) as i32,
                                    (rect.y * state.scale + state.origin.1) as i32,
                                    (rect.w * state.scale) as i32,
                                    (rect.h * state.scale) as i32,
                                ),
                            )
                        })
                    }
                    _ => None,
                }
            }) else {
                return;
            };
            click.set_state(gtk::EventSequenceState::Claimed);
            *menu_handle.borrow_mut() = Some(handle);
            popover.set_pointing_to(Some(&bounds));
            popover.popup();
        });
    }
    area.add_controller(click);

    container.append(&area);

    let hint = gtk::Label::builder()
        .label("Drag a screen to change the edge the pointer crosses to reach it")
        .halign(gtk::Align::Center)
        .build();
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    container.append(&hint);
}

fn set_source(cr: &cairo::Context, color: (f64, f64, f64), alpha: f64) {
    cr.set_source_rgba(color.0, color.1, color.2, alpha);
}

fn rounded_rect(cr: &cairo::Context, rect: Rect, radius: f64) {
    let r = radius.min(rect.h / 2.0).min(rect.w / 2.0);
    cr.new_sub_path();
    cr.arc(rect.x + rect.w - r, rect.y + r, r, -PI / 2.0, 0.0);
    cr.arc(rect.x + rect.w - r, rect.y + rect.h - r, r, 0.0, PI / 2.0);
    cr.arc(rect.x + r, rect.y + rect.h - r, r, PI / 2.0, PI);
    cr.arc(rect.x + r, rect.y + r, r, PI, 3.0 * PI / 2.0);
    cr.close_path();
}

/// Draw the text centred on (`cx`, `cy`), shortened with an ellipsis when it
/// does not fit into `max_width`.
fn draw_text(cr: &cairo::Context, text: &str, cx: f64, cy: f64, max_width: f64, size: f64) {
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(size);

    let fits = |candidate: &str| {
        cr.text_extents(candidate)
            .map(|extents| extents.width() <= max_width)
            .unwrap_or(true)
    };

    let mut label = text.to_string();
    if !fits(&label) {
        while !label.is_empty() {
            label.pop();
            let candidate = format!("{label}…");
            if fits(&candidate) {
                label = candidate;
                break;
            }
        }
    }

    let Ok(extents) = cr.text_extents(&label) else {
        return;
    };
    let x = cx - (extents.width() / 2.0 + extents.x_bearing());
    let y = cy - (extents.height() / 2.0 + extents.y_bearing());
    cr.move_to(x, y);
    let _ = cr.show_text(&label);
}

fn draw(_area: &gtk::DrawingArea, cr: &cairo::Context, width: f64, height: f64, state: &State) {
    /* The colours are derived from the dark/light preference instead of being
     * looked up in the style context: `gtk_style_context_lookup_color` is
     * deprecated, and these values read well on both themes. */
    let dark = adw::StyleManager::default().is_dark();
    let foreground = if dark {
        (0.92, 0.92, 0.94)
    } else {
        (0.11, 0.11, 0.13)
    };
    let palette = Palette {
        foreground,
        accent: if dark {
            (0.36, 0.62, 0.95)
        } else {
            (0.20, 0.47, 0.85)
        },
        success: if dark {
            (0.35, 0.82, 0.47)
        } else {
            (0.15, 0.65, 0.32)
        },
    };

    /* Canvas: a soft panel with a dot grid, so the empty space still reads as
     * a layout editor. */
    cr.set_operator(cairo::Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    let _ = cr.paint();
    cr.set_operator(cairo::Operator::Over);

    let canvas = Rect {
        x: 0.5,
        y: 0.5,
        w: width - 1.0,
        h: height - 1.0,
    };
    rounded_rect(cr, canvas, 14.0);
    set_source(cr, palette.foreground, 0.04);
    let _ = cr.fill_preserve();
    set_source(cr, palette.foreground, 0.12);
    cr.set_line_width(1.0);
    let _ = cr.stroke();

    let mut y = GRID_STEP;
    while y < height {
        let mut x = GRID_STEP;
        while x < width {
            cr.new_sub_path();
            cr.arc(x, y, 1.2, 0.0, 2.0 * PI);
            set_source(cr, palette.foreground, 0.12);
            let _ = cr.fill();
            x += GRID_STEP;
        }
        y += GRID_STEP;
    }

    /* Everything below is laid out in layout coordinates; scale and centre it so
     * a wide arrangement still fits into the panel. */
    let _ = cr.save();
    cr.translate(state.origin.0, state.origin.1);
    cr.scale(state.scale, state.scale);

    /* Links first, they run underneath the cards. */
    for machine in &state.machines {
        let Some(peer) = state.rect_of(machine.handle) else {
            continue;
        };
        /* Anchor both ends at the peer's own offset along the edge, otherwise
         * several peers on one edge all converge on the middle and the bridges
         * cross each other. */
        let from = state.local.point_at(machine.position, peer);
        let to = peer.edge_mid(machine.position.opposite());
        let (color, alpha) = if machine.active {
            (palette.success, 0.95)
        } else {
            (palette.foreground, 0.28)
        };
        /* A thick bridge between the two facing edges, the way Synergy draws a
         * link: the machines touch each other through it. */
        set_source(cr, color, alpha);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_width(9.0);
        cr.move_to(from.0, from.1);
        cr.line_to(to.0, to.1);
        let _ = cr.stroke();
        cr.set_line_cap(cairo::LineCap::Butt);

        /* Louder when the peer holds the pointer. */
        if machine.active {
            set_source(cr, palette.success, 0.35);
            cr.set_line_width(3.0);
            cr.move_to(from.0, from.1);
            cr.line_to(to.0, to.1);
            let _ = cr.stroke();
        }
    }

    /* Highlight the edge the dragged card is going to snap to. */
    if let Some(drag) = state.drag.as_ref() {
        let local = state.local;
        let (x0, y0, x1, y1) = match drag.target {
            Position::Left => (local.x, local.y, local.x, local.y + local.h),
            Position::Right => (
                local.x + local.w,
                local.y,
                local.x + local.w,
                local.y + local.h,
            ),
            Position::Top => (local.x, local.y, local.x + local.w, local.y),
            Position::Bottom => (
                local.x,
                local.y + local.h,
                local.x + local.w,
                local.y + local.h,
            ),
        };
        set_source(cr, palette.accent, 0.9);
        cr.set_line_width(4.0);
        cr.move_to(x0, y0);
        cr.line_to(x1, y1);
        let _ = cr.stroke();
    }

    /* The local screen. */
    let local = state.local;
    rounded_rect(cr, local, CARD_RADIUS);
    set_source(cr, palette.accent, 0.16);
    let _ = cr.fill_preserve();
    set_source(cr, palette.accent, 0.85);
    cr.set_line_width(2.0);
    let _ = cr.stroke();
    draw_card_contents(
        cr,
        local,
        &state.local_hostname,
        "this screen",
        palette.foreground,
        palette.accent,
    );

    /* Peer screens, the dragged one on top. */
    let mut order = state
        .machines
        .iter()
        .map(|machine| machine.handle)
        .collect::<Vec<_>>();
    if let Some(drag) = state.drag.as_ref() {
        order.retain(|handle| *handle != drag.handle);
        order.push(drag.handle);
    }
    for handle in order {
        let Some(machine) = state.machine(handle) else {
            continue;
        };
        let Some(rect) = state.rect_of(handle) else {
            continue;
        };
        let dragging = state
            .drag
            .as_ref()
            .is_some_and(|drag| drag.handle == handle);
        let hovered = state.hover == Some(handle);

        if dragging {
            /* Lift the card off the canvas with a soft shadow. */
            let shadow = Rect {
                x: rect.x + 1.5,
                y: rect.y + 3.0,
                w: rect.w,
                h: rect.h,
            };
            rounded_rect(cr, shadow, CARD_RADIUS);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.22);
            let _ = cr.fill();
        }

        rounded_rect(cr, rect, CARD_RADIUS);
        set_source(cr, palette.foreground, if dragging { 0.10 } else { 0.06 });
        let _ = cr.fill_preserve();
        if machine.active {
            set_source(cr, palette.success, 0.9);
        } else if hovered || dragging {
            set_source(cr, palette.accent, 0.9);
        } else {
            set_source(cr, palette.foreground, 0.28);
        }
        cr.set_line_width(if hovered || dragging { 2.0 } else { 1.5 });
        let _ = cr.stroke();

        draw_card_contents(
            cr,
            rect,
            &machine.hostname,
            edge_label(machine.position),
            palette.foreground,
            if machine.active {
                palette.success
            } else {
                palette.accent
            },
        );

        /* Status dot in the top right corner, filled while the peer holds the
         * pointer. */
        cr.new_sub_path();
        cr.arc(rect.x + rect.w - 12.0, rect.y + 12.0, 3.5, 0.0, 2.0 * PI);
        if machine.active {
            let _ = cr.fill();
        } else {
            cr.set_line_width(1.2);
            let _ = cr.stroke();
        }
    }

    let _ = cr.restore();

    if state.machines.is_empty() {
        set_source(cr, palette.foreground, 0.55);
        draw_text(
            cr,
            "No machines yet - add one on the Devices page",
            width / 2.0,
            height - 28.0,
            width - 40.0,
            12.0,
        );
    }
}

fn draw_card_contents(
    cr: &cairo::Context,
    rect: Rect,
    name: &str,
    subtitle: &str,
    foreground: (f64, f64, f64),
    accent: (f64, f64, f64),
) {
    /* A small monitor glyph in the top left corner, drawn instead of loading an
     * icon so the cards stay crisp while they are dragged around. */
    let glyph = Rect {
        x: rect.x + 14.0,
        y: rect.y + 14.0,
        w: 20.0,
        h: 12.0,
    };
    rounded_rect(cr, glyph, 2.0);
    set_source(cr, accent, 0.8);
    cr.set_line_width(1.4);
    let _ = cr.stroke();
    cr.move_to(rect.x + 20.0, glyph.y + glyph.h + 3.0);
    cr.line_to(rect.x + 28.0, glyph.y + glyph.h + 3.0);
    let _ = cr.stroke();

    /* The three dot menu in the top right corner, matching the Synergy cards. */
    set_source(cr, foreground, 0.45);
    for i in 0..3 {
        cr.new_sub_path();
        cr.arc(
            rect.x + rect.w - 22.0 + i as f64 * 4.0,
            rect.y + 16.0,
            1.1,
            0.0,
            2.0 * PI,
        );
        let _ = cr.fill();
    }

    set_source(cr, foreground, 0.95);
    draw_text(
        cr,
        name,
        rect.x + rect.w / 2.0,
        rect.y + rect.h / 2.0 + 8.0,
        rect.w - 28.0,
        13.5,
    );

    /* The edge this screen is reached through, shown as a pill so it does not
     * read as part of the hostname. */
    let (text_width, _) = measure(cr, subtitle, 10.0);
    let pill = Rect {
        x: rect.x + rect.w / 2.0 - text_width / 2.0 - 7.0,
        y: rect.y + rect.h - 25.0,
        w: text_width + 14.0,
        h: 16.0,
    };
    rounded_rect(cr, pill, 8.0);
    set_source(cr, accent, 0.16);
    let _ = cr.fill();
    set_source(cr, accent, 0.9);
    draw_text(
        cr,
        subtitle,
        pill.x + pill.w / 2.0,
        pill.y + pill.h / 2.0,
        pill.w - 4.0,
        10.0,
    );
}

/// Width and height of `text` at `size`, for laying out the pills.
fn measure(cr: &cairo::Context, text: &str, size: f64) -> (f64, f64) {
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(size);
    match cr.text_extents(text) {
        Ok(extents) => (extents.width(), extents.height()),
        Err(_) => (size * text.chars().count() as f64 * 0.6, size),
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
