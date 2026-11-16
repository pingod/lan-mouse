use async_trait::async_trait;
use futures_core::Stream;
use std::{
    collections::{HashMap, VecDeque},
    env,
    fmt::{self, Display},
    io::{self, ErrorKind},
    os::fd::{AsFd, RawFd},
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::unix::AsyncFd;

use std::{
    fs::File,
    io::{BufWriter, Write},
    os::unix::prelude::AsRawFd,
    sync::Arc,
};

use wayland_protocols::{
    wp::{
        keyboard_shortcuts_inhibit::zv1::client::{
            zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
            zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
        },
        pointer_constraints::zv1::client::{
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
        },
        relative_pointer::zv1::client::{
            zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
            zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
        },
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};

use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use wayland_client::{
    Connection, Dispatch, DispatchError, EventQueue, QueueHandle, WEnum,
    backend::{ReadEventsGuard, WaylandError},
    delegate_noop,
    globals::{Global, GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_region,
        wl_registry::{self, WlRegistry},
        wl_seat, wl_shm, wl_shm_pool,
        wl_surface::WlSurface,
    },
};

use input_event::Layout as ScreenLayout;
use input_event::{EdgeRange, Event, KeyboardEvent, PointerEvent};

use crate::{CaptureError, CaptureEvent};

use super::{
    Capture, Position,
    error::{LayerShellCaptureCreationError, WaylandBindError},
};

struct Globals {
    compositor: wl_compositor::WlCompositor,
    pointer_constraints: ZwpPointerConstraintsV1,
    relative_pointer_manager: ZwpRelativePointerManagerV1,
    shortcut_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    seat: wl_seat::WlSeat,
    shm: wl_shm::WlShm,
    layer_shell: ZwlrLayerShellV1,
    xdg_output_manager: ZxdgOutputManagerV1,
}

#[derive(Clone, Debug)]
struct Output {
    wl_output: WlOutput,
    global: Global,
    info: Option<OutputInfo>,
    pending_info: OutputInfo,
    has_xdg_info: bool,
}

impl Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(info) = &self.info {
            write!(
                f,
                "{} {}x{} @pos {:?} ({})",
                info.name, info.size.0, info.size.1, info.position, info.description
            )
        } else {
            write!(f, "unknown output")
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct OutputInfo {
    description: String,
    name: String,
    position: (i32, i32),
    size: (i32, i32),
}

struct State {
    /// the segments of each edge that clients can be reached through
    active_positions: HashMap<Position, Vec<EdgeRange>>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    pointer_lock: Option<ZwpLockedPointerV1>,
    rel_pointer: Option<ZwpRelativePointerV1>,
    shortcut_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    active_windows: Vec<Arc<Window>>,
    focused: Option<Arc<Window>>,
    global_list: GlobalList,
    globals: Globals,
    read_guard: Option<ReadEventsGuard>,
    qh: QueueHandle<Self>,
    pending_events: VecDeque<(Position, CaptureEvent)>,
    outputs: Vec<Output>,
    /// scroll events of the pointer frame currently being assembled
    scroll_frame: ScrollFrame,
}

/// The scroll events of one `wl_pointer` frame, indexed by axis.
///
/// A wheel reports one detent twice: `axis` carries the movement in surface
/// units and `axis_value120` the same detent in 120ths. The order is not fixed
/// (Hyprland sends the continuous event first, wlroots sends the discrete one
/// first), so filtering as the events arrive cannot tell which pair belongs
/// together. The frame is held until `Frame` instead, and one shape per axis is
/// released: a whole notch as the discrete event, a partial one as the
/// continuous delta because the receivers' notch conversion drops the fraction.
#[derive(Default)]
struct ScrollFrame {
    continuous: [Option<(Position, u32, f64)>; 2],
    discrete: [Option<(Position, i32)>; 2],
    stop: [Option<(Position, u32)>; 2],
}

impl ScrollFrame {
    fn set_continuous(&mut self, pos: Position, axis: u32, time: u32, value: f64) {
        if let Some(slot) = self.continuous.get_mut(axis as usize) {
            /* axis values are deltas: two events for one axis in a frame add up */
            match slot {
                Some((_, _, total)) => *total += value,
                None => *slot = Some((pos, time, value)),
            }
        }
    }

    fn set_discrete(&mut self, pos: Position, axis: u32, value: i32) {
        if let Some(slot) = self.discrete.get_mut(axis as usize) {
            match slot {
                Some((_, total)) => *total += value,
                None => *slot = Some((pos, value)),
            }
        }
    }

    fn set_stop(&mut self, pos: Position, axis: u32, time: u32) {
        if let Some(slot) = self.stop.get_mut(axis as usize) {
            *slot = Some((pos, time));
        }
    }

    /// Release the frame. Each axis contributes at most one scroll event, then
    /// its stop notification if one was buffered.
    fn release(&mut self, out: &mut VecDeque<(Position, CaptureEvent)>) {
        for axis in 0..2 {
            let discrete = self.discrete[axis].take();
            let continuous = self.continuous[axis].take();
            let scalar = axis as u8;

            /* A whole notch is unambiguous, and the receivers turn it into one
             * notch of their own instead of guessing the surface-unit scale. */
            let whole_notch = discrete.is_some_and(|(_, value)| value != 0 && value % 120 == 0);
            if whole_notch {
                if let Some((pos, value)) = discrete {
                    out.push_back((
                        pos,
                        CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                            axis: scalar,
                            value,
                        })),
                    ));
                }
            } else if let Some((pos, time, value)) =
                continuous.filter(|(_, _, value)| *value != 0.0)
            {
                out.push_back((
                    pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                        time,
                        axis: scalar,
                        value,
                    })),
                ));
            } else if let Some((pos, value)) = discrete.filter(|(_, value)| *value != 0) {
                out.push_back((
                    pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                        axis: scalar,
                        value,
                    })),
                ));
            }

            if let Some((pos, time)) = self.stop[axis].take() {
                out.push_back((
                    pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::AxisStop {
                        time,
                        axis: scalar,
                    })),
                ));
            }
        }
    }
}

