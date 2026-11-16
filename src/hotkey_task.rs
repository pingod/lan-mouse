//! Global hotkeys that fire while the pointer stays on this machine.
//!
//! The capture-based `[[remap]]` path only sees input after a barrier has been
//! crossed, so it cannot serve a hotkey: that has to work while the user is
//! still typing on their own machine.
//!
//! [`Hotkeys`] is deliberately platform-neutral: the service references the
//! field from its `select!` unconditionally, so the type has to exist
//! everywhere. Linux reads evdev devices, macOS a passive Quartz event tap,
//! and anything else falls back to a listener that never fires.

#[cfg(any(unix, windows))]
use crate::connect::{LanMouseConnection, LanMouseConnectionError};
#[cfg(any(unix, windows))]
use crate::hotkey;

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) use imp::Hotkeys;

#[cfg(target_os = "macos")]
pub(crate) use imp_macos::Hotkeys;

#[cfg(windows)]
pub(crate) use imp_windows::Hotkeys;

#[cfg(not(any(unix, windows)))]
pub(crate) use noop::Hotkeys;

/// A global hotkey rule: pressing every key in `from` sends `to` to the peer.
///
/// Defined here rather than in `hotkey` because the config and the GUI need it
/// on every platform, while the evdev listener does not exist off Linux.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hotkey {
    pub from: Vec<u32>,
    pub to: Vec<u32>,
    /// Run a local action instead of typing `to` on the peer.
    pub action: Option<HotkeyAction>,
    /// Number of presses required. `1` is a chord; `2` means double-tap.
    pub taps: u8,
}

/// What a hotkey trigger produced.
///
/// Constructed by the platform listener (evdev on Linux, the Quartz event tap
/// on macOS, the low level keyboard hook on Windows).
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HotkeyFire {
    /// Type these keys on the peer.
    Keys(Vec<u32>),
    /// Run a local action.
    Action(HotkeyAction),
}

/// What a hotkey does locally instead of sending keystrokes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HotkeyAction {
    /// Hand the pointer back to this machine, or forward it to the next peer.
    SwitchDevice,
    /// Release the pointer without moving it.
    ForceRelease,
}

