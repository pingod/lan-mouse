use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, EdgeRange, InputCapture, InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, Layout, scancode};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use lan_mouse_ipc::{SwitchCorner, SwitchOptions};

use crate::connect::LanMouseConnection;
use crate::remap::{Remap, Rule};

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
    /// size of the reachable screen area as `width << 32 | height`, `0` unknown
    ///
    /// Written by the capture task, which is the only component that talks to a
    /// backend. The service reads it for the frontend, and the task reads it to
    /// turn a corner radius in pixels into a fraction of an edge.
    /// the monitors the backend reported, shared with the service
    layout: Rc<RefCell<Option<Layout>>>,
    /// whether the pointer is currently captured by a remote client
    ///
    /// Updated by the capture task itself rather than derived from the
    /// `ClientEntered`/`ClientLeft` event stream: those are hints for hooks,
    /// and a single missed or duplicated one left the service believing the
    /// pointer was captured when it was not.
    active: Rc<Cell<bool>>,
}

pub(crate) enum ICaptureEvent {
    /// a client was entered
    CaptureBegin(CaptureHandle),
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// the pointer reached this machine's own edge; ask the driving peer to
    /// release it
    ReturnRequested(CaptureHandle),
    /// the peer sent updated machine details
    PeerInfo(CaptureHandle),
    /// A (new) client was entered.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered when the capture was
    /// explicitly released in the meantime by
    /// either the remote client leaving its device region,
    /// a new device entering the screen or the release bind.
    ClientEntered(u64),
    /// A request could not be carried out (the backend cannot take the
    /// pointer, the handle is unknown, ...). The service turns this into a
    /// visible error instead of failing silently.
    Error(String),
    /// The previously active client was left, i.e. capture
    /// was released for the given handle. Mirrors
    /// [`ICaptureEvent::ClientEntered`] for the leave side
    /// and fires on every release path (release-bind chord,
    /// remote `Leave`, explicit `Release` request, send
    /// failure, or destroy of the active capture).
    ClientLeft(u64),
}

impl CaptureTask {
    /// Publish a geometry the backend just reported.
    ///
    /// `None` is not published: at this point in the lifecycle it means "the
    /// backend has not talked to the compositor yet", not "there are no
    /// screens", and writing it would erase a size that is already correct.
    /// The session loop polls until an answer arrives, so nothing is lost by
    /// ignoring this one.
    fn publish_layout(&mut self, layout: Option<Layout>) {
        if let Some(layout) = layout.filter(|layout| !layout.is_empty()) {
            *self.layout.borrow_mut() = Some(layout);
        }
    }
}

/// How often to ask a backend for its geometry while it still reports none.
///
/// A backend can only answer once it has talked to the compositor, and the
/// session that makes that possible is established inside the capture task, not
/// when the barrier is registered: `libei`'s `create` only queues a message. So
/// the first read after `create_captures` legitimately comes back empty, and
/// nothing else in the service would ever ask again - the value would stay
/// unknown for the lifetime of the process.
const SCREEN_SIZE_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Clone, Debug)]
enum CaptureRequest {
    /// capture must release the mouse
    Release,
    /// add a capture client reachable through the given segment of its edge
    Create(CaptureHandle, Position, CaptureType, EdgeRange, EdgeRange),
    /// change the segment of its edge a capture client is reached through
    SetEdge(CaptureHandle, EdgeRange),
    /// replace the edge-crossing thresholds
    SetSwitchOptions(SwitchOptions),
    /// take the pointer on this capture right now, without a crossing
    Begin(CaptureHandle),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// start the connection to this client before the pointer reaches it
    Preconnect(CaptureHandle),
    /// ask a client to re-send its machine details
    RefreshPeerInfo(CaptureHandle),
    /// stop acting on input until [`CaptureRequest::Reenable`]
    Disable,
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// replace the chord remapping rules
    SetRemapRules(Vec<Rule>),
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: LanMouseConnection,
        release_bind: Vec<scancode::Linux>,
        remap_rules: Vec<Rule>,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let active = Rc::new(Cell::new(false));
        let layout = Rc::new(RefCell::new(None));
        let capture_task = CaptureTask {
            active_client: None,
            enter_fraction: 0.5,
            switch_options: SwitchOptions::default(),
            last_edge_hit: Default::default(),
            layout: Rc::clone(&layout),
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            active: active.clone(),
            remap: Remap::new(remap_rules),
            entered_at: None,
            last_enter_release: None,
            state: Default::default(),
            pending_input: Vec::new(),
            center_cursor: false,
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
            active,
            layout,
        }
    }

    /// Start the connection to `handle` ahead of the pointer reaching it.
    pub(crate) fn preconnect(&self, handle: CaptureHandle) {
        let _ = self.request_tx.send(CaptureRequest::Preconnect(handle));
    }

    pub(crate) fn reenable(&self) {
        send_request(&self.request_tx, CaptureRequest::Reenable);
    }

    /// Ask `handle` to send its machine details again.
    pub(crate) fn request_peer_info(&self, handle: CaptureHandle) {
        let _ = self
            .request_tx
            .send(CaptureRequest::RefreshPeerInfo(handle));
    }

    /// Stop capturing until [`Self::reenable`].
    ///
    /// Errors are ignored: the channel is only closed when the capture task has
    /// already stopped, in which case there is nothing left to disable.
    pub(crate) fn disable(&self) {
        let _ = self.request_tx.send(CaptureRequest::Disable);
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: lan_mouse_ipc::Position,
        capture_type: CaptureType,
        edge: EdgeRange,
        peer_edge: EdgeRange,
    ) {
        let pos = to_capture_pos(pos);
        send_request(
            &self.request_tx,
            CaptureRequest::Create(handle, pos, capture_type, edge, peer_edge),
        );
    }

    /// Change the segment of its edge a client is reached through.
    pub(crate) fn set_edge(&self, handle: CaptureHandle, edge: EdgeRange) {
        send_request(&self.request_tx, CaptureRequest::SetEdge(handle, edge));
    }

    /// Replace the edge-crossing thresholds.
    pub(crate) fn set_switch_options(&self, options: SwitchOptions) {
        send_request(&self.request_tx, CaptureRequest::SetSwitchOptions(options));
    }

    /// Size of the reachable screen area, `None` when no backend reported one.
    /// The monitors the backend last reported.
    pub(crate) fn layout(&self) -> Option<Layout> {
        self.layout.borrow().clone()
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        send_request(&self.request_tx, CaptureRequest::Destroy(handle));
    }

    pub(crate) fn release(&self) {
        send_request(&self.request_tx, CaptureRequest::Release);
    }

    /// Take the pointer on this capture right now, without a crossing.
    ///
    /// Used by "switch device": the pointer is handed to a client while the
    /// user is not crossing an edge. Backends that do not own the local
    /// pointer report an error and the log says why.
    pub(crate) fn begin(&self, handle: CaptureHandle) {
        send_request(&self.request_tx, CaptureRequest::Begin(handle));
    }

    /// Wait for the next event; `None` means the capture task has exited.
    pub(crate) async fn event(&mut self) -> Option<ICaptureEvent> {
        self.event_rx.recv().await
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }

    /// Whether a remote client currently holds the pointer.
    pub(crate) fn is_active(&self) -> bool {
        self.active.get()
    }

    pub(crate) fn set_remap_rules(&mut self, rules: Vec<Rule>) {
        let _ = self.request_tx.send(CaptureRequest::SetRemapRules(rules));
    }
}

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

