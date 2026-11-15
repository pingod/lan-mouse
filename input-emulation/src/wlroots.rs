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

use input_event::{Event, KeyboardEvent, PointerEvent, scancode};

use super::EmulationHandle;
use super::error::WaylandBindError;

struct State {
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

        let input_for_client: HashMap<EmulationHandle, VirtualInput> = HashMap::new();

        let mut emulate = WlrootsEmulation {
            last_flush_failed: false,
            state: State {
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
                // TODO: use server side keymap
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

struct VirtualInput {
    pointer: Vp,
    keyboard: Vk,
    modifiers: Arc<Mutex<XMods>>,
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
                    self.keyboard.key(now, key, state as u32);
                    if let Ok(mut mods) = self.modifiers.lock() {
                        if mods.update_by_key_event(key, state) {
                            log::trace!("Key triggers modifier change: {mods:?}");
                            self.keyboard.modifiers(
                                mods.mask_pressed().bits(),
                                0,
                                mods.mask_locks().bits(),
                                0,
                            );
                        }
                    }
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

    fn update_by_key_event(&mut self, key: u32, state: u8) -> bool {
        if let Ok(key) = scancode::Linux::try_from(key) {
            log::trace!("Attempting to process modifier from: {key:#?}");
            let pressed_mask = match key {
                scancode::Linux::KeyLeftShift | scancode::Linux::KeyRightShift => XMods::ShiftMask,
                scancode::Linux::KeyLeftCtrl | scancode::Linux::KeyRightCtrl => XMods::ControlMask,
                // Right Alt is AltGr (ISO_Level3_Shift), which xkb places in
                // Mod5 — not Mod1. Mapping it to Mod1 made AltGr+Q produce
                // "q" instead of "@" on layouts that need level 3 (issue
                // #486, #408).
                scancode::Linux::KeyLeftAlt => XMods::Mod1Mask,
                scancode::Linux::KeyRightalt => XMods::Mod5Mask,
                scancode::Linux::KeyLeftMeta | scancode::Linux::KeyRightmeta => XMods::Mod4Mask,
                _ => XMods::empty(),
            };

            let locked_mask = match key {
                scancode::Linux::KeyCapsLock => XMods::LockMask,
                scancode::Linux::KeyNumlock => XMods::Mod2Mask,
                scancode::Linux::KeyScrollLock => XMods::Mod3Mask,
                _ => XMods::empty(),
            };

            // unchanged
            if pressed_mask.is_empty() && locked_mask.is_empty() {
                log::trace!("{key:#?} is not a modifier key");
                return false;
            }
            match state {
                1 => {
                    self.insert(pressed_mask);
                    // lock keys toggle when they go down, e.g. CapsLock must
                    // already be locked for the character typed after it
                    self.toggle(locked_mask);
                }
                _ => self.remove(pressed_mask),
            }
            true
        } else {
            false
        }
    }

    fn mask_locks(&self) -> XMods {
        *self & (XMods::LockMask | XMods::Mod2Mask | XMods::Mod3Mask)
    }

    fn mask_pressed(&self) -> XMods {
        *self & (XMods::ShiftMask | XMods::ControlMask | XMods::Mod1Mask | XMods::Mod4Mask)
    }
}
