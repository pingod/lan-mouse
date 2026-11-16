//! Visual arrangement of the screens lan-mouse can cross to.
//!
//! The canvas is a map of the local screen with the peers attached to its edges,
//! drawn to scale: a laptop beside a 4K panel is visibly smaller, and a peer is as
//! long along the shared edge as the segment of it it is reached through. That is
//! the point - the picture and the daemon's barriers come out of the same
//! [`layout`] functions, so a stretch of edge the canvas shows as a wall is a
//! stretch where a crossing reaches nobody.
//!
//! Dragging a screen changes its link: the overlap of the two facing edges becomes
//! the pair of ranges the daemon routes by, because [`layout::snap`] is the inverse
//! of [`layout::place`]. The handles on the local edge move one end of a peer's
//! coverage, which is how a range is set precisely - dragging a whole card across a
//! long edge is not.
//!
//! Everything is painted with cairo on a single [`gtk::DrawingArea`]: the layout,
//! the links, the walls and the handles share one coordinate system, which is one
//! layout pass rather than a nest of styled boxes.

mod draw;
pub mod layout;
mod paint;

use std::cell::RefCell;
use std::rc::Rc;

use gtk::{glib, prelude::*};
use lan_mouse_ipc::{ClientHandle, EdgeRange, Position, SwitchOptions};

use self::layout::{Placement, Screen};
use self::paint::Palette;

/// One peer that can be dragged around the local screen.
#[derive(Clone, Debug)]
pub(super) struct Machine {
    pub handle: ClientHandle,
    pub hostname: String,
    pub position: Position,
    pub active: bool,
    /// the local monitor this link is attached to, by name
    ///
    /// `None` = the whole machine, so `edge` is a fraction of the bounding box.
    pub display: Option<String>,
    /// segment of the local edge the pointer crosses to reach it
    pub edge: EdgeRange,
    /// segment of the peer's facing edge the cursor lands in
    pub peer_edge: EdgeRange,
    /// what the peer reported about its screen, in pixels
    pub peer_size: Option<(f64, f64)>,
    /// the picture the peer says both machines are drawing, if it sent one
    ///
    /// The two windows have to show the same desk, and two derivations from two
    /// configs cannot guarantee that. The peer publishes the picture it is drawing;
    /// the higher `(revision, author)` wins, and the loser adopts it.
    pub peer_picture: Option<input_event::Arrangement>,
    /// where the peer's group sits, exactly as the adopted picture said.
    ///
    /// Set only by `adopt`. Drawing from the placement instead would round-trip the
    /// picture through "which edge and which range", which cannot reproduce an
    /// arbitrary rectangle: a peer placed by hand ended up somewhere else than the
    /// picture said, on both machines.
    pub peer_rect: Option<Screen>,
    /// the peer's monitors, normalised so the group's origin is (0,0)
    ///
    /// Drawn as individual screens rather than one box, for the same reason the
    /// local machine is: the ranges and the barriers are per monitor, so a peer
    /// flattened into one rectangle would put its screens where they are not.
    pub peer_monitors: Vec<layout::Monitor>,
}

/// What the canvas asks the window to do.
///
/// The canvas never talks to the daemon itself: it reports the intent, so the same
/// canvas works for a page that only shows the layout and one that edits it.
#[derive(Clone, Debug)]
pub(super) enum MachineRequest {
    /// move to another edge, and/or change the ranges of the link it has
    Move {
        handle: ClientHandle,
        position: Position,
        display: Option<String>,
        edge: EdgeRange,
        peer_edge: EdgeRange,
    },
    /// hand the shared picture to the daemon, which puts it in the report it sends
    Publish(String),
    /// give every peer of an edge an equal, adjacent stretch of it
    AutoArrange(Vec<(ClientHandle, Position, Option<String>, EdgeRange, EdgeRange)>),
}

/// The monitors a peer reported, as rectangles normalised to their own origin.
///
/// The report is the same per-monitor form the local backend sends, so it decodes
/// with the same code. A peer that sent nothing, or an older one that only sent a
/// size, leaves this empty and is drawn as one box - which is all that is known
/// about it.
pub fn peer_monitors(displays: &str) -> Vec<layout::Monitor> {
    let Some(layout) = input_event::Layout::decode(displays) else {
        return Vec::new();
    };
    let rects = layout
        .enabled()
        .map(|display| {
            let rect = display.on_screen();
            layout::Monitor::new(
                display.name.clone(),
                Screen::new(
                    rect.x as f64,
                    rect.y as f64,
                    rect.width as f64,
                    rect.height as f64,
                ),
            )
        })
        .collect::<Vec<_>>();
    /* Normalised to the group's own corner so a monitor's rectangle can be added
     * straight onto wherever the group was placed. Without this a peer whose
     * monitors do not start at (0,0) - which is most desks with a rotated panel -
     * would be drawn offset from its own box. */
    let Some(first) = rects.first() else {
        return Vec::new();
    };
    let mut min_x = first.rect.x;
    let mut min_y = first.rect.y;
    for monitor in &rects {
        min_x = min_x.min(monitor.rect.x);
        min_y = min_y.min(monitor.rect.y);
    }
    rects
        .into_iter()
        .map(|mut monitor| {
            monitor.rect.x -= min_x;
            monitor.rect.y -= min_y;
            monitor
        })
        .collect()
}

/// Milliseconds since the epoch, used as the revision of an edit.
///
/// A clock rather than a counter because the two machines each keep their own
/// counter and a counter cannot order edits made on different machines.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The bounding box of a set of monitors, in layout units.
fn union_screen(monitors: &[layout::Monitor]) -> Option<Screen> {
    let mut rects = monitors.iter().map(|monitor| monitor.rect);
    let first = rects.next()?;
    Some(rects.fold(first, |acc, rect| acc.merged(rect)))
}

/// A screen rectangle as the wire type.
fn screen_rect(rect: Screen) -> input_event::ScreenRect {
    input_event::ScreenRect {
        x: rect.x as i32,
        y: rect.y as i32,
        width: rect.w.max(1.0) as u32,
        height: rect.h.max(1.0) as u32,
    }
}

/// The reverse of [`screen_rect`].
fn screen_from(rect: input_event::ScreenRect) -> Screen {
    Screen::new(
        rect.x as f64,
        rect.y as f64,
        rect.width as f64,
        rect.height as f64,
    )
}

/// A display entry that occupies exactly this rectangle.
///
/// The picture carries rectangles, not panels and rotations: it describes where the
/// screens ended up, which is the whole point of it, so there is nothing left to
/// rotate.
fn display_of(rect: &input_event::ScreenRect) -> input_event::DisplayInfo {
    input_event::DisplayInfo {
        name: String::new(),
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
        rotation: input_event::Rotation::Normal,
        enabled: true,
    }
}

/// The bounding box of a set of rectangles.
fn union_of(rects: &[input_event::ScreenRect]) -> Option<input_event::ScreenRect> {
    let mut screens = rects.iter();
    let first = *screens.next()?;
    Some(screens.fold(first, |acc, rect| {
        let x = acc.x.min(rect.x);
        let y = acc.y.min(rect.y);
        let right = (acc.x + acc.width as i32).max(rect.x + rect.width as i32);
        let bottom = (acc.y + acc.height as i32).max(rect.y + rect.height as i32);
        input_event::ScreenRect {
            x,
            y,
            width: (right - x) as u32,
            height: (bottom - y) as u32,
        }
    }))
}

/// Rectangles as monitors normalised to their own corner, so a group can be drawn
/// wherever it was placed.
fn normalise_monitors(rects: &[input_event::ScreenRect]) -> Vec<layout::Monitor> {
    let Some(bounds) = union_of(rects) else {
        return Vec::new();
    };
    rects
        .iter()
        .map(|rect| {
            let screen = screen_from(*rect);
            layout::Monitor::new(
                "",
                Screen::new(
                    screen.x - bounds.x as f64,
                    screen.y - bounds.y as f64,
                    screen.w,
                    screen.h,
                ),
            )
        })
        .collect()
}

/// The canvas height the page asks for before the window grows it.
const CANVAS_HEIGHT: i32 = 460;

/// All four edges, in the order the daemon resolves them in.
fn positions() -> [Position; 4] {
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
}

/// What the last draw produced, so the gestures can hit test without recomputing.
#[derive(Clone, Default)]
struct Geometry {
    /// the local screen, in layout units
    local: Screen,
    /// scale from layout units to canvas units
    scale: f64,
    /// translation applied to the scaled layout
    offset: (f64, f64),
    /// where each peer ended up
    peers: Vec<(ClientHandle, Screen)>,
}

/// A peer being dragged.
struct Drag {
    handle: ClientHandle,
    /// the card as it was when the drag started, in layout units
    original: Screen,
    /// where the pointer currently puts it
    current: Screen,
    /// what the drop would configure
    placement: Placement,
}

struct State {
    local_hostname: String,
    /// every monitor of this machine, in pixels; empty when no backend reported
    /// any geometry
    local_displays: Vec<input_event::DisplayInfo>,
    machines: Vec<Machine>,
    switch_options: SwitchOptions,
    on_request: Rc<dyn Fn(MachineRequest)>,
    geometry: Geometry,
    drag: Option<Drag>,
    /// the card the pointer is over, which is also the keyboard-selected one
    hover: Option<ClientHandle>,
    /// the view's pan when a pan began, while the view is being dragged
    panning: Option<(f64, f64)>,
    /// where a drag of this machine's own screens began, in canvas units
    moving_local: Option<(f64, f64)>,
    /// how far this machine's screens have been moved from where the compositor put
    /// them, in layout units - a drag of the local screens, not a pan
    ///
    /// Only the drawing and the hit test see this. Peers are placed against the
    /// monitors where the compositor has them, so dragging this machine does not
    /// drag the peers along with it - the point is to change where this machine sits
    /// relative to them.
    local_offset: (f64, f64),
    /// when this side last changed the layout, in milliseconds
    ///
    /// It is the tie-breaker in the shared picture and it must only move on an
    /// edit: a value that also moved when the picture was merely recomputed would
    /// always win, and the two machines would never settle on one picture.
    local_revision: u64,
    /// the last picture handed to the daemon, so an unchanged one is not resent
    published: String,
    /// zoom applied on top of the automatic fit
    zoom: f64,
    /// pan applied on top of the automatic centring, in canvas units
    pan: (f64, f64),
    /// the last canvas size, needed to place the grid and the empty-state text
    canvas: (f64, f64),
    show_grid: bool,
}

