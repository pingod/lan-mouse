//! Global hotkeys that fire while the pointer stays on this machine.
//!
//! The capture-based `[[remap]]` path only sees input after a barrier has been
//! crossed, so it cannot serve a hotkey: that has to work while the user is
//! still typing on their own machine.
//!
//! [`Hotkeys`] is deliberately platform-neutral. Only Linux has an evdev
//! listener; everywhere else the same type exists but never fires, which keeps
//! the service free of `#[cfg]` around a field that the `select!` loop
//! references unconditionally.

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) use imp::Hotkeys;

#[cfg(not(all(unix, not(target_os = "macos"))))]
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
/// Constructed by the evdev matcher, which only exists on unix; Windows and
/// macOS reach it through the no-op `Hotkeys`, so the variants are unused there.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
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

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::{path::PathBuf, sync::Arc};

    use tokio::io::unix::AsyncFd;
    use tokio::sync::mpsc;

    use super::{Hotkey, HotkeyFire};
    use crate::{
        config::Config,
        connect::{LanMouseConnection, LanMouseConnectionError},
        hotkey::{self, HotkeyState, KeySample},
    };

    /// How long to wait for an on-demand connection before giving up on a
    /// hotkey.
    const CONNECT_ATTEMPTS: usize = 20;
    const CONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

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
            let HotkeyFire::Keys(keys) = fire else {
                // Local actions are handled by the service, which owns the
                // capture and client state.
                return;
            };
            if capture_active {
                log::trace!("hotkey suppressed: capture is forwarding input");
                return;
            }
            let Some(handle) = self.conn.first_active_client() else {
                log::debug!("hotkey fired but no active client to receive it");
                return;
            };

            // A hotkey is usually the first thing to touch the peer when the
            // pointer never left this machine, so the connection does not exist
            // yet. `send` starts it in the background and reports
            // `NotConnected`; retry briefly so the first press is not lost.
            let events = hotkey::tap_events(&keys);
            for attempt in 0..CONNECT_ATTEMPTS {
                let mut retry = false;
                for event in &events {
                    match self
                        .conn
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

#[cfg(not(all(unix, not(target_os = "macos"))))]
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
