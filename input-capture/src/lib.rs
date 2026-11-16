use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    task::{Poll, ready},
};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use input_event::{Event, KeyboardEvent, scancode};

// The edge a client is reached through is part of every backend's barrier
// geometry, so the type is part of this crate's API rather than an import.
pub use input_event::EdgeRange;

pub use error::{CaptureCreationError, CaptureError, InputCaptureError};

pub mod error;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(layer_shell)]
mod layer_shell;

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

/// fallback input capture (does not produce events)
mod dummy;

pub type CaptureHandle = u64;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// Capture on this handle is now active.
    ///
    /// The payload is where the pointer left the screen as a fraction (0..=1)
    /// along the shared edge, counted from its left or top end. The peer puts
    /// its cursor there, so the pointer does not have to travel from wherever
    /// it happened to be. `0.5` when the backend cannot tell.
    Begin(f64),
    /// input event coming from capture handle
    Input(Event),
    /// the compositor took the pointer away from us.
    ///
    /// Happens when a lock screen, a new surface or a display reconfiguration
    /// grabs the seat: the barrier is gone and no further events will arrive,
    /// so the caller must release the capture instead of waiting for input
    /// that will never come (issues #545, #512).
    Interrupted,
}

impl Display for CaptureEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureEvent::Begin(fraction) => {
                write!(f, "begin capture at {:.0}% of the edge", fraction * 100.0)
            }
            CaptureEvent::Input(e) => write!(f, "{e}"),
            CaptureEvent::Interrupted => write!(f, "capture interrupted"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Self::Right,
            Position::Right => Self::Left,
            Position::Top => Self::Bottom,
            Position::Bottom => Self::Top,
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(libei)]
    InputCapturePortal,
    #[cfg(layer_shell)]
    LayerShell,
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
            #[cfg(libei)]
            Backend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell)]
            Backend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "MacOS"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputCapture {
    /// capture backend
    capture: Box<dyn Capture>,
    /// keys pressed by active capture
    pressed_keys: HashSet<scancode::Linux>,
    /// map from position to ids
    position_map: HashMap<Position, Vec<CaptureHandle>>,
    /// map from id to position
    id_map: HashMap<CaptureHandle, Position>,
    /// segments of each edge that a crossing can be routed to
    segments: HashMap<Position, Vec<EdgeRange>>,
    /// pending events
    pending: VecDeque<(CaptureHandle, CaptureEvent)>,
}

impl InputCapture {
    /// create a new client with the given id on the given edge
    pub async fn create(&mut self, id: CaptureHandle, pos: Position) -> Result<(), CaptureError> {
        /* The service only creates a handle once, but a duplicate must not
         * abort the process (this crate is built with `panic = "abort"`):
         * move the handle to its new position instead.
         */
        if self.id_map.contains_key(&id) {
            log::warn!("capture handle {id} already exists, moving it to {pos}");
            self.destroy(id).await?;
        }

        self.id_map.insert(id, pos);

        if let Some(v) = self.position_map.get_mut(&pos) {
            v.push(id);
        } else {
            self.position_map.insert(pos, vec![id]);
            let segments = self
                .segments
                .get(&pos)
                .cloned()
                .unwrap_or_else(|| vec![EdgeRange::FULL]);
            self.capture.create(pos, segments).await?;
        }
        Ok(())
    }

