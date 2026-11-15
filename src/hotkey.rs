//! Global hotkeys that fire regardless of where the pointer is.
//!
//! The pointer-capture path only sees input while the cursor is inside a
//! barrier zone on a remote screen. A *hotkey* has to work while the user is
//! still typing on their own machine, so it cannot ride on the capture
//! backend. On Linux we read the raw evdev devices directly: that sees every
//! keystroke system-wide without grabbing anything, and needs no portal
//! session.
//!
//! A hotkey press sends its `to` sequence to the peer and swallows the local
//! trigger keys from the point of view of the remote side only — the local
//! machine still sees the keys, because evdev reads are passive. That is the
//! intended behaviour: `Shift+Alt+S` keeps doing whatever it did locally and
//! additionally types `s` on the peer.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

#[cfg(all(unix, not(target_os = "macos")))]
use std::{
    fs::File,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use input_event::{Event, KeyboardEvent};
#[cfg(all(unix, not(target_os = "macos")))]
use tokio::io::unix::AsyncFd;

#[cfg(all(unix, not(target_os = "macos")))]
/// `struct input_event` on 64-bit Linux: timeval (16 bytes) + type, code, value.
pub(crate) const EVENT_SIZE: usize = 24;
#[cfg(all(unix, not(target_os = "macos")))]
const EV_KEY: u16 = 0x01;
#[cfg(all(unix, not(target_os = "macos")))]
const EV_SYN: u16 = 0x00;

pub(crate) use crate::hotkey_task::{Hotkey, HotkeyFire};

/// One key event read from a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeySample {
    pub key: u32,
    /// 1 = press, 0 = release, 2 = autorepeat
    pub value: u32,
}

#[cfg(all(unix, not(target_os = "macos")))]
/// Open every keyboard device under `/dev/input`.
///
/// Devices the process cannot read are skipped with a warning rather than
/// failing the whole feature: a missing permission on one keyboard should not
/// disable hotkeys for the others.
pub(crate) fn open_keyboards() -> Vec<(PathBuf, AsyncFd<File>)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
        })
        .collect();
    paths.sort();

    for path in paths {
        if !is_keyboard(&path) {
            continue;
        }
        match open_device(&path) {
            Ok(dev) => out.push((path, dev)),
            Err(e) => log::debug!("hotkeys: skipping {}: {e}", path.display()),
        }
    }
    out
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_device(path: &Path) -> std::io::Result<AsyncFd<File>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;

    // Discard whatever the kernel already queued for this device.
    //
    // Opening an evdev node can hand back events that were pending before we
    // opened it (the service starting up, a key held during boot, a device
    // that just re-appeared). Feeding those to the hotkey matcher makes it
    // fire on keystrokes the user never performed while we were listening —
    // observed as a burst of spurious triggers right after login.
    drain(&mut file);

    AsyncFd::new(file)
}