struct Inner {
    state: State,
    queue: EventQueue<State>,
}

impl AsRawFd for Inner {
    fn as_raw_fd(&self) -> RawFd {
        self.queue.as_fd().as_raw_fd()
    }
}

pub struct LayerShellInputCapture(AsyncFd<Inner>);

/// Where a barrier surface goes: one segment of one edge of one output.
///
/// Grouped because the geometry is a single decision - the position of the edge,
/// the extent of the output it sits on, the segment that edge is measured over
/// and the whole layout the segment is a fraction of - and passing them apart
/// invites mixing up which axis a number belongs to.
#[derive(Clone, Copy, Debug)]
struct Placement {
    pos: Position,
    /// size of the output this surface is placed on
    size: (i32, i32),
    /// position of that output in layout coordinates
    origin: (i32, i32),
    /// inclusive pixel span along the crossing axis, in layout coordinates,
    /// already intersected with this output
    segment: (i32, i32),
    /// the span of the whole capture area along the crossing axis
    axis: (i32, i32),
}

struct Window {
    buffer: wl_buffer::WlBuffer,
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    pos: Position,
    /// where along the crossing axis the surface starts, in layout coordinates
    ///
    /// `wl_pointer::Enter` reports the position inside the surface, which is all
    /// this backend ever gets: adding the offset turns it into the point of the
    /// edge the pointer crossed.
    offset: i32,
    /// the span of the whole capture area along the crossing axis, `(lo, hi)`
    /// inclusive
    axis: (i32, i32),
}

impl Window {
    /// Create the barrier surface for one segment of one edge of one output.
    fn new(
        state: &State,
        qh: &QueueHandle<State>,
        output: &WlOutput,
        place: Placement,
    ) -> Option<Window> {
        let Placement {
            pos,
            size,
            origin,
            segment,
            axis,
        } = place;
        log::debug!("creating window output: {output:?}, size: {size:?}, segment: {segment:?}");
        let g = &state.globals;

        /* the surface spans the segment along the crossing axis and is one pixel
         * wide (or tall) perpendicular to it */
        let along = segment.1 - segment.0 + 1;
        if along <= 0 {
            return None;
        }
        let (width, height) = match pos {
            Position::Left | Position::Right => (1u32, along as u32),
            Position::Top | Position::Bottom => (along as u32, 1u32),
        };
        /* Margins are measured from the anchored edges of the output. The last
         * pixel of the output is at `origin + extent - 1`, so the trailing
         * margin is that minus the last pixel of the segment. */
        let (start, end) = match pos {
            Position::Left | Position::Right => {
                (segment.0 - origin.1, origin.1 + size.1 - 1 - segment.1)
            }
            Position::Top | Position::Bottom => {
                (segment.0 - origin.0, origin.0 + size.0 - 1 - segment.1)
            }
        };
        let (start, end) = (start.max(0), end.max(0));
        let mut file = match tempfile::tempfile() {
            Ok(f) => f,
            Err(e) => {
                log::error!("unable to create a temporary file for the capture window: {e}");
                return None;
            }
        };
        draw(&mut file, (width, height));
        let pool = g
            .shm
            .create_pool(file.as_fd(), (width * height * 4) as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        // the pool object is no longer needed once the buffer exists, and the
        // buffer keeps the underlying mapping alive
        pool.destroy();
        let surface = g.compositor.create_surface(qh, ());

        let layer_surface = g.layer_shell.get_layer_surface(
            &surface,
            Some(output),
            Layer::Overlay,
            "LAN Mouse Sharing".into(),
            qh,
            (),
        );
        /* The edge plus both perpendicular edges: the surface is then stretched
         * between the two margins along the crossing axis, which is what places
         * it over exactly the segment a client owns. */
        let anchor = match pos {
            Position::Left => Anchor::Left | Anchor::Top | Anchor::Bottom,
            Position::Right => Anchor::Right | Anchor::Top | Anchor::Bottom,
            Position::Top => Anchor::Top | Anchor::Left | Anchor::Right,
            Position::Bottom => Anchor::Bottom | Anchor::Left | Anchor::Right,
        };

        layer_surface.set_anchor(anchor);
        layer_surface.set_size(width, height);
        layer_surface.set_exclusive_zone(-1);
        match pos {
            Position::Left | Position::Right => {
                layer_surface.set_margin(start, 0, end, 0);
            }
            Position::Top | Position::Bottom => {
                layer_surface.set_margin(0, end, 0, start);
            }
        }
        surface.set_input_region(None);
        surface.commit();
        Some(Window {
            pos,
            offset: segment.0,
            axis,
            buffer,
            surface,
            layer_surface,
        })
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        log::debug!("destroying window!");
        self.layer_surface.destroy();
        self.surface.destroy();
        self.buffer.destroy();
    }
}

/// The span of the whole capture area along the axis an edge is measured on.
///
/// Edge ranges are fractions of *this* span, not of the output that happens to
/// touch them: it is the span the pointer crosses along and the span the
/// frontend draws, so both agree on what `0-60%` of an edge means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    /// inclusive minimum x of every output
    x0: i32,
    /// inclusive maximum x of every output
    x1: i32,
    /// inclusive minimum y of every output
    y0: i32,
    /// inclusive maximum y of every output
    y1: i32,
}

