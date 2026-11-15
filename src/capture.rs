use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, scancode};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::connect::LanMouseConnection;
use crate::remap::{Remap, Rule};

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
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
    /// The previously active client was left, i.e. capture
    /// was released for the given handle. Mirrors
    /// [`ICaptureEvent::ClientEntered`] for the leave side
    /// and fires on every release path (release-bind chord,
    /// remote `Leave`, explicit `Release` request, send
    /// failure, or destroy of the active capture).
    ClientLeft(u64),
}

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
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
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
        let capture_task = CaptureTask {
            active_client: None,
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
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
            active,
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
    ) {
        let pos = to_capture_pos(pos);
        send_request(
            &self.request_tx,
            CaptureRequest::Create(handle, pos, capture_type),
        );
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        send_request(&self.request_tx, CaptureRequest::Destroy(handle));
    }

    pub(crate) fn release(&self) {
        send_request(&self.request_tx, CaptureRequest::Release);
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

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
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
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t)| p == pos && t == CaptureType::Default)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Option<Position> {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .map(|&(_, p, _)| p)
    }

    fn get_type(&self, handle: CaptureHandle) -> Option<CaptureType> {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .map(|&(_, _, t)| t)
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
                            CaptureRequest::Create(h, p, t) => self.add_capture(h, p, t),
                            CaptureRequest::Destroy(h) => self.remove_capture(h),
                            CaptureRequest::Release => { /* nothing to do */ }
                            CaptureRequest::SetReleaseBind(bind) => {
                                self.release_bind.borrow_mut().clone_from(&bind);
                            }
                            CaptureRequest::SetRemapRules(rules) => self.remap.set_rules(rules),
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
        let captures = self.captures.clone();
        for (handle, pos, _type) in captures {
            tokio::select! {
                r = capture.create(handle, pos) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        loop {
            tokio::select! {
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
                    Some(CaptureRequest::Create(h, p, t)) => {
                        self.add_capture(h, p, t);
                        capture.create(h, p).await?;
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
                        self.remove_capture(h);
                        capture.destroy(h).await?;
                    }
                    Some(CaptureRequest::SetReleaseBind(bind)) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                    Some(CaptureRequest::SetRemapRules(rules)) => self.remap.set_rules(rules),
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

        if event == CaptureEvent::Begin {
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
        if self.active_client.is_none() && event != CaptureEvent::Begin {
            return Ok(());
        }

        // activated a new client
        if event == CaptureEvent::Begin && Some(handle) != self.active_client {
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
            CaptureEvent::Begin => vec![ProtoEvent::Enter(opposite_pos)],
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
                    vec![ProtoEvent::Enter(opposite_pos)]
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
