use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    time::Instant,
};

use input_event::{Event, KeyboardEvent, PointerEvent};

pub use self::error::{EmulationCreationError, EmulationError, InputEmulationError};

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

#[cfg(wlroots)]
mod wlroots;

#[cfg(rdp)]
mod xdg_desktop_portal;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

/// fallback input emulation (logs events)
mod dummy;
mod error;
mod scroll;

pub use scroll::ScrollSmoothing;

pub type EmulationHandle = u64;

/// The edge of a screen a pointer crosses when it moves to another device.
///
/// It is named from the point of view of the device being entered: a peer that
/// leaves through its own top edge enters this device through `Bottom`? No —
/// the sender reports the edge of the receiving screen, so `Top` always means
/// "came in over the top edge of this screen".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenEdge {
    Left,
    Right,
    Top,
    Bottom,
}

impl ScreenEdge {
    /// The point `fraction` (0..=1) along this edge of the rectangle spanning
    /// `(x0, y0)` to `(x1, y1)`.
    pub fn point_on(self, fraction: f64, x0: f64, y0: f64, x1: f64, y1: f64) -> (f64, f64) {
        self.point_on_inset(fraction, x0, y0, x1, y1, 0.0)
    }

    /// Like [`ScreenEdge::point_on`], but `inset` pixels inside the rectangle.
    ///
    /// Placing the cursor exactly on the edge makes the receiving side see it
    /// cross that same edge again, which bounces the pointer straight back to
    /// the peer, so the cursor is kept a little inside.
    pub fn point_on_inset(
        self,
        fraction: f64,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        inset: f64,
    ) -> (f64, f64) {
        let fraction = fraction.clamp(0.0, 1.0);
        let along_x = x0 + fraction * (x1 - x0);
        let along_y = y0 + fraction * (y1 - y0);
        match self {
            Self::Left => (x0 + inset, along_y),
            Self::Right => (x1 - inset, along_y),
            Self::Top => (along_x, y0 + inset),
            Self::Bottom => (along_x, y1 - inset),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(wlroots)]
    Wlroots,
    #[cfg(libei)]
    Libei,
    #[cfg(rdp)]
    Xdp,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(wlroots)]
            Backend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei)]
            Backend::Libei => write!(f, "libei"),
            #[cfg(rdp)]
            Backend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "macos"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputEmulation {
    emulation: Box<dyn Emulation>,
    handles: HashSet<EmulationHandle>,
    pressed_keys: HashMap<EmulationHandle, HashSet<u32>>,
    /// receiver-side wheel smoothing, when enabled and supported by the backend
    smoothing: Option<scroll::Smoother>,
}

impl InputEmulation {
    async fn with_backend(
        backend: Backend,
        smoothing: ScrollSmoothing,
    ) -> Result<InputEmulation, EmulationCreationError> {
        let emulation: Box<dyn Emulation> = match backend {
            #[cfg(wlroots)]
            Backend::Wlroots => Box::new(wlroots::WlrootsEmulation::new()?),
            #[cfg(libei)]
            Backend::Libei => Box::new(libei::LibeiEmulation::new().await?),
            #[cfg(x11)]
            Backend::X11 => Box::new(x11::X11Emulation::new()?),
            #[cfg(rdp)]
            Backend::Xdp => Box::new(xdg_desktop_portal::DesktopPortalEmulation::new().await?),
            #[cfg(windows)]
            Backend::Windows => Box::new(windows::WindowsEmulation::new()?),
            #[cfg(target_os = "macos")]
            Backend::MacOs => Box::new(macos::MacOSEmulation::new()?),
            Backend::Dummy => Box::new(dummy::DummyEmulation::new()),
        };
        let mut emulation = Self {
            emulation,
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
            smoothing: None,
        };
        emulation.set_scroll_smoothing(smoothing);
        Ok(emulation)
    }

    /// Enable, disable or retime receiver-side wheel smoothing.
    ///
    /// Takes effect immediately: the backend itself is untouched, only the
    /// queue that spreads notches over time is replaced. That is why the
    /// daemon can toggle this while a peer is connected.
    pub fn set_scroll_smoothing(&mut self, smoothing: ScrollSmoothing) {
        self.smoothing = if !smoothing.is_on() {
            None
        } else {
            match self.emulation.scroll_units_per_notch() {
                Some(units_per_notch) => {
                    log::info!(
                        "scroll smoothing: {} ms per notch",
                        smoothing.duration().as_millis()
                    );
                    Some(scroll::Smoother::new(smoothing.duration(), units_per_notch))
                }
                None => {
                    log::warn!(
                        "scroll smoothing is enabled but this backend cannot inject \
                         continuous scroll; wheel notches stay discrete"
                    );
                    None
                }
            }
        };
    }

