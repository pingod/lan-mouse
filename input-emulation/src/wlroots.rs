use crate::error::EmulationError;

use super::{Emulation, error::WlrootsEmulationCreationError};
use async_trait::async_trait;
use bitflags::bitflags;
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wayland_client::WEnum;
use wayland_client::backend::WaylandError;

use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 as VpManager,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1 as Vp,
};

use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 as VkManager,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1 as Vk,
};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};

use input_event::{Event, KeyboardEvent, PointerEvent};

use super::error::WaylandBindError;
use super::{EmulationHandle, ScreenEdge};

/// The layout rectangle of one output, in compositor coordinates.
#[derive(Debug, Clone, Copy, Default)]
struct OutputInfo {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

struct State {
    /// every output's layout rectangle, needed to place the cursor on the
    /// output the peer actually crossed into instead of the whole layout
    outputs: Vec<OutputInfo>,
    /// outputs being built: `wl_output` reports geometry, mode and scale in
    /// several events, `done` says the picture is complete
    pending_outputs: HashMap<WlOutput, OutputInfo>,
    keymap: Option<(u32, OwnedFd, u32)>,
    /// whether the seat already reported its capabilities
    capabilities_known: bool,
    /// whether the seat advertised a keyboard (and thus a keymap)
    keyboard_available: bool,
    /// shared by all clients, so the keymap fd is sent once instead of once per client
    keyboard: Option<Vk>,
    input_for_client: HashMap<EmulationHandle, VirtualInput>,
    seat: wl_seat::WlSeat,
    qh: QueueHandle<Self>,
    vpm: VpManager,
    vkm: VkManager,
}

// App State, implements Dispatch event handlers
pub(crate) struct WlrootsEmulation {
    last_flush_failed: bool,
    state: State,
    queue: EventQueue<State>,
}

impl WlrootsEmulation {
    pub(crate) fn new() -> Result<Self, WlrootsEmulationCreationError> {
        let conn = Connection::connect_to_env()?;
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();

        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 7..=8, ())
            .map_err(|e| WaylandBindError::new(e, "wl_seat 7..=8"))?;

        let vpm: VpManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wlr-virtual-pointer-unstable-v1"))?;
        let vkm: VkManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "virtual-keyboard-unstable-v1"))?;

        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" {
                let _ = globals.registry().bind::<WlOutput, _, _>(
                    global.name,
                    global.version.min(4),
                    &qh,
                    (),
                );
            }
        }

        let input_for_client: HashMap<EmulationHandle, VirtualInput> = HashMap::new();

        let mut emulate = WlrootsEmulation {
            last_flush_failed: false,
            state: State {
                outputs: Vec::new(),
                pending_outputs: HashMap::new(),
                keymap: None,
                capabilities_known: false,
                keyboard_available: false,
                keyboard: None,
                input_for_client,
                seat,
                vpm,
                vkm,
                qh,
            },
            queue,
        };
        /* The compositor answers the bind requests promptly, but a compositor
         * that never sends a keymap (e.g. a seat without keyboard capability)
         * used to hang the whole startup in an unconditional busy wait.
         */
        let deadline = Instant::now() + Duration::from_secs(5);
        while !emulate.state.capabilities_known {
            if Instant::now() > deadline {
                return Err(WlrootsEmulationCreationError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for the seat capabilities",
                )));
            }
            emulate.queue.blocking_dispatch(&mut emulate.state)?;
        }

        if emulate.state.keyboard_available {
            while emulate.state.keymap.is_none() {
                if Instant::now() > deadline {
                    return Err(WlrootsEmulationCreationError::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for the keyboard keymap",
                    )));
                }
                emulate.queue.blocking_dispatch(&mut emulate.state)?;
            }
        } else {
            log::warn!(
                "the seat does not advertise a keyboard: key events can not be emulated yet"
            );
        }
        /* The output rectangles arrive right after binding them; without this
         * the cursor placement falls back to the whole layout, which puts it
         * into dead space when the outputs are not a filled rectangle.
         */
        for _ in 0..8 {
            if !emulate.state.outputs.is_empty() {
                break;
            }
            emulate.queue.blocking_dispatch(&mut emulate.state)?;
        }
        if emulate.state.outputs.is_empty() {
            log::debug!("the compositor reported no outputs, the cursor is placed on the layout");
        }
        // let fd = unsafe { &File::from_raw_fd(emulate.state.keymap.unwrap().1.as_raw_fd()) };
        // let mmap = unsafe { MmapOptions::new().map_copy(fd).unwrap() };
        // log::debug!("{:?}", &mmap[..100]);
        Ok(emulate)
    }
}

