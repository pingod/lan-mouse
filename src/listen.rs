use futures::{Stream, StreamExt};
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    sync::Mutex as AsyncMutex,
    task::{JoinHandle, spawn_local},
};
use webrtc_dtls::{
    config::{ClientAuthType::RequireAnyClientCert, Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
    listener::listen,
};
use webrtc_util::{Conn, Error, conn::Listener};

use crate::crypto;

#[derive(Error, Debug)]
pub enum ListenerCreationError {
    #[error(transparent)]
    WebrtcUtil(#[from] webrtc_util::Error),
    #[error(transparent)]
    WebrtcDtls(#[from] webrtc_dtls::Error),
    /// the listener task exited before it reported the new port
    #[error("the listener task exited before reporting the new port")]
    Closed,
}

type ArcConn = Arc<dyn Conn + Send + Sync>;

#[allow(clippy::large_enum_variant)]
pub(crate) enum ListenEvent {
    Msg {
        event: ProtoEvent,
        addr: SocketAddr,
    },
    Accept {
        addr: SocketAddr,
        fingerprint: String,
    },
    Rejected {
        fingerprint: String,
    },
}

pub(crate) struct LanMouseListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    listen_task: JoinHandle<()>,
    conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>>,
    request_port_change: Sender<u16>,
    port_changed: Receiver<Result<u16, ListenerCreationError>>,
}

type VerifyPeerCertificateFn = Arc<
    dyn (Fn(&[Vec<u8>], &[CertificateDer<'static>]) -> Result<(), webrtc_dtls::Error>)
        + Send
        + Sync,
>;

impl LanMouseListener {
    pub(crate) async fn new(
        port: u16,
        cert: Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();
        let (request_port_change, mut request_port_change_rx) = channel();
        let (port_changed_tx, port_changed) = channel();
        let connection_attempts: Arc<Mutex<VecDeque<String>>> = Default::default();

        let authorized = authorized_keys.clone();
        let verify_peer_certificate: Option<VerifyPeerCertificateFn> = {
            let connection_attempts = connection_attempts.clone();
            Some(Arc::new(
                move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
                    // A well behaved peer sends exactly one certificate, but this
                    // runs in a DTLS callback fed by remote data: never panic here
                    // (a panic would take down the whole daemon). Reject an empty
                    // chain and use the leaf certificate of a longer chain.
                    let Some(cert) = certs.first() else {
                        log::warn!("dtls client did not present a certificate");
                        return Err(webrtc_dtls::Error::ErrVerifyDataMismatch);
                    };
                    if certs.len() > 1 {
                        log::warn!(
                            "dtls client presented {} certificates, verifying the first one",
                            certs.len()
                        );
                    }
                    let fingerprint = crypto::generate_fingerprint(cert);
                    if authorized.read().expect("lock").contains_key(&fingerprint) {
                        Ok(())
                    } else {
                        connection_attempts
                            .lock()
                            .expect("lock")
                            .push_back(fingerprint);
                        Err(webrtc_dtls::Error::ErrVerifyDataMismatch)
                    }
                },
            ))
        };
        let cfg = Config {
            certificates: vec![cert.clone()],
            extended_master_secret: ExtendedMasterSecretType::Require,
            client_auth: RequireAnyClientCert,
            verify_peer_certificate,
            ..Default::default()
        };

        let listen_addr = SocketAddr::new("0.0.0.0".parse().expect("invalid ip"), port);
        let mut listener = listen(listen_addr, cfg.clone()).await?;

        let conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>> =
            Rc::new(AsyncMutex::new(Vec::new()));

        let conns_clone = conns.clone();
        let listen_task: JoinHandle<()> = {
            let listen_tx = listen_tx.clone();
            let connection_attempts = connection_attempts.clone();
            spawn_local(async move {
                loop {
                    let sleep = tokio::time::sleep(Duration::from_secs(2));
                    tokio::select! {
                        /* workaround for https://github.com/webrtc-rs/webrtc/issues/614 */
                        _ = sleep => continue,
                        c = listener.accept() => match c {
                            Ok((conn, addr)) => {
                                log::info!("dtls client connected, ip: {addr}");
                                let Some(dtls_conn) = conn.as_any().downcast_ref::<DTLSConn>() else {
                                    log::error!("accepted a connection that is not a dtls connection, dropping it");
                                    let _ = conn.close().await;
                                    continue;
                                };
                                let certs = dtls_conn.connection_state().await.peer_certificates;
                                let Some(cert) = certs.first() else {
                                    log::warn!("dtls client {addr} sent no certificate, dropping the connection");
                                    let _ = conn.close().await;
                                    continue;
                                };
                                let fingerprint = crypto::generate_fingerprint(cert);
                                conns_clone.lock().await.push((addr, conn.clone()));
                                if listen_tx.send(ListenEvent::Accept { addr, fingerprint }).is_err() {
                                    // listener was dropped, nothing left to do
                                    break;
                                }
                                spawn_local(read_loop(conns_clone.clone(), addr, conn, listen_tx.clone()));
                            },
                            Err(e) => {
                                if let Error::Std(ref e) = e {
                                    if let Some(e) = e.0.downcast_ref::<webrtc_dtls::Error>() {
                                        match e {
                                            webrtc_dtls::Error::ErrVerifyDataMismatch => {
                                                if let Some(fingerprint) = connection_attempts.lock().expect("lock").pop_front() {
                                                    let _ = listen_tx.send(ListenEvent::Rejected { fingerprint });
                                                }
                                            }
                                            _ => log::warn!("accept: {e}"),
                                        }
                                    } else {
                                        log::warn!("accept: {e:?}");
                                    }
                                } else {
                                    log::warn!("accept: {e:?}");
                                }
                                /* A persistent accept error (e.g. EMFILE) would
                                 * spin this loop, so wait a bit before retrying.
                                 */
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        },
                        port = request_port_change_rx.recv() => {
                            let Some(port) = port else {
                                log::debug!("port change channel closed");
                                break;
                            };
                            let listen_addr = SocketAddr::new("0.0.0.0".parse().expect("invalid ip"), port);
                            match listen(listen_addr, cfg.clone()).await {
                                Ok(new_listener) => {
                                    let _ = listener.close().await;
                                    listener = new_listener;
                                    let _ = port_changed_tx.send(Ok(port));
                                }
                                Err(e) => {
                                    log::warn!("unable to change port: {e}");
                                    let _ = port_changed_tx.send(Err(e.into()));
                                }
                            };
                        },
                    };
                }
            })
        };

        Ok(Self {
            conns,
            listen_rx,
            listen_tx,
            listen_task,
            port_changed,
            request_port_change,
        })
    }

    pub(crate) fn request_port_change(&mut self, port: u16) {
        /* the listener task only goes away during shutdown; this binary is
         * built with `panic = "abort"`, so a failed send must not panic
         */
        if self.request_port_change.send(port).is_err() {
            log::debug!("listener task is gone, dropping the port change request");
        }
    }

    pub(crate) async fn port_changed(&mut self) -> Result<u16, ListenerCreationError> {
        self.port_changed
            .recv()
            .await
            .unwrap_or(Err(ListenerCreationError::Closed))
    }

    pub(crate) async fn terminate(&mut self) {
        self.listen_task.abort();
        let mut conns = self.conns.lock().await;
        for (_, conn) in conns.iter() {
            let _ = conn.close().await;
        }
        conns.clear();
        self.listen_tx.close();
    }

    pub(crate) async fn reply(&self, addr: SocketAddr, event: ProtoEvent) {
        log::debug!("reply {event} to {addr}");
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let conns = self.conns.lock().await;
        let mut sent = false;
        for (a, conn) in conns.iter() {
            if *a == addr {
                // Log the failure: a swallowed send made a broken reply path
                // look identical to a peer that never asked.
                match conn.send(&buf[..len]).await {
                    Ok(_) => sent = true,
                    Err(e) => log::warn!("reply to {addr} failed: {e}"),
                }
            }
        }
        if !sent {
            let known: Vec<SocketAddr> = conns.iter().map(|(a, _)| *a).collect();
            log::debug!("reply to {addr} dropped: known connections {known:?}");
        }
    }

    pub(crate) async fn get_certificate_fingerprint(&self, addr: SocketAddr) -> Option<String> {
        if let Some(conn) = self
            .conns
            .lock()
            .await
            .iter()
            .find(|(a, _)| *a == addr)
            .map(|(_, c)| c.clone())
        {
            let conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
            let certs = conn.connection_state().await.peer_certificates;
            let cert = certs.first()?;
            let fingerprint = crypto::generate_fingerprint(cert);
            Some(fingerprint)
        } else {
            None
        }
    }
}

impl Stream for LanMouseListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.listen_rx.poll_next_unpin(cx)
    }
}

async fn read_loop(
    conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>>,
    addr: SocketAddr,
    conn: ArcConn,
    dtls_tx: Sender<ListenEvent>,
) -> Result<(), Error> {
    let mut b = [0u8; MAX_EVENT_SIZE];

    loop {
        // Decode only the bytes actually received: the tail of `b` holds the
        // previous packet, and parsing it as part of this one yields garbage.
        let n = match conn.recv(&mut b).await {
            Ok(n) => n,
            Err(_) => break,
        };
        match b[..n].try_into() {
            Ok(event) => {
                if dtls_tx.send(ListenEvent::Msg { event, addr }).is_err() {
                    // the emulation task is gone, stop buffering events
                    break;
                }
            }
            Err(e) => {
                // Skip the malformed/unknown datagram and keep
                // listening. Each DTLS recv returns one full
                // datagram, so a parse error here can't desync a
                // stream; the next call gets a fresh, framed
                // message. This makes the protocol forward-
                // compatible: a peer running a newer Lan Mouse
                // version can introduce additional event types
                // and old peers will simply ignore them rather
                // than dropping the connection.
                log::debug!("ignoring undecodable event from {addr}: {e}");
            }
        }
    }
    log::info!("dtls client disconnected {addr:?}");
    let mut conns = conns.lock().await;
    // Compare the connection itself, not just the address: a client that
    // reconnects from the same address can have two entries here and removing
    // the wrong one would kill the fresh connection.
    if let Some(index) = conns
        .iter()
        .position(|(a, c)| *a == addr && Arc::ptr_eq(c, &conn))
    {
        conns.remove(index);
    }
    Ok(())
}