impl Layout {
    /// The span an edge is measured on.
    fn axis(&self, pos: Position) -> (i32, i32) {
        match pos {
            Position::Left | Position::Right => (self.y0, self.y1),
            Position::Top | Position::Bottom => (self.x0, self.x1),
        }
    }
}

/// Bounding box of every output whose geometry is known.
fn layout_geometry(outputs: &[Output]) -> Option<Layout> {
    let infos = outputs
        .iter()
        .filter_map(|output| output.info.as_ref())
        .collect::<Vec<_>>();
    let first = infos.first()?;
    let mut layout = Layout {
        x0: first.position.0,
        y0: first.position.1,
        x1: first.position.0 + first.size.0 - 1,
        y1: first.position.1 + first.size.1 - 1,
    };
    for info in infos {
        layout.x0 = layout.x0.min(info.position.0);
        layout.y0 = layout.y0.min(info.position.1);
        layout.x1 = layout.x1.max(info.position.0 + info.size.0 - 1);
        layout.y1 = layout.y1.max(info.position.1 + info.size.1 - 1);
    }
    Some(layout)
}

/// Where along the whole edge a crossing at `along` happened, as a fraction.
///
/// `offset` is where the barrier surface starts in layout coordinates and
/// `axis` is the span of the whole capture area along the crossing axis.
fn crossing_fraction(offset: i32, axis: (i32, i32), along: f64) -> f64 {
    let span = (axis.1 - axis.0 + 1) as f64;
    if span <= 0.0 {
        return 0.5;
    }
    ((offset as f64 + along - axis.0 as f64) / span).clamp(0.0, 1.0)
}

/// The inclusive pixel span of an edge range, or `None` if it is empty.
fn segment_spans(axis: (i32, i32), edge: EdgeRange) -> Option<(i32, i32)> {
    let span = axis.1 - axis.0 + 1;
    if span <= 0 {
        return None;
    }
    let scale = |permille: u16| axis.0 + (permille as i64 * span as i64 / 1000) as i32;
    let lo = scale(edge.start());
    // `end` is exclusive, so the last pixel of the range is one before it
    let hi = scale(edge.end()) - 1;
    (lo <= hi).then_some((lo, hi))
}

fn get_edges(outputs: &[Output], pos: Position) -> Vec<(Output, i32)> {
    outputs
        .iter()
        .filter_map(|output| {
            output.info.as_ref().map(|info| {
                (
                    output.clone(),
                    match pos {
                        Position::Left => info.position.0,
                        Position::Right => info.position.0 + info.size.0,
                        Position::Top => info.position.1,
                        Position::Bottom => info.position.1 + info.size.1,
                    },
                )
            })
        })
        .collect()
}

fn get_output_configuration(state: &State, pos: Position) -> Vec<Output> {
    // get all output edges corresponding to the position
    let edges = get_edges(&state.outputs, pos);
    let opposite_edges = get_edges(&state.outputs, pos.opposite());

    // remove those edges that are at the same position
    // as an opposite edge of a different output
    edges
        .iter()
        .filter(|(_, edge)| !opposite_edges.iter().map(|(_, e)| *e).any(|e| &e == edge))
        .map(|(o, _)| o.clone())
        .collect()
}

fn draw(f: &mut File, (width, height): (u32, u32)) {
    let mut buf = BufWriter::new(f);
    for _ in 0..height {
        for _ in 0..width {
            if env::var("LM_DEBUG_LAYER_SHELL").ok().is_some() {
                // AARRGGBB
                buf.write_all(&0xff11d116u32.to_ne_bytes()).unwrap();
            } else {
                // AARRGGBB
                buf.write_all(&0x00000000u32.to_ne_bytes()).unwrap();
            }
        }
    }
}