    /// Replace the segments of `pos` that a crossing can be routed to.
    ///
    /// The caller owns the resolution: it knows which client asked for what and
    /// which of those requests overlap, and it is the one that routes the
    /// crossing, so the barriers have to be exactly what it will route to. An
    /// empty list removes every barrier of the edge, which is what makes the
    /// pointer hit a wall there.
    pub async fn set_segments(
        &mut self,
        pos: Position,
        segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        if !self.position_map.contains_key(&pos) {
            /* nothing is registered for this edge: remember the segments so the
             * barriers are created with the right geometry when the first client
             * of the edge arrives */
            self.segments.insert(pos, segments);
            return Ok(());
        }
        let previous = self.segments.get(&pos).cloned().unwrap_or_default();
        if previous == segments {
            return Ok(());
        }
        log::debug!(
            "capture @ {pos} now covers [{}]",
            segments
                .iter()
                .map(EdgeRange::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.segments.insert(pos, segments);
        self.capture.update(pos, self.segments[&pos].clone()).await
    }

    /// Size of the area the pointer can reach, in pixels.
    ///
    /// The arrangement canvas draws the local screen to scale with it, which is
    /// what turns an edge range from a decoration into something the user can
    /// see the length of.
    pub async fn screen_size(&mut self) -> Option<(u32, u32)> {
        self.capture.screen_size().await
    }

    /// destroy the client with the given id, if it exists
    pub async fn destroy(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let Some(pos) = self.id_map.remove(&id) else {
            log::debug!("capture handle {id} does not exist, nothing to destroy");
            return Ok(());
        };

        // events that are still queued for this handle must not be delivered
        self.pending.retain(|(handle, _)| *handle != id);

        log::debug!("destroying capture {id} @ {pos}");
        let Some(remaining) = self.position_map.get_mut(&pos) else {
            log::warn!("capture position {pos} has no handles, discarding it");
            return Ok(());
        };
        remaining.retain(|&i| i != id);

        log::debug!("remaining ids @ {pos}: {remaining:?}");
        if remaining.is_empty() {
            log::debug!("destroying capture @ {pos} - no remaining ids");
            self.position_map.remove(&pos);
            self.capture.destroy(pos).await?;
        }
        Ok(())
    }

    /// Take the pointer on this handle without waiting for a crossing.
    ///
    /// Only a backend that owns the local pointer can do this: macOS can, the
    /// compositor driven ones cannot (there the compositor decides who gets
    /// the pointer). The backend reports [`CaptureEvent::Begin`] like it does
    /// for a real crossing, so callers keep a single code path.
    pub async fn begin(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let Some(&pos) = self.id_map.get(&id) else {
            log::debug!("capture handle {id} does not exist, not taking the pointer");
            return Err(CaptureError::BeginUnsupported);
        };
        log::debug!("taking the pointer at {pos} on request");
        self.capture.begin(pos).await
    }

    /// release mouse
    pub async fn release(&mut self) -> Result<(), CaptureError> {
        self.pressed_keys.clear();
        self.capture.release().await
    }

    /// Drain and return every key the capture has forwarded as
    /// down-but-not-up. The caller is expected to synthesize key-up
    /// events to the remote peer for each — otherwise the peer
    /// retains phantom-held keys after capture is released. The
    /// canonical case is the release-bind chord
    /// (Ctrl+Shift+Alt+Meta): the down events were sent while
    /// capture was active, but the matching up events arrive after
    /// the local tap has flipped to passthrough and never reach
    /// the peer.
    pub fn take_pressed_keys(&mut self) -> HashSet<scancode::Linux> {
        std::mem::take(&mut self.pressed_keys)
    }

    /// destroy the input capture
    pub async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.capture.terminate().await
    }

    /// creates a new [`InputCapture`]
    pub async fn new(backend: Option<Backend>) -> Result<Self, CaptureCreationError> {
        let capture = create(backend).await?;
        Ok(Self {
            capture,
            id_map: Default::default(),
            segments: Default::default(),
            pending: Default::default(),
            position_map: Default::default(),
            pressed_keys: HashSet::new(),
        })
    }

    /// check whether the given keys are pressed
    ///
    /// An empty `keys` slice means "no release bind configured" and never
    /// matches: `[].iter().all(..)` is vacuously true, which would otherwise
    /// release the capture on every single event.
    pub fn keys_pressed(&self, keys: &[scancode::Linux]) -> bool {
        !keys.is_empty() && keys.iter().all(|k| self.pressed_keys.contains(k))
    }

    fn update_pressed_keys(&mut self, key: u32, state: u8) {
        if let Ok(scancode) = scancode::Linux::try_from(key) {
            log::debug!("key: {key}, state: {state}, scancode: {scancode:?}");
            match state {
                1 => self.pressed_keys.insert(scancode),
                _ => self.pressed_keys.remove(&scancode),
            };
        }
    }
}

impl Stream for InputCapture {
    type Item = Result<(CaptureHandle, CaptureEvent), CaptureError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(e) = self.pending.pop_front() {
                return Poll::Ready(Some(Ok(e)));
            }