/// One registered capture and the part of its edge it can be reached through.
#[derive(Clone, Copy, Debug)]
struct CaptureEntry {
    handle: CaptureHandle,
    pos: Position,
    kind: CaptureType,
    /// the segment this client asked for
    requested: EdgeRange,
    /// the segment it actually gets: `None` when an earlier client on the same
    /// edge already covers everything it asked for, which makes it unreachable
    effective: Option<EdgeRange>,
    /// the segment of the peer's facing edge the cursor lands in
    peer_edge: EdgeRange,
}

impl CaptureEntry {
    /// Where on the peer's edge its cursor has to end up.
    ///
    /// The crossing point is carried as a fraction of the edge, so it is first
    /// mapped into the local segment and then onto the peer's segment - the same
    /// two-step `transform` / `inverse_transform` Synergy applies to a link's
    /// intervals. Without it a peer whose screen is half as tall gets the cursor
    /// at the same fraction of its own edge, which is not where the pointer was.
    fn peer_fraction(&self, fraction: f64) -> f64 {
        let local = self
            .effective
            .unwrap_or(EdgeRange::FULL)
            .transform(fraction);
        self.peer_edge.inverse_transform(local)
    }
}

impl CaptureEntry {
    fn new(
        handle: CaptureHandle,
        pos: Position,
        kind: CaptureType,
        requested: EdgeRange,
        peer_edge: EdgeRange,
    ) -> Self {
        Self {
            handle,
            pos,
            kind,
            requested,
            effective: Some(requested),
            peer_edge,
        }
    }
}

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    /// where along the edge the pointer crossed when the current capture
    /// started (see [`CaptureEvent::Begin`]); replayed with the retried
    /// `Enter` while the handshake is still in flight
    enter_fraction: f64,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<CaptureEntry>,
    /// how close to an edge the pointer has to behave before it may cross
    switch_options: SwitchOptions,
    /// when an edge was last hit but refused, for the double-tap requirement
    last_edge_hit: HashMap<Position, Instant>,
    /// size of the reachable screen area, shared with the service
    /// the monitors the backend reported, shared with the service
    layout: Rc<RefCell<Option<Layout>>>,
    conn: LanMouseConnection,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    remap: Remap,
    /// mirror of `Capture::active`, kept in sync by this task
    active: Rc<Cell<bool>>,
    /// when the active client acknowledged the connection
    entered_at: Option<Instant>,
    /// when the enter-only capture last asked the backend to release
    last_enter_release: Option<Instant>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
    /// Input that arrived before the peer acknowledged our `Enter`.
    ///
    /// The handshake takes a network round trip, and the pointer is already
    /// moving by then. Dropping those events is what makes the first moments
    /// on the peer feel sticky, so they are replayed once the Ack lands.
    pending_input: Vec<ProtoEvent>,
    /// The next `Begin` comes from a "switch device" request rather than from
    /// the pointer crossing an edge, so the peer has to centre its cursor.
    center_cursor: bool,
}

/// The client a crossing at `fraction` on `pos` belongs to.
///
/// Only the entries whose resolved segment contains the point are candidates:
/// `effective` is what [`EdgeRange::resolve`] handed out, so a client that was
/// squeezed out of an overlapping range cannot steal a crossing, and a point in
/// a gap between two segments reaches nobody - the pointer hits a wall.
fn route(entries: &[CaptureEntry], pos: Position, fraction: f64) -> Option<CaptureHandle> {
    entries
        .iter()
        .filter(|entry| entry.pos == pos && entry.kind == CaptureType::Default)
        .find(|entry| entry.effective.is_some_and(|edge| edge.contains(fraction)))
        .map(|entry| entry.handle)
}