/// How long to wait for an on-demand connection before giving up on a hotkey.
#[cfg(any(unix, windows))]
const CONNECT_ATTEMPTS: usize = 20;
#[cfg(any(unix, windows))]
const CONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Type a hotkey's key sequence on the peer.
///
/// Shared by the evdev (Linux), Quartz (macOS) and low level hook (Windows)
/// listeners: the trigger comes from a different source on each platform, but
/// delivering it is identical.
#[cfg(any(unix, windows))]
pub(crate) async fn deliver(conn: &LanMouseConnection, fire: HotkeyFire, capture_active: bool) {
    let HotkeyFire::Keys(keys) = fire else {
        // Local actions are handled by the service, which owns the capture and
        // client state.
        return;
    };
    if capture_active {
        log::trace!("hotkey suppressed: capture is forwarding input");
        return;
    }
    let Some(handle) = conn.first_active_client() else {
        log::debug!("hotkey fired but no active client to receive it");
        return;
    };

    // A hotkey is usually the first thing to touch the peer when the pointer
    // never left this machine, so the connection does not exist yet. `send`
    // starts it in the background and reports `NotConnected`; retry briefly so
    // the first press is not lost.
    let events = hotkey::tap_events(&keys);
    for attempt in 0..CONNECT_ATTEMPTS {
        let mut retry = false;
        for event in &events {
            match conn
                .send(lan_mouse_proto::ProtoEvent::Input(*event), handle)
                .await
            {
                Ok(()) => {}
                Err(LanMouseConnectionError::NotConnected) => {
                    retry = true;
                    break;
                }
                Err(e) => {
                    log::debug!("hotkey: could not deliver event: {e}");
                    return;
                }
            }
        }
        if !retry {
            log::info!("hotkey fired: sent {keys:?} to client {handle}");
            return;
        }
        if attempt + 1 < CONNECT_ATTEMPTS {
            tokio::time::sleep(CONNECT_DELAY).await;
        }
    }
    log::warn!("hotkey fired but the connection to client {handle} did not come up");
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::{path::PathBuf, sync::Arc};

    use tokio::io::unix::AsyncFd;
    use tokio::sync::mpsc;

    use super::{Hotkey, HotkeyFire};
    use crate::{
        config::Config,
        connect::LanMouseConnection,
        hotkey::{self, HotkeyState, KeySample},
    };

    pub(crate) struct Hotkeys {
        state: HotkeyState,
        /// one reader task per keyboard device, forwarding raw key samples
        key_rx: mpsc::UnboundedReceiver<KeySample>,
        readers: Vec<tokio::task::JoinHandle<()>>,
        conn: LanMouseConnection,
    }

    impl Hotkeys {
        pub(crate) fn new(config: &Config, conn: LanMouseConnection) -> Self {
            let hotkeys = config.hotkeys();
            let (key_tx, key_rx) = mpsc::unbounded_channel();
            let readers = if hotkeys.is_empty() {
                Vec::new()
            } else {
                Self::spawn_readers(key_tx)
            };
            Self {
                state: HotkeyState::new(hotkeys),
                key_rx,
                readers,
                conn,
            }
        }

        fn spawn_readers(
            key_tx: mpsc::UnboundedSender<KeySample>,
        ) -> Vec<tokio::task::JoinHandle<()>> {
            let (devices, access) = hotkey::open_keyboards();
            if let Some(advice) = access.advice() {
                /* A warning, not an error: hotkeys are one feature and the rest
                 * of the daemon works without them. The message names the fix
                 * that applies to the reason that was actually detected - the
                 * old text told everybody to change their group membership, which
                 * is the wrong advice on a desktop that only ever granted
                 * /dev/uinput. */
                log::warn!("hotkeys are inactive: {advice}");
                return Vec::new();
            }
            log::info!("hotkeys: listening on {} keyboard device(s)", devices.len());

            devices
                .into_iter()
                .map(|(path, dev)| tokio::task::spawn_local(read_device(path, dev, key_tx.clone())))
                .collect()
        }

        /// Replace the rules from the GUI.
        pub(crate) fn set_hotkeys(&mut self, hotkeys: Vec<Hotkey>) {
            let was_empty = self.state.is_empty();
            self.state.set_hotkeys(hotkeys.clone());
            if was_empty && !hotkeys.is_empty() && self.readers.is_empty() {
                let (key_tx, key_rx) = mpsc::unbounded_channel();
                self.key_rx = key_rx;
                self.readers = Self::spawn_readers(key_tx);
            }
        }

        pub(crate) fn reload(&mut self, config: &Config) {
            let hotkeys = config.hotkeys();
            let was_empty = self.state.is_empty();
            self.state.set_hotkeys(hotkeys.clone());
            if was_empty && !hotkeys.is_empty() && self.readers.is_empty() {
                let (key_tx, key_rx) = mpsc::unbounded_channel();
                self.key_rx = key_rx;
                self.readers = Self::spawn_readers(key_tx);
            }
        }

        /// Wait for the next hotkey trigger.
        ///
        /// Returns the action when the rule is local, otherwise the key
        /// sequence to type on the peer.
        pub(crate) async fn next(&mut self) -> HotkeyFire {
            loop {
                let Some(sample) = self.key_rx.recv().await else {
                    // every device closed; nothing left to watch
                    std::future::pending::<()>().await;
                    unreachable!()
                };
                if let Some(fire) = self.state.on_key(sample) {
                    return fire;
                }
            }
        }

        /// `capture_active` reports whether the pointer is currently captured
        /// by a remote client. While it is, the capture path already forwards
        /// every keystroke, so firing here would send the chord twice.
        pub(crate) async fn send(&self, fire: HotkeyFire, capture_active: bool) {
            super::deliver(&self.conn, fire, capture_active).await;
        }
    }

    /// Read key samples from one device forever.
    async fn read_device(
        path: PathBuf,
        dev: AsyncFd<std::fs::File>,
        key_tx: mpsc::UnboundedSender<KeySample>,
    ) {
        let dev = Arc::new(dev);
        let mut buf = [0u8; hotkey::EVENT_SIZE * 64];
        loop {
            // `readable()` resolves when the fd has data; `try_io` then performs
            // the read and re-arms readiness automatically. Returning `Err` from
            // the closure means "would block" — the only expected non-event.
            let mut guard = match dev.readable().await {
                Ok(g) => g,
                Err(e) => {
                    log::warn!("hotkeys: {} unreadable: {e}", path.display());
                    return;
                }
            };

            let read = guard.try_io(|inner| {
                use std::io::Read;
                inner.get_ref().read(&mut buf)
            });

            match read {
                // would block: readiness was a false positive, just retry
                Err(_) => continue,
                Ok(Ok(0)) => return,
                Ok(Ok(n)) => {
                    let mut closed = false;
                    hotkey::parse_events(&buf[..n], |s| {
                        if key_tx.send(s).is_err() {
                            closed = true;
                        }
                    });
                    if closed {
                        return;
                    }
                }
                Ok(Err(e)) => {
                    log::warn!("hotkeys: read error on {}: {e}", path.display());
                    return;
                }
            }
        }
    }
}