impl State {
    fn add_client(&mut self, client: EmulationHandle) {
        let pointer: Vp = self.vpm.create_virtual_pointer(None, &self.qh, ());

        let keyboard = match self.keyboard.as_ref() {
            Some(keyboard) => keyboard.clone(),
            None => {
                /* A `zwp_virtual_keyboard_v1` must upload its own keymap: the
                 * protocol has no way to borrow the compositor's, so a key that
                 * the shipped keymap does not know cannot be emulated. The
                 * keymap comes from the seat's `wl_keyboard.keymap` event, which
                 * is the compositor's active layout, so in practice this matches
                 * what the user has configured. */
                let Some((format, fd, size)) = self.keymap.as_ref() else {
                    log::error!("cannot create virtual keyboard: no keymap available");
                    return;
                };
                let keyboard: Vk = self.vkm.create_virtual_keyboard(&self.seat, &self.qh, ());
                keyboard.keymap(*format, fd.as_fd(), *size);
                self.keyboard = Some(keyboard.clone());
                keyboard
            }
        };

        let vinput = VirtualInput {
            pointer,
            keyboard,
            modifiers: Arc::new(Mutex::new(XMods::empty())),
        };

        self.input_for_client.insert(client, vinput);
    }

    fn destroy_client(&mut self, handle: EmulationHandle) {
        // the shared keyboard outlives every client; keys are released by InputEmulation::destroy
        if let Some(input) = self.input_for_client.remove(&handle) {
            input.pointer.destroy();
        }
    }
}

impl State {
    /// The rectangle spanning every output. Absolute pointer motion is in
    /// these coordinates, so the layout is the coordinate space the cursor is
    /// placed in.
    fn layout(&self) -> Option<OutputInfo> {
        let first = *self.outputs.first()?;
        let mut layout = first;
        for output in &self.outputs[1..] {
            let right = layout.x + layout.width;
            let bottom = layout.y + layout.height;
            let out_right = output.x + output.width;
            let out_bottom = output.y + output.height;
            layout.x = layout.x.min(output.x);
            layout.y = layout.y.min(output.y);
            layout.width = right.max(out_right) - layout.x;
            layout.height = bottom.max(out_bottom) - layout.y;
        }
        Some(layout)
    }

    /// The output whose rectangle touches the layout boundary the peer came
    /// through, i.e. the one that is actually reachable over that edge.
    fn output_at_edge(&self, edge: ScreenEdge) -> Option<OutputInfo> {
        self.outputs
            .iter()
            .copied()
            .min_by_key(|output| match edge {
                ScreenEdge::Left => output.x,
                ScreenEdge::Right => -(output.x + output.width),
                ScreenEdge::Top => output.y,
                ScreenEdge::Bottom => -(output.y + output.height),
            })
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let output = state.pending_outputs.entry(proxy.clone()).or_default();
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                output.x = x;
                output.y = y;
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } => {
                // only the current mode describes the rectangle in use
                if flags.contains(wl_output::Mode::Current) {
                    output.width = width;
                    output.height = height;
                }
            }
            wl_output::Event::Done => {
                let done = *output;
                if done.width > 0 && done.height > 0 {
                    state.outputs.push(done);
                }
                state.pending_outputs.remove(proxy);
            }
            _ => {}
        }
    }
}

