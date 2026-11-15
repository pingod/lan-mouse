use crate::client::ClientManager;
use crate::config::local_commit;
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_local},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Reserved `Leave` serial marking a locally-detected disconnect.
///
/// The serial is otherwise unused on the wire, so it cannot collide with a
/// real Leave from a peer.
pub(crate) const DISCONNECT_SERIAL: u32 = u32::MAX;

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Sync + Send>, SocketAddr), (SocketAddr, LanMouseConnectionError)> {
    log::info!("connecting to {addr} ...");
    let conn = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, LanMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), LanMouseConnectionError> {
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(LanMouseConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`")
                }
            },
        };
    }
}

/// Shared handle to the connection set. Cloning yields another handle to the
/// same connections and client state — used by the hotkey task, which must be
/// able to send without owning the capture side.
pub(crate) struct LanMouseConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    /// Only the primary handle owns the receiving end; clones created for
    /// other tasks (hotkeys) get `None` so events cannot be split between two
    /// readers.
    recv_rx: Option<Receiver<(ClientHandle, ProtoEvent)>>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
}

impl Clone for LanMouseConnection {
    fn clone(&self) -> Self {
        Self {
            cert: self.cert.clone(),
            client_manager: self.client_manager.clone(),
            conns: self.conns.clone(),
            connecting: self.connecting.clone(),
            recv_rx: None,
            recv_tx: self.recv_tx.clone(),
            ping_response: self.ping_response.clone(),
        }
    }
}

impl LanMouseConnection {
    pub(crate) fn new(cert: Certificate, client_manager: ClientManager) -> Self {
        let (recv_tx, recv_rx) = channel();
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            connecting: Default::default(),
            recv_rx: Some(recv_rx),
            recv_tx,
            ping_response: Default::default(),
        }
    }

    /// Handle of the first active client. Hotkeys use this to pick a peer
    /// without a capture barrier.
    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn first_active_client(&self) -> Option<ClientHandle> {
        self.client_manager.first_active_client()
    }

    /// Start connecting to `handle` without sending anything.
    ///
    /// A capture that begins before the connection exists has to drop the
    /// first events, because the handshake needs a round trip and the pointer
    /// is already moving. Warming the connection as soon as a client is
    /// activated means the handshake usually completes before the user
    /// reaches the edge.
    pub(crate) async fn preconnect(&self, handle: ClientHandle) {
        if self.client_manager.active_addr(handle).is_some() {
            return;
        }
        let mut connecting = self.connecting.lock().await;
        if connecting.contains(&handle) {
            return;
        }
        connecting.insert(handle);
        log::debug!("preconnecting to client {handle}");
        spawn_local(connect_to_handle(
            self.client_manager.clone(),
            self.cert.clone(),
            handle,
            self.conns.clone(),
            self.connecting.clone(),
            self.recv_tx.clone(),
            self.ping_response.clone(),
        ));
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, ProtoEvent) {
        self.recv_rx
            .as_mut()
            .expect("recv() on a cloned connection handle")
            .recv()
            .await
            .expect("channel closed")
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let buf = &buf[..len];
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let conn = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(conn) = conn {
                if !self.client_manager.alive(handle) {
                    return Err(LanMouseConnectionError::TargetEmulationDisabled);
                }
                match conn.send(buf).await {
                    Ok(_) => {
                        self.client_manager.record_forwarded(handle);
                    }
                    Err(e) => {
                        log::warn!("client {handle} failed to send: {e}");
                        disconnect(
                            &self.client_manager,
                            handle,
                            addr,
                            &self.conns,
                            &self.recv_tx,
                        )
                        .await;
                    }
                }
                log::trace!("{event} >->->->->- {addr}");
                return Ok(());
            }
        }

        // check if we are already trying to connect
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            // connect in the background
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                self.conns.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                self.ping_response.clone(),
            ));
        }
        Err(LanMouseConnectionError::NotConnected)
    }
}

async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(addrs) = client_manager.get_ips(handle) {
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = addrs
            .into_iter()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let res = connect_any(&addrs, cert).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                connecting.lock().await.remove(&handle);
                return Err(e);
            }
        };
        log::info!("client ({handle}) connected @ {addr}");
        client_manager.set_active_addr(handle, Some(addr));
        conns.lock().await.insert(addr, conn.clone());
        connecting.lock().await.remove(&handle);

        // Best-effort version handshake. Send our commit hash once
        // immediately after the DTLS handshake; the listen side
        // mirrors a Hello back so the receive loop can populate
        // `peer_commit`. Old peers will silently skip this event
        // per the forward-compat handler in [`receive_loop`].
        let (buf, len) = ProtoEvent::Hello {
            commit: local_commit(),
        }
        .into();
        if let Err(e) = conn.send(&buf[..len]).await {
            log::debug!("hello send to {addr} failed: {e}");
        }

        // poll connection for active
        spawn_local(ping_pong(
            addr,
            conn.clone(),
            ping_response.clone(),
            client_manager.clone(),
        ));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            handle,
            addr,
            conn,
            conns,
            tx,
            ping_response.clone(),
        ));
        return Ok(());
    }
    connecting.lock().await.remove(&handle);
    Err(LanMouseConnectionError::NotConnected)
}

async fn ping_pong(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    client_manager: ClientManager,
) {
    loop {
        let (buf, len) = ProtoEvent::Ping.into();

        // Send four pings 500 ms apart so a single lost datagram does not tear
        // the connection down; at least one must be answered.
        //
        // The round-trip estimate is measured from the *last* ping, not from
        // the start of the round: the round itself deliberately spans 2 s, and
        // reporting that as latency made a healthy link look like 2000 ms.
        let mut sent_at: Option<Instant> = None;
        for _ in 0..4 {
            let sent = Instant::now();
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                let _ = conn.close().await;
                break;
            }
            sent_at = Some(sent);
            log::trace!("PING >->->->->- {addr}");

            // Sleep in short steps and stop as soon as a pong lands, so the
            // measurement is not padded by the full 500 ms wait.
            let deadline = sent + Duration::from_millis(500);
            while Instant::now() < deadline {
                if ping_response.borrow().contains(&addr) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if ping_response.borrow().contains(&addr) {
                break;
            }
        }

        if !ping_response.borrow_mut().remove(&addr) {
            log::warn!("{addr} did not respond, closing connection");
            let _ = conn.close().await;
            return;
        }

        // Time from the last ping to the moment its pong was observed.
        if let Some(handle) = client_manager.handle_for_addr(addr) {
            let rtt = sent_at.map(|t| t.elapsed());
            client_manager.set_health(handle, rtt, true);
        }
    }
}

async fn receive_loop(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    let mut buf = [0u8; MAX_EVENT_SIZE];
    while conn.recv(&mut buf).await.is_ok() {
        match buf.try_into() {
            Ok(event) => {
                log::trace!("{addr} <==<==<== {event}");
                match event {
                    ProtoEvent::Pong(b) => {
                        client_manager.record_received(handle);
                        client_manager.set_active_addr(handle, Some(addr));
                        client_manager.set_alive(handle, b);
                        ping_response.borrow_mut().insert(addr);
                    }
                    ProtoEvent::Hello { commit } => {
                        client_manager.set_peer_commit(handle, Some(commit));
                    }
                    event => tx.send((handle, event)).expect("channel closed"),
                }
            }
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            Err(e) => log::debug!("ignoring undecodable event from {addr}: {e}"),
        }
    }
    log::warn!("recv error");
    disconnect(&client_manager, handle, addr, &conns, &tx).await;
}

async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conns: &Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>,
    tx: &Sender<(ClientHandle, ProtoEvent)>,
) {
    log::warn!("client ({handle}) @ {addr} connection closed");
    conns.lock().await.remove(&addr);
    client_manager.set_active_addr(handle, None);
    client_manager.set_peer_commit(handle, None);
    // Clear the health flags too, otherwise the UI keeps showing the peer as
    // connected with a stale round-trip time.
    client_manager.set_disconnected(handle);
    let active: Vec<SocketAddr> = conns.lock().await.keys().copied().collect();
    log::info!("active connections: {active:?}");

    // Tell the service so it can drop the capture. Without this the sender
    // keeps believing it owns the pointer: the compositor has it hidden on the
    // remote edge and the peer is gone, so there is no one to send `Leave` and
    // the pointer can never come back (the "pointer left and never returned"
    // symptom after the receiving machine is stopped).
    //
    // A `Leave` with the reserved serial `DISCONNECT_SERIAL` is an in-process
    // marker meaning "the connection is gone", not a peer-initiated Leave. The
    // capture task acts on it immediately: the dwell check exists to ignore a
    // peer's enter/leave fight, and there is no peer left to fight with.
    let _ = tx.send((handle, ProtoEvent::Leave(DISCONNECT_SERIAL)));
}