impl LayerShellInputCapture {
    pub fn new() -> std::result::Result<Self, LayerShellCaptureCreationError> {
        let conn = Connection::connect_to_env()?;
        let (global_list, mut queue) = registry_queue_init::<State>(&conn)?;

        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor = global_list
            .bind(&qh, 4..=5, ())
            .map_err(|e| WaylandBindError::new(e, "wl_compositor 4..=5"))?;
        let xdg_output_manager: ZxdgOutputManagerV1 = global_list
            .bind(&qh, 1..=3, ())
            .map_err(|e| WaylandBindError::new(e, "xdg_output_manager 1..=3"))?;
        let shm: wl_shm::WlShm = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wl_shm"))?;
        let layer_shell: ZwlrLayerShellV1 = global_list
            .bind(&qh, 3..=4, ())
            .map_err(|e| WaylandBindError::new(e, "wlr_layer_shell 3..=4"))?;
        let seat: wl_seat::WlSeat = global_list
            .bind(&qh, 7..=8, ())
            .map_err(|e| WaylandBindError::new(e, "wl_seat 7..=8"))?;

        let pointer_constraints: ZwpPointerConstraintsV1 = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_pointer_constraints_v1"))?;
        let relative_pointer_manager: ZwpRelativePointerManagerV1 = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_relative_pointer_manager_v1"))?;
        let shortcut_inhibit_manager: Result<
            ZwpKeyboardShortcutsInhibitManagerV1,
            WaylandBindError,
        > = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_keyboard_shortcuts_inhibit_manager_v1"));
        // layer-shell backend still works without this protocol so we make it an optional dependency
        if let Err(e) = &shortcut_inhibit_manager {
            log::warn!("shortcut_inhibit_manager not supported: {e}\nkeybinds handled by the compositor will not be passed
                to the client");
        }
        let shortcut_inhibit_manager = shortcut_inhibit_manager.ok();

        let mut state = State {
            active_positions: Default::default(),
            pointer: None,
            keyboard: None,
            global_list,
            globals: Globals {
                compositor,
                shm,
                layer_shell,
                seat,
                pointer_constraints,
                relative_pointer_manager,
                shortcut_inhibit_manager,
                xdg_output_manager,
            },
            pointer_lock: None,
            rel_pointer: None,
            shortcut_inhibitor: None,
            active_windows: Vec::new(),
            focused: None,
            qh,
            read_guard: None,
            pending_events: VecDeque::new(),
            outputs: vec![],
            scroll_frame: ScrollFrame::default(),
        };

        for global in state.global_list.contents().clone_list() {
            state.register_global(global);
        }

        // flush outgoing events
        queue.flush()?;

        let read_guard = loop {
            match queue.prepare_read() {
                Some(r) => break r,
                None => {
                    queue.dispatch_pending(&mut state)?;
                    continue;
                }
            }
        };
        state.read_guard = Some(read_guard);

        let inner = AsyncFd::new(Inner { queue, state })?;

        Ok(LayerShellInputCapture(inner))
    }

    fn add_client(&mut self, pos: Position, segments: Vec<EdgeRange>) {
        self.0.get_mut().state.add_client(pos, segments);
    }

    fn delete_client(&mut self, pos: Position) {
        let inner = self.0.get_mut();
        inner.state.active_positions.remove(&pos);
        // Release the grab before dropping the window it belongs to: leaving
        // `pointer_lock`/`rel_pointer` set would make `grab()` skip creating
        // them for the next session, which permanently disables pointer
        // motion (it is only delivered via the relative pointer object).
        if inner.state.focused.as_ref().is_some_and(|w| w.pos == pos) {
            inner.state.ungrab();
            inner.state.focused = None;
        }
        // remove all windows corresponding to this client
        while let Some(i) = inner.state.active_windows.iter().position(|w| w.pos == pos) {
            inner.state.active_windows.remove(i);
        }
    }
}

impl State {
    fn update_output_info(&mut self, name: u32) {
        let Some(output) = self.outputs.iter_mut().find(|o| o.global.name == name) else {
            // the output can be gone already when both `GlobalRemove` and the
            // output's `Done` arrive in the same dispatch batch
            log::debug!("output {name} is gone, ignoring its info update");
            return;
        };
        if output.has_xdg_info && output.info.as_ref() != Some(&output.pending_info) {
            output.info.replace(output.pending_info.clone());
            self.update_windows();
        }
    }

    fn register_global(&mut self, global: Global) {
        if global.interface.as_str() == "wl_output" {
            log::debug!("new output global: wl_output {}", global.name);
            let wl_output = self.global_list.registry().bind::<WlOutput, _, _>(
                global.name,
                4,
                &self.qh,
                global.name,
            );
            self.globals
                .xdg_output_manager
                .get_xdg_output(&wl_output, &self.qh, global.name);
            self.outputs.push(Output {
                wl_output,
                global,
                info: None,
                has_xdg_info: false,
                pending_info: Default::default(),
            })
        }
    }

    fn deregister_global(&mut self, name: u32) {
        self.outputs.retain(|o| {
            if o.global.name == name {
                log::debug!("{o} (global {:?}) removed", o.global);
                o.wl_output.release();
                false
            } else {
                true
            }
        });
    }

    fn grab(
        &mut self,
        surface: &WlSurface,
        pointer: &WlPointer,
        serial: u32,
        qh: &QueueHandle<State>,
    ) {
        let Some(window) = self.focused.clone() else {
            log::debug!("grab requested without a focused window");
            return;
        };

        // hide the cursor
        pointer.set_cursor(serial, None, 0, 0);

        // capture input
        window
            .layer_surface
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        window.surface.commit();

        // lock pointer
        if self.pointer_lock.is_none() {
            self.pointer_lock = Some(self.globals.pointer_constraints.lock_pointer(
                surface,
                pointer,
                None,
                Lifetime::Persistent,
                qh,
                (),
            ));
        }

        // request relative input
        if self.rel_pointer.is_none() {
            self.rel_pointer = Some(self.globals.relative_pointer_manager.get_relative_pointer(
                pointer,
                qh,
                (),
            ));
        }

        // capture modifier keys
        if let Some(shortcut_inhibit_manager) = &self.globals.shortcut_inhibit_manager {
            if self.shortcut_inhibitor.is_none() {
                self.shortcut_inhibitor = Some(shortcut_inhibit_manager.inhibit_shortcuts(
                    surface,
                    &self.globals.seat,
                    qh,
                    (),
                ));
            }
        }
    }