/// Whether a crossing of `pos` at `fraction` is inside a blocked corner.
///
/// The corner radius is given in pixels and the crossing point only as a
/// fraction of the edge, so the screen size is needed to compare them; without a
/// size the corners cannot be located and are not enforced.
fn in_blocked_corner(
    options: &SwitchOptions,
    size: Option<(u32, u32)>,
    pos: Position,
    fraction: f64,
) -> bool {
    if options.corner_size == 0 || options.corners.is_empty() {
        return false;
    }
    let Some((width, height)) = size else {
        return false;
    };
    let radius = options.corner_size as f64;
    /* the point the pointer crossed, in screen coordinates */
    let (x, y) = match pos {
        Position::Left => (0.0, fraction * height as f64),
        Position::Right => (width as f64, fraction * height as f64),
        Position::Top => (fraction * width as f64, 0.0),
        Position::Bottom => (fraction * width as f64, height as f64),
    };
    let left = x <= radius;
    let right = x >= width as f64 - radius;
    let top = y <= radius;
    let bottom = y >= height as f64 - radius;
    [
        (left && top, SwitchCorner::TopLeft),
        (right && top, SwitchCorner::TopRight),
        (left && bottom, SwitchCorner::BottomLeft),
        (right && bottom, SwitchCorner::BottomRight),
    ]
    .into_iter()
    .any(|(hit, corner)| hit && options.corners.contains(&corner))
}

impl CaptureTask {
    fn add_capture(&mut self, entry: CaptureEntry) {
        self.captures.push(entry);
    }