            // ready
            let event = ready!(self.capture.poll_next_unpin(cx));

            // stream closed
            let event = match event {
                Some(e) => e,
                None => return Poll::Ready(None),
            };

            // error occurred
            let (pos, event) = match event {
                Ok(e) => e,
                Err(e) => return Poll::Ready(Some(Err(e))),
            };

            // handle key presses
            if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) =
                event
            {
                self.update_pressed_keys(key, state);
            }

            /* The backend produced an event for a position that no longer has
             * any handle (it was destroyed while the event was queued).
             * Dropping it is fine, but we have to keep polling instead of
             * returning `Pending`: that would swallow the event without
             * registering a waker.
             */
            let Some(ids) = self.position_map.get(&pos).cloned() else {
                continue;
            };
            let Some((first, rest)) = ids.split_first() else {
                continue;
            };

            /* deliver a copy to every handle registered at this position */
            for id in rest {
                self.pending.push_back((*id, event));
            }
            return Poll::Ready(Some(Ok((*first, event))));
        }
    }
}

#[async_trait]
trait Capture: Stream<Item = Result<(Position, CaptureEvent), CaptureError>> + Unpin {
    /// create a client on `pos`, reachable through `segments`
    ///
    /// The segments are disjoint and given in the order a crossing is routed
    /// in. Where the edge is not covered there is no barrier, so the pointer
    /// hits a wall instead of switching to a machine that does not own that
    /// part of the edge.
    async fn create(&mut self, pos: Position, segments: Vec<EdgeRange>)
    -> Result<(), CaptureError>;

    /// replace the segments of `pos` that clients can be reached through
    async fn update(&mut self, pos: Position, segments: Vec<EdgeRange>)
    -> Result<(), CaptureError>;

    /// destroy the client with the given id, if it exists
    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// Size of the area the pointer can reach, in pixels.
    ///
    /// `None` when the backend has no display geometry. The frontend draws the
    /// local screen to scale with it, which is what makes an edge range mean
    /// something visually instead of being a decoration.
    async fn screen_size(&mut self) -> Option<(u32, u32)>;

    /// release mouse
    async fn release(&mut self) -> Result<(), CaptureError>;

    /// Take the pointer at `pos` without a barrier crossing.
    ///
    /// Backends where the compositor owns the pointer report
    /// [`CaptureError::BeginUnsupported`]; only the ones that own the local
    /// pointer (macOS) can do it.
    async fn begin(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// destroy the input capture
    async fn terminate(&mut self) -> Result<(), CaptureError>;
}

async fn create_backend(
    backend: Backend,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    match backend {
        #[cfg(libei)]
        Backend::InputCapturePortal => Ok(Box::new(libei::LibeiInputCapture::new().await?)),
        #[cfg(layer_shell)]
        Backend::LayerShell => Ok(Box::new(layer_shell::LayerShellInputCapture::new()?)),
        #[cfg(x11)]
        Backend::X11 => Ok(Box::new(x11::X11InputCapture::new()?)),
        #[cfg(windows)]
        Backend::Windows => Ok(Box::new(windows::WindowsInputCapture::new())),
        #[cfg(target_os = "macos")]
        Backend::MacOs => Ok(Box::new(macos::MacOSInputCapture::new().await?)),
        Backend::Dummy => Ok(Box::new(dummy::DummyInputCapture::new())),
    }
}

async fn create(
    backend: Option<Backend>,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    if let Some(backend) = backend {
        let b = create_backend(backend).await;
        if b.is_ok() {
            log::info!("using capture backend: {backend}");
        }
        return b;
    }

    for backend in [
        #[cfg(libei)]
        Backend::InputCapturePortal,
        #[cfg(layer_shell)]
        Backend::LayerShell,
        #[cfg(x11)]
        Backend::X11,
        #[cfg(windows)]
        Backend::Windows,
        #[cfg(target_os = "macos")]
        Backend::MacOs,
    ] {
        match create_backend(backend).await {
            Ok(b) => {
                log::info!("using capture backend: {backend}");
                return Ok(b);
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
            Err(e) => log::warn!("{backend} input capture backend unavailable: {e}"),
        }
    }
    Err(CaptureCreationError::NoAvailableBackend)
}