#[async_trait]
impl Emulation for WlrootsEmulation {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        if let Some(virtual_input) = self.state.input_for_client.get(&handle) {
            if self.last_flush_failed {
                match self.queue.flush() {
                    Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                        /*
                         * outgoing buffer is full - sending more events
                         * will overwhelm the output buffer and leave the
                         * wayland connection in a broken state
                         */
                        log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                        return Ok(());
                    }
                    Ok(()) => self.last_flush_failed = false,
                    // do not swallow real errors here: the connection may be
                    // gone and the caller has to tear the backend down
                    Err(e) => {
                        log::error!("failed to flush wayland connection: {e}");
                        return Err(e.into());
                    }
                }
            }
            virtual_input.consume_event(event)?;
            match self.queue.flush() {
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.last_flush_failed = true;
                    log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                }
                Ok(()) => self.last_flush_failed = false,
                Err(e) => {
                    log::error!("failed to flush wayland connection: {e}");
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    async fn enter(&mut self, handle: EmulationHandle, edge: ScreenEdge, fraction: f64) {
        if !self.state.input_for_client.contains_key(&handle) {
            log::debug!("no virtual pointer for handle {handle}, not placing the cursor");
            return;
        }
        /*
         * The pointer continues from wherever it was on this machine, which
         * looks like the cursor teleported when the user left the peer at the
         * top of its screen while this one had it parked at the bottom.
         * Absolute motion is the only way to place it without knowing its
         * current position.
         *
         * The point is placed on the output that touches the layout edge the
         * peer came through: with an irregular layout (say a second monitor
         * that hangs below the first) a fraction of the whole layout can land
         * in dead space, and the compositor then clamps it to whatever output
         * is closest, which is neither the expected monitor nor the expected
         * position.
         */
        let Some(layout) = self.state.layout() else {
            log::debug!("no output is known, not placing the cursor");
            return;
        };
        let target = self.state.output_at_edge(edge).unwrap_or(layout);
        /* Keep the cursor a few pixels inside: on the edge itself this side
         * immediately sees it leave the screen again and hands the pointer
         * straight back to the peer.
         */
        const INSET: f64 = 8.0;
        let (x, y) = edge.point_on_inset(
            fraction,
            target.x as f64,
            target.y as f64,
            (target.x + target.width - 1) as f64,
            (target.y + target.height - 1) as f64,
            INSET,
        );
        log::debug!(
            "placing the cursor at {x}/{y} (edge {edge:?}, fraction {fraction:.2}, \
             output {}x{}+{}+{})",
            target.width,
            target.height,
            target.x,
            target.y
        );
        self.absolute(handle, layout, x, y);
    }

    async fn enter_center(&mut self, handle: EmulationHandle) {
        let Some(layout) = self.state.layout() else {
            log::debug!("no output is known, not placing the cursor");
            return;
        };
        /* No crossing point to line up with: the middle of the layout is what
         * the person pressing "switch" expects to land on.
         */
        let x = layout.x as f64 + layout.width as f64 / 2.0;
        let y = layout.y as f64 + layout.height as f64 / 2.0;
        log::debug!(
            "placing the cursor at the centre ({x}, {y}) of the {}x{}+{}+{} layout",
            layout.width,
            layout.height,
            layout.x,
            layout.y
        );
        self.absolute(handle, layout, x, y);
    }

    async fn create(&mut self, handle: EmulationHandle) {
        self.state.add_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }
    async fn destroy(&mut self, handle: EmulationHandle) {
        self.state.destroy_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }
    async fn terminate(&mut self) {
        /* nothing to do */
    }
}

impl WlrootsEmulation {
    /// Move the virtual pointer to `(x, y)` of the layout, in one absolute
    /// motion event.
    ///
    /// `motion_absolute` is in per-mille of the whole layout. Strict wlroots
    /// compositors place the cursor exactly there; Hyprland is known to apply
    /// its pointer acceleration to these events instead, so the placement can
    /// be off there.
    fn absolute(&self, handle: EmulationHandle, layout: OutputInfo, x: f64, y: f64) {
        let Some(input) = self.state.input_for_client.get(&handle) else {
            log::debug!("no virtual pointer for handle {handle}, not placing the cursor");
            return;
        };
        static START: OnceLock<Instant> = OnceLock::new();
        let now: u32 = START
            .get_or_init(Instant::now)
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u32::MAX);
        const EXTENT: u32 = 1000;
        let rel_x = (x - layout.x as f64) / (layout.width.max(1) as f64);
        let rel_y = (y - layout.y as f64) / (layout.height.max(1) as f64);
        let target_x = (rel_x.clamp(0.0, 1.0) * EXTENT as f64) as u32;
        let target_y = (rel_y.clamp(0.0, 1.0) * EXTENT as f64) as u32;
        input
            .pointer
            .motion_absolute(now, target_x, target_y, EXTENT, EXTENT);
        input.pointer.frame();
        if let Err(e) = self.queue.flush() {
            log::error!("failed to position the cursor: {e}");
        }
    }
}

struct VirtualInput {
    pointer: Vp,
    keyboard: Vk,
    modifiers: Arc<Mutex<XMods>>,
}

/// Map the protocol axis index (0 = vertical, anything else = horizontal) to
/// the wayland enum, rejecting garbage from the network.
fn axis_from(axis: u8) -> Option<Axis> {
    match axis {
        0 => Some(Axis::VerticalScroll),
        1 => Some(Axis::HorizontalScroll),
        axis => {
            log::warn!("invalid scroll axis: {axis}");
            None
        }
    }
}