#[cfg(windows)]
mod imp_windows {
    use std::sync::Mutex;

    use tokio::sync::mpsc;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
        LLKHF_INJECTED, MSG, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    use input_event::scancode;

    use super::{Hotkey, HotkeyFire};
    use crate::config::Config;
    use crate::connect::LanMouseConnection;
    use crate::hotkey::{HotkeyState, KeySample};

    /// Where the hook callback hands its key samples to.
    ///
    /// A `WH_KEYBOARD_LL` callback runs on the thread that installed the hook and
    /// cannot capture environment, so the sender lives in a static.
    static KEY_TX: Mutex<Option<mpsc::UnboundedSender<KeySample>>> = Mutex::new(None);

    pub(crate) struct Hotkeys {
        key_rx: mpsc::UnboundedReceiver<KeySample>,
        state: HotkeyState,
        conn: LanMouseConnection,
    }

    impl Hotkeys {
        pub(crate) fn new(config: &Config, conn: LanMouseConnection) -> Self {
            let (key_tx, key_rx) = mpsc::unbounded_channel();
            spawn_listener(key_tx);
            Self {
                key_rx,
                state: HotkeyState::new(config.hotkeys()),
                conn,
            }
        }

        pub(crate) fn set_hotkeys(&mut self, hotkeys: Vec<Hotkey>) {
            log::debug!("hotkeys: {} rule(s) armed", hotkeys.len());
            self.state.set_hotkeys(hotkeys);
        }

        pub(crate) fn reload(&mut self, config: &Config) {
            self.state.set_hotkeys(config.hotkeys());
        }

        /// Wait for the next hotkey trigger.
        pub(crate) async fn next(&mut self) -> HotkeyFire {
            loop {
                let Some(sample) = self.key_rx.recv().await else {
                    // The hook thread exited; keep the service select loop alive
                    // instead of spinning on a closed channel.
                    log::debug!("hotkeys: the keyboard hook is gone");
                    std::future::pending::<()>().await;
                    unreachable!()
                };
                if let Some(fire) = self.state.on_key(sample) {
                    return fire;
                }
            }
        }

        pub(crate) async fn send(&self, fire: HotkeyFire, capture_active: bool) {
            super::deliver(&self.conn, fire, capture_active).await;
        }
    }