    pub async fn new(
        backend: Option<Backend>,
        smoothing: ScrollSmoothing,
    ) -> Result<InputEmulation, EmulationCreationError> {
        if let Some(backend) = backend {
            let b = Self::with_backend(backend, smoothing).await;
            if b.is_ok() {
                log::info!("using emulation backend: {backend}");
            }
            return b;
        }

        for backend in [
            #[cfg(wlroots)]
            Backend::Wlroots,
            #[cfg(libei)]
            Backend::Libei,
            #[cfg(rdp)]
            Backend::Xdp,
            #[cfg(x11)]
            Backend::X11,
            #[cfg(windows)]
            Backend::Windows,
            #[cfg(target_os = "macos")]
            Backend::MacOs,
            Backend::Dummy,
        ] {
            match Self::with_backend(backend, smoothing).await {
                Ok(b) => {
                    log::info!("using emulation backend: {backend}");
                    return Ok(b);
                }
                Err(e) if e.cancelled_by_user() => return Err(e),
                Err(e) => log::warn!("{e}"),
            }
        }

        Err(EmulationCreationError::NoAvailableBackend)
    }

    pub async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        if self.smoothing.is_some() {
            match &event {
                Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
                    self.smoothing.as_mut().expect("checked above").add_notches(
                        handle,
                        *axis,
                        *value,
                        Instant::now(),
                    );
                    return Ok(());
                }
                /* A click or a key must not be overtaken by the rest of a
                 * scroll animation. Pointer motion is independent and may
                 * arrive while the wheel is still settling. */
                Event::Pointer(PointerEvent::Button { .. }) | Event::Keyboard(_) => {
                    self.flush_scroll(handle).await?;
                }
                _ => {}
            }
        }

        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                // prevent double pressed / released keys
                if self.update_pressed_keys(handle, key, state) {
                    self.emulation.consume(event, handle).await?;
                }
                Ok(())
            }
            _ => self.emulation.consume(event, handle).await,
        }
    }

    /// When the next smoothing tick is due, if an animation is running.
    ///
    /// The caller sleeps until this instant and then calls [`Self::scroll_tick`];
    /// the emulation backend has no clock of its own.
    pub fn next_scroll_tick(&self) -> Option<Instant> {
        self.smoothing.as_ref().and_then(scroll::Smoother::deadline)
    }

    /// Emit the part of every wheel animation that is due now.
    pub async fn scroll_tick(&mut self) -> Result<(), EmulationError> {
        let mut due = Vec::new();
        if let Some(smoothing) = self.smoothing.as_mut() {
            smoothing.tick(Instant::now(), &mut due);
        }
        self.emit_scroll(due).await
    }

    /// Emit everything still queued for `handle` and forget it.
    async fn flush_scroll(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        let mut due = Vec::new();
        if let Some(smoothing) = self.smoothing.as_mut() {
            smoothing.flush(handle, &mut due);
        }
        self.emit_scroll(due).await
    }

    async fn emit_scroll(
        &mut self,
        due: Vec<(EmulationHandle, u8, f64)>,
    ) -> Result<(), EmulationError> {
        for (handle, axis, value) in due {
            if value == 0.0 {
                continue;
            }
            self.emulation
                .consume(
                    Event::Pointer(PointerEvent::Axis {
                        time: 0,
                        axis,
                        value,
                    }),
                    handle,
                )
                .await?;
        }
        Ok(())
    }

    /// Place the cursor where the peer crossed the matching edge of this
    /// screen. See [`Emulation::enter`].
    pub async fn enter(&mut self, handle: EmulationHandle, edge: ScreenEdge, fraction: f64) {
        self.emulation.enter(handle, edge, fraction).await;
    }

    /// Put the cursor in the middle of this machine's screen.
    ///
    /// Used when a peer hands the pointer over on request instead of crossing
    /// an edge: there is no crossing point to line up with. See
    /// [`Emulation::enter_center`].
    pub async fn enter_center(&mut self, handle: EmulationHandle) {
        self.emulation.enter_center(handle).await;
    }

    pub async fn create(&mut self, handle: EmulationHandle) -> bool {
        if self.handles.insert(handle) {
            self.pressed_keys.insert(handle, HashSet::new());
            self.emulation.create(handle).await;
            true
        } else {
            false
        }
    }

    pub async fn destroy(&mut self, handle: EmulationHandle) {
        let _ = self.release_keys(handle).await;
        if self.handles.remove(&handle) {
            self.pressed_keys.remove(&handle);
            if let Some(smoothing) = self.smoothing.as_mut() {
                smoothing.remove(handle);
            }
            self.emulation.destroy(handle).await
        }
    }

    pub async fn terminate(&mut self) {
        if let Some(smoothing) = self.smoothing.as_mut() {
            /* the backend is about to go away: emit nothing more */
            smoothing.clear();
        }
        for handle in self.handles.iter().cloned().collect::<Vec<_>>() {
            self.destroy(handle).await
        }
        self.emulation.terminate().await
    }

    pub async fn release_keys(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        if let Some(keys) = self.pressed_keys.get_mut(&handle) {
            let keys = keys.drain().collect::<Vec<_>>();
            for key in keys {
                let event = Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key,
                    state: 0,
                });
                self.emulation.consume(event, handle).await?;
                if let Ok(key) = input_event::scancode::Linux::try_from(key) {
                    log::warn!("releasing stuck key: {key:?}");
                }
            }
        }

        /* Send the modifier state that is actually still held: blindly
         * zeroing the modifiers would also release keys that are still held
         * by another client, since the emulation backends share a single
         * keyboard.
         */
        let (depressed, locked) = self.pressed_modifier_masks();
        let event = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed,
            latched: 0,
            locked,
            group: 0,
        });
        self.emulation.consume(event, handle).await?;
        Ok(())
    }

    /// compute the modifier masks (X11 convention) of every key that is
    /// currently held by any client
    fn pressed_modifier_masks(&self) -> (u32, u32) {
        // X11 modifier masks
        const SHIFT: u32 = 1 << 0;
        const LOCK: u32 = 1 << 1;
        const CONTROL: u32 = 1 << 2;
        const MOD1: u32 = 1 << 3;
        const MOD2: u32 = 1 << 4;
        const MOD3: u32 = 1 << 5;
        const MOD4: u32 = 1 << 6;

        let mut depressed = 0;
        let mut locked = 0;
        for key in self.pressed_keys.values().flatten() {
            let Ok(key) = input_event::scancode::Linux::try_from(*key) else {
                continue;
            };
            match key {
                input_event::scancode::Linux::KeyLeftShift
                | input_event::scancode::Linux::KeyRightShift => depressed |= SHIFT,
                input_event::scancode::Linux::KeyLeftCtrl
                | input_event::scancode::Linux::KeyRightCtrl => depressed |= CONTROL,
                input_event::scancode::Linux::KeyLeftAlt
                | input_event::scancode::Linux::KeyRightalt => depressed |= MOD1,
                input_event::scancode::Linux::KeyLeftMeta
                | input_event::scancode::Linux::KeyRightmeta => depressed |= MOD4,
                input_event::scancode::Linux::KeyCapsLock => locked |= LOCK,
                input_event::scancode::Linux::KeyNumlock => locked |= MOD2,
                input_event::scancode::Linux::KeyScrollLock => locked |= MOD3,
                _ => {}
            }
        }
        (depressed, locked)
    }

    pub fn has_pressed_keys(&self, handle: EmulationHandle) -> bool {
        self.pressed_keys
            .get(&handle)
            .is_some_and(|p| !p.is_empty())
    }

    /// update the pressed_keys for the given handle
    /// returns whether the event should be processed
    fn update_pressed_keys(&mut self, handle: EmulationHandle, key: u32, state: u8) -> bool {
        let Some(pressed_keys) = self.pressed_keys.get_mut(&handle) else {
            return false;
        };

        if state == 0 {
            // currently pressed => can release
            pressed_keys.remove(&key)
        } else {
            // currently not pressed => can press
            pressed_keys.insert(key)
        }
    }
}