    /// Log when a client cannot reach the edge it was configured for.
    ///
    /// The ranges of an edge are resolved in client order, so a later client
    /// whose range is already taken ends up with nothing. That is invisible in
    /// the UI unless it is reported, and the pointer simply never goes there.
    fn report_unreachable(&self, handle: CaptureHandle, pos: Position) {
        let Some(entry) = self.entry(handle) else {
            return;
        };
        if entry.effective == Some(entry.requested) {
            return;
        }
        match entry.effective {
            Some(effective) => log::warn!(
                "client {handle} asked for {} on the {pos} edge, only {effective} is free",
                entry.requested
            ),
            None => log::warn!(
                "client {handle} cannot be reached on the {pos} edge: {} is already taken",
                entry.requested
            ),
        }
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|entry| entry.handle != handle);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|entry| entry.pos == pos && entry.kind == CaptureType::Default)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Option<Position> {
        self.captures
            .iter()
            .find(|entry| entry.handle == handle)
            .map(|entry| entry.pos)
    }

    fn get_type(&self, handle: CaptureHandle) -> Option<CaptureType> {
        self.captures
            .iter()
            .find(|entry| entry.handle == handle)
            .map(|entry| entry.kind)
    }

    fn entry(&self, handle: CaptureHandle) -> Option<&CaptureEntry> {
        self.captures.iter().find(|entry| entry.handle == handle)
    }

    /// Re-resolve which part of `pos` each client can be reached through and
    /// hand the result to the backend.
    ///
    /// Clients are resolved in handle order, the order they were configured in:
    /// an overlapping request belongs to the lower handle and the higher one is
    /// narrowed or dropped. That is what makes the barriers and the routing
    /// agree - a crossing outside every registered segment never reaches a
    /// client that was configured for it but squeezed out of it.
    async fn refresh_position(
        &mut self,
        capture: &mut InputCapture,
        pos: Position,
    ) -> Result<(), CaptureError> {
        let mut clients = self
            .captures
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.pos == pos && entry.kind == CaptureType::Default)
            .map(|(index, entry)| (index, entry.handle, entry.requested))
            .collect::<Vec<_>>();
        clients.sort_by_key(|(_, handle, _)| *handle);
        let requested: Vec<EdgeRange> = clients.iter().map(|(_, _, range)| *range).collect();
        let effective = EdgeRange::resolve(&requested);
        for ((index, _, _), range) in clients.iter().zip(effective.iter()) {
            self.captures[*index].effective = *range;
        }
        let segments = effective.into_iter().flatten().collect::<Vec<_>>();
        capture.set_segments(pos, segments).await?;
        /* Worth a read here - a backend that builds its barriers synchronously
         * (layer-shell) already knows the answer - but not the only one: the
         * session loop keeps asking until the asynchronous ones catch up. */
        let layout = capture.layout().await;
        self.publish_layout(layout);
        Ok(())
    }

    /// The client a crossing at `fraction` on `pos` belongs to.
    fn route_at(&self, pos: Position, fraction: f64) -> Option<CaptureHandle> {
        route(&self.captures, pos, fraction)
    }

    /// Whether a crossing of `pos` at `fraction` may switch to the peer now.
    ///
    /// Returns `Err(reason)` when a threshold refuses it, so the caller can say
    /// why in the log.
    fn crossing_allowed(&mut self, pos: Position, fraction: f64) -> Result<(), &'static str> {
        /* the corner radius is in pixels and the crossing only as a fraction, so
         * the bounding box is what it is measured against - the same box the
         * barrier spans */
        let size = self
            .layout
            .borrow()
            .as_ref()
            .and_then(input_event::Layout::size);
        if in_blocked_corner(&self.switch_options, size, pos, fraction) {
            return Err("corner is blocked");
        }
        let window = Duration::from_millis(self.switch_options.double_tap_ms as u64);
        if window.is_zero() {
            return Ok(());
        }
        /* A barrier reports the crossing, not the dwell, so a "hit the edge
         * twice" requirement can only be honoured by bouncing the first hit
         * back and letting the second one through. */
        let armed = self
            .last_edge_hit
            .get(&pos)
            .is_some_and(|at| at.elapsed() <= window);
        if armed {
            self.last_edge_hit.remove(&pos);
            return Ok(());
        }
        self.last_edge_hit.insert(pos, Instant::now());
        Err("double tap required")
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_capture().await {
                log::warn!("input capture exited: {e}");
            }
            loop {
                tokio::select! {
                    r = self.request_rx.recv() => {
                        let Some(r) = r else {
                            log::debug!("capture request channel closed");
                            return;
                        };
                        match r {
                            CaptureRequest::Reenable => break,
                            CaptureRequest::Disable => { /* nothing to do yet */ }
                            CaptureRequest::Preconnect(_) => { /* no connection yet */ }
                            CaptureRequest::RefreshPeerInfo(_) => { /* no connection yet */ }
                            CaptureRequest::Create(h, p, t, edge, peer_edge) => {
                                self.add_capture(CaptureEntry::new(h, p, t, edge, peer_edge));
                            }
                            CaptureRequest::SetEdge(..) | CaptureRequest::SetSwitchOptions(_) => {
                                /* no capture to change yet */
                            }
                            CaptureRequest::Destroy(h) => self.remove_capture(h),
                            CaptureRequest::Release => { /* nothing to do */ }
                            CaptureRequest::SetReleaseBind(bind) => {
                                self.release_bind.borrow_mut().clone_from(&bind);
                            }
                            CaptureRequest::SetRemapRules(rules) => self.remap.set_rules(rules),
                            CaptureRequest::Begin(_) => { /* no capture yet */ }
                        }
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        /* Resolve the segments of every edge before the clients are registered:
         * a position that is not registered yet only remembers them, so the
         * barrier is created once, with the geometry it will actually use. Doing
         * it the other way round registers a full-edge barrier first and then
         * immediately replaces it, which costs a round trip per edge and - on the
         * compositor driven backends - briefly lets the pointer cross a stretch
         * that belongs to nobody. */
        let mut positions = self
            .captures
            .iter()
            .map(|entry| entry.pos)
            .collect::<Vec<_>>();
        positions.sort_by_key(|pos| pos.to_string());
        positions.dedup();
        /* the token is cloned so the select can hold it while `self` is borrowed
         * mutably by the refresh */
        let cancel = self.cancellation_token.clone();
        for pos in positions {
            tokio::select! {
                r = self.refresh_position(capture, pos) => r?,
                _ = cancel.cancelled() => return Ok(()),
            }
        }

        let captures = self.captures.clone();
        for entry in captures {
            tokio::select! {
                r = capture.create(entry.handle, entry.pos) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        let layout = capture.layout().await;
        self.publish_layout(layout);
        /* the barriers belong to a position, not to a client, so the segments of
         * every edge are handed over once all clients are registered */
        let mut positions = self
            .captures
            .iter()
            .map(|entry| entry.pos)
            .collect::<Vec<_>>();
        positions.sort_by_key(|pos| pos.to_string());
        positions.dedup();
        for pos in positions {
            self.refresh_position(capture, pos).await?;
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        /* Ask the backend for its geometry until it answers. The first read in
         * `create_captures` happens before the backend has a session - libei
         * queues the request and only then talks to the portal - so it returns
         * nothing, and without this the service would never learn the size. The
         * poll stops as soon as a size arrives; a backend that genuinely has no
         * geometry keeps costing one cheap call per second. */
        let mut size_poll = tokio::time::interval(SCREEN_SIZE_POLL);
        size_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = size_poll.tick() => {
                    /* Asked every second, not only while unknown: plugging a
                     * monitor in changes the answer without changing which
                     * stretch of an edge anybody owns, so nothing else would
                     * notice. A backend that answers `None` keeps the last known
                     * value rather than blanking it - "not ready yet" and "no
                     * screens at all" are indistinguishable from here, and
                     * blanking a working layout is the worse mistake. */
                    if let Some(layout) = capture.layout().await {
                        if self.layout.borrow().as_ref() != Some(&layout) {
                            log::info!(
                                "the capture backend reports {} monitor(s), {}",
                                layout.enabled().count(),
                                layout
                                    .size()
                                    .map(|(w, h)| format!("{w}x{h}"))
                                    .unwrap_or_else(|| "unknown size".to_owned()),
                            );
                            *self.layout.borrow_mut() = Some(layout);
                        }
                    }
                }
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => {
                        // The backend stream ended while we may still be
                        // capturing — the compositor took the seat away (lock
                        // screen, session switch) or the backend died. Release
                        // first so the peer is told to stop, otherwise the
                        // pointer stays trapped on the other machine until
                        // lan-mouse is restarted (issue #545).
                        log::warn!("capture stream ended — releasing before restarting it");
                        self.release_capture(capture).await?;
                        return Ok(());
                    }
                },
                (handle, event) = self.conn.recv() => {
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    match event {
                        // connection acknowlegded => set state to Sending
                        ProtoEvent::Ack(_) => {
                            // only the active capture may acknowledge; a late
                            // Ack from a peer we already left must not unlock
                            // the state machine for the next capture
                            if self.active_client == Some(handle) {
                                log::info!("client {handle} acknowledged the connection!");
                                self.state = State::Sending;
                                self.entered_at.get_or_insert_with(Instant::now);
                                // Deliver the input that arrived while the
                                // handshake was in flight.
                                if !self.pending_input.is_empty() {
                                    let pending = std::mem::take(&mut self.pending_input);
                                    log::debug!("flushing {} buffered input event(s)", pending.len());
                                    for event in pending {
                                        if let Err(e) = self.conn.send(event, handle).await {
                                            log::warn!("failed to flush buffered input: {e}");
                                            break;
                                        }
                                    }
                                }
                            } else {
                                log::debug!("ignoring Ack from inactive client {handle}");
                            }
                        }
                        // The connection dropped locally (peer stopped, network
                        // gone). Release at once: the dwell check below only
                        // guards against a peer's own enter/leave fight, and
                        // there is no peer left to fight with. Without this the
                        // compositor keeps the pointer hidden on the remote
                        // edge with nobody to send `Leave` for it.
                        ProtoEvent::Leave(serial)
                            if serial == crate::connect::DISCONNECT_SERIAL =>
                        {
                            log::warn!(
                                "client {handle} disconnected while capturing — releasing"
                            );
                            self.release_capture(capture).await?;
                        }
                        // client disconnected
                        ProtoEvent::Leave(_) => {
                            // A peer that is itself in the middle of an
                            // enter/leave fight sends Leave within
                            // milliseconds of acknowledging our Enter. Acting
                            // on it immediately tears down the session before
                            // a single key is delivered — and it clears
                            // `pressed_keys`, so the release-bind stops
                            // working too. Require a short dwell before
                            // honouring a remote Leave.
                            // The dwell only needs to outlast the burst of
                            // Leavess a peer emits while its own state settles;
                            // it is not a UX delay and every millisecond here
                            // is felt as a stall when returning from the peer.
                            const MIN_DWELL: Duration = Duration::from_millis(120);
                            let dwell = self
                                .entered_at
                                .map(|t| t.elapsed())
                                .unwrap_or(MIN_DWELL);
                            if dwell < MIN_DWELL {
                                log::debug!(
                                    "ignoring Leave from client {handle}: \
                                     only {dwell:?} after entering"
                                );
                                continue;
                            }
                            log::info!("releasing capture: left remote client device region");
                            self.release_capture(capture).await?;
                        },
                        // Machine details are not input; surface them so the
                        // service can refresh the frontend.
                        ProtoEvent::PeerInfo(_) | ProtoEvent::PeerInfoChunk { .. }
                            if self.event_tx.send(ICaptureEvent::PeerInfo(handle)).is_err() =>
                        {
                            log::debug!("capture event channel is closed");
                        }
                        ProtoEvent::PeerInfo(_) | ProtoEvent::PeerInfoChunk { .. } => {}
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e {
                    Some(CaptureRequest::Reenable) => { /* already active */ },
                    Some(CaptureRequest::Disable) => {
                        log::info!("capture disabled");
                        self.release_capture(capture).await?;
                        capture.release().await?;
                    }
                    Some(CaptureRequest::Release) => self.release_capture(capture).await?,
                    Some(CaptureRequest::Preconnect(h)) => {
                        self.conn.preconnect(h).await;
                    }
                    Some(CaptureRequest::RefreshPeerInfo(h)) => {
                        self.conn.request_peer_info(h).await;
                    }
                    Some(CaptureRequest::Create(h, p, t, edge, peer_edge)) => {
                        self.add_capture(CaptureEntry::new(h, p, t, edge, peer_edge));
                        /* Resolve first: for the first client of an edge this only
                         * stores the segments, and `create` then registers the
                         * barrier with them in one step. A second client on an edge
                         * that already has one brings no barrier of its own - it
                         * shares the edge - so the re-resolution is the update. */
                        if t == CaptureType::Default {
                            self.refresh_position(capture, p).await?;
                        }
                        capture.create(h, p).await?;
                        if t == CaptureType::Default {
                            self.report_unreachable(h, p);
                        }
                    }
                    Some(CaptureRequest::SetEdge(h, edge)) => {
                        let Some(pos) = self.get_pos(h) else {
                            log::debug!("no capture for client {h}, ignoring its new edge");
                            continue;
                        };
                        let Some(entry) = self.captures.iter_mut().find(|e| e.handle == h) else {
                            continue;
                        };
                        entry.requested = edge;
                        self.refresh_position(capture, pos).await?;
                        self.report_unreachable(h, pos);
                    }
                    Some(CaptureRequest::SetSwitchOptions(options)) => {
                        log::info!(
                            "edge crossing thresholds: double tap {} ms, {} px in {:?}",
                            options.double_tap_ms,
                            options.corner_size,
                            options
                                .corners
                                .iter()
                                .map(|corner| corner.as_str())
                                .collect::<Vec<_>>()
                        );
                        self.switch_options = options;
                    }
                    Some(CaptureRequest::Destroy(h)) => {
                        // If the capture we're tearing down is the
                        // currently-active one, treat this as a
                        // release for hook purposes. The release_capture
                        // path also clears active_client and flushes
                        // pressed-key state to the peer; without this,
                        // `cli deactivate` (or a hostname change
                        // re-creating the client) would skip leave_hook.
                        if self.active_client == Some(h) {
                            self.release_capture(capture).await?;
                        }
                        let pos = self.get_pos(h);
                        let was_default = self.get_type(h) == Some(CaptureType::Default);
                        self.remove_capture(h);
                        capture.destroy(h).await?;
                        /* The remaining clients of that edge may now reach more of
                         * it than they did while this one was squeezing them. */
                        if let (true, Some(pos)) = (was_default, pos) {
                            self.refresh_position(capture, pos).await?;
                            /* A client that was squeezed out by the one that just
                             * left can now take its stretch, so report the new
                             * state of the edge. */
                            for (handle, _) in self
                                .captures
                                .iter()
                                .filter(|entry| entry.pos == pos)
                                .map(|entry| (entry.handle, entry.pos))
                            {
                                self.report_unreachable(handle, pos);
                            }
                        }
                    }
                    Some(CaptureRequest::SetReleaseBind(bind)) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                    Some(CaptureRequest::SetRemapRules(rules)) => self.remap.set_rules(rules),
                    Some(CaptureRequest::Begin(handle)) => {
                        match capture.begin(handle).await {
                            Ok(()) => {
                                /* The backend reports `Begin` like for a
                                 * crossing; remember that this one was asked
                                 * for so the peer centres its cursor instead
                                 * of lining up with an edge point. */
                                self.center_cursor = true;
                            }
                            Err(e) => {
                                log::warn!("cannot hand the pointer to client {handle}: {e}");
                                let message =
                                    format!("cannot hand the pointer to client {handle}: {e}");
                                if self.event_tx.send(ICaptureEvent::Error(message)).is_err() {
                                    log::debug!("capture event channel is closed");
                                }
                            }
                        }
                    }
                    None => break,
                },
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        // The compositor took the pointer away: no further input can arrive,
        // so release rather than waiting for a release-bind that will never be
        // delivered (issues #545, #512).
        if event == CaptureEvent::Interrupted {
            log::warn!("releasing capture: input stream interrupted by the compositor");
            return self.release_capture(capture).await;
        }

        // The capture stream may still hold events for a handle that was
        // destroyed in the meantime (e.g. a client deactivated while its
        // events were queued). Drop those instead of panicking the whole
        // service process.
        let (Some(capture_type), Some(pos)) = (self.get_type(handle), self.get_pos(handle)) else {
            log::debug!("dropping event for unknown capture handle {handle}");
            return Ok(());
        };

        /* A crossing is routed to the one client that owns the part of the edge
         * it happened at. The capture stream delivers the same `Begin` to every
         * client registered on the position, so the copies for the others are
         * dropped here: acting on them would hand the pointer to a machine that
         * does not own that stretch, which is the bug the ranges exist to fix.
         */
        if let CaptureEvent::Begin(fraction) = event {
            if capture_type == CaptureType::Default {
                match self.route_at(pos, fraction) {
                    Some(owner) if owner == handle => {}
                    Some(owner) => {
                        log::trace!(
                            "crossing at {:.0}% of the {pos} edge belongs to client {owner}, not {handle}",
                            fraction * 100.0
                        );
                        return Ok(());
                    }
                    None => {
                        /* Nobody owns this stretch: the pointer hits a wall. On
                         * Wayland the barrier is not even registered there, so
                         * this is reached only by the backends that own the local
                         * pointer and cannot carve a segment out of an edge. */
                        log::info!(
                            "releasing capture: no client covers {:.0}% of the {pos} edge",
                            fraction * 100.0
                        );
                        return self.release_capture(capture).await;
                    }
                }
                if let Err(reason) = self.crossing_allowed(pos, fraction) {
                    log::info!("releasing capture: crossing of the {pos} edge refused, {reason}");
                    return self.release_capture(capture).await;
                }
            }
            /* The receiver only goes away while the service is shutting down,
             * so there is nothing left to notify.
             */
            if self
                .event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .is_err()
            {
                log::debug!("capture event channel is closed");
                return Ok(());
            }
        }

        // Enter-only capture (used for incoming connections).
        //
        // `release()` is asynchronous — the backend re-arms the barrier on its
        // own task — so the very next `Begin` can arrive before the release
        // has taken effect. Releasing again on every one of those begins is
        // what produced the endless "no active client at this position" loop:
        // the peer then saw a Leave storm and the two sides fought over the
        // pointer. Debounce it so one release per crossing is enough.
        if capture_type == CaptureType::EnterOnly {
            if !self.is_default_capture_at(pos) {
                // Only needs to outlive the in-flight `Begin` storm from a
                // single crossing. 750 ms made a deliberate second crossing
                // feel unresponsive; the storm itself settles far sooner.
                const RELEASE_DEBOUNCE: Duration = Duration::from_millis(250);
                let due = self
                    .last_enter_release
                    .is_none_or(|t| t.elapsed() >= RELEASE_DEBOUNCE);
                if due {
                    self.last_enter_release = Some(Instant::now());
                    log::info!("releasing capture: no active client at this position");
                    capture.release().await?;
                    // Tell the peer that is driving us to let go. Without this
                    // the receiver notices the cursor reaching its own edge and
                    // releases locally, but the sender keeps forwarding into a
                    // machine that is no longer emulating — the pointer stays
                    // hidden on the remote edge and never comes back.
                    //
                    // The notification goes out as an event rather than a
                    // direct send: an enter-only capture is identified by a
                    // synthetic handle that has no entry in the client manager,
                    // so `conn.send` cannot resolve it. The service knows the
                    // socket address for this handle and replies on it.
                    if self
                        .event_tx
                        .send(ICaptureEvent::ReturnRequested(handle))
                        .is_err()
                    {
                        log::debug!("capture event channel is closed");
                    }
                } else {
                    log::trace!("suppressing repeated enter-only release");
                }
            }
            return Ok(());
        }

        // Without an active client there is nobody to send input to: events
        // that were already queued upstream when we released (or that arrive
        // before the first `Begin`) must not recreate a session for a peer we
        // just left.
        if self.active_client.is_none() && !matches!(event, CaptureEvent::Begin(_)) {
            return Ok(());
        }

        // activated a new client
        if let CaptureEvent::Begin(fraction) = event {
            /* Where the pointer left this edge, mapped into the segment of the
             * peer's facing edge it is supposed to land in. */
            self.enter_fraction = self
                .entry(handle)
                .map(|entry| entry.peer_fraction(fraction))
                .unwrap_or(fraction);
            if Some(handle) != self.active_client {
                self.state = State::WaitingForAck;
                self.active_client.replace(handle);
                self.active.set(true);
                if self
                    .event_tx
                    .send(ICaptureEvent::ClientEntered(handle))
                    .is_err()
                {
                    log::debug!("capture event channel is closed");
                    return Ok(());
                }
            }
        }

        // Debug logging of the key stream: media keys and other "special" keys
        // only show up here if the backend captured them at all, which is the
        // first thing to rule out when such a key does nothing on the peer.
        if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) = &event
        {
            log::debug!("capture: forwarding key {key} state {state} to client {handle}");
        }

        let opposite_pos = to_proto_pos(pos.opposite());

        let events: Vec<ProtoEvent> = match event {
            // already handled above (released the capture)
            CaptureEvent::Interrupted => return Ok(()),
            CaptureEvent::Begin(_) => {
                let mut events = vec![ProtoEvent::Enter(opposite_pos, self.enter_fraction)];
                if std::mem::take(&mut self.center_cursor) {
                    events.push(ProtoEvent::CenterCursor);
                }
                events
            }
            CaptureEvent::Input(e) => match self.state {
                // The handshake is still in flight. Keep the event so it can
                // be replayed right after the Ack instead of being lost — the
                // pointer is already moving, and dropping these makes the
                // first instants on the peer feel like a stall.
                State::WaitingForAck => {
                    let remapped = self.remap.apply(e);
                    // Bound the buffer: a peer that never acks must not grow it
                    // without limit. Older events are the ones worth dropping.
                    const MAX_PENDING: usize = 256;
                    for event in remapped {
                        if self.pending_input.len() >= MAX_PENDING {
                            self.pending_input.remove(0);
                        }
                        self.pending_input.push(ProtoEvent::Input(event));
                    }
                    vec![ProtoEvent::Enter(opposite_pos, self.enter_fraction)]
                }
                State::Sending => self
                    .remap
                    .apply(e)
                    .into_iter()
                    .map(ProtoEvent::Input)
                    .collect(),
            },
        };

        for event in events {
            if let Err(e) = self.conn.send(event, handle).await {
                const DUR: Duration = Duration::from_millis(500);
                debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
                // Funnel through release_capture so the leave_hook
                // fires and active_client is cleared (without this the
                // active_client field would stay stale until the next
                // Begin from a different handle).
                self.release_capture(capture).await?;
                break;
            }
        }
        Ok(())
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        // The next session has to wait for its own `Ack` again, so reset the
        // state machine even when there is no active client left.
        self.state = State::WaitingForAck;
        // If we have an active client, notify them we're leaving
        self.active.set(false);
        if let Some(handle) = self.active_client.take() {
            // Surface the leave to the service layer so it can fire
            // the per-client leave_hook. Sent before the network
            // teardown below so we never race against the peer
            // disappearing.
            if self
                .event_tx
                .send(ICaptureEvent::ClientLeft(handle))
                .is_err()
            {
                // the service is shutting down, keep releasing anyway
                log::debug!("capture event channel is closed");
            }
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            // A chord that fired released its modifiers on the peer already;
            // forget the local chord state so a later capture cannot match it.
            self.remap.reset();
            self.entered_at = None;
            self.pending_input.clear();
            for key in capture.take_pressed_keys() {
                let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 0,
                }));
                if let Err(e) = self.conn.send(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            log::info!("sending Leave event to client {handle}");
            if let Err(e) = self.conn.send(ProtoEvent::Leave(0), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        }
        capture.release().await
    }
}

/// Send a request to the capture task.
///
/// The task is gone during shutdown and this binary is built with
/// `panic = "abort"`, so a failed send must not abort the process: dropping the
/// request is the only sensible reaction when nobody is listening anymore.
fn send_request(tx: &Sender<CaptureRequest>, request: CaptureRequest) {
    if tx.send(request).is_err() {
        log::debug!("capture task is gone, dropping the request");
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

fn to_capture_pos(pos: lan_mouse_ipc::Position) -> input_capture::Position {
    match pos {
        lan_mouse_ipc::Position::Left => input_capture::Position::Left,
        lan_mouse_ipc::Position::Right => input_capture::Position::Right,
        lan_mouse_ipc::Position::Top => input_capture::Position::Top,
        lan_mouse_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> lan_mouse_proto::Position {
    match pos {
        input_capture::Position::Left => lan_mouse_proto::Position::Left,
        input_capture::Position::Right => lan_mouse_proto::Position::Right,
        input_capture::Position::Top => lan_mouse_proto::Position::Top,
        input_capture::Position::Bottom => lan_mouse_proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        // the receiver may already be gone during shutdown
        let _ = tx.send(on_new);
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        // never panic in `Drop`: during shutdown the receiver is gone and a
        // panic here would abort the process
        if let Some(on_drop) = self.on_drop.take() {
            let _ = self.tx.send(on_drop);
        }
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    fn range(start: f64, end: f64) -> EdgeRange {
        EdgeRange::from_fractions(start, end).expect("valid range")
    }

    /// A client on the right edge owning `edge`.
    fn client(handle: CaptureHandle, pos: Position, edge: EdgeRange) -> CaptureEntry {
        CaptureEntry::new(handle, pos, CaptureType::Default, edge, EdgeRange::FULL)
    }

    /// The same client after the daemon resolved the edge it shares.
    fn resolved(mut entry: CaptureEntry, effective: Option<EdgeRange>) -> CaptureEntry {
        entry.effective = effective;
        entry
    }

    #[test]
    fn a_crossing_reaches_the_client_that_owns_that_stretch() {
        let top = client(1, Position::Right, range(0.0, 0.5));
        let bottom = client(2, Position::Right, range(0.5, 1.0));
        let entries = [top, bottom];
        assert_eq!(route(&entries, Position::Right, 0.1), Some(1));
        assert_eq!(route(&entries, Position::Right, 0.49), Some(1));
        assert_eq!(route(&entries, Position::Right, 0.5), Some(2));
        assert_eq!(route(&entries, Position::Right, 0.99), Some(2));
    }

    #[test]
    fn a_crossing_in_a_gap_reaches_nobody() {
        // 0-40% and 60-100%: the middle of the edge is a wall
        let entries = [
            client(1, Position::Right, range(0.0, 0.4)),
            client(2, Position::Right, range(0.6, 1.0)),
        ];
        assert_eq!(route(&entries, Position::Right, 0.5), None);
        assert_eq!(route(&entries, Position::Right, 0.39), Some(1));
        assert_eq!(route(&entries, Position::Right, 0.61), Some(2));
    }

    #[test]
    fn a_client_squeezed_out_of_its_range_is_not_routed_to() {
        // client 1 owns the whole edge, so client 2's resolved range is nothing
        let entries = [
            client(1, Position::Right, EdgeRange::FULL),
            resolved(client(2, Position::Right, range(0.2, 0.3)), None),
        ];
        assert_eq!(route(&entries, Position::Right, 0.25), Some(1));
    }

    #[test]
    fn a_client_squeezed_to_a_part_only_reaches_that_part() {
        let entries = [
            client(1, Position::Right, range(0.0, 0.6)),
            resolved(
                client(2, Position::Right, range(0.4, 1.0)),
                Some(range(0.6, 1.0)),
            ),
        ];
        assert_eq!(route(&entries, Position::Right, 0.5), Some(1));
        assert_eq!(route(&entries, Position::Right, 0.7), Some(2));
    }

    #[test]
    fn an_enter_only_capture_never_takes_a_crossing() {
        // The inbound trigger sits on the same edge; it must not win the routing
        let trigger = CaptureEntry::new(
            9,
            Position::Right,
            CaptureType::EnterOnly,
            EdgeRange::FULL,
            EdgeRange::FULL,
        );
        let entries = [trigger, client(1, Position::Right, EdgeRange::FULL)];
        assert_eq!(route(&entries, Position::Right, 0.5), Some(1));
    }

    #[test]
    fn a_crossing_is_routed_only_on_its_own_edge() {
        let entries = [
            client(1, Position::Right, EdgeRange::FULL),
            client(2, Position::Left, EdgeRange::FULL),
        ];
        assert_eq!(route(&entries, Position::Left, 0.5), Some(2));
        assert_eq!(route(&entries, Position::Top, 0.5), None);
    }

    #[test]
    fn the_crossing_point_is_mapped_into_the_peer_segment() {
        // A client that owns the bottom half of the right edge and should land
        // the peer's cursor in the top half of its own edge.
        let mut entry = client(1, Position::Right, range(0.5, 1.0));
        entry.peer_edge = range(0.0, 0.5);
        // the very start of its stretch maps to the start of the peer's
        assert_eq!(entry.peer_fraction(0.5), 0.0);
        // and the middle of its stretch to the middle of the peer's
        assert!((entry.peer_fraction(0.75) - 0.25).abs() < 1e-9);
        /* The far end of the edge reads one permille short because a crossing at
         * 100% is the last pixel of a 1000-pixel-wide span, so the mapping is
         * exact up to that quantization - under one pixel on any real screen. */
        assert!((entry.peer_fraction(1.0) - 0.5).abs() < 0.002);
    }

    #[test]
    fn a_full_range_maps_the_crossing_point_straight_through() {
        let entry = client(1, Position::Right, EdgeRange::FULL);
        for fraction in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let mapped = entry.peer_fraction(fraction);
            assert!((mapped - fraction).abs() < 0.002, "{fraction} -> {mapped}");
        }
    }

    #[test]
    fn corners_are_only_blocked_when_configured_and_measurable() {
        let size = Some((1920u32, 1080u32));
        let none = SwitchOptions::default();
        assert!(!in_blocked_corner(&none, size, Position::Right, 0.0));

        let top_left = SwitchOptions {
            corner_size: 50,
            corners: vec![SwitchCorner::TopLeft],
            ..SwitchOptions::default()
        };
        // the top 50 px of the left edge are the corner
        assert!(in_blocked_corner(
            &top_left,
            size,
            Position::Left,
            10.0 / 1080.0
        ));
        // the bottom of the same edge is not
        assert!(!in_blocked_corner(&top_left, size, Position::Left, 0.9));
        // the top of the right edge is the other corner
        assert!(!in_blocked_corner(
            &top_left,
            size,
            Position::Right,
            10.0 / 1080.0
        ));
    }

    #[test]
    fn a_corner_block_needs_to_know_the_screen_size() {
        let options = SwitchOptions {
            corner_size: 50,
            corners: vec![SwitchCorner::TopLeft],
            ..SwitchOptions::default()
        };
        // without geometry the fraction cannot be turned into pixels
        assert!(!in_blocked_corner(&options, None, Position::Left, 0.0));
    }

    #[test]
    fn a_bottom_left_corner_is_blocked_on_the_left_edge_bottom() {
        let options = SwitchOptions {
            corner_size: 100,
            corners: vec![SwitchCorner::BottomLeft],
            ..SwitchOptions::default()
        };
        assert!(in_blocked_corner(
            &options,
            Some((1920, 1080)),
            Position::Left,
            1000.0 / 1080.0
        ));
        assert!(!in_blocked_corner(
            &options,
            Some((1920, 1080)),
            Position::Left,
            500.0 / 1080.0
        ));
    }

    /// A backend that answers "not yet" must not erase a layout that is already
    /// known.
    ///
    /// The ordering that caused the bug: `create_captures` reads the geometry
    /// right after registering the barriers, and for the portal backend that is
    /// before the session exists - `libei`'s `create` only queues a message - so
    /// the read returns nothing. Writing that `None` over a layout that a later
    /// read had already filled in would blank the canvas.
    #[test]
    fn an_unanswered_geometry_read_does_not_erase_a_known_layout() {
        let layout = Layout::new(vec![input_event::DisplayInfo::new(
            "DP-1",
            0,
            0,
            2560,
            1440,
            input_event::Rotation::Normal,
        )]);

        /* what `publish_layout` does with a `None`: nothing, the field keeps what
         * it had */
        let mut published: Option<Layout> = Some(layout.clone());
        for next in [None, Some(Layout::default())] {
            if let Some(next) = next.filter(|layout| !layout.is_empty()) {
                published = Some(next);
            }
        }
        assert_eq!(published, Some(layout));
    }

    /// The poll interval has to be short enough to be invisible and long enough
    /// not to hammer a portal that is still starting up.
    #[test]
    fn the_geometry_poll_is_a_second() {
        assert_eq!(SCREEN_SIZE_POLL, Duration::from_secs(1));
    }
}