    /// Install the passive low level keyboard hook in its own thread.
    ///
    /// The hook only observes: it always chains to the next hook and never
    /// reports the key as handled, so the local application still receives the
    /// keystroke (that is what makes `[[hotkey]]` different from `[[remap]]`).
    fn spawn_listener(key_tx: mpsc::UnboundedSender<KeySample>) {
        if let Ok(mut slot) = KEY_TX.lock() {
            *slot = Some(key_tx);
        }
        let spawned = std::thread::Builder::new()
            .name("windows-hotkeys".to_owned())
            .spawn(move || {
                let hook =
                    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) }
                    {
                        Ok(hook) => hook,
                        Err(e) => {
                            log::warn!(
                                "hotkeys: could not install the keyboard hook ({e}), global                                  hotkeys are disabled"
                            );
                            return;
                        }
                    };
                log::info!("hotkeys: listening on the low level keyboard hook");
                /* Low level hooks only fire on a thread that pumps messages. */
                let mut msg = MSG::default();
                while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                    let _ = unsafe { TranslateMessage(&msg) };
                    unsafe { DispatchMessageW(&msg) };
                }
                let _ = unsafe { UnhookWindowsHookEx(hook) };
            });
        if let Err(e) = spawned {
            log::warn!("hotkeys: could not spawn the hook thread: {e}");
        }
    }

    unsafe extern "system" fn keyboard_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if ncode >= 0 {
            /* SAFETY: for `HC_ACTION` the system passes a `KBDLLHOOKSTRUCT`. */
            let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
            /* Keys we injected ourselves (the emulation backend forwards what the
             * peer typed) must not re-trigger a local hotkey. */
            if !info.flags.contains(LLKHF_INJECTED) {
                if let Some(sample) = sample_of(wparam, &info) {
                    if let Ok(slot) = KEY_TX.lock() {
                        if let Some(tx) = slot.as_ref() {
                            let _ = tx.send(sample);
                        }
                    }
                }
            }
        }
        unsafe { CallNextHookEx(None, ncode, wparam, lparam) }
    }

    fn sample_of(wparam: WPARAM, info: &KBDLLHOOKSTRUCT) -> Option<KeySample> {
        let value = match wparam.0 as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => 1,
            WM_KEYUP | WM_SYSKEYUP => 0,
            _ => return None,
        };
        let mut scan_code = info.scanCode;
        if info.flags.contains(LLKHF_EXTENDED) {
            scan_code |= 0xE000;
        }
        let win_scan_code = scancode::Windows::try_from(scan_code).ok()?;
        let Ok(key): Result<scancode::Linux, ()> = win_scan_code.try_into() else {
            log::trace!("hotkeys: no linux key code for {win_scan_code:?}");
            return None;
        };
        Some(KeySample {
            key: key as u32,
            value,
        })
    }
}

#[cfg(not(any(unix, windows)))]
mod noop {
    use crate::{config::Config, connect::LanMouseConnection};

    use super::HotkeyFire;

    /// Placeholder for platforms without a keyboard listener.
    ///
    /// The service references this field from its `select!` unconditionally, so
    /// the type has to exist everywhere; here it simply never resolves.
    pub(crate) struct Hotkeys;

    impl Hotkeys {
        pub(crate) fn new(_config: &Config, _conn: LanMouseConnection) -> Self {
            Self
        }

        pub(crate) fn set_hotkeys(&mut self, _hotkeys: Vec<super::Hotkey>) {}

        pub(crate) fn reload(&mut self, _config: &Config) {}

        pub(crate) async fn next(&mut self) -> HotkeyFire {
            std::future::pending::<()>().await;
            unreachable!()
        }

        pub(crate) async fn send(&self, _fire: HotkeyFire, _capture_active: bool) {}
    }
}

/// Global hotkeys on macOS.
///
/// Linux reads raw evdev devices; macOS has no evdev equivalent, so a passive
/// (`ListenOnly`) Quartz event tap does the same job. It sees every keystroke
/// system-wide without grabbing anything, so the local machine still receives
/// the keys while the hotkey additionally types its sequence on the peer. The
/// tap needs the Accessibility grant the capture backend already holds.
#[cfg(target_os = "macos")]
mod imp_macos {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::sync::{Arc, OnceLock};

    use core_foundation::base::TCFType;
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
    use core_graphics::event::{
        CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventType, CallbackResult, EventField,
    };
    use keycode::{KeyMap, KeyMapping};
    use tokio::sync::mpsc;

    use super::{Hotkey, HotkeyFire};
    use crate::config::Config;
    use crate::connect::LanMouseConnection;
    use crate::hotkey::{HotkeyState, KeySample};