impl State {
    fn machine(&self, handle: ClientHandle) -> Option<&Machine> {
        self.machines.iter().find(|m| m.handle == handle)
    }

    /// The local screen in layout units.
    ///
    /// Without a reported size the canvas falls back to 16:9 and says so on the
    /// card: an edge range means nothing visually if the screen it is a range of
    /// has no size, but the layout is still worth drawing.
    /// The local screen in layout units: the bounding box of every monitor.
    ///
    /// The bounding box is what an edge range is a fraction of - the barrier spans
    /// it - so the peers are placed against it. The monitors themselves are drawn
    /// inside it by [`State::local_monitors`], because a desk with a rotated
    /// monitor has area inside the box that no screen covers.
    fn local_screen(&self) -> Screen {
        let Some(bounds) = self.local_bounds() else {
            /* no backend reported geometry: fall back to a 16:9 box and say so on
             * the card, so the layout is still worth drawing */
            return Screen::new(0.0, 0.0, 1920.0, 1080.0);
        };
        Screen::new(
            bounds.x as f64,
            bounds.y as f64,
            bounds.width as f64,
            bounds.height as f64,
        )
    }

    fn local_layout(&self) -> Option<input_event::Layout> {
        (!self.local_displays.is_empty())
            .then(|| input_event::Layout::new(self.local_displays.clone()))
    }

    fn local_bounds(&self) -> Option<input_event::ScreenRect> {
        self.local_layout()?.bounds()
    }

    /// Record that this side changed the layout, so its picture wins.
    ///
    /// Called from the gestures and only from the gestures. Detecting an edit by
    /// watching the configuration for changes does not work: the daemon rewrites the
    /// file, a peer's action changes a link, and this window's own re-derivation
    /// changes the geometry - all of those look like edits and all of them stamped a
    /// fresh revision, which made the local picture always newer than the peer's and
    /// the canvas never adopt one.
    fn mark_edited(&mut self) {
        self.local_revision = now_millis();
        self.published.clear();
    }

    /// The picture this side would draw, from its own geometry.
    ///
    /// Published so the other machine can draw exactly this rather than deriving
    /// its own - two derivations from two configs can disagree, and did.
    fn local_picture(&self) -> input_event::Arrangement {
        let mut picture =
            input_event::Arrangement::new(self.local_revision, self.local_hostname.clone());
        /* `drawn_monitors`, not `local_monitors`: dragging this machine's own screens
         * moves them, and that move has to reach the picture. Using the compositor
         * positions ignored the drag, so a user dragging this machine - the only way to
         * say "this screen sits above that one" from this side - published an unchanged
         * picture and nothing happened anywhere. */
        let mine = self
            .drawn_monitors()
            .iter()
            .map(|monitor| screen_rect(monitor.rect))
            .collect::<Vec<_>>();
        picture.machines.push(input_event::ArrangedMachine {
            hostname: self.local_hostname.clone(),
            authored: true,
            screens: mine,
        });
        for machine in &self.machines {
            if !machine.active {
                continue;
            }
            let screens = if machine.peer_monitors.is_empty() {
                self.rect_of(machine.handle)
                    .map(|rect| vec![screen_rect(rect)])
                    .unwrap_or_default()
            } else {
                self.screens_of(machine.handle)
                    .iter()
                    .map(|rect| screen_rect(*rect))
                    .collect()
            };
            if screens.is_empty() {
                continue;
            }
            picture.machines.push(input_event::ArrangedMachine {
                hostname: machine.hostname.clone(),
                authored: false,
                screens,
            });
        }
        picture
    }

    /// The peer's picture, when it is newer than this side's own.
    ///
    /// Both machines run this with the same inputs, so both reach the same answer
    /// without a round trip: the revision decides, and the author breaks a tie
    /// because two sides can each reach the same revision without having seen the
    /// other's edit.
    fn newer_peer_picture(&self) -> Option<input_event::Arrangement> {
        let local = input_event::Arrangement::new(self.local_revision, self.local_hostname.clone());
        self.machines
            .iter()
            .filter_map(|machine| machine.peer_picture.clone())
            .filter(|picture| picture.supersedes(&local))
            .max_by(|a, b| (a.revision, a.author.clone()).cmp(&(b.revision, b.author.clone())))
    }

    /// Draw `picture` instead of deriving one.
    ///
    /// The geometry is replaced rather than layered on top: every drawing and
    /// hit-testing path reads `local_displays`, `machines[].position` and the peers'
    /// monitors, so putting the picture there is what makes the whole canvas agree
    /// with it - including the handles, which have to stay on the edge the picture
    /// says the link is on.
    fn adopt(&mut self, picture: &input_event::Arrangement) {
        /* This machine is whichever entry is not the author's. Matching on the
         * hostname cannot work: the picture was produced by the peer, which names
         * this machine however *its* configuration says - `192.168.1.5` for a
         * machine that calls itself `pavia-macbook-pro.local`. That mismatch made
         * adoption return early and the two windows never agreed. */
        let peers = picture.peer_screens();
        let mine = match peers.len() {
            1 => peers[0].1.to_vec(),
            /* more than one peer in the picture: fall back to the name, which is
             * right at least when both sides happen to agree on it */
            _ => peers
                .iter()
                .find(|(hostname, _)| *hostname == self.local_hostname)
                .map(|(_, screens)| screens.to_vec())
                .unwrap_or_default(),
        };
        if mine.is_empty() {
            /* the picture does not describe this machine - drawing nothing would be
             * worse than drawing this side's own derivation */
            log::warn!(
                "cannot adopt the peer's arrangement: it has {} machine(s) but none of them is this one",
                picture.machines.len()
            );
            return;
        }
        self.local_displays = mine.iter().map(display_of).collect();
        let local = self.local_screen();
        let monitors = self.local_monitors();
        let authored = picture.authored_screens().to_vec();
        for machine in &mut self.machines {
            /* every peer in the picture is the author, because this side is the only
             * receiver there is */
            let theirs = authored.clone();
            if theirs.is_empty() {
                continue;
            }
            let Some(bounds) = union_of(&theirs) else {
                continue;
            };
            let candidate = Screen::new(
                bounds.x as f64,
                bounds.y as f64,
                bounds.width as f64,
                bounds.height as f64,
            );
            /* The picture gives absolute rectangles; the canvas places a peer group
             * from a link. Inverting the placement here means the rest of the canvas
             * keeps working unchanged, and both machines invert the same picture the
             * same way, so they still draw the same desk. */
            let placement = layout::snap(local, &monitors, candidate);
            machine.position = placement.position;
            machine.display = placement.display;
            machine.edge = placement.edge;
            machine.peer_edge = placement.peer_edge;
            machine.peer_monitors = normalise_monitors(&theirs);
            machine.peer_rect = Some(candidate);
            machine.peer_size = Some((candidate.w, candidate.h));
        }
    }

    /// Re-derive every peer's link from where this machine has been dragged to.
    ///
    /// The link is a statement about which monitor the peer is beside and how much
    /// of that edge it covers, so moving this machine is a way of editing all of
    /// them at once. Whatever a link already says is kept if the geometry still
    /// implies it, so a drag that only moves the machine does not rewrite links that
    /// did not need to change.
    fn reanchor_after_moving_local(
        &self,
    ) -> Vec<(ClientHandle, Position, Option<String>, EdgeRange, EdgeRange)> {
        /* The *drawn* bounds, not the compositor ones: the whole point of the drag is
         * that this machine has moved, and judging the drop against where the screens
         * used to be made every drag compute the arrangement it already had. That is why
         * a screen could not be put above another one on either machine. */
        let local = union_screen(&self.drawn_monitors()).unwrap_or_else(|| self.local_screen());
        let monitors = self.drawn_monitors();
        let mut out = Vec::new();
        for machine in self.machines.iter().filter(|machine| machine.active) {
            let Some(rect) = self.rect_of(machine.handle) else {
                continue;
            };
            let placement = layout::snap(local, &monitors, rect);
            if placement.position == machine.position
                && placement.display == machine.display
                && placement.edge == machine.edge
                && placement.peer_edge == machine.peer_edge
            {
                continue;
            }
            out.push((
                machine.handle,
                placement.position,
                placement.display,
                placement.edge,
                placement.peer_edge,
            ));
        }
        out
    }

    /// This machine's monitors where they are *drawn*, which is where the compositor
    /// put them unless a drag has moved them.
    fn drawn_monitors(&self) -> Vec<layout::Monitor> {
        let (dx, dy) = self.local_offset;
        self.local_monitors()
            .into_iter()
            .map(|monitor| {
                layout::Monitor::new(
                    monitor.name,
                    Screen::new(
                        monitor.rect.x + dx,
                        monitor.rect.y + dy,
                        monitor.rect.w,
                        monitor.rect.h,
                    ),
                )
            })
            .collect()
    }

    /// The individual monitors, in layout units, with their names.
    ///
    /// The name is what a link stores, so it has to reach the geometry code: it is
    /// the only stable handle on "the screen the user dropped this card beside".
    fn local_monitors(&self) -> Vec<layout::Monitor> {
        let Some(layout) = self.local_layout() else {
            /* no backend geometry: one unnamed screen, and links stay machine-level
             * because there is nothing to name */
            return vec![layout::Monitor::new("", self.local_screen())];
        };
        layout
            .enabled()
            .map(|display| {
                let rect = display.on_screen();
                layout::Monitor::new(
                    display.name.clone(),
                    Screen::new(
                        rect.x as f64,
                        rect.y as f64,
                        rect.width as f64,
                        rect.height as f64,
                    ),
                )
            })
            .collect()
    }

    fn placement_of(&self, machine: &Machine, local: Screen) -> Placement {
        Placement {
            position: machine.position,
            edge: machine.edge,
            peer_edge: machine.peer_edge,
            display: machine.display.clone(),
            size: layout::peer_size(
                local,
                machine.position,
                machine.edge,
                machine.peer_edge,
                machine.peer_size,
            ),
        }
    }

