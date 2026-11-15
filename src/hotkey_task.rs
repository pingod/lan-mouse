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

#[cfg(unix)]
use crate::connect::{LanMouseConnection, LanMouseConnectionError};
#[cfg(unix)]
use crate::hotkey;

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) use imp::Hotkeys;

#[cfg(target_os = "macos")]
pub(crate) use imp_macos::Hotkeys;

#[cfg(not(unix))]
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
/// on macOS). Windows reaches this type only through the no-op `Hotkeys`, so
/// the variants are unused there.
#[cfg_attr(not(unix), allow(dead_code))]
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
#[cfg(unix)]
const CONNECT_ATTEMPTS: usize = 20;
#[cfg(unix)]
const CONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Type a hotkey's key sequence on the peer.
///
/// Shared by the evdev (Linux) and Quartz (macOS) listeners: the trigger comes
/// from a different source on each platform, but delivering it is identical.
/// Windows has no listener yet, so it does not compile this.
#[cfg(unix)]
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
            let devices = hotkey::open_keyboards();
            if devices.is_empty() {
                log::warn!(
                    "hotkeys configured but no readable keyboard found; \
                     add this user to the `input` group and re-login"
                );
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

#[cfg(not(unix))]
mod noop {
    use crate::{config::Config, connect::LanMouseConnection};

    use super::HotkeyFire;

    /// Placeholder for platforms without an evdev listener.
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
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult, EventField,
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
        // Modifier keys arrive as `FlagsChanged` without a press/release flag,
        // so remember which of them we last reported as held.
        let held: RefCell<HashSet<u16>> = RefCell::new(HashSet::new());
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
            match event_type {
                CGEventType::KeyDown | CGEventType::KeyUp => {
                    if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA)
                        == INJECTED_EVENT_TAG
                    {
                        return CallbackResult::Keep;
                    }
                    let keycode =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    let Some(key) = evdev_key(keycode) else {
                        return CallbackResult::Keep;
                    };
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
                    let _ = key_tx.send(KeySample { key, value });
                }
                CGEventType::FlagsChanged => {
                    if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA)
                        == INJECTED_EVENT_TAG
                    {
                        return CallbackResult::Keep;
                    }
                    let keycode =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    let Some(key) = evdev_key(keycode) else {
                        return CallbackResult::Keep;
                    };
                    let value = match held.try_borrow_mut() {
                        Ok(mut held) => {
                            if held.remove(&keycode) {
                                0
                            } else {
                                held.insert(keycode);
                                1
                            }
                        }
                        Err(_) => return CallbackResult::Keep,
                    };
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
            CGEventTapLocation::HID,
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