#[cfg(all(unix, not(target_os = "macos")))]
/// Read until the device reports nothing more.
fn drain(file: &mut File) {
    let mut buf = [0u8; EVENT_SIZE * 64];
    loop {
        match std::io::Read::read(file, &mut buf) {
            Ok(0) => return,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(_) => return,
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
/// Whether the device reports keys (a keyboard, a mouse with buttons, …).
///
/// Reading `/dev/input/event*` for a mouse is harmless but wasteful, and
/// button codes would collide with key codes, so require the device to look
/// like a keyboard: it must advertise the key event type and at least one
/// alphanumeric key.
fn is_keyboard(path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let fd = file.as_raw_fd();

    // EVIOCGBIT(0, …) — which event types the device produces.
    const EV_MAX: usize = 0x1f;
    let mut types = [0u8; EV_MAX.div_ceil(8)];
    let ok = unsafe {
        libc::ioctl(
            fd,
            eviocgbit(0, types.len()),
            types.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if ok < 0 {
        log::debug!(
            "hotkeys: {}: EVIOCGBIT(0) failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        return false;
    }
    if types[EV_KEY as usize / 8] & (1 << (EV_KEY % 8)) == 0 {
        return false;
    }

    // EVIOCGBIT(EV_KEY, …) — which key codes it can emit. Require KEY_A.
    const KEY_MAX: usize = 0x2ff;
    const KEY_A: usize = 30;
    let mut keys = [0u8; KEY_MAX.div_ceil(8)];
    let ok = unsafe {
        libc::ioctl(
            fd,
            eviocgbit(EV_KEY as usize, keys.len()),
            keys.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if ok < 0 {
        return false;
    }
    keys[KEY_A / 8] & (1 << (KEY_A % 8)) != 0
}

#[cfg(all(unix, not(target_os = "macos")))]
/// `EVIOCGBIT(ev, len)` = `_IOC(_IOC_READ, 'E', 0x20 + ev, len)`.
///
/// The `0x20` offset is part of the ioctl number: omitting it yields a request
/// the kernel does not recognise, `ioctl` returns -1 and every device is
/// silently rejected.
fn eviocgbit(ev: usize, len: usize) -> libc::c_ulong {
    const IOC_READ: libc::c_ulong = 2;
    const IOC_TYPESHIFT: libc::c_ulong = 8;
    const IOC_SIZESHIFT: libc::c_ulong = 16;
    const IOC_DIRSHIFT: libc::c_ulong = 30;
    (IOC_READ << IOC_DIRSHIFT)
        | ((b'E' as libc::c_ulong) << IOC_TYPESHIFT)
        | ((0x20 + ev) as libc::c_ulong)
        | ((len as libc::c_ulong) << IOC_SIZESHIFT)
}

#[cfg(all(unix, not(target_os = "macos")))]
/// Parse the events contained in `buf`, calling `f` for every key event.
pub(crate) fn parse_events(buf: &[u8], mut f: impl FnMut(KeySample)) {
    for chunk in buf.as_chunks::<EVENT_SIZE>().0 {
        let ty = u16::from_ne_bytes([chunk[16], chunk[17]]);
        if ty == EV_SYN {
            continue;
        }
        if ty != EV_KEY {
            continue;
        }
        let code = u16::from_ne_bytes([chunk[18], chunk[19]]);
        let value = u32::from_ne_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
        f(KeySample {
            key: u32::from(code),
            value,
        });
    }
}

/// Tracks which hotkey triggers are currently held, so a press fires exactly
/// once even though the keyboard repeats.
pub(crate) struct HotkeyState {
    hotkeys: Vec<Hotkey>,
    /// keys currently held down (all of them, not just hotkey members)
    held: HashSet<u32>,
    /// trigger sets already fired; cleared when any of their keys is released
    fired: HashSet<Vec<u32>>,
    /// when a hotkey last fired
    ///
    /// One physical keystroke reaches us once per matching evdev device — a
    /// keyboard commonly exposes several event nodes — and their press/release
    /// sequences interleave. Without a time guard a single chord fires once per
    /// device (14 times in practice). The guard collapses those duplicates
    /// while still allowing a deliberate second press.
    last_fire: Option<Instant>,
    /// progress of multi-tap rules, keyed by the sorted `from` chord
    tap_progress: HashMap<Vec<u32>, (u8, Instant)>,
}

/// Minimum spacing between two firings of any hotkey.
const FIRE_DEBOUNCE: Duration = Duration::from_millis(250);

/// How long a double-tap rule waits for the second press.
const MULTI_TAP_WINDOW: Duration = Duration::from_millis(400);

impl HotkeyState {
    pub(crate) fn new(hotkeys: Vec<Hotkey>) -> Self {
        Self {
            hotkeys,
            held: HashSet::new(),
            fired: HashSet::new(),
            last_fire: None,
            tap_progress: HashMap::new(),
        }
    }

    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn is_empty(&self) -> bool {
        self.hotkeys.is_empty()
    }

    pub(crate) fn set_hotkeys(&mut self, hotkeys: Vec<Hotkey>) {
        self.hotkeys = hotkeys;
        self.held.clear();
        self.fired.clear();
        self.last_fire = None;
        self.tap_progress.clear();
    }

    /// Feed one key sample. Returns what to do if a hotkey just fired.
    pub(crate) fn on_key(&mut self, sample: KeySample) -> Option<HotkeyFire> {
        if self.hotkeys.is_empty() {
            return None;
        }
        match sample.value {
            0 => {
                self.held.remove(&sample.key);
                // Re-arm any hotkey this release belongs to.
                self.fired.retain(|from| !from.contains(&sample.key));
                None
            }
            // value 1 is a press, 2 is autorepeat; only the press fires
            1 => {
                self.held.insert(sample.key);
                let mut hit = None;
                for hk in &self.hotkeys {
                    if hk.from.is_empty() {
                        continue;
                    }
                    let all_held = hk.from.iter().all(|k| self.held.contains(k));
                    if !all_held {
                        continue;
                    }
                    let mut sorted = hk.from.clone();
                    sorted.sort_unstable();

                    // A multi-tap rule counts presses of the same chord inside
                    // `MULTI_TAP_WINDOW`; a single-tap rule fires immediately.
                    let taps = hk.taps.max(1);
                    if taps > 1 {
                        let count = match self.tap_progress.get(&sorted) {
                            Some((n, at)) if at.elapsed() < MULTI_TAP_WINDOW => *n + 1,
                            _ => 1,
                        };
                        self.tap_progress
                            .insert(sorted.clone(), (count, Instant::now()));
                        if count < taps {
                            log::trace!("hotkey: tap {count}/{taps} on {sorted:?}");
                            continue;
                        }
                        self.tap_progress.remove(&sorted);
                    } else if !self.fired.insert(sorted) {
                        // Already fired for this hold; wait for a release.
                        continue;
                    }

                    if self.last_fire.is_some_and(|t| t.elapsed() < FIRE_DEBOUNCE) {
                        log::trace!("hotkey: suppressing duplicate within debounce window");
                        continue;
                    }
                    self.last_fire = Some(Instant::now());
                    hit = Some(match hk.action {
                        Some(action) => HotkeyFire::Action(action),
                        None => HotkeyFire::Keys(hk.to.clone()),
                    });
                }
                hit
            }
            _ => None,
        }
    }
}

/// Turn a key code sequence into press/release events for the peer.
pub(crate) fn tap_events(keys: &[u32]) -> Vec<Event> {
    let mut out: Vec<Event> = keys
        .iter()
        .map(|&key| {
            Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key,
                state: 1,
            })
        })
        .collect();
    out.extend(keys.iter().rev().map(|&key| {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key,
            state: 0,
        })
    }));
    out
}

// The helpers under test (`parse_events`, `drain`, `EVENT_SIZE`) are the evdev
// ones, so these tests only exist on the platforms that compile them.
#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;
    use input_event::scancode;
    use input_event::scancode::Linux::*;

    fn press(key: scancode::Linux) -> KeySample {
        KeySample {
            key: key as u32,
            value: 1,
        }
    }

    fn release(key: scancode::Linux) -> KeySample {
        KeySample {
            key: key as u32,
            value: 0,
        }
    }

    fn sas() -> HotkeyState {
        HotkeyState::new(vec![Hotkey {
            from: vec![KeyLeftShift as u32, KeyLeftAlt as u32, KeyS as u32],
            to: vec![KeyS as u32],
            action: None,
            taps: 1,
        }])
    }

    #[test]
    fn fires_on_complete_chord() {
        let mut st = sas();
        assert_eq!(st.on_key(press(KeyLeftShift)), None);
        assert_eq!(st.on_key(press(KeyLeftAlt)), None);
        assert_eq!(
            st.on_key(press(KeyS)),
            Some(HotkeyFire::Keys(vec![KeyS as u32]))
        );
    }

    #[test]
    fn duplicate_devices_fire_once() {
        // one physical chord delivered twice (two event nodes)
        let mut st = sas();
        for _ in 0..2 {
            for k in [KeyLeftShift, KeyLeftAlt, KeyS] {
                st.on_key(press(k));
            }
            for k in [KeyS, KeyLeftAlt, KeyLeftShift] {
                st.on_key(release(k));
            }
        }
        assert_eq!(st.on_key(press(KeyLeftShift)), None);
        st.on_key(press(KeyLeftAlt));
        // still within the debounce window: suppressed
        assert_eq!(st.on_key(press(KeyS)), None);
    }

    #[test]
    fn autorepeat_does_not_refire() {
        let mut st = sas();
        st.on_key(press(KeyLeftShift));
        st.on_key(press(KeyLeftAlt));
        st.on_key(press(KeyS));
        // kernel autorepeat of the trigger
        let repeat = KeySample {
            key: KeyS as u32,
            value: 2,
        };
        assert_eq!(st.on_key(repeat), None);
    }

    #[test]
    fn rearm_after_release() {
        let mut st = sas();
        for k in [KeyLeftShift, KeyLeftAlt, KeyS] {
            assert!(st.on_key(press(k)).is_some() || k != KeyS);
        }
        // releasing the trigger re-arms the chord
        assert_eq!(st.on_key(release(KeyS)), None);
        // step past the duplicate-device debounce window
        st.last_fire = Some(Instant::now() - FIRE_DEBOUNCE * 2);
        assert_eq!(
            st.on_key(press(KeyS)),
            Some(HotkeyFire::Keys(vec![KeyS as u32]))
        );
    }

    #[test]
    fn incomplete_chord_does_nothing() {
        let mut st = sas();
        assert_eq!(st.on_key(press(KeyLeftShift)), None);
        assert_eq!(st.on_key(press(KeyS)), None);
    }

    #[test]
    fn unrelated_key_does_not_fire() {
        let mut st = sas();
        for k in [KeyLeftShift, KeyLeftAlt, KeyA] {
            st.on_key(press(k));
        }
        assert_eq!(
            st.on_key(press(KeyS)),
            Some(HotkeyFire::Keys(vec![KeyS as u32]))
        );
    }

    #[test]
    fn stale_events_do_not_fire_after_drain() {
        // Regression: opening an evdev node returns events queued *before* the
        // open, which made the service fire chords on keystrokes the user never
        // made while it was listening (observed as a burst right after boot).
        // The drain helper consumes them, so the matcher starts clean.
        let dir = std::env::temp_dir().join(format!("lm-hotkey-drain-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("stale");
        let mut ev = vec![0u8; EVENT_SIZE];
        ev[16..18].copy_from_slice(&EV_KEY.to_ne_bytes());
        ev[18..20].copy_from_slice(&(KeyS as u16).to_ne_bytes());
        ev[20..24].copy_from_slice(&1u32.to_ne_bytes());
        std::fs::write(&path, &ev).expect("write");

        let mut file = std::fs::File::open(&path).expect("open");
        drain(&mut file);

        // nothing left: a non-blocking read now returns nothing
        use std::io::Read;
        let mut buf = [0u8; EVENT_SIZE];
        let n = file.read(&mut buf).expect("read");
        assert_eq!(n, 0, "drain must consume everything");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_input_event_struct() {
        let mut buf = Vec::new();
        // one EV_KEY press of KEY_S, one EV_SYN
        let mut ev = vec![0u8; EVENT_SIZE];
        ev[16..18].copy_from_slice(&EV_KEY.to_ne_bytes());
        ev[18..20].copy_from_slice(&(KeyS as u16).to_ne_bytes());
        ev[20..24].copy_from_slice(&1u32.to_ne_bytes());
        buf.extend_from_slice(&ev);
        let mut syn = vec![0u8; EVENT_SIZE];
        syn[16..18].copy_from_slice(&EV_SYN.to_ne_bytes());
        buf.extend_from_slice(&syn);

        let mut seen = Vec::new();
        parse_events(&buf, |s| seen.push(s));
        assert_eq!(
            seen,
            vec![KeySample {
                key: KeyS as u32,
                value: 1
            }]
        );
    }
}