impl VirtualInput {
    fn consume_event(&self, event: Event) -> Result<(), EmulationError> {
        /* The peer's timestamps use its own clock and unit (libei sends
         * microseconds, windows and macos send 0), so they are useless here.
         * Clients compare the timestamps to detect double clicks, so a
         * monotonic millisecond clock fits much better than the wall clock
         * (which jumps on NTP updates and wraps every 49.7 days).
         */
        static START: OnceLock<Instant> = OnceLock::new();
        let now: u32 = START
            .get_or_init(Instant::now)
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u32::MAX);

        match event {
            Event::Pointer(e) => {
                match e {
                    PointerEvent::AxisStop { axis, .. } => {
                        let Some(axis) = axis_from(axis) else {
                            return Ok(());
                        };
                        self.pointer.axis_stop(now, axis);
                    }
                    PointerEvent::AxisCancel { axis, .. } => {
                        /* wl_pointer has no cancel request; stopping the axis
                         * is the closest thing and also ends kinetic scroll */
                        log::debug!("scroll cancelled on axis {axis}, stopping it instead");
                        let Some(axis) = axis_from(axis) else {
                            return Ok(());
                        };
                        self.pointer.axis_stop(now, axis);
                    }
                    PointerEvent::Motion { dx, dy, .. } => self.pointer.motion(now, dx, dy),
                    PointerEvent::Button { button, state, .. } => {
                        // the event comes from the network, so the state can be
                        // garbage instead of 0/1/2
                        let Ok(state): Result<ButtonState, _> = state.try_into() else {
                            log::warn!("invalid button state: {state}");
                            return Ok(());
                        };
                        self.pointer.button(now, button, state);
                    }
                    PointerEvent::Axis { axis, value, .. } => {
                        let Ok(axis): Result<Axis, _> = (axis as u32).try_into() else {
                            log::warn!("invalid axis: {axis}");
                            return Ok(());
                        };
                        self.pointer.axis(now, axis, value);
                        self.pointer.axis_source(AxisSource::Continuous);
                    }
                    PointerEvent::AxisDiscrete120 { axis, value } => {
                        let Ok(axis): Result<Axis, _> = (axis as u32).try_into() else {
                            log::warn!("invalid axis: {axis}");
                            return Ok(());
                        };
                        // `axis_discrete` takes the scroll amount in units of
                        // 120 (one wheel notch) and `axis` the same amount in
                        // surface units (10 per notch)
                        let notches = value as f64 / 120.;
                        self.pointer.axis_discrete(now, axis, notches, value / 120);
                        self.pointer.axis(now, axis, notches * 10.);
                        self.pointer.axis_source(AxisSource::Wheel);
                    }
                }
                self.pointer.frame();
            }
            Event::Keyboard(e) => match e {
                KeyboardEvent::Key { key, state, .. } => {
                    // Only the key itself: the compositor's keymap derives
                    // both the depressed and the locked (CapsLock / NumLock)
                    // modifier state from it. Pushing our own guess here made
                    // lock keys toggle twice — NumLock turned itself back off
                    // and the numpad kept typing arrows.
                    self.keyboard.key(now, key, state as u32);
                }
                KeyboardEvent::Modifiers {
                    depressed: mods_depressed,
                    latched: mods_latched,
                    locked: mods_locked,
                    group,
                } => {
                    // Synchronize internal modifier state, assuming server is authoritative
                    if let Ok(mut mods) = self.modifiers.lock() {
                        mods.update_by_mods_event(e);
                    }
                    self.keyboard
                        .modifiers(mods_depressed, mods_latched, mods_locked, group);
                }
            },
        }
        Ok(())
    }
}

delegate_noop!(State: Vp);
delegate_noop!(State: Vk);
delegate_noop!(State: VpManager);
delegate_noop!(State: VkManager);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut State,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            state.keymap = Some((u32::from(format), fd, size));
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            state.capabilities_known = true;
            state.keyboard_available = capabilities.contains(wl_seat::Capability::Keyboard);
            if state.keyboard_available {
                seat.get_keyboard(qhandle, ());
            }
        }
    }
}

// From X11/X.h
bitflags! {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct XMods: u32 {
        const ShiftMask = (1<<0);
        const LockMask = (1<<1);
        const ControlMask = (1<<2);
        const Mod1Mask = (1<<3);
        const Mod2Mask = (1<<4);
        const Mod3Mask = (1<<5);
        const Mod4Mask = (1<<6);
        const Mod5Mask = (1<<7);
    }
}

impl XMods {
    fn update_by_mods_event(&mut self, evt: KeyboardEvent) {
        if let KeyboardEvent::Modifiers {
            depressed, locked, ..
        } = evt
        {
            *self = XMods::from_bits_truncate(depressed) | XMods::from_bits_truncate(locked);
        }
    }
}