    fn ungrab(&mut self) {
        // ungrab surface (the focused window may already be gone)
        if let Some(window) = self.focused.as_ref() {
            window
                .layer_surface
                .set_keyboard_interactivity(KeyboardInteractivity::None);
            window.surface.commit();
        }

        // destroy pointer lock
        if let Some(pointer_lock) = &self.pointer_lock {
            pointer_lock.destroy();
            self.pointer_lock = None;
        }

        // destroy relative input
        if let Some(rel_pointer) = &self.rel_pointer {
            rel_pointer.destroy();
            self.rel_pointer = None;
        }

        // destroy shortcut inhibitor
        if let Some(shortcut_inhibitor) = &self.shortcut_inhibitor {
            shortcut_inhibitor.destroy();
            self.shortcut_inhibitor = None;
        }
    }

    fn add_client(&mut self, pos: Position, segments: Vec<EdgeRange>) {
        self.active_positions.insert(pos, segments);
        self.create_windows(pos);
    }

    /// Create the barrier surfaces for one position.
    ///
    /// The surfaces of that position are dropped first: the segments change as
    /// clients arrive and leave, and a surface cannot be resized in place
    /// without a configure round trip.
    fn create_windows(&mut self, pos: Position) {
        let Some(segments) = self.active_positions.get(&pos).cloned() else {
            return;
        };
        if self.focused.as_ref().is_some_and(|w| w.pos == pos) {
            self.ungrab();
            self.focused = None;
        }
        self.active_windows.retain(|w| w.pos != pos);
        let outputs = get_output_configuration(self, pos);

        log::info!(
            "adding capture for position {pos} - using outputs: {:?}, segments: {}",
            outputs
                .iter()
                .map(|o| o
                    .info
                    .as_ref()
                    .map(|i| i.name.to_owned())
                    .unwrap_or("unknown output".to_owned()))
                .collect::<Vec<_>>(),
            segments
                .iter()
                .map(EdgeRange::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        );
        let Some(layout) = layout_geometry(&outputs) else {
            log::warn!("no output geometry is known, not creating a barrier for {pos}");
            return;
        };
        for segment in &segments {
            let Some(spans) = segment_spans(layout.axis(pos), *segment) else {
                continue;
            };
            for output in &outputs {
                let Some(info) = output.info.as_ref() else {
                    continue;
                };
                /* the segment is in layout coordinates; the surface lives on one
                 * output, so only the part of it that overlaps this output can be
                 * covered here */
                let (lo, hi) = match pos {
                    Position::Left | Position::Right => {
                        (info.position.1, info.position.1 + info.size.1 - 1)
                    }
                    Position::Top | Position::Bottom => {
                        (info.position.0, info.position.0 + info.size.0 - 1)
                    }
                };
                let segment = (spans.0.max(lo), spans.1.min(hi));
                if segment.0 > segment.1 {
                    continue;
                }
                let place = Placement {
                    pos,
                    size: info.size,
                    origin: info.position,
                    segment,
                    axis: layout.axis(pos),
                };
                if let Some(window) = Window::new(self, &self.qh, &output.wl_output, place) {
                    self.active_windows.push(Arc::new(window));
                }
            }
        }
    }

    fn update_windows(&mut self) {
        log::info!("active outputs: ");
        for output in self.outputs.iter().filter(|o| o.info.is_some()) {
            log::info!(" * {output}");
        }

        // The windows are about to be recreated, so drop the grabs first: they
        // are bound to surfaces that would be destroyed underneath them, and a
        // stale `Some` would prevent `grab()` from recreating them.
        self.ungrab();
        self.focused = None;
        self.active_windows.clear();

        let active = self
            .active_positions
            .iter()
            .map(|(pos, segments)| (*pos, segments.clone()))
            .collect::<Vec<_>>();
        for (pos, segments) in active {
            self.add_client(pos, segments);
        }
    }
}

impl Inner {
    fn read(&mut self) -> io::Result<()> {
        let Some(guard) = self.state.read_guard.take() else {
            log::debug!("read called without a prepared read guard");
            return Ok(());
        };
        match guard.read() {
            Ok(_) => Ok(()),
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(WaylandError::Io(e)) => {
                log::error!("error reading from wayland socket: {e}");
                Ok(())
            }
            Err(WaylandError::Protocol(e)) => {
                // A broken connection must be reported, not abort the process:
                // the compositor may have restarted, which is recoverable.
                Err(io::Error::other(format!("wayland protocol violation: {e}")))
            }
        }
    }

    fn prepare_read(&mut self) -> io::Result<()> {
        loop {
            if let Some(guard) = self.queue.prepare_read() {
                self.state.read_guard = Some(guard);
                break Ok(());
            } else {
                self.dispatch_events()?;
            }
        }
    }