    /// `CGEventSourceUserData` value `input-emulation` stamps on every
    /// synthesized event (see `input_emulation::macos::INJECTED_EVENT_TAG`).
    ///
    /// Keys the peer typed must not re-trigger a local hotkey: two machines
    /// forwarding the same chord to each other would replay it forever.
    const INJECTED_EVENT_TAG: i64 = 0x6c616e6d; // "lanm"

    extern "C" {
        /// Re-enable a tap the kernel disabled, e.g. after a callback timeout.
        fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
    }

    pub(crate) struct Hotkeys {
        key_rx: mpsc::UnboundedReceiver<KeySample>,
        state: HotkeyState,
        conn: LanMouseConnection,
    }

    impl Hotkeys {
        pub(crate) fn new(config: &Config, conn: LanMouseConnection) -> Self {
            let (key_tx, key_rx) = mpsc::unbounded_channel();
            spawn_listener(key_tx);
            Self {
                key_rx,
                state: HotkeyState::new(config.hotkeys()),
                conn,
            }
        }

        pub(crate) fn set_hotkeys(&mut self, hotkeys: Vec<Hotkey>) {
            log::debug!("hotkeys: {} rule(s) armed", hotkeys.len());
            self.state.set_hotkeys(hotkeys);
        }

        pub(crate) fn reload(&mut self, config: &Config) {
            self.state.set_hotkeys(config.hotkeys());
        }

        /// Wait for the next hotkey trigger.
        pub(crate) async fn next(&mut self) -> HotkeyFire {
            loop {
                let Some(sample) = self.key_rx.recv().await else {
                    // The tap thread exited (permission denied at startup?);
                    // keep the service select loop alive instead of spinning.
                    log::debug!("hotkeys: the event tap is gone");
                    std::future::pending::<()>().await;
                    unreachable!()
                };
                if let Some(fire) = self.state.on_key(sample) {
                    return fire;
                }
            }
        }

        pub(crate) async fn send(&self, fire: HotkeyFire, capture_active: bool) {
            super::deliver(&self.conn, fire, capture_active).await;
        }
    }

    /// Convert a macOS virtual key code into the evdev code the matching logic
    /// and the wire protocol use.
    /// evdev key codes of the modifiers we track, with the flag that reports
    /// them. Only the left variants exist on the wire: `HotkeyState` matches
    /// either side, and macOS reports both with the same flag.
    const MODIFIER_KEYS: [(u64, u32); 4] = [
        (CGEventFlags::CGEventFlagShift.bits(), 42), // KeyLeftShift
        (CGEventFlags::CGEventFlagControl.bits(), 29), // KeyLeftCtrl
        (CGEventFlags::CGEventFlagAlternate.bits(), 56), // KeyLeftAlt
        (CGEventFlags::CGEventFlagCommand.bits(), 125), // KeyLeftMeta
    ];

    fn is_modifier(key: u32) -> bool {
        MODIFIER_KEYS.iter().any(|(_, modifier)| *modifier == key)
    }

    /// Emit press/release samples for every modifier whose state changed.
    fn sync_modifiers(
        event: &core_graphics::event::CGEvent,
        held: &RefCell<HashSet<u32>>,
        key_tx: &mpsc::UnboundedSender<KeySample>,
    ) {
        let flags = event.get_flags().bits();
        let Ok(mut held) = held.try_borrow_mut() else {
            return;
        };
        for (mask, key) in MODIFIER_KEYS {
            let is_down = flags & mask != 0;
            match (is_down, held.contains(&key)) {
                (true, false) => {
                    held.insert(key);
                    log::trace!("hotkeys: sample {key} 1");
                    let _ = key_tx.send(KeySample { key, value: 1 });
                }
                (false, true) => {
                    held.remove(&key);
                    log::trace!("hotkeys: sample {key} 0");
                    let _ = key_tx.send(KeySample { key, value: 0 });
                }
                _ => {}
            }
        }
    }

    fn evdev_key(keycode: u16) -> Option<u32> {
        match KeyMap::from_key_mapping(KeyMapping::Mac(keycode)) {
            Ok(map) => Some(u32::from(map.evdev)),
            Err(_) => {
                log::trace!("hotkeys: no evdev mapping for macOS key code {keycode}");
                None
            }
        }
    }