    /// The peers placed around the local screen, in layout units.
    fn placements(&self) -> Vec<(ClientHandle, Screen)> {
        let local = self.local_screen();
        let placements = self
            .machines
            .iter()
            .filter(|machine| machine.active)
            .map(|machine| (machine.handle, self.placement_of(machine, local)))
            .collect::<Vec<_>>();
        /* `place_all` rather than `place`: two peers whose cards overlap along the
         * edge have to be drawn in successive lanes, or one hides the other and
         * the canvas looks like a layout that cannot exist */
        layout::place_all(local, &self.local_monitors(), &placements)
    }

    /// Map canvas coordinates to layout coordinates.
    fn to_layout(&self, x: f64, y: f64) -> (f64, f64) {
        let scale = self.geometry.scale.max(f64::EPSILON);
        (
            (x - self.geometry.offset.0) / scale,
            (y - self.geometry.offset.1) / scale,
        )
    }

    /// Map a layout rectangle to canvas coordinates.
    fn to_canvas(&self, rect: Screen) -> Screen {
        let scale = self.geometry.scale;
        let (ox, oy) = self.geometry.offset;
        Screen::new(
            rect.x * scale + ox,
            rect.y * scale + oy,
            rect.w * scale,
            rect.h * scale,
        )
    }

    /// The card of `handle`: a drag first, then the picture, then the placement.
    ///
    /// The picture before the placement, because those two disagree by construction:
    /// the placement is derived from the link (an edge and two fractions) and cannot
    /// reproduce an arbitrary rectangle, while an adopted picture carries the exact
    /// rectangles both machines agreed on. Reading the placement here made the link
    /// band, the hit test and the drawn card use three different answers.
    fn rect_of(&self, handle: ClientHandle) -> Option<Screen> {
        if let Some(drag) = self.drag.as_ref().filter(|d| d.handle == handle) {
            return Some(drag.current);
        }
        if let Some(rect) = self.machine(handle).and_then(|machine| machine.peer_rect) {
            return Some(rect);
        }
        self.geometry
            .peers
            .iter()
            .find(|(other, _)| *other == handle)
            .map(|(_, rect)| *rect)
    }

    /// Where a machine's monitors are, in layout coordinates.
    ///
    /// A peer that reported its screens is as many rectangles as it has; one that
    /// reported nothing is the single box it was placed as.
    fn screens_of(&self, handle: ClientHandle) -> Vec<Screen> {
        let Some(group) = self
            .machine(handle)
            .and_then(|machine| machine.peer_rect)
            .or_else(|| self.rect_of(handle))
        else {
            return Vec::new();
        };
        let monitors = self
            .machine(handle)
            .map(|machine| machine.peer_monitors.clone())
            .unwrap_or_default();
        if monitors.is_empty() {
            return vec![group];
        }
        monitors
            .iter()
            .map(|monitor| {
                Screen::new(
                    group.x + monitor.rect.x,
                    group.y + monitor.rect.y,
                    monitor.rect.w,
                    monitor.rect.h,
                )
            })
            .collect()
    }

    /// The local monitor a machine's link is attached to.
    ///
    /// The link is drawn against this rather than against the bounding box: the
    /// range is a fraction of *this* monitor's edge, so the bridge has to start
    /// where this monitor ends. Using the box made the bridge's height negative on
    /// a desk with unequal heights, which is why the two screens stopped looking
    /// connected at all.
    fn anchor_of(&self, machine: &Machine) -> Screen {
        let local = self.local_screen();
        let placement = self.placement_of(machine, local);
        layout::anchor_monitor(local, &self.local_monitors(), &placement)
    }

    /// A machine's position in the list, for picking its colour.
    ///
    /// By handle rather than by list index, so a machine does not change colour
    /// when another one is enabled, disabled or removed.
    fn machine_index(&self, handle: ClientHandle) -> usize {
        self.machines
            .iter()
            .position(|machine| machine.handle == handle)
            .unwrap_or(0)
    }

    /// The topmost card under a canvas point.
    fn hit(&self, x: f64, y: f64) -> Option<ClientHandle> {
        let (x, y) = self.to_layout(x, y);
        // the dragged card is drawn last, so it is tested first
        if let Some(drag) = self.drag.as_ref() {
            if drag.current.contains(x, y) {
                return Some(drag.handle);
            }
        }
        self.geometry
            .peers
            .iter()
            .rev()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(handle, _)| *handle)
    }

    /// Recompute the layout for a canvas of `width` x `height`.
    ///
    /// A pure function of the machines and the view, so it is redone on every draw
    /// and a window resize needs nothing but a redraw.
    fn relayout(&mut self, width: f64, height: f64) {
        self.canvas = (width, height);
        let local = self.local_screen();
        let peers = self.placements();
        let mut bounds = local;
        for (_, rect) in &peers {
            bounds = bounds.merged(*rect);
        }
        // room for the border, the handles and the text drawn outside the cards
        const PADDING: f64 = 28.0;
        let available_w = (width - 2.0 * PADDING).max(1.0);
        let available_h = (height - 2.0 * PADDING).max(1.0);
        let fit = (available_w / bounds.w.max(1.0))
            .min(available_h / bounds.h.max(1.0))
            .clamp(0.01, 4.0);
        let scale = fit * self.zoom;
        let offset = (
            (width - bounds.w * scale) / 2.0 - bounds.x * scale + self.pan.0,
            (height - bounds.h * scale) / 2.0 - bounds.y * scale + self.pan.1,
        );
        self.geometry = Geometry {
            local,
            scale,
            offset,
            peers,
        };
        /* Hand the picture this canvas is drawing to the daemon, which puts it in
         * the report it sends peers - that is what makes the other window show this
         * desk instead of deriving its own. Skipped while a peer's picture is being
         * drawn, otherwise each side would republish the other's and the revision
         * would only ever go up. */
        /* Not while something is being dragged: the picture would be the transient
         * geometry of a card mid-gesture, published dozens of times a second to a peer
         * that cannot use it. The commit that ends the drag publishes the result. */
        let settled = self.drag.is_none() && self.moving_local.is_none();
        if settled && self.newer_peer_picture().is_none() {
            let encoded = self.local_picture().encode();
            if encoded != self.published {
                self.published = encoded.clone();
                (self.on_request)(MachineRequest::Publish(encoded));
            }
        }
    }

    /// The peers that do not get the whole stretch of edge they asked for.
    /// Reset the zoom and pan.
    fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
    }

    fn unreachable(&self) -> Vec<ClientHandle> {
        let mut out = Vec::new();
        for position in positions() {
            let on_edge = self
                .machines
                .iter()
                .filter(|m| m.position == position && m.active)
                .collect::<Vec<_>>();
            if on_edge.len() < 2 {
                continue;
            }
            let requested = on_edge.iter().map(|m| m.edge).collect::<Vec<_>>();
            for (machine, effective) in on_edge.into_iter().zip(EdgeRange::resolve(&requested)) {
                if effective != Some(machine.edge) {
                    out.push(machine.handle);
                }
            }
        }
        out
    }

    /// Commit a drag or a resize: report what the pointer decided.
    fn commit(&mut self) {
        if let Some(drag) = self.drag.take() {
            let placement = drag.placement;
            let changed = self.machine(drag.handle).is_some_and(|machine| {
                machine.position != placement.position
                    || machine.display != placement.display
                    || machine.edge != placement.edge
                    || machine.peer_edge != placement.peer_edge
            });
            if changed {
                self.mark_edited();
                /* The picture this side adopted is now out of date: the user just moved
                 * this machine's link, so the placement is the truth and the stale
                 * rectangles would drag the card back to where it used to be. That is
                 * why a drop needed several drags to land - each one moved the link and
                 * the card sprang back to the adopted position. */
                if let Some(machine) = self.machines.iter_mut().find(|m| m.handle == drag.handle) {
                    machine.peer_rect = None;
                }
                (self.on_request)(MachineRequest::Move {
                    handle: drag.handle,
                    position: placement.position,
                    display: placement.display,
                    edge: placement.edge,
                    peer_edge: placement.peer_edge,
                });
            }
        }
    }
}

/// The canvas, kept alive between updates so a drag is never interrupted.
pub(super) struct Canvas {
    area: gtk::DrawingArea,
    state: Rc<RefCell<State>>,
    /// The bar that appears when two machines claim the same stretch of an edge.
    warning: gtk::Box,
    warning_label: gtk::Label,
    /// The label under the canvas, refreshed with the thresholds.
    hint: gtk::Label,
}

impl Canvas {
    /// Build the canvas inside `container` and wire its gestures.
    pub(super) fn build(
        container: &gtk::Box,
        on_request: impl Fn(MachineRequest) + 'static,
    ) -> Self {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }

        let state = Rc::new(RefCell::new(State {
            local_hostname: String::new(),
            local_displays: Vec::new(),
            machines: Vec::new(),
            switch_options: SwitchOptions::default(),
            on_request: Rc::new(on_request),
            geometry: Geometry::default(),
            drag: None,
            hover: None,
            panning: None,
            moving_local: None,
            local_offset: (0.0, 0.0),
            local_revision: 0,
            published: String::new(),
            zoom: 1.0,
            pan: (0.0, 0.0),
            canvas: (1.0, 1.0),
            show_grid: true,
        }));

        /* Two machines claiming the same stretch is legal - the daemon gives the
         * earlier one what it asked for and narrows the other - but it is never
         * what the user meant, and the canvas is where it is visible. A plain box
         * rather than `GtkInfoBar`, which is deprecated since GTK 4.10.
         */
        let warning = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        warning.add_css_class("warning");
        warning.add_css_class("card");
        warning.set_margin_bottom(6);
        warning.set_visible(false);
        let warning_label = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .hexpand(true)
            .build();
        warning.append(&warning_label);
        let fix = gtk::Button::builder().label("Auto arrange").build();
        {
            let state = Rc::clone(&state);
            let warning = warning.clone();
            fix.connect_clicked(move |_| {
                warning.set_visible(false);
                auto_arrange(&state);
            });
        }
        warning.append(&fix);

        let area = gtk::DrawingArea::builder()
            .content_height(CANVAS_HEIGHT)
            .content_width(600)
            .hexpand(true)
            .vexpand(true)
            .build();
        area.set_cursor_from_name(Some("default"));
        area.set_can_focus(true);
        area.set_tooltip_text(Some(
            "Drag a screen so the edges that should connect touch; drag the handles on this screen's edge to set how far a machine reaches along it",
        ));
        let toolbar = toolbar(&state, &area);

