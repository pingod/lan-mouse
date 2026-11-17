use crate::config::local_commit;
use crate::listen::{LanMouseListener, ListenEvent, ListenerCreationError};
use futures::StreamExt;
use input_emulation::{EmulationHandle, InputEmulation, InputEmulationError, ScreenEdge};
use input_event::Event;
use lan_mouse_proto::{Position, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    task::{JoinHandle, spawn_local},
};

/// Send on one of the internal channels.
///
/// The receiving half is dropped during shutdown and this binary is built
/// with `panic = "abort"`, so a failed send must not panic: when nobody is
/// listening anymore the only sensible thing left to do is to drop the event.
fn send_event<T>(tx: &Sender<T>, event: T) {
    if tx.send(event).is_err() {
        log::debug!("receiver is gone, dropping the event");
    }
}

/// emulation handling events received from a listener
pub(crate) struct Emulation {
    task: JoinHandle<()>,
    request_tx: Sender<EmulationRequest>,
    event_rx: Receiver<EmulationEvent>,
}

pub(crate) enum EmulationEvent {
    Connected {
        addr: SocketAddr,
        fingerprint: String,
    },
    ConnectionAttempt {
        fingerprint: String,
    },
    /// new connection
    Entered {
        /// address of the connection
        addr: SocketAddr,
        /// position of the connection
        pos: lan_mouse_ipc::Position,
        /// certificate fingerprint of the connection
        fingerprint: String,
    },
    /// connection closed
    Disconnected {
        addr: SocketAddr,
    },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled
    EmulationDisabled,
    /// emulation was enabled
    EmulationEnabled,
    /// capture should be released
    ReleaseNotify,
    /// peer described its machine
    PeerInfo {
        addr: SocketAddr,
        info: Box<lan_mouse_proto::PeerInfo>,
    },
    /// peer sent us a Hello with its build commit hash. Used to
    /// populate `client_manager.peer_commit` from the listen side
    /// too — without this, peer-version visibility silently fails
    /// whenever the outgoing connection in the *other* direction is
    /// broken (one-way setups, asymmetric NAT, peer's TCP listener
    /// down). The connect-side path stays as the primary source;
    /// this is the defensive fallback.
    PeerHello {
        addr: SocketAddr,
        commit: [u8; 8],
    },
}

enum EmulationRequest {
    Reenable,
    /// stop injecting input until `Reenable`
    Disable,
    Release(SocketAddr),
    ChangePort(u16),
    SetScrollSmoothing(input_emulation::ScrollSmoothing),
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        smoothing: input_emulation::ScrollSmoothing,
        listener: LanMouseListener,
    ) -> Self {
        let emulation_proxy = EmulationProxy::new(backend, smoothing);
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_task = ListenTask {
            reassemblers: HashMap::new(),
            listener,
            emulation_proxy,
            request_rx,
            event_tx,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            task,
            request_tx,
            event_rx,
        }
    }

    pub(crate) fn send_leave_event(&self, addr: SocketAddr) {
        send_event(&self.request_tx, EmulationRequest::Release(addr));
    }

    pub(crate) fn reenable(&self) {
        send_event(&self.request_tx, EmulationRequest::Reenable);
    }

    /// Stop emulating input until [`Self::reenable`].
    pub(crate) fn disable(&self) {
        let _ = self.request_tx.send(EmulationRequest::Disable);
    }

    /// Change receiver-side wheel smoothing without restarting the backend.
    pub(crate) fn set_scroll_smoothing(&self, smoothing: input_emulation::ScrollSmoothing) {
        let _ = self
            .request_tx
            .send(EmulationRequest::SetScrollSmoothing(smoothing));
    }

    pub(crate) fn request_port_change(&self, port: u16) {
        send_event(&self.request_tx, EmulationRequest::ChangePort(port));
    }

    /// Wait for the next event.
    ///
    /// `None` means the emulation task has exited; the service has to shut
    /// down in that case instead of waiting for events that never arrive.
    pub(crate) async fn event(&mut self) -> Option<EmulationEvent> {
        self.event_rx.recv().await
    }

    /// wait for termination
    pub(crate) async fn terminate(&mut self) {
        log::debug!("terminating emulation");
        send_event(&self.request_tx, EmulationRequest::Terminate);
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }
}

struct ListenTask {
    listener: LanMouseListener,
    emulation_proxy: EmulationProxy,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
    /// partial peer reports, keyed by peer address
    reassemblers: HashMap<SocketAddr, crate::sysinfo::Reassembler>,
}