    fn spawn_listener(key_tx: mpsc::UnboundedSender<KeySample>) {
        let spawned = std::thread::Builder::new()
            .name("macos-hotkeys".into())
            .spawn(move || run_tap(key_tx));
        if let Err(e) = spawned {
            log::error!("hotkeys: could not start the event tap thread: {e}");
        }
    }

    fn run_tap(key_tx: mpsc::UnboundedSender<KeySample>) {
        // Modifiers we last reported as held, so a change in the event flags
        // can be turned into press/release samples.
        let held: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
        // Mach port of the tap, for re-enabling it from inside the callback.
        let tap_port: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
        let tap_port_cb = Arc::clone(&tap_port);

        let events_of_interest = vec![
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::TapDisabledByTimeout,
            CGEventType::TapDisabledByUserInput,
        ];

        let callback = move |_proxy,
                             event_type: CGEventType,
                             event: &core_graphics::event::CGEvent| {
            // Runs on the tap thread: never block and never panic, or the
            // kernel disables the tap (and `panic = "abort"` kills the daemon).
            log::trace!("hotkeys: tap event {event_type:?}");
            match event_type {
                CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
                    if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA)
                        == INJECTED_EVENT_TAG
                    {
                        return CallbackResult::Keep;
                    }
                    // Modifiers come from the event flags rather than from the
                    // `FlagsChanged` key codes: the flags are set correctly on
                    // every key event (also for synthesized ones) and they tell
                    // press and release apart, which `FlagsChanged` does not.
                    sync_modifiers(event, &held, &key_tx);
                    if matches!(event_type, CGEventType::FlagsChanged) {
                        return CallbackResult::Keep;
                    }
                    let keycode =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    let Some(key) = evdev_key(keycode) else {
                        log::trace!("hotkeys: ignoring unmapped keycode {keycode}");
                        return CallbackResult::Keep;
                    };
                    // a modifier key event is already covered by the flags
                    if is_modifier(key) {
                        return CallbackResult::Keep;
                    }
                    let value = if matches!(event_type, CGEventType::KeyDown) {
                        // 1 is a press, 2 is autorepeat; only a press fires.
                        if event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0
                        {
                            2
                        } else {
                            1
                        }
                    } else {
                        0
                    };
                    log::trace!("hotkeys: sample {key} {value}");
                    let _ = key_tx.send(KeySample { key, value });
                }
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                    let port = tap_port_cb.get().copied().unwrap_or_default();
                    if port == 0 {
                        log::warn!("hotkeys: the event tap was disabled");
                    } else {
                        log::info!("hotkeys: re-enabling the event tap");
                        unsafe { CGEventTapEnable(port as *mut std::ffi::c_void, true) };
                    }
                }
                _ => {}
            }
            CallbackResult::Keep
        };

        match CGEventTap::new(
            // `Session` is the level the capture backend listens at; a HID
            // level tap is created but never delivers here, which is why this
            // listener stayed silent.
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            events_of_interest,
            callback,
        ) {
            Ok(tap) => {
                // `CGEventTap::new` only creates the port: the tap stays
                // disabled and does nothing until its run loop source is added
                // to a loop and it is explicitly enabled. Without these two
                // steps `run_current` returns at once (the loop has no
                // sources) and the whole listener is dead.
                let source = match tap.mach_port().create_runloop_source(0) {
                    Ok(source) => source,
                    Err(_) => {
                        log::warn!("hotkeys: could not create a run loop source for the event tap");
                        return;
                    }
                };
                unsafe {
                    CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes);
                }
                // The port is needed by the callback to undo a timeout disable.
                let _ = tap_port.set(tap.mach_port().as_concrete_TypeRef() as usize);
                // `CGEventTap::new` leaves the tap disabled: without this it
                // never delivers a single event.
                tap.enable();
                log::info!("hotkeys: listening on the macOS event tap");
                // `tap` must outlive the loop: dropping it invalidates the port.
                CFRunLoop::run_current();
                drop(tap);
            }
            Err(()) => log::warn!(
                "hotkeys: could not create the macOS event tap \
                 (Accessibility permission missing?), global hotkeys are disabled"
            ),
        }
    }
}
