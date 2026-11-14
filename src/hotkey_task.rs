//! Runs the evdev hotkey listener as a background task.
//!
//! Kept separate from `capture` because hotkeys must work while the pointer is
//! still local: the capture backend only sees input after a barrier has been
//! crossed.

use std::{path::PathBuf, sync::Arc};

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

use crate::{
    connect::{LanMouseConnection, LanMouseConnectionError},
    hotkey::{self, Hotkey, HotkeyState, KeySample},
};

/// How long to wait for an on-demand connection before giving up on a hotkey.
const HOTKEY_CONNECT_ATTEMPTS: usize = 20;
const HOTKEY_CONNECT_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Which client receives hotkey output.
///
/// Hotkeys are meant for "the other machine", so they go to the first active
/// client. Resolved on every trigger, so activating a different client takes
/// effect immediately.
pub(crate) struct HotkeyTask {
    state: HotkeyState,
    /// one reader task per keyboard device, forwarding raw key samples
    key_rx: mpsc::UnboundedReceiver<KeySample>,
    readers: Vec<tokio::task::JoinHandle<()>>,
    conn: LanMouseConnection,
}

impl HotkeyTask {
    pub(crate) fn new(hotkeys: Vec<Hotkey>, conn: LanMouseConnection) -> Self {
        let (key_tx, key_rx) = mpsc::unbounded_channel();
        let devices = if hotkeys.is_empty() {
            Vec::new()
        } else {
            Self::spawn_readers(key_tx)
        };
        Self {
            state: HotkeyState::new(hotkeys),
            key_rx,
            readers: devices,
            conn,
        }
    }

    fn spawn_readers(key_tx: mpsc::UnboundedSender<KeySample>) -> Vec<tokio::task::JoinHandle<()>> {
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

    pub(crate) fn set_hotkeys(&mut self, hotkeys: Vec<Hotkey>) {
        let was_empty = self.state.is_empty();
        self.state.set_hotkeys(hotkeys.clone());
        if was_empty && !hotkeys.is_empty() && self.readers.is_empty() {
            let (key_tx, key_rx) = mpsc::unbounded_channel();
            self.key_rx = key_rx;
            self.readers = Self::spawn_readers(key_tx);
        }
    }

    /// Wait for the next hotkey trigger.
    pub(crate) async fn next(&mut self) -> Vec<u32> {
        loop {
            let Some(sample) = self.key_rx.recv().await else {
                // every device closed; nothing left to watch
                std::future::pending::<()>().await;
                unreachable!()
            };
            if let Some(seq) = self.state.on_key(sample) {
                return seq;
            }
        }
    }

    pub(crate) async fn send(&self, keys: &[u32]) {
        let Some(handle) = self.conn.first_active_client() else {
            log::debug!("hotkey fired but no active client to receive it");
            return;
        };

        // A hotkey is the first thing to touch the peer when the pointer never
        // left this machine, so the connection usually does not exist yet.
        // `send` starts it in the background and reports `NotConnected`;
        // retry briefly so the very first press is not swallowed.
        let events = hotkey::tap_events(keys);
        for attempt in 0..HOTKEY_CONNECT_ATTEMPTS {
            let mut failed = false;
            for event in &events {
                match self
                    .conn
                    .send(lan_mouse_proto::ProtoEvent::Input(*event), handle)
                    .await
                {
                    Ok(()) => {}
                    Err(LanMouseConnectionError::NotConnected) => {
                        failed = true;
                        break;
                    }
                    Err(e) => {
                        log::debug!("hotkey: could not deliver event: {e}");
                        return;
                    }
                }
            }
            if !failed {
                log::info!("hotkey fired: sent {keys:?} to client {handle}");
                return;
            }
            if attempt + 1 < HOTKEY_CONNECT_ATTEMPTS {
                tokio::time::sleep(HOTKEY_CONNECT_DELAY).await;
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
        let mut guard = match dev.readable().await {
            Ok(g) => g,
            Err(e) => {
                log::warn!("hotkeys: {} unreadable: {e}", path.display());
                return;
            }
        };
        match guard.try_io(|inner| {
            use std::io::Read;
            inner.get_ref().read(&mut buf)
        }) {
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
            Err(_would_block) => {}
        }
        guard.clear_ready();
    }
}