impl ListenTask {
    async fn run(mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut last_response = HashMap::new();
        let mut rejected_connections = HashMap::new();
        loop {
            select! {
                e = self.listener.next() => {match e {
                    Some(ListenEvent::Msg { event, addr }) => {
                        log::trace!("{event} <-<-<-<-<- {addr}");
                        last_response.insert(addr, Instant::now());
                        match event {
                            ProtoEvent::Enter(pos, fraction) => {
                                if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
                                    log::info!("releasing capture: {addr} entered this device");
                                    // `pos` is the edge of *this* device the
                                    // peer came through, `fraction` the point
                                    // along that edge it crossed at.
                                    self.emulation_proxy.enter(addr, to_screen_edge(pos), fraction);
                                    send_event(&self.event_tx, EmulationEvent::ReleaseNotify);
                                    self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                                    send_event(&self.event_tx, EmulationEvent::Entered{addr, pos: to_ipc_pos(pos), fingerprint});
                                }
                            }
                            ProtoEvent::CenterCursor => {
                                log::info!(
                                    "{addr} handed the pointer over, placing the cursor in the middle"
                                );
                                self.emulation_proxy.center_cursor(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Leave(_) => {
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => self.emulation_proxy.consume(event, addr),
                            ProtoEvent::Ping => self.listener.reply(addr, ProtoEvent::Pong(self.emulation_proxy.emulation_active.get())).await,
                            // Peer's version handshake. Echo our own
                            // commit back so the peer's connect-side
                            // receive_loop populates its `peer_commit`,
                            // AND publish a PeerHello upward so our
                            // service can populate ours from the listen
                            // side too — the connect side is the primary
                            // path, but if the outbound direction is
                            // broken (one-way setup, NAT, peer's TCP
                            // listener down) the version display would
                            // otherwise silently say "unknown" while
                            // the peer is in fact happily talking to us.
                            ProtoEvent::Hello { commit } => {
                                self.listener.reply(addr, ProtoEvent::Hello { commit: local_commit() }).await;
                                // Describe this machine in pieces: the
                                // listening side cannot push a whole record to
                                // the connecting side.
                                for chunk in crate::sysinfo::chunks(&crate::sysinfo::local()) {
                                    self.listener.reply(addr, chunk).await;
                                }
                                send_event(&self.event_tx, EmulationEvent::PeerHello { addr, commit });
                            }
                            ProtoEvent::PeerInfoRequest => {
                                for chunk in crate::sysinfo::chunks(&crate::sysinfo::local()) {
                                    self.listener.reply(addr, chunk).await;
                                }
                            }
                            ProtoEvent::PeerInfo(info) => {
                                send_event(
                                    &self.event_tx,
                                    EmulationEvent::PeerInfo {
                                        addr,
                                        info: Box::new(info),
                                    },
                                );
                            }
                            ProtoEvent::PeerInfoChunk {
                                index,
                                total,
                                data,
                            } => {
                                let entry = self.reassemblers.entry(addr).or_default();
                                log::debug!("peer report chunk {}/{} from {addr}", index as u32 + 1, total);
                                if let Some(info) = entry.push(index, total, &data) {
                                    log::debug!("peer report complete from {addr}: world={:?}", info.world());
                                    send_event(
                                        &self.event_tx,
                                        EmulationEvent::PeerInfo {
                                            addr,
                                            info: Box::new(info),
                                        },
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Accept { addr, fingerprint }) => {
                        send_event(&self.event_tx, EmulationEvent::Connected { addr, fingerprint });
                    }
                    Some(ListenEvent::Rejected { fingerprint }) => {
                        if rejected_connections.insert(fingerprint.clone(), Instant::now())
                            .is_none_or(|i| i.elapsed() >= Duration::from_secs(2)) {
                                send_event(&self.event_tx, EmulationEvent::ConnectionAttempt { fingerprint });
                            }
                    }
                    None => break
                }}
                event = self.emulation_proxy.event() => {
                    let Some(event) = event else { break };
                    send_event(&self.event_tx, event);
                }
                request = self.request_rx.recv() => match request {
                    None => break,
                    // reenable emulation
                    Some(EmulationRequest::Reenable) => self.emulation_proxy.reenable(),
                    Some(EmulationRequest::Disable) => self.emulation_proxy.disable(),
                    Some(EmulationRequest::SetScrollSmoothing(smoothing)) => {
                        self.emulation_proxy.set_scroll_smoothing(smoothing)
                    }
                    // notify the other end that we hit a barrier (should release capture)
                    Some(EmulationRequest::Release(addr)) => self.listener.reply(addr, ProtoEvent::Leave(0)).await,
                    Some(EmulationRequest::ChangePort(port)) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        send_event(&self.event_tx, EmulationEvent::PortChanged(result));
                    }
                    Some(EmulationRequest::Terminate) => break,
                },
                _ = interval.tick() => {
                    // The threshold must outlast the peer's keep-alive period,
                    // otherwise a healthy link is torn down between two pings.
                    // The connecting side sends one round every
                    // `PING_ROUND` (2 s), so allow several rounds of slack.
                    const PEER_TIMEOUT: Duration = Duration::from_secs(10);
                    last_response.retain(|&addr, instant| {
                        if instant.elapsed() > PEER_TIMEOUT {
                            log::warn!("releasing keys: {addr} not responding!");
                            self.emulation_proxy.remove(addr);
                            send_event(&self.event_tx, EmulationEvent::Disconnected { addr });
                            false
                        } else {
                            true
                        }
                    });
                }
            }
        }
        self.listener.terminate().await;
        self.emulation_proxy.terminate().await;
    }
}

/// proxy handling the actual input emulation,
/// discarding events when it is disabled
pub(crate) struct EmulationProxy {
    emulation_active: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    /// The peer entered this device through `ScreenEdge`, at that fraction
    /// (0..=1) along the edge. Also clears any prior `Leave` latch.
    Enter(SocketAddr, ScreenEdge, f64),
    /// The peer handed the pointer over on request: put the cursor in the
    /// middle of this screen.
    CenterCursor(SocketAddr),
    Remove(SocketAddr),
    /// replace the wheel smoothing configuration while the session runs
    SetScrollSmoothing(input_emulation::ScrollSmoothing),
    Terminate,
    Reenable,
    /// drop every handle so nothing is injected until `Reenable`
    Disable,
}

impl EmulationProxy {
    fn new(
        backend: Option<input_emulation::Backend>,
        smoothing: input_emulation::ScrollSmoothing,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_active = Rc::new(Cell::new(false));
        let exit_requested = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            backend,
            smoothing,
            exit_requested: exit_requested.clone(),
            request_rx,
            event_tx,
            handles: Default::default(),
            left: Default::default(),
            next_id: 0,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            emulation_active,
            exit_requested,
            request_tx,
            task,
            event_rx,
        }
    }

    async fn event(&mut self) -> Option<EmulationEvent> {
        let event = self.event_rx.recv().await?;
        if let EmulationEvent::EmulationEnabled = event {
            self.emulation_active.replace(true);
        }
        if let EmulationEvent::EmulationDisabled = event {
            self.emulation_active.replace(false);
        }
        Some(event)
    }

    fn consume(&self, event: Event, addr: SocketAddr) {
        // ignore events if emulation is currently disabled
        if self.emulation_active.get() {
            send_event(&self.request_tx, ProxyRequest::Input(event, addr));
        }
    }

    fn enter(&self, addr: SocketAddr, edge: ScreenEdge, fraction: f64) {
        send_event(&self.request_tx, ProxyRequest::Enter(addr, edge, fraction));
    }

    fn center_cursor(&self, addr: SocketAddr) {
        send_event(&self.request_tx, ProxyRequest::CenterCursor(addr));
    }

    fn remove(&self, addr: SocketAddr) {
        send_event(&self.request_tx, ProxyRequest::Remove(addr));
    }

    fn reenable(&self) {
        send_event(&self.request_tx, ProxyRequest::Reenable);
    }

    fn disable(&self) {
        let _ = self.request_tx.send(ProxyRequest::Disable);
    }

    fn set_scroll_smoothing(&self, smoothing: input_emulation::ScrollSmoothing) {
        let _ = self
            .request_tx
            .send(ProxyRequest::SetScrollSmoothing(smoothing));
    }

    async fn terminate(&mut self) {
        self.exit_requested.replace(true);
        send_event(&self.request_tx, ProxyRequest::Terminate);
        let _ = (&mut self.task).await;
    }
}

struct EmulationTask {
    backend: Option<input_emulation::Backend>,
    smoothing: input_emulation::ScrollSmoothing,
    exit_requested: Rc<Cell<bool>>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    /// Peers that sent `Leave` and have not sent a new `Enter` since.
    ///
    /// Without this, an `Input` that arrives after `Leave` silently recreates
    /// a handle and keeps injecting: the sender believes it released the
    /// pointer while the receiver keeps typing into this machine (issue #542,
    /// "release not taking effect").
    ///
    /// Only packets that race the Leave are dropped. A hotkey legitimately
    /// arrives without a preceding Enter, so the latch expires: after
    /// `LEAVE_GRACE` the peer is treated as starting a new session.
    left: std::collections::HashMap<SocketAddr, Instant>,
    next_id: EmulationHandle,
}

impl EmulationTask {
    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_emulation().await {
                log::warn!("input emulation exited: {e}");
            }
            if self.exit_requested.get() {
                break;
            }
            // wait for reenable request
            loop {
                match self.request_rx.recv().await {
                    Some(ProxyRequest::Reenable) => break,
                    // the proxy was dropped: nothing left to emulate for
                    None | Some(ProxyRequest::Terminate) => return,
                    Some(ProxyRequest::Input(..)) => { /* emulation inactive => ignore */ }
                    Some(ProxyRequest::Enter(..)) => { /* emulation inactive => ignore */ }
                    Some(ProxyRequest::CenterCursor(..)) => { /* inactive => ignore */ }
                    Some(ProxyRequest::Remove(..)) => { /* emulation inactive => ignore */ }
                    Some(ProxyRequest::SetScrollSmoothing(smoothing)) => self.smoothing = smoothing,
                    Some(ProxyRequest::Disable) => { /* already inactive */ }
                }
            }
        }
    }

    async fn do_emulation(&mut self) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let mut emulation = tokio::select! {
            r = InputEmulation::new(self.backend, self.smoothing) => r?,
            // allow termination event while requesting input emulation
            _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
        };

        // used to send enabled and disabled events
        let _emulation_guard = DropGuard::new(
            self.event_tx.clone(),
            EmulationEvent::EmulationEnabled,
            EmulationEvent::EmulationDisabled,
        );

        // create active handles
        if let Err(e) = self.create_clients(&mut emulation).await {
            emulation.terminate().await;
            return Err(e);
        }

        let res = self.do_emulation_session(&mut emulation).await;
        // FIXME replace with async drop when stabilized
        emulation.terminate().await;
        res
    }

    async fn create_clients(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        for handle in self.handles.values() {
            tokio::select! {
                _ = emulation.create(*handle) => {},
                _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_emulation_session(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        loop {
            /* The smoother has no clock of its own: sleep until its next step
             * is due, otherwise wait for the network. */
            let scroll_tick = emulation.next_scroll_tick();
            tokio::select! {
                _ = async {
                    match scroll_tick {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => emulation.scroll_tick().await?,
                e = self.request_rx.recv() => match e {
                    // the proxy was dropped: stop emulating
                    None => break Ok(()),
                    Some(ProxyRequest::Input(event, addr)) => {
                        let now = Instant::now();
                        // Drop expired latches as we go. The map is keyed by
                        // peer address and a busy network would otherwise keep
                        // one entry per address that ever said Leave.
                        self.left
                            .retain(|_, left_at| now.duration_since(*left_at) < LEAVE_GRACE);
                        if !should_accept_input(&self.left, addr, now) {
                            log::debug!("dropping input from {addr}: peer has left");
                            continue;
                        }
                        let handle = match self.handles.get(&addr) {
                            Some(&handle) => handle,
                            None => {
                                let handle = self.next_id;
                                self.next_id += 1;
                                emulation.create(handle).await;
                                self.handles.insert(addr, handle);
                                handle
                            }
                        };
                        emulation.consume(event, handle).await?;
                    },
                    Some(ProxyRequest::Enter(addr, edge, fraction)) => {
                        self.left.remove(&addr);
                        // The pointer just crossed into this device. Make sure
                        // the peer has a virtual input to place the cursor
                        // with: input from an unknown peer creates one lazily,
                        // but the first crossing arrives before any input.
                        let handle = match self.handles.get(&addr) {
                            Some(&handle) => handle,
                            None => {
                                let handle = self.next_id;
                                self.next_id += 1;
                                emulation.create(handle).await;
                                self.handles.insert(addr, handle);
                                handle
                            }
                        };
                        emulation.enter(handle, edge, fraction).await;
                    }
                    Some(ProxyRequest::CenterCursor(addr)) => {
                        self.left.remove(&addr);
                        let handle = match self.handles.get(&addr) {
                            Some(&handle) => handle,
                            None => {
                                let handle = self.next_id;
                                self.next_id += 1;
                                emulation.create(handle).await;
                                self.handles.insert(addr, handle);
                                handle
                            }
                        };
                        emulation.enter_center(handle).await;
                    }
                    Some(ProxyRequest::Remove(addr)) => {
                        self.left.insert(addr, Instant::now());
                        if let Some(handle) = self.handles.remove(&addr) {
                            emulation.destroy(handle).await;
                        }
                    }
                    Some(ProxyRequest::Terminate) => break Ok(()),
                    Some(ProxyRequest::Reenable) => continue,
                    Some(ProxyRequest::SetScrollSmoothing(smoothing)) => {
                        self.smoothing = smoothing;
                        emulation.set_scroll_smoothing(smoothing);
                    }
                    Some(ProxyRequest::Disable) => {
                        // Drop every handle so a resumed session starts clean;
                        // the peer keeps sending until it leaves, and those
                        // events are ignored while no handle exists.
                        for (_, handle) in std::mem::take(&mut self.handles) {
                            emulation.destroy(handle).await;
                        }
                        self.left.clear();
                    }
                },
            }
        }
    }
}

fn to_screen_edge(pos: Position) -> ScreenEdge {
    match pos {
        Position::Left => ScreenEdge::Left,
        Position::Right => ScreenEdge::Right,
        Position::Top => ScreenEdge::Top,
        Position::Bottom => ScreenEdge::Bottom,
    }
}

fn to_ipc_pos(pos: Position) -> lan_mouse_ipc::Position {
    match pos {
        Position::Left => lan_mouse_ipc::Position::Left,
        Position::Right => lan_mouse_ipc::Position::Right,
        Position::Top => lan_mouse_ipc::Position::Top,
        Position::Bottom => lan_mouse_ipc::Position::Bottom,
    }
}

async fn wait_for_termination(rx: &mut Receiver<ProxyRequest>) {
    loop {
        match rx.recv().await {
            None | Some(ProxyRequest::Terminate) => return,
            Some(ProxyRequest::Input(_, _)) => continue,
            Some(ProxyRequest::Enter(..)) => continue,
            Some(ProxyRequest::CenterCursor(_)) => continue,
            Some(ProxyRequest::Remove(..)) => continue,
            Some(ProxyRequest::SetScrollSmoothing(_)) => continue,
            Some(ProxyRequest::Reenable) => continue,
            Some(ProxyRequest::Disable) => continue,
        }
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        send_event(&tx, on_new);
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        // never panic in `Drop`: the process would abort on shutdown
        if let Some(on_drop) = self.on_drop.take() {
            send_event(&self.tx, on_drop);
        }
    }
}

/// Whether input from `addr` should be injected.
///
/// A peer that sent `Leave` gave the pointer back; late events must not be
/// injected until it enters again (issue #542).
/// How long after a Leave an incoming packet is still considered a straggler.
///
/// Late packets race the Leave by milliseconds. A hotkey legitimately arrives
/// with no preceding Enter, so the latch must not be permanent: after this
/// window the peer is treated as starting a new session. Kept short so the
/// #542 protection (a stuck peer must not keep typing here) still holds, but
/// long enough to cover in-flight packets on a busy link.
const LEAVE_GRACE: Duration = Duration::from_millis(250);

fn should_accept_input(
    left: &std::collections::HashMap<SocketAddr, Instant>,
    addr: SocketAddr,
    now: Instant,
) -> bool {
    match left.get(&addr) {
        None => true,
        Some(left_at) => now.duration_since(*left_at) >= LEAVE_GRACE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([192, 168, 1, 5], port))
    }

    #[test]
    fn input_accepted_before_leave() {
        let left = HashMap::new();
        assert!(should_accept_input(&left, addr(1), Instant::now()));
    }

    #[test]
    fn straggler_dropped_right_after_leave() {
        let mut left = HashMap::new();
        let now = Instant::now();
        left.insert(addr(1), now);
        // A packet racing the Leave is still dropped (issue #542).
        assert!(!should_accept_input(&left, addr(1), now));
        assert!(!should_accept_input(&left, addr(1), now + LEAVE_GRACE / 2));
    }

    #[test]
    fn hotkey_accepted_after_grace() {
        let mut left = HashMap::new();
        let now = Instant::now();
        left.insert(addr(1), now);
        // A hotkey arrives with no preceding Enter; after the grace period it
        // must reach the emulation layer instead of being dropped forever.
        assert!(should_accept_input(&left, addr(1), now + LEAVE_GRACE));
    }

    #[test]
    fn leave_is_per_peer() {
        let mut left = HashMap::new();
        left.insert(addr(1), Instant::now());
        // a different peer is unaffected
        assert!(should_accept_input(&left, addr(2), Instant::now()));
    }
}