        {
            let state = Rc::clone(&state);
            area.set_draw_func(move |_area, cr, width, height| {
                let mut state = state.borrow_mut();
                state.relayout(width as f64, height as f64);
                let palette = Palette::resolve(adw::StyleManager::default().is_dark());
                draw::draw(cr, width as f64, height as f64, &state, &palette);
            });
        }

        add_gestures(&area, &state);

        let hint = hint(&state.borrow());
        container.append(&warning);
        container.append(&toolbar);
        container.append(&area);
        container.append(&hint);

        Self {
            area,
            state,
            warning,
            warning_label,
            hint,
        }
    }

    /// Replace what the canvas shows, without rebuilding it.
    ///
    /// Rebuilding the widget was the bug that made the canvas unusable while
    /// dragging: every state broadcast from the daemon - which happens whenever a
    /// peer connects, is activated or merely answers a ping - destroyed the widget
    /// the pointer was pressed into, so the drag died mid-gesture and the zoom and
    /// pan were reset underneath the user.
    pub(super) fn update(
        &self,
        local_hostname: &str,
        machines: &[Machine],
        displays: &str,
        switch_options: &SwitchOptions,
    ) {
        {
            let mut state = self.state.borrow_mut();
            /* a daemon answer must not yank a card out from under the pointer: the
             * drag owns the machine it started on until it ends */
            if state.drag.is_none() {
                state.local_hostname = local_hostname.to_string();
                state.local_displays = input_event::Layout::decode(displays)
                    .map(|l| l.displays)
                    .unwrap_or_default();
                state.switch_options = switch_options.clone();
                /* A link that changed here is this side's edit, so its picture
                 * becomes the current one and the peer will adopt it. Detected from
                 * the report rather than from the gesture so a change made with
                 * `cli set-edge` counts too. */
                /* Nothing here decides whether an edit happened. A configuration that
                 * changed is not the same thing as a user changing the layout: the
                 * daemon rewrites the file, a peer's action edits a link, and this
                 * window's own re-derivation changes the geometry. Watching for that
                 * made every tick look like an edit, so the local revision was always
                 * the newest and the peer's picture never won. The gestures mark their
                 * own edits with `mark_edited`. */
                state.machines = machines.to_vec();
                if let Some(path) = std::env::var_os("LAN_MOUSE_CANVAS_TRACE") {
                    use std::io::Write;
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        let _ = writeln!(
                            file,
                            "update: local_rev={} peer_pictures={}",
                            state.local_revision,
                            state
                                .machines
                                .iter()
                                .filter(|m| m.peer_picture.is_some())
                                .count(),
                        );
                    }
                }
                match state.newer_peer_picture() {
                    Some(picture) => {
                        log::info!(
                            "the peer's arrangement wins (revision {} vs local {}): adopting its {} machine(s)",
                            picture.revision,
                            state.local_revision,
                            picture.machines.len()
                        );
                        state.adopt(&picture);
                    }
                    None => {
                        let received = state
                            .machines
                            .iter()
                            .filter(|machine| machine.peer_picture.is_some())
                            .count();
                        if received > 0 {
                            log::info!(
                                "keeping the local arrangement (revision {}); the peer's {} picture(s) are not newer",
                                state.local_revision,
                                received
                            );
                        }
                    }
                }
            }
        }
        self.hint
            .set_text(&describe(&self.state.borrow().switch_options));
        let names = {
            let state = self.state.borrow();
            state
                .unreachable()
                .into_iter()
                .filter_map(|handle| state.machine(handle).map(|m| m.hostname.clone()))
                .collect::<Vec<_>>()
        };
        self.warning.set_visible(!names.is_empty());
        if !names.is_empty() {
            self.warning_label.set_text(&format!(
                "{} do not get the whole stretch of the edge they were put on. Drag the handles to give each one a stretch of its own, or use Auto arrange.",
                paint::list_names(&names)
            ));
        }
        self.area.queue_draw();
    }
}

/// The row of buttons above the canvas.
///
/// The area is passed in rather than reached through the state: the closures that
/// change the view are held by the buttons, and the widget they redraw owns the
/// state they mutate.
fn toolbar(state: &Rc<RefCell<State>>, area: &gtk::DrawingArea) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_halign(gtk::Align::End);

    let add = |label: &str, tooltip: &str, action: fn(&Rc<RefCell<State>>, &gtk::DrawingArea)| {
        let button = gtk::Button::builder()
            .label(label)
            .tooltip_text(tooltip)
            .css_classes(["flat"])
            .build();
        let state = Rc::clone(state);
        let area = area.clone();
        button.connect_clicked(move |_| action(&state, &area));
        row.append(&button);
    };

    add(
        "Auto arrange",
        "Give every machine on an edge an equal, adjacent stretch of it",
        |state, area| {
            auto_arrange(state);
            area.queue_draw();
        },
    );
    add("−", "Zoom out", |state, area| {
        let mut state = state.borrow_mut();
        state.zoom = (state.zoom / 1.25).max(0.2);
        area.queue_draw();
    });
    add("+", "Zoom in", |state, area| {
        let mut state = state.borrow_mut();
        state.zoom = (state.zoom * 1.25).min(4.0);
        area.queue_draw();
    });
    add(
        "Fit",
        "Scale and centre the whole layout again",
        |state, area| {
            state.borrow_mut().reset_view();
            area.queue_draw();
        },
    );
    add("Grid", "Show or hide the background grid", |state, area| {
        let mut state = state.borrow_mut();
        state.show_grid = !state.show_grid;
        area.queue_draw();
    });
    row
}

/// Hand every edge's peers an equal share of it.
fn auto_arrange(state: &Rc<RefCell<State>>) {
    let requests = {
        let state = state.borrow();
        let mut requests = Vec::new();
        /* Grouped by the monitor as well as the edge: two peers above two
         * different screens are not competing for one stretch, so splitting the
         * edge between them would give each half of a screen it is not on. */
        let mut groups: Vec<(Position, Option<String>)> = Vec::new();
        for machine in state.machines.iter().filter(|machine| machine.active) {
            let key = (machine.position, machine.display.clone());
            if !groups.contains(&key) {
                groups.push(key);
            }
        }
        for (position, display) in groups {
            let on_edge = state
                .machines
                .iter()
                .filter(|machine| {
                    machine.active && machine.position == position && machine.display == display
                })
                .collect::<Vec<_>>();
            if on_edge.len() < 2 {
                continue;
            }
            let shares = layout::auto_arrange(on_edge.len());
            for (machine, (edge, peer_edge)) in on_edge.into_iter().zip(shares) {
                if machine.edge != edge || machine.peer_edge != peer_edge {
                    requests.push((machine.handle, position, display.clone(), edge, peer_edge));
                }
            }
        }
        requests
    };
    if !requests.is_empty() {
        (state.borrow().on_request)(MachineRequest::AutoArrange(requests));
    }
}

/// The label under the canvas.
fn hint(state: &State) -> gtk::Label {
    let label = gtk::Label::builder()
        .halign(gtk::Align::Center)
        .wrap(true)
        .build();
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label.set_text(&describe(&state.switch_options));
    label
}