#[async_trait]
trait Emulation: Send {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError>;
    async fn create(&mut self, handle: EmulationHandle);
    async fn destroy(&mut self, handle: EmulationHandle);
    /// A peer started driving this device: place the cursor where it crossed
    /// the peer's `edge`, at the same `fraction` (0..=1) along that edge of
    /// this screen.
    ///
    /// Without it the pointer continues from wherever it was parked here,
    /// which looks like the cursor jumped when the peer was left at one corner
    /// of its screen while this one had it parked in the opposite corner.
    /// Backends without absolute pointer positioning keep the default, which
    /// leaves the cursor alone.
    async fn enter(&mut self, _handle: EmulationHandle, _edge: ScreenEdge, _fraction: f64) {}
    /// Put the cursor in the middle of the screen, for a handover that has no
    /// crossing point (a "switch device" request instead of a barrier).
    /// Backends without absolute pointer positioning keep the default.
    async fn enter_center(&mut self, _handle: EmulationHandle) {}
    /// Continuous axis units corresponding to one wheel notch, when the
    /// backend can inject continuous scrolling at all.
    ///
    /// `None` means wheel notches have to stay discrete (X11 and Windows only
    /// expose wheel buttons), which turns receiver-side smoothing off.
    fn scroll_units_per_notch(&self) -> Option<f64> {
        None
    }
    async fn terminate(&mut self);
}