    fn dispatch_events(&mut self) -> io::Result<()> {
        match self.queue.dispatch_pending(&mut self.state) {
            Ok(_) => Ok(()),
            Err(DispatchError::Backend(WaylandError::Io(e))) => {
                log::error!("Wayland Error: {e}");
                Ok(())
            }
            Err(DispatchError::Backend(e)) => Err(io::Error::other(format!("backend error: {e}"))),
            Err(DispatchError::BadMessage {
                sender_id,
                interface,
                opcode,
            }) => Err(io::Error::other(format!(
                "bad message {sender_id}, {interface} , {opcode}"
            ))),
        }
    }

    fn flush_events(&mut self) -> io::Result<()> {
        // flush outgoing events
        match self.queue.flush() {
            Ok(_) => Ok(()),
            Err(WaylandError::Io(e)) => Err(e),
            Err(WaylandError::Protocol(e)) => {
                Err(io::Error::other(format!("wayland protocol violation: {e}")))
            }
        }
    }
}

#[async_trait]
impl Capture for LayerShellInputCapture {
    async fn create(
        &mut self,
        pos: Position,
        segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        self.add_client(pos, segments);
        let inner = self.0.get_mut();
        Ok(inner.flush_events()?)
    }

    async fn update(
        &mut self,
        pos: Position,
        segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        /* replacing the surfaces is the same work as creating them: the
         * compositor gives no way to move a layer surface's margins without a
         * configure round trip, and the pointer may be sitting in the segment
         * that just changed */
        self.add_client(pos, segments);
        let inner = self.0.get_mut();
        Ok(inner.flush_events()?)
    }

    async fn layout(&mut self) -> Option<ScreenLayout> {
        /* the output list is filled by the event loop, which runs on this task's
         * polls, so whatever it has seen so far is what is available here */
        let inner = self.0.get_mut();
        let displays = inner
            .state
            .outputs
            .iter()
            .filter_map(|output| output.info.as_ref())
            .map(|info| input_event::DisplayInfo {
                name: info.name.clone(),
                x: info.position.0,
                y: info.position.1,
                width: info.size.0.max(0) as u32,
                height: info.size.1.max(0) as u32,
                /* wl_output reports the size after the transform, and the
                 * compositor applies it when it lays the output out, so the
                 * rectangle here is already the one the pointer moves in */
                rotation: input_event::Rotation::Normal,
                enabled: true,
            })
            .collect::<Vec<_>>();
        (!displays.is_empty()).then(|| ScreenLayout::new(displays))
    }

    async fn begin(&mut self, _pos: Position) -> Result<(), CaptureError> {
        /* The compositor decides who owns the pointer: it cannot be taken
         * without the pointer actually crossing a barrier. */
        Err(CaptureError::BeginUnsupported)
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.delete_client(pos);
        let inner = self.0.get_mut();
        Ok(inner.flush_events()?)
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        log::debug!("releasing pointer");
        let inner = self.0.get_mut();
        inner.state.ungrab();
        Ok(inner.flush_events()?)
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl Stream for LayerShellInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.0.get_mut().state.pending_events.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }

        loop {
            let mut guard = ready!(self.0.poll_read_ready_mut(cx))?;

            {
                let inner = guard.get_inner_mut();

                // read events
                inner.read()?;

                // dispatch the events
                inner.dispatch_events()?;

                // flush outgoing events
                if let Err(e) = inner.flush_events() {
                    if e.kind() != ErrorKind::WouldBlock {
                        return Poll::Ready(Some(Err(e.into())));
                    }
                }

                // prepare for the next read
                inner.prepare_read()?;
            }

            // clear read readiness for tokio read guard
            // guard.clear_ready_matching(Ready::READABLE);
            guard.clear_ready();

            // if an event has been queued during dispatch_events() we return it
            match guard.get_inner_mut().state.pending_events.pop_front() {
                Some(event) => return Poll::Ready(Some(Ok(event))),
                None => continue,
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: <wl_seat::WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) {
                if let Some(p) = state.pointer.take() {
                    p.release();
                }
                state.pointer.replace(seat.get_pointer(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                if let Some(k) = state.keyboard.take() {
                    k.release();
                }
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        app: &mut Self,
        pointer: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                // get client corresponding to the focused surface
                let Some(window) = app
                    .active_windows
                    .iter()
                    .find(|w| w.surface == surface)
                    .cloned()
                else {
                    return;
                };
                app.focused = Some(window.clone());
                app.grab(&surface, pointer, serial, qh);
                let pos = window.pos;
                /* The surface is one pixel thick and spans the segment along the
                 * crossing axis, so the position inside it says where on that
                 * segment the pointer arrived. Until now every crossing was
                 * reported as the middle of the edge, which parked the peer's
                 * cursor in the middle no matter where the pointer actually
                 * left - the cursor teleporting when crossing back and forth. */
                let along = match pos {
                    Position::Left | Position::Right => surface_y,
                    Position::Top | Position::Bottom => surface_x,
                };
                let fraction = crossing_fraction(window.offset, window.axis, along);
                app.pending_events
                    .push_back((pos, CaptureEvent::Begin(fraction)));
            }
            wl_pointer::Event::Leave { .. } => {
                /* There are rare cases, where when a window is opened in
                 * just the wrong moment, the pointer is released, while
                 * still grabbed.
                 * In that case, the pointer must be ungrabbed, otherwise
                 * it is impossible to grab it again (since the pointer
                 * lock, relative pointer,... objects are still in place)
                 */
                if app.pointer_lock.is_some() {
                    /* Normal: a gesture ends, or the compositor takes the pointer back. */
                    log::debug!("compositor released mouse");
                }
                // If we were actively capturing, the compositor just took the
                // pointer away (lock screen, new surface, display change). The
                // barrier is gone and no further input will arrive, so tell the
                // service to release instead of waiting for the release-bind
                // chord that can never be delivered (issues #545, #512).
                if let Some(pos) = app.focused.as_ref().map(|w| w.pos) {
                    if app.pointer_lock.is_some() {
                        log::info!("compositor took the pointer — capture interrupted");
                        app.pending_events
                            .push_back((pos, CaptureEvent::Interrupted));
                    }
                }
                app.ungrab();
            }
            wl_pointer::Event::Button {
                serial: _,
                time,
                button,
                state,
            } => {
                let Some(pos) = app.focused.as_ref().map(|w| w.pos) else {
                    return;
                };
                app.pending_events.push_back((
                    pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                        time,
                        button,
                        state: u32::from(state),
                    })),
                ));
            }
            wl_pointer::Event::Axis { time, axis, value } => {
                let Some(pos) = app.focused.as_ref().map(|w| w.pos) else {
                    return;
                };
                app.scroll_frame
                    .set_continuous(pos, u32::from(axis), time, value);
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                let Some(pos) = app.focused.as_ref().map(|w| w.pos) else {
                    return;
                };
                app.scroll_frame
                    .set_discrete(pos, u32::from(axis), value120);
            }
            wl_pointer::Event::AxisStop { time, axis } => {
                let Some(pos) = app.focused.as_ref().map(|w| w.pos) else {
                    return;
                };
                app.scroll_frame.set_stop(pos, u32::from(axis), time);
            }
            wl_pointer::Event::Frame => {
                /* the frame is complete: the scroll pair can be told apart */
                let State {
                    scroll_frame,
                    pending_events,
                    ..
                } = app;
                scroll_frame.release(pending_events);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        app: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let window = &app.focused;
        match event {
            wl_keyboard::Event::Key {
                serial: _,
                time,
                key,
                state,
            } => {
                if let Some(window) = window {
                    app.pending_events.push_back((
                        window.pos,
                        CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                            time,
                            key,
                            state: u32::from(state) as u8,
                        })),
                    ));
                }
            }
            wl_keyboard::Event::Modifiers {
                serial: _,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                if let Some(window) = window {
                    app.pending_events.push_back((
                        window.pos,
                        CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                            depressed: mods_depressed,
                            latched: mods_latched,
                            locked: mods_locked,
                            group,
                        })),
                    ));
                }
            }
            _ => (),
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        app: &mut Self,
        _: &ZwpRelativePointerV1,
        event: <ZwpRelativePointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            utime_hi,
            utime_lo,
            dx_unaccel: dx,
            dy_unaccel: dy,
            ..
        } = event
        {
            if let Some(window) = &app.focused {
                let time = ((((utime_hi as u64) << 32) | utime_lo as u64) / 1000) as u32;
                app.pending_events.push_back((
                    window.pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { time, dx, dy })),
                ));
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        app: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            if let Some(window) = app
                .active_windows
                .iter()
                .find(|w| &w.layer_surface == layer_surface)
            {
                // client corresponding to the layer_surface
                let surface = &window.surface;
                let buffer = &window.buffer;
                surface.attach(Some(buffer), 0, 0);
                layer_surface.ack_configure(serial);
                surface.commit();
            }
        }
    }
}

// delegate wl_registry events to App itself
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        _registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                state.register_global(Global {
                    name,
                    interface,
                    version,
                });
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.deregister_global(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as wayland_client::Proxy>::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let output = state
            .outputs
            .iter_mut()
            .find(|o| o.global.name == *name)
            .expect("output");

        log::debug!("xdg_output {name} - {event:?}");
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.pending_info.position = (x, y);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.pending_info.size = (width, height);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Done => {
                log::warn!("Use of deprecated xdg-output event \"done\"");
                state.update_output_info(*name);
            }
            zxdg_output_v1::Event::Name { name } => {
                output.pending_info.name = name;
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Description { description } => {
                output.pending_info.description = description;
                output.has_xdg_info = true;
            }
            event => {
                /* newer protocol versions can add events; panicking here would
                 * take the whole capture backend down
                 */
                log::debug!("unhandled xdg_output event: {event:?}");
            }
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _wl_output: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        name: &u32,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        log::debug!("wl_output {name} - {event:?}");
        if let wl_output::Event::Done = event {
            state.update_output_info(*name);
        }
    }
}

// don't emit any events
delegate_noop!(State: wl_region::WlRegion);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ZwpRelativePointerManagerV1);
delegate_noop!(State: ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: ZwpPointerConstraintsV1);

// ignore events
delegate_noop!(State: ignore ZxdgOutputManagerV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitorV1);
delegate_noop!(State: ignore ZwpLockedPointerV1);

#[cfg(test)]
mod scroll_frame_tests {
    use super::*;