/// How to use the canvas, plus the thresholds that change what a crossing means.
///
/// The thresholds belong here because they change what the picture promises: an
/// edge that needs a second hit, or whose corners are blocked, is not reachable the
/// way the bridges alone suggest.
fn describe(options: &SwitchOptions) -> String {
    let mut parts = vec![
        "Drag a screen so the edges that should connect touch".to_owned(),
        "drag the handles on this screen's edge to set how far a machine reaches along it"
            .to_owned(),
    ];
    if options.double_tap_ms > 0 {
        parts.push(format!(
            "an edge has to be hit twice within {} ms",
            options.double_tap_ms
        ));
    }
    if options.corner_size > 0 && !options.corners.is_empty() {
        parts.push(format!(
            "{} px of the corners ({}) are blocked",
            options.corner_size,
            options
                .corners
                .iter()
                .map(|corner| corner.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    parts.join("; ")
}

/// Wire the pointer gestures.
///
/// The order is the substance: the handle controller runs in the capture phase and
/// claims a press that lands on a handle, so the same press never starts a card
/// drag. Everything else falls through to the drag; the middle button pans.
fn add_gestures(area: &gtk::DrawingArea, state: &Rc<RefCell<State>>) {
    /* hover: the cursor changes over a card, and the card lights up */
    let motion = gtk::EventControllerMotion::new();
    {
        let state = Rc::clone(state);
        let area = area.clone();
        motion.connect_motion(move |_motion, x, y| {
            let mut state = state.borrow_mut();
            let hover = state.hit(x, y);
            if state.hover != hover {
                state.hover = hover;
                let cursor = if hover.is_some() { "grab" } else { "default" };
                area.set_cursor_from_name(Some(cursor));
                area.queue_draw();
            }
        });
    }
    area.add_controller(motion);

    /* range handles: capture phase, so a press on one never drags a card */

    /* drag a card */
    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    {
        let state = Rc::clone(state);
        let area = area.clone();
        drag.connect_drag_begin(move |_gesture, x, y| {
            let mut state = state.borrow_mut();

            /* A press on this machine's own screens moves them, which is how the
             * user changes where this machine sits relative to the peers. The
             * monitors are drawn from an offset so the peers stay where they are;
             * on release the links are re-derived from the new geometry. */
            let (lx, ly) = state.to_layout(x, y);
            if state
                .drawn_monitors()
                .iter()
                .any(|monitor| monitor.rect.contains(lx, ly))
            {
                state.moving_local = Some((x, y));
                state.local_offset = (0.0, 0.0);
                area.set_cursor_from_name(Some("grabbing"));
                area.queue_draw();
                return;
            }
            let Some(handle) = state.hit(x, y) else {
                return;
            };
            let Some(rect) = state.rect_of(handle) else {
                return;
            };
            let Some(machine) = state.machine(handle).cloned() else {
                return;
            };
            state.drag = Some(Drag {
                handle,
                original: rect,
                current: rect,
                placement: Placement {
                    position: machine.position,
                    display: machine.display.clone(),
                    edge: machine.edge,
                    peer_edge: machine.peer_edge,
                    size: (rect.w, rect.h),
                },
            });
            area.set_cursor_from_name(Some("grabbing"));
            area.queue_draw();
        });
    }
    {
        let state = Rc::clone(state);
        let area = area.clone();
        drag.connect_drag_update(move |_gesture, dx, dy| {
            let mut state = state.borrow_mut();
            if state.moving_local.is_some() {
                let scale = state.geometry.scale.max(f64::EPSILON);
                state.local_offset = (dx / scale, dy / scale);
                area.queue_draw();
                return;
            }
            if let Some(from) = state.panning {
                /* `dx` is the offset from where the drag began, not the step since
                 * the last update: adding it to the current pan accumulates
                 * quadratically, which is why the picture ran away from the
                 * pointer. It is applied to the pan the drag started from. */
                state.pan = (from.0 + dx, from.1 + dy);
                area.queue_draw();
                return;
            }
            let scale = state.geometry.scale.max(f64::EPSILON);
            let local = state.geometry.local;
            /* read the card's start position before writing to it: the drag lives
             * inside the state the geometry was taken from */
            let Some(original) = state.drag.as_ref().map(|drag| drag.original) else {
                return;
            };
            let current = Screen::new(
                original.x + dx / scale,
                original.y + dy / scale,
                original.w,
                original.h,
            );
            /* The monitors as *drawn*, so a machine the user has dragged out of the way
             * is judged where it now is rather than where the compositor has it. */
            let placement = layout::snap(local, &state.drawn_monitors(), current);
            /* Show where the drop would land while the card is still moving: the link
             * preview and the card's own position come from the same `placement`, so a
             * user can see the answer before letting go instead of discovering it after
             * a drop that needed repeating. */
            if let Some(drag) = state.drag.as_mut() {
                drag.current = current;
                drag.placement = placement;
            }
            area.queue_draw();
        });
    }
    {
        let state = Rc::clone(state);
        let area = area.clone();
        drag.connect_drag_end(move |_gesture, _dx, _dy| {
            let mut state = state.borrow_mut();
            if state.moving_local.take().is_some() {
                area.set_cursor_from_name(Some("default"));
                let moved = state.local_offset;
                let requests = state.reanchor_after_moving_local();
                if moved != (0.0, 0.0) {
                    /* The drag *is* the edit: this machine's screens are now where the
                     * user put them, so the picture has to say so and win the comparison.
                     * Without this the offset was reset to zero below and the whole drag
                     * left no trace anywhere. */
                    state.mark_edited();
                    state.local_offset = moved;
                }
                for (handle, position, display, edge, peer_edge) in requests {
                    (state.on_request)(MachineRequest::Move {
                        handle,
                        position,
                        display,
                        edge,
                        peer_edge,
                    });
                }
                area.queue_draw();
                return;
            }
            if state.panning.take().is_some() {
                area.set_cursor_from_name(Some("default"));
                area.queue_draw();
                return;
            }
            /* move the card right away: the daemon answers asynchronously and a card
             * that jumped back while it thought looked broken */
            let placement = state
                .drag
                .as_ref()
                .map(|drag| (drag.handle, drag.placement.clone()));
            if let Some((handle, placement)) = placement {
                if let Some(machine) = state.machines.iter_mut().find(|m| m.handle == handle) {
                    machine.position = placement.position;
                    machine.edge = placement.edge;
                    machine.peer_edge = placement.peer_edge;
                }
            }
            state.commit();
            area.set_cursor_from_name(Some("default"));
            area.queue_draw();
        });
    }
    area.add_controller(drag);

    /* pan: middle button, so a left drag on a card still reaches the drag above */
    let pan = gtk::GestureDrag::new();
    pan.set_button(2);
    {
        let state = Rc::clone(state);
        let area = area.clone();
        pan.connect_drag_update(move |_gesture, dx, dy| {
            let mut state = state.borrow_mut();
            state.pan = (state.pan.0 + dx, state.pan.1 + dy);
            area.queue_draw();
        });
    }
    area.add_controller(pan);

    /* ctrl + wheel zooms, plain wheel pans */
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let state = Rc::clone(state);
        let area = area.clone();
        scroll.connect_scroll(move |controller, dx, dy| {
            let ctrl = controller.current_event().is_some_and(|event| {
                event
                    .modifier_state()
                    .contains(gtk::gdk::ModifierType::CONTROL_MASK)
            });
            let mut state = state.borrow_mut();
            if ctrl {
                let factor = if dy > 0.0 { 1.1 } else { 1.0 / 1.1 };
                state.zoom = (state.zoom * factor).clamp(0.2, 4.0);
            } else {
                state.pan = (state.pan.0 - dx * 40.0, state.pan.1 - dy * 40.0);
            }
            area.queue_draw();
            glib::Propagation::Stop
        });
    }
    area.add_controller(scroll);

    /* keyboard: the arrows move the card the pointer is over */
    let keys = gtk::EventControllerKey::new();
    {
        let state = Rc::clone(state);
        let area = area.clone();
        keys.connect_key_pressed(move |_controller, key, _code, _modifiers| {
            let mut state = state.borrow_mut();
            let moved = match key {
                gtk::gdk::Key::plus | gtk::gdk::Key::KP_Add => {
                    state.zoom = (state.zoom * 1.25).min(4.0);
                    true
                }
                gtk::gdk::Key::minus | gtk::gdk::Key::KP_Subtract => {
                    state.zoom = (state.zoom / 1.25).max(0.2);
                    true
                }
                gtk::gdk::Key::_0 | gtk::gdk::Key::KP_0 => {
                    state.reset_view();
                    true
                }
                gtk::gdk::Key::Left
                | gtk::gdk::Key::Right
                | gtk::gdk::Key::Up
                | gtk::gdk::Key::Down => {
                    let position = match key {
                        gtk::gdk::Key::Left => Position::Left,
                        gtk::gdk::Key::Right => Position::Right,
                        gtk::gdk::Key::Up => Position::Top,
                        _ => Position::Bottom,
                    };
                    /* the keyboard moves a card by whole edges: the ranges come from
                     * where the card is, so a card nudged to a new side keeps the
                     * stretch it had */
                    if let Some(handle) = state.hover {
                        if let Some(machine) = state.machine(handle) {
                            let (edge, peer_edge) = (machine.edge, machine.peer_edge);
                            let display = state.machine(handle).and_then(|m| m.display.clone());
                            state.mark_edited();
                            (state.on_request)(MachineRequest::Move {
                                handle,
                                position,
                                display,
                                edge,
                                peer_edge,
                            });
                        }
                    }
                    true
                }
                _ => false,
            };
            if moved {
                area.queue_draw();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    area.add_controller(keys);
}

#[cfg(test)]
mod tests {
    use super::draw::{blocked_ends, reachable_stretches};
    use super::*;

    fn machine(handle: ClientHandle, position: Position, edge: (f64, f64)) -> Machine {
        Machine {
            handle,
            peer_rect: None,
            peer_picture: None,
            peer_monitors: Vec::new(),
            display: None,
            hostname: format!("host{handle}"),
            position,
            active: true,
            edge: EdgeRange::from_fractions(edge.0, edge.1).expect("range"),
            peer_edge: EdgeRange::FULL,
            peer_size: None,
        }
    }

    fn state(machines: Vec<Machine>) -> State {
        State {
            local_hostname: "local".to_owned(),
            local_displays: vec![input_event::DisplayInfo::new(
                "test",
                0,
                0,
                1920,
                1080,
                input_event::Rotation::Normal,
            )],
            machines,
            switch_options: SwitchOptions::default(),
            on_request: Rc::new(|_| {}),
            geometry: Geometry::default(),
            drag: None,
            hover: None,
            panning: None,
            moving_local: None,
            local_offset: (0.0, 0.0),
            local_revision: 0,
            published: String::new(),
            zoom: 1.0,
            pan: (0.0, 0.0),
            canvas: (800.0, 460.0),
            show_grid: true,
        }
    }

    #[test]
    fn two_peers_on_one_edge_are_drawn_side_by_side() {
        let mut state = state(vec![
            machine(0, Position::Right, (0.0, 0.5)),
            machine(1, Position::Right, (0.5, 1.0)),
        ]);
        state.relayout(800.0, 460.0);
        assert_eq!(state.geometry.peers.len(), 2);
        let (first, second) = (
            state.rect_of(0).expect("first"),
            state.rect_of(1).expect("second"),
        );
        // stacked along the edge, not on top of each other
        assert!(first.bottom() <= second.y + 1.0, "{first:?} {second:?}");
    }

    #[test]
    fn a_disabled_peer_is_not_placed_or_hit() {
        let mut peer = machine(0, Position::Right, (0.0, 1.0));
        peer.active = false;
        let mut state = state(vec![peer]);
        state.relayout(800.0, 460.0);
        assert!(state.geometry.peers.is_empty());
        assert_eq!(state.hit(700.0, 200.0), None);
    }

    #[test]
    fn a_hit_is_reported_in_canvas_coordinates() {
        let mut state = state(vec![machine(0, Position::Right, (0.0, 1.0))]);
        state.relayout(800.0, 460.0);
        let rect = state.rect_of(0).expect("rect");
        let centre = state.to_canvas(rect).center();
        assert_eq!(state.hit(centre.0, centre.1), Some(0));
        assert_eq!(state.hit(4.0, 4.0), None);
    }

    #[test]
    fn overlapping_claims_are_reported_and_do_not_include_the_lucky_one() {
        let state = state(vec![
            machine(0, Position::Right, (0.0, 1.0)),
            machine(1, Position::Right, (0.0, 0.5)),
        ]);
        let squeezed = state.unreachable();
        assert_eq!(squeezed, vec![1]);
    }

    #[test]
    fn disjoint_claims_are_not_reported() {
        let state = state(vec![
            machine(0, Position::Right, (0.0, 0.5)),
            machine(1, Position::Right, (0.5, 1.0)),
        ]);
        assert!(state.unreachable().is_empty());
    }

    #[test]
    fn claims_on_different_edges_never_conflict() {
        let state = state(vec![
            machine(0, Position::Right, (0.0, 1.0)),
            machine(1, Position::Left, (0.0, 1.0)),
        ]);
        assert!(state.unreachable().is_empty());
    }

    #[test]
    fn the_layout_fits_and_reacts_to_zoom() {
        let mut state = state(vec![machine(0, Position::Right, (0.0, 1.0))]);
        state.relayout(800.0, 460.0);
        let base = state.geometry.scale;
        state.zoom = 2.0;
        state.relayout(800.0, 460.0);
        assert!(state.geometry.scale > base, "zoom did not take effect");
        state.reset_view();
        state.relayout(800.0, 460.0);
        assert!((state.geometry.scale - base).abs() < 1e-9);
    }

    #[test]
    fn a_local_screen_of_unknown_size_still_lays_out() {
        let mut state = state(vec![machine(0, Position::Right, (0.0, 1.0))]);
        state.local_displays.clear();
        state.relayout(800.0, 460.0);
        assert!(state.geometry.scale > 0.0);
        assert_eq!(state.geometry.peers.len(), 1);
    }

    #[test]
    fn the_hint_lists_the_thresholds_that_change_a_crossing() {
        let plain = describe(&SwitchOptions::default());
        assert!(!plain.contains("ms"), "{plain}");
        let options = SwitchOptions {
            double_tap_ms: 250,
            corner_size: 40,
            corners: vec![lan_mouse_ipc::SwitchCorner::TopLeft],
        };
        let text = describe(&options);
        assert!(text.contains("250 ms"), "{text}");
        assert!(text.contains("top-left"), "{text}");
    }

    #[test]
    fn a_blocked_corner_eats_the_end_of_the_edge_it_touches() {
        /* 40 px of a 1080 px edge is 37.04 permille, rounded up to 38: the daemon
         * blocks the whole 40 px, so the canvas may not claim a sliver of it. */
        let corners = [lan_mouse_ipc::SwitchCorner::TopRight];
        let (low, high) = blocked_ends(Position::Right, &corners, 1080.0, 40);
        assert_eq!(low, 38);
        assert_eq!(high, 0);
        // the same corner is the right end of the top edge, over a wider screen
        let (low, high) = blocked_ends(Position::Top, &corners, 1920.0, 40);
        assert_eq!(low, 0);
        assert_eq!(high, 21);
    }

    #[test]
    fn corners_do_not_block_an_edge_they_are_not_on() {
        let corners = [lan_mouse_ipc::SwitchCorner::TopLeft];
        assert_eq!(blocked_ends(Position::Bottom, &corners, 1920.0, 40), (0, 0));
        assert_eq!(blocked_ends(Position::Right, &corners, 1080.0, 40), (0, 0));
    }

    #[test]
    fn a_blocked_corner_takes_its_share_of_a_peer_claim() {
        let mut state = state(vec![machine(0, Position::Right, (0.0, 1.0))]);
        state.switch_options = SwitchOptions {
            double_tap_ms: 0,
            corner_size: 108, // 10% of a 1080 px edge
            corners: vec![lan_mouse_ipc::SwitchCorner::TopRight],
        };
        state.relayout(800.0, 460.0);
        let reachable = reachable_stretches(&state, state.geometry.local);
        assert_eq!(reachable.len(), 1);
        assert_eq!(reachable[0].1.fractions(), [0.1, 1.0]);
    }

    #[test]
    fn a_peer_whose_whole_claim_is_a_blocked_corner_reaches_nobody() {
        let mut state = state(vec![machine(0, Position::Right, (0.0, 0.05))]);
        state.switch_options = SwitchOptions {
            double_tap_ms: 0,
            corner_size: 108,
            corners: vec![lan_mouse_ipc::SwitchCorner::TopRight],
        };
        state.relayout(800.0, 460.0);
        assert!(reachable_stretches(&state, state.geometry.local).is_empty());
    }

    #[test]
    fn no_corners_means_the_whole_claim_is_reachable() {
        let state = state(vec![machine(0, Position::Right, (0.2, 0.8))]);
        let reachable = reachable_stretches(&state, state.geometry.local);
        assert_eq!(reachable.len(), 1);
        assert_eq!(
            reachable[0].1,
            EdgeRange::from_fractions(0.2, 0.8).expect("range")
        );
    }

    #[test]
    fn a_corner_larger_than_the_edge_blocks_all_of_it() {
        let (low, high) = blocked_ends(
            Position::Right,
            &[lan_mouse_ipc::SwitchCorner::TopRight],
            100.0,
            1000,
        );
        assert_eq!((low, high), (1000, 0));
    }
}

/// Render the canvas off screen.
///
/// The drawing code takes a cairo context and a palette, so the whole picture can
/// be produced without a compositor - which is how it gets tested, and how a
/// screenshot can be looked at in a headless session.
#[cfg(test)]
mod render {
    use super::*;

    fn machine(
        handle: ClientHandle,
        position: Position,
        edge: (f64, f64),
        active: bool,
    ) -> Machine {
        Machine {
            handle,
            peer_rect: None,
            peer_picture: None,
            peer_monitors: Vec::new(),
            display: None,
            hostname: format!("host{handle}"),
            position,
            active,
            edge: EdgeRange::from_fractions(edge.0, edge.1).expect("range"),
            peer_edge: EdgeRange::FULL,
            peer_size: None,
        }
    }

    fn state(machines: Vec<Machine>) -> State {
        State {
            local_hostname: "workstation".to_owned(),
            local_displays: vec![input_event::DisplayInfo::new(
                "test",
                0,
                0,
                1920,
                1080,
                input_event::Rotation::Normal,
            )],
            machines,
            switch_options: SwitchOptions::default(),
            on_request: Rc::new(|_| {}),
            geometry: Geometry::default(),
            drag: None,
            hover: None,
            panning: None,
            moving_local: None,
            local_offset: (0.0, 0.0),
            local_revision: 0,
            published: String::new(),
            zoom: 1.0,
            pan: (0.0, 0.0),
            canvas: (1.0, 1.0),
            show_grid: true,
        }
    }

    /// Paint `state` onto an ARGB surface and return it.
    ///
    /// The light palette is fixed rather than read from the theme: a test has to
    /// compare against colours it knows. The surface is primed with the window
    /// background because everything the canvas draws is alpha blended - on a
    /// transparent surface the pixels would come out premultiplied and no colour
    /// would match what was asked for.
    fn render(state: &mut State, width: i32, height: i32) -> cairo::ImageSurface {
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).expect("surface");
        let cr = cairo::Context::new(&surface).expect("context");
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let _ = cr.paint();
        state.relayout(width as f64, height as f64);
        draw::draw(
            &cr,
            width as f64,
            height as f64,
            state,
            &Palette::resolve(false),
        );
        /* a surface cannot be read while a context still draws into it: drop the
         * context, flush, then hand the pixels out */
        drop(cr);
        surface.flush();
        surface
    }

    /// The colour of one pixel, as (red, green, blue) in 0..=255.
    fn pixel(surface: &mut cairo::ImageSurface, x: i32, y: i32) -> (u8, u8, u8) {
        let stride = surface.stride() as usize;
        let data = surface.data().expect("surface data");
        let offset = y as usize * stride + x as usize * 4;
        // ARGB32 is native-endian premultiplied: on little-endian that is B, G, R, A
        (data[offset + 2], data[offset + 1], data[offset])
    }

    /// How many pixels are closer to `color` at `alpha` over the background than
    /// to any of `others` at their own alpha.
    ///
    /// The canvas paints with alpha, so what lands in the surface is the blend, not
    /// the palette entry; comparing against the raw colour would find nothing and
    /// the test would only prove that the palette is opaque.
    fn count_closest(
        surface: &mut cairo::ImageSurface,
        color: (f64, f64, f64),
        alpha: f64,
        others: &[((f64, f64, f64), f64)],
    ) -> usize {
        let target = blend(color, alpha);
        let others = others
            .iter()
            .map(|(c, a)| blend(*c, *a))
            .collect::<Vec<_>>();
        let mut hits = 0;
        for y in 0..surface.height() {
            for x in 0..surface.width() {
                let (r, g, b) = pixel(surface, x, y);
                let here = (r as i32, g as i32, b as i32);
                let distance = |c: &(i32, i32, i32)| {
                    (c.0 - here.0).abs() + (c.1 - here.1).abs() + (c.2 - here.2).abs()
                };
                let best = distance(&target);
                if best < 40 && others.iter().all(|c| distance(c) > best) {
                    hits += 1;
                }
            }
        }
        hits
    }

    /// The colour that `color` at `alpha` becomes over the white background.
    fn blend(color: (f64, f64, f64), alpha: f64) -> (i32, i32, i32) {
        let channel = |value: f64| (value * alpha + 1.0 * (1.0 - alpha)) * 255.0;
        (
            channel(color.0) as i32,
            channel(color.1) as i32,
            channel(color.2) as i32,
        )
    }

    #[test]
    fn the_local_screen_is_drawn_bigger_than_a_laptop_peer() {
        /* The point of drawing to scale: a 1366x768 laptop next to a 1920x1080
         * screen has to look smaller. A canvas that drew equal cards would make
         * an edge range meaningless. */
        let mut small = machine(0, Position::Right, (0.0, 1.0), true);
        small.peer_size = Some((1366.0, 768.0));
        let mut state = state(vec![small]);
        let mut surface = render(&mut state, 900, 500);

        let local = state.to_canvas(state.geometry.local);
        let peer = state.to_canvas(state.rect_of(0).expect("peer"));
        assert!(
            local.h > peer.h,
            "the local screen should tower over a laptop: {local:?} vs {peer:?}"
        );
        // and the cards are actually painted: green outline pixels exist
        let palette = Palette::resolve(false);
        let green = count_closest(
            &mut surface,
            palette.success,
            0.6,
            &[(palette.accent, 0.9), (palette.danger, 0.95)],
        );
        assert!(green > 50, "no peer card was drawn ({green} green pixels)");
    }

    #[test]
    fn two_peers_on_one_edge_are_drawn_one_above_the_other() {
        let mut state = state(vec![
            machine(0, Position::Right, (0.0, 0.5), true),
            machine(1, Position::Right, (0.5, 1.0), true),
        ]);
        let mut surface = render(&mut state, 900, 500);
        let first = state.rect_of(0).expect("first");
        let second = state.rect_of(1).expect("second");
        let (first, second) = (state.to_canvas(first), state.to_canvas(second));
        // both are inside the canvas and stacked, not side by side
        assert!(first.bottom() <= second.y + 1.0);
        assert!(
            second.bottom() <= 500.0,
            "the layout overflowed: {second:?}"
        );
        // the local screen's blue border is drawn
        let palette = Palette::resolve(false);
        let blue = count_closest(
            &mut surface,
            palette.accent,
            0.9,
            &[(palette.success, 0.6), (palette.danger, 0.95)],
        );
        assert!(blue > 100, "the local screen is missing ({blue} pixels)");
    }

    #[test]
    fn a_disabled_peer_is_not_painted() {
        let mut state = state(vec![machine(0, Position::Right, (0.0, 1.0), false)]);
        let mut surface = render(&mut state, 900, 500);
        let palette = Palette::resolve(false);
        let green = count_closest(
            &mut surface,
            palette.success,
            0.6,
            &[(palette.accent, 0.9), (palette.danger, 0.95)],
        );
        assert_eq!(green, 0, "a disabled peer was drawn as reachable");
    }

    #[test]
    fn an_unreachable_claim_is_marked_on_the_card() {
        let mut state = state(vec![
            machine(0, Position::Right, (0.0, 1.0), true),
            machine(1, Position::Right, (0.0, 0.5), true),
        ]);
        let mut surface = render(&mut state, 900, 500);
        let palette = Palette::resolve(false);
        let red = count_closest(
            &mut surface,
            palette.danger,
            0.95,
            &[(palette.success, 0.6), (palette.accent, 0.9)],
        );
        assert!(red > 20, "the squeezed peer was not flagged ({red} pixels)");
    }

    /// A desk with a rotated monitor: the bounding box covers area no screen does,
    /// and the canvas has to draw the monitors, not the box.
    ///
    /// This is the layout that made the bug visible: `4000x2560` is the bounding
    /// box of a 2560x1440 panel turned 270 degrees at (2560,0) and an upright
    /// 2560x1440 at (0,1120). The box contains a 1120 px tall band at the top left
    /// where there is no screen at all, and a crossing there must not be drawn as
    /// reachable.
    #[test]
    fn a_rotated_desk_is_drawn_as_two_monitors_not_one_box() {
        let mut state = state(Vec::new());
        state.local_displays = vec![
            input_event::DisplayInfo {
                name: "DP-2".to_owned(),
                x: 2560,
                y: 0,
                width: 2560,
                height: 1440,
                rotation: input_event::Rotation::Right,
                enabled: true,
            },
            input_event::DisplayInfo {
                name: "DP-3".to_owned(),
                x: 0,
                y: 1120,
                width: 2560,
                height: 1440,
                rotation: input_event::Rotation::Normal,
                enabled: true,
            },
        ];
        state.relayout(1000.0, 620.0);

        let monitors = state.local_monitors();
        assert_eq!(monitors.len(), 2, "both monitors are drawn");

        /* the rotated one is taller than it is wide on screen */
        let rotated = monitors
            .iter()
            .map(|monitor| monitor.rect)
            .find(|rect| (rect.w - 1440.0).abs() < 1.0)
            .expect("the rotated monitor");
        assert!(rotated.h > rotated.w, "{rotated:?}");

        /* and the two do not cover the whole bounding box: the band at the top
         * left is empty */
        let bounds = state.local_screen();
        assert_eq!((bounds.w, bounds.h), (4000.0, 2560.0));
        let covered: f64 = monitors
            .iter()
            .map(|monitor| monitor.rect.w * monitor.rect.h)
            .sum();
        assert!(
            covered < bounds.w * bounds.h,
            "the monitors cover the whole bounding box, so they were drawn as one"
        );

        /* and the empty corner really is empty on the surface */
        let mut surface = render(&mut state, 1000, 620);
        let palette = Palette::resolve(false);
        let background = count_closest(
            &mut surface,
            palette.foreground,
            0.04,
            &[(palette.accent, 0.9), (palette.card, 1.0)],
        );
        assert!(background > 1000, "{background} background pixels");
    }

    /// A peer's two monitors are decoded and normalised to the group's corner.
    ///
    /// The report is the per-monitor form, so a peer of the same shape as this desk
    /// has to come back as two rectangles with their offsets, not as one box.
    /// Normalised, because the group is drawn at wherever the link put it and the
    /// monitors are then placed inside that - an unnormalised report would draw the
    /// peer's second monitor outside its own card.
    #[test]
    fn a_peers_monitors_are_decoded_and_normalised() {
        /* The wire form carries the rectangle *after* rotation and records the
         * rotation as normal: the backends that report a region (portal, layer-shell,
         * CoreGraphics) hand back a rect, not a panel size plus a transform. */
        let monitors =
            peer_monitors("DP-2:2560,0:1440x2560:normal:1;DP-3:0,1120:2560x1440:normal:1");
        assert_eq!(monitors.len(), 2);
        let dp2 = monitors.iter().find(|m| m.name == "DP-2").expect("DP-2");
        let dp3 = monitors.iter().find(|m| m.name == "DP-3").expect("DP-3");
        /* normalised: the group's own corner is (0,0), so the rotated panel keeps its
         * offset to the right of the upright one and the upright one keeps its
         * offset below the top */
        assert_eq!(dp2.rect, Screen::new(2560.0, 0.0, 1440.0, 2560.0));
        assert_eq!(dp3.rect, Screen::new(0.0, 1120.0, 2560.0, 1440.0));

        /* a report that says nothing is one box, which is all that is known */
        assert!(peer_monitors("").is_empty());
        assert!(peer_monitors("2560x1440").is_empty());
    }

    /// Two machines with mirrored configurations end up drawing the same desk.
    ///
    /// This is the whole point of the shared picture. Side A is this desk: two
    /// monitors, with the peer above the lower one. Side B is a laptop below it,
    /// which on its own would draw A as one box under itself. After A publishes and
    /// B adopts, B draws A's two monitors in the same places A does.
    #[test]
    fn both_machines_draw_the_same_desk_after_adopting_the_picture() {
        let mut a = state(vec![]);
        a.local_hostname = "desk".to_owned();
        a.local_displays = vec![
            input_event::DisplayInfo {
                name: "DP-2".to_owned(),
                x: 2560,
                y: 0,
                width: 1440,
                height: 2560,
                rotation: input_event::Rotation::Normal,
                enabled: true,
            },
            input_event::DisplayInfo {
                name: "DP-3".to_owned(),
                x: 0,
                y: 1120,
                width: 2560,
                height: 1440,
                rotation: input_event::Rotation::Normal,
                enabled: true,
            },
        ];
        let mut laptop = machine(4, Position::Top, (0.0, 1.0), true);
        laptop.hostname = "laptop".to_owned();
        laptop.display = Some("DP-3".to_owned());
        laptop.peer_size = Some((1920.0, 1080.0));
        a.machines = vec![laptop];
        /* the desk edited more recently than the laptop */
        a.local_revision = 100;
        a.relayout(1000.0, 620.0);

        let published = a.local_picture();
        assert_eq!(published.author, "desk");
        assert_eq!(published.authored_screens().len(), 2);
        assert_eq!(published.peer_screens().len(), 1);
        assert_eq!(published.peer_screens()[0].1.len(), 1);

        /* the laptop's own view: one screen, the desk below it, no monitor names */
        let mut b = state(vec![]);
        b.local_hostname = "laptop".to_owned();
        b.local_displays = vec![input_event::DisplayInfo {
            name: "eDP-1".to_owned(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            rotation: input_event::Rotation::Normal,
            enabled: true,
        }];
        let mut desk = machine(4, Position::Bottom, (0.0, 1.0), true);
        desk.hostname = "desk".to_owned();
        desk.peer_size = Some((4000.0, 2560.0));
        b.machines = vec![desk];
        b.local_revision = 50;
        b.relayout(1000.0, 620.0);
        /* before adopting, it draws the desk as one 4000x2560 box */
        assert_eq!(b.screens_of(4).len(), 1);

        b.machines[0].peer_picture = Some(published.clone());
        let winner = b.newer_peer_picture().expect("the peer is newer");
        b.adopt(&winner);
        b.relayout(1000.0, 620.0);

        /* now it draws the desk's two monitors, in the same relative places */
        let theirs = b.screens_of(4);
        let mine = a
            .local_monitors()
            .iter()
            .map(|monitor| monitor.rect)
            .collect::<Vec<_>>();
        assert_eq!(theirs.len(), mine.len(), "{theirs:?} vs {mine:?}");
        let sorted = |screens: &[Screen]| {
            let mut out = screens.to_vec();
            out.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
            out
        };
        let (theirs, mine) = (sorted(&theirs), sorted(&mine));
        /* the two windows frame the desk from different corners, so what has to
         * match is the arrangement, not the absolute origin */
        let (ax, ay) = (theirs[0].x, theirs[0].y);
        let (bx, by) = (mine[0].x, mine[0].y);
        for (theirs, mine) in theirs.iter().zip(&mine) {
            assert_eq!(
                (theirs.w, theirs.h),
                (mine.w, mine.h),
                "{theirs:?} vs {mine:?}"
            );
            assert!(
                ((theirs.x - ax) - (mine.x - bx)).abs() < 2.0
                    && ((theirs.y - ay) - (mine.y - by)).abs() < 2.0,
                "the two machines disagree: {theirs:?} vs {mine:?}"
            );
        }
    }

    /// Only a gesture makes this side's picture newer.
    ///
    /// The daemon rewrites its config, a peer's action edits a link, and the canvas
    /// re-derives its geometry on every resize - all of those looked like edits when
    /// they were detected from the configuration, and each stamped a fresh revision.
    /// The local picture then always won, so the peer's was never adopted: the reported
    /// "拖了也一动不动" with the data arriving correctly the whole time.
    #[test]
    fn an_untouched_canvas_keeps_a_revision_of_zero() {
        let mut state = state(vec![]);
        state.local_displays = vec![input_event::DisplayInfo {
            name: "eDP-1".to_owned(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            rotation: input_event::Rotation::Normal,
            enabled: true,
        }];
        let mut desk = machine(4, Position::Bottom, (0.0, 1.0), true);
        desk.hostname = "desk".to_owned();
        state.machines = vec![desk];

        /* updates, resizes and re-derivations must not touch the revision */
        state.relayout(800.0, 500.0);
        state.relayout(1200.0, 700.0);
        assert_eq!(state.local_revision, 0);

        /* a gesture does */
        state.mark_edited();
        assert!(state.local_revision > 0);
    }

    /// Dragging this machine changes the arrangement, so a drag can put a peer
    /// anywhere - including above a screen it was not above before.
    ///
    /// Reported on macOS as "屏幕无法放到下方横屏的正上方". The drop was judged against
    /// the compositor positions, which a drag does not change, so every drag computed
    /// the arrangement that was already configured and the result was always a no-op.
    #[test]
    fn dragging_this_machine_moves_the_peers_relative_to_it() {
        let mut state = state(vec![]);
        /* one screen at the origin */
        state.local_displays = vec![input_event::DisplayInfo {
            name: "eDP-1".to_owned(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            rotation: input_event::Rotation::Normal,
            enabled: true,
        }];
        let mut peer = machine(3, Position::Bottom, (0.0, 1.0), true);
        peer.hostname = "desk".to_owned();
        peer.peer_size = Some((2560.0, 1440.0));
        state.machines = vec![peer];
        state.relayout(900.0, 600.0);

        /* no drag at all: the geometry already implies the configured link, so nothing
         * needs rewriting (whatever the placement is, it must not churn) */
        let before = state.reanchor_after_moving_local();
        for (handle, position, display, edge, peer_edge) in &before {
            let machine = state.machine(*handle).expect("the peer");
            assert!(
                !(machine.position == *position
                    && machine.display == *display
                    && machine.edge == *edge
                    && machine.peer_edge == *peer_edge),
                "a no-op drag must not produce a request"
            );
        }

        /* drag this machine *up* by more than the peer's height: now the peer is below
         * the machine, and dropping it there must rewrite the link */
        state.local_offset = (0.0, -3000.0);
        let requests = state.reanchor_after_moving_local();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].1, Position::Bottom);

        /* drag this machine *down*: the peer ends up above it, which is the case that
         * could not be expressed before */
        state.local_offset = (0.0, 3000.0);
        let requests = state.reanchor_after_moving_local();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].1, Position::Top);
    }

    /// A picture survives a round trip through adoption on the other side.
    ///
    /// The peer turns the rectangles into a placement and back. That is lossy for an
    /// arbitrary rectangle - the placement stores an edge and two ranges - so the adopted
    /// rectangles are kept as they arrived and drawing uses them. Without this the two
    /// windows showed a picture neither had published: "还是不行" with every layer
    /// reporting success.
    #[test]
    fn an_adopted_picture_is_drawn_where_it_said() {
        let mut state = state(vec![]);
        state.local_hostname = "laptop".to_owned();
        state.local_displays = vec![input_event::DisplayInfo {
            name: "eDP-1".to_owned(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            rotation: input_event::Rotation::Normal,
            enabled: true,
        }];
        let mut desk = machine(4, Position::Bottom, (0.0, 1.0), true);
        desk.hostname = "desk".to_owned();
        state.machines = vec![desk];

        /* a desk that is *not* centred under this machine - the placement cannot
         * reproduce this offset, which is the point */
        let mut picture = input_event::Arrangement::new(9, "desk");
        picture.machines.push(input_event::ArrangedMachine {
            hostname: "desk".to_owned(),
            authored: true,
            screens: vec![input_event::ScreenRect {
                x: 3000,
                y: 500,
                width: 2560,
                height: 1440,
            }],
        });
        picture.machines.push(input_event::ArrangedMachine {
            hostname: "laptop".to_owned(),
            authored: false,
            screens: vec![input_event::ScreenRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }],
        });
        state.adopt(&picture);
        state.relayout(900.0, 600.0);

        let drawn = state.screens_of(4);
        assert_eq!(drawn.len(), 1, "{drawn:?}");
        /* the rectangle the picture carried, not a re-derived one */
        assert_eq!((drawn[0].w, drawn[0].h), (2560.0, 1440.0));
        assert_eq!(state.machines[0].peer_rect, Some(drawn[0]));
    }

    /// A window that has not edited anything adopts any picture it is offered.
    ///
    /// The local revision starts at zero and only moves when this side changes a link.
    /// Stamping it on the first report - which is this window merely learning what the
    /// configuration already is - made it always newer than the peer's, so the peer's
    /// picture could never win and the canvas never changed: the reported "拖了也一动不动"
    /// while every layer underneath was working.
    #[test]
    fn a_fresh_window_adopts_the_peers_picture() {
        let mut state = state(vec![]);
        state.local_hostname = "laptop".to_owned();
        state.local_displays = vec![input_event::DisplayInfo {
            name: "eDP-1".to_owned(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            rotation: input_event::Rotation::Normal,
            enabled: true,
        }];
        let mut desk = machine(4, Position::Bottom, (0.0, 1.0), true);
        desk.hostname = "desk".to_owned();
        desk.peer_size = Some((4000.0, 2560.0));
        state.machines = vec![desk];

        /* nothing has been edited on this side */
        assert_eq!(state.local_revision, 0);

        let mut picture = input_event::Arrangement::new(1, "desk");
        picture.machines.push(input_event::ArrangedMachine {
            hostname: "desk".to_owned(),
            authored: true,
            screens: vec![
                input_event::ScreenRect {
                    x: 0,
                    y: 1120,
                    width: 2560,
                    height: 1440,
                },
                input_event::ScreenRect {
                    x: 2560,
                    y: 0,
                    width: 1440,
                    height: 2560,
                },
            ],
        });
        picture.machines.push(input_event::ArrangedMachine {
            hostname: "laptop".to_owned(),
            authored: false,
            screens: vec![input_event::ScreenRect {
                x: 0,
                y: -1080,
                width: 1920,
                height: 1080,
            }],
        });
        state.machines[0].peer_picture = Some(picture);

        assert!(
            state.newer_peer_picture().is_some(),
            "a window that has not edited must adopt"
        );
    }

    /// Dragging this machine's own screens moves the peers' links, not the peers.
    ///
    /// The link is a statement about which monitor a peer is beside, so moving this
    /// machine is a way of editing all of them at once - which is why the local
    /// screens have to be draggable at all.
    #[test]
    fn moving_this_machine_rewrites_the_links_that_it_changed() {
        let mut state = state(vec![]);
        state.local_displays = vec![
            input_event::DisplayInfo {
                name: "DP-2".to_owned(),
                x: 2560,
                y: 0,
                width: 1440,
                height: 2560,
                rotation: input_event::Rotation::Normal,
                enabled: true,
            },
            input_event::DisplayInfo {
                name: "DP-3".to_owned(),
                x: 0,
                y: 1120,
                width: 2560,
                height: 1440,
                rotation: input_event::Rotation::Normal,
                enabled: true,
            },
        ];
        let mut peer = machine(7, Position::Top, (0.0, 1.0), true);
        peer.display = Some("DP-3".to_owned());
        peer.peer_size = Some((2560.0, 1440.0));
        state.machines = vec![peer];
        state.relayout(1000.0, 620.0);

        /* nothing was moved, so nothing needs rewriting */
        assert!(state.reanchor_after_moving_local().is_empty());

        /* drag this machine far to the left: the peer is now to its right */
        state.local_offset = (-6000.0, 0.0);
        let requests = state.reanchor_after_moving_local();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let (handle, position, _, _, _) = requests[0];
        assert_eq!(handle, 7);
        assert_eq!(position, Position::Right);
    }

    /// The local monitors are drawn where the compositor put them.
    ///
    /// A rotated panel occupies a different rectangle from its panel size, and on a
    /// desk of unequal heights the two monitors do not start on the same line. Both
    /// are easy to lose: the canvas drew one box for the whole desk until recently,
    /// and a box has no offsets to get wrong.
    #[test]
    fn the_local_monitors_keep_their_real_offsets() {
        let mut state = state(vec![]);
        state.local_displays = vec![
            input_event::DisplayInfo {
                name: "DP-2".to_owned(),
                x: 2560,
                y: 0,
                width: 2560,
                height: 1440,
                rotation: input_event::Rotation::Right,
                enabled: true,
            },
            input_event::DisplayInfo {
                name: "DP-3".to_owned(),
                x: 0,
                y: 1120,
                width: 2560,
                height: 1440,
                rotation: input_event::Rotation::Normal,
                enabled: true,
            },
        ];
        let monitors = state.local_monitors();
        let dp2 = monitors.iter().find(|m| m.name == "DP-2").expect("DP-2");
        let dp3 = monitors.iter().find(|m| m.name == "DP-3").expect("DP-3");
        /* the rotated panel occupies 1440x2560 at its offset, and it starts at the
         * top of the desk while the upright one starts 1120 px lower */
        assert_eq!(dp2.rect, Screen::new(2560.0, 0.0, 1440.0, 2560.0));
        assert_eq!(dp3.rect, Screen::new(0.0, 1120.0, 2560.0, 1440.0));
        assert_ne!(dp2.rect.y, dp3.rect.y);
    }

    /// Write a PNG of a representative layout so the picture can be looked at.
    ///
    /// Only with `LAN_MOUSE_RENDER_DEMO=/path`: a test that writes files
    /// unconditionally is a test that fails on a read-only checkout.
    #[test]
    fn write_a_demo_image_when_asked() {
        let Some(path) = std::env::var_os("LAN_MOUSE_RENDER_DEMO") else {
            return;
        };
        let mut first = machine(0, Position::Right, (0.0, 0.55), true);
        first.hostname = "thinkpad".to_owned();
        first.peer_size = Some((1920.0, 1080.0));
        let mut second = machine(1, Position::Right, (0.55, 1.0), true);
        second.hostname = "nas-服务器".to_owned();
        second.peer_size = Some((1280.0, 1024.0));
        let third = machine(2, Position::Top, (0.2, 0.8), true);
        let mut state = state(vec![first, second, third]);
        /* put one peer above the lower monitor, which is the case that used to be
         * drawn floating above the whole desk */
        let mut above = machine(3, Position::Top, (0.0, 0.64), true);
        above.hostname = "above-lower".to_owned();
        above.peer_size = Some((2560.0, 1440.0));
        state.machines.push(above);
        /* the desk this was written for: a 2560x1440 panel turned 270 degrees at
         * (2560,0) beside an upright 2560x1440 at (0,1120) */
        state.local_displays = vec![
            input_event::DisplayInfo {
                name: "DP-2".to_owned(),
                x: 2560,
                y: 0,
                width: 2560,
                height: 1440,
                rotation: input_event::Rotation::Right,
                enabled: true,
            },
            input_event::DisplayInfo {
                name: "DP-3".to_owned(),
                x: 0,
                y: 1120,
                width: 2560,
                height: 1440,
                rotation: input_event::Rotation::Normal,
                enabled: true,
            },
        ];
        let surface = render(&mut state, 1600, 1000);
        let mut file =
            std::io::BufWriter::new(std::fs::File::create(path).expect("create the demo image"));
        surface
            .write_to_png(&mut file)
            .expect("write the demo image");
    }
}