    fn released(frame: &mut ScrollFrame) -> Vec<PointerEvent> {
        let mut out = VecDeque::new();
        frame.release(&mut out);
        out.into_iter()
            .map(|(_, event)| match event {
                CaptureEvent::Input(Event::Pointer(event)) => event,
                other => panic!("unexpected capture event: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_notch_is_forwarded_once_even_when_the_shapes_arrive_in_the_other_order() {
        // Hyprland sends `axis` first, wlroots sends `axis_value120` first.
        for wheel_first in [false, true] {
            let mut frame = ScrollFrame::default();
            if wheel_first {
                frame.set_discrete(Position::Top, 0, 120);
                frame.set_continuous(Position::Top, 0, 1, 15.0);
            } else {
                frame.set_continuous(Position::Top, 0, 1, 15.0);
                frame.set_discrete(Position::Top, 0, 120);
            }
            assert_eq!(
                released(&mut frame),
                vec![PointerEvent::AxisDiscrete120 {
                    axis: 0,
                    value: 120
                }],
                "wheel_first={wheel_first}"
            );
        }
    }

    #[test]
    fn a_touchpad_frame_keeps_the_continuous_delta() {
        let mut frame = ScrollFrame::default();
        frame.set_continuous(Position::Top, 0, 7, 2.5);
        assert_eq!(
            released(&mut frame),
            vec![PointerEvent::Axis {
                time: 7,
                axis: 0,
                value: 2.5
            }]
        );
    }

    #[test]
    fn a_partial_notch_is_not_rounded_away() {
        // High-resolution wheels report fractions of a notch; a receiver that
        // converts 120ths to whole notches would drop them.
        let mut frame = ScrollFrame::default();
        frame.set_continuous(Position::Top, 0, 1, 7.5);
        frame.set_discrete(Position::Top, 0, 60);
        assert_eq!(
            released(&mut frame),
            vec![PointerEvent::Axis {
                time: 1,
                axis: 0,
                value: 7.5
            }]
        );
    }

    #[test]
    fn horizontal_scroll_keeps_its_axis() {
        let mut frame = ScrollFrame::default();
        frame.set_continuous(Position::Bottom, 1, 1, -4.0);
        assert_eq!(
            released(&mut frame),
            vec![PointerEvent::Axis {
                time: 1,
                axis: 1,
                value: -4.0
            }]
        );
    }

    #[test]
    fn a_stop_is_released_after_the_scroll_it_ends() {
        let mut frame = ScrollFrame::default();
        frame.set_continuous(Position::Top, 0, 3, 0.0);
        frame.set_stop(Position::Top, 0, 3);
        assert_eq!(
            released(&mut frame),
            vec![PointerEvent::AxisStop { time: 3, axis: 0 }]
        );
    }

    #[test]
    fn a_released_frame_is_empty() {
        let mut frame = ScrollFrame::default();
        frame.set_discrete(Position::Top, 0, -240);
        assert_eq!(
            released(&mut frame),
            vec![PointerEvent::AxisDiscrete120 {
                axis: 0,
                value: -240
            }]
        );
        assert!(released(&mut frame).is_empty());
    }
}

#[cfg(test)]
mod segment_tests {
    use super::*;

    fn range(start: f64, end: f64) -> EdgeRange {
        EdgeRange::from_fractions(start, end).expect("valid range")
    }

    #[test]
    fn a_full_range_spans_the_whole_axis() {
        assert_eq!(segment_spans((0, 1079), EdgeRange::FULL), Some((0, 1079)));
        // an axis that does not start at zero is spanned from its own start
        assert_eq!(
            segment_spans((100, 1179), EdgeRange::FULL),
            Some((100, 1179))
        );
    }

    #[test]
    fn half_ranges_meet_without_overlapping_or_gapping() {
        let axis = (0, 1079);
        let top = range(0.0, 0.5);
        let bottom = range(0.5, 1.0);
        let (top_lo, top_hi) = segment_spans(axis, top).expect("top");
        let (bottom_lo, bottom_hi) = segment_spans(axis, bottom).expect("bottom");
        assert_eq!(top_hi + 1, bottom_lo);
        assert_eq!((top_lo, top_hi), (0, 539));
        assert_eq!((bottom_lo, bottom_hi), (540, 1079));
    }

    #[test]
    fn three_ranges_cover_an_edge_exactly() {
        let axis = (0, 999);
        let ranges = [
            range(0.0, 1.0 / 3.0),
            range(1.0 / 3.0, 2.0 / 3.0),
            range(2.0 / 3.0, 1.0),
        ];
        let spans = ranges
            .iter()
            .filter_map(|range| segment_spans(axis, *range))
            .collect::<Vec<_>>();
        assert_eq!(spans.len(), 3);
        for pair in spans.windows(2) {
            assert_eq!(pair[0].1 + 1, pair[1].0, "{spans:?} leave a gap");
        }
        assert_eq!(spans.first().map(|s| s.0), Some(0));
        assert_eq!(spans.last().map(|s| s.1), Some(999));
    }

    #[test]
    fn a_range_narrower_than_a_pixel_yields_no_surface() {
        // 1 px edge, 1 permille requested: nothing to cover
        assert_eq!(segment_spans((0, 0), range(0.0, 0.001)), None);
    }

    #[test]
    fn the_crossing_point_is_measured_over_the_whole_edge() {
        // a surface starting at 540 with the pointer 10 px into it
        assert!((crossing_fraction(540, (0, 1079), 10.0) - 0.51).abs() < 0.002);
        assert_eq!(crossing_fraction(0, (0, 1079), 0.0), 0.0);
        // a surface that starts at the layout's own start
        assert_eq!(crossing_fraction(100, (100, 1179), 0.0), 0.0);
    }
}
