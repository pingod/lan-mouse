use crate::hotkey_task::Hotkeys;
use crate::{
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    config::{Config, ConfigClient},
    connect::LanMouseConnection,
    crypto,
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    listen::{LanMouseListener, ListenerCreationError},
};
use futures::StreamExt;
use lan_mouse_ipc::{
    AsyncFrontendListener, ClientConfig, ClientHandle, FrontendEvent, FrontendRequest, IpcError,
    IpcListenerCreationError, Position, Status,
};
use log;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
}

pub struct Service {
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// global hotkeys (work while the pointer stays local)
    hotkeys: Hotkeys,
    /// input emulation
    emulation: Emulation,
    /// dns resolver
    resolver: DnsResolver,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    next_trigger_handle: u64,
    /// capture and emulation are torn down while paused
    paused: bool,
    /// periodic restart interval, `None` disables it
    restart_interval: Option<Duration>,
    /// when the periodic restart timer last fired
    last_restart: Instant,
    /// set when an IPC request asked for a restart
    should_exit: bool,
    /// pause state requested over IPC, applied by the main loop
    pending_pause: Option<bool>,
    /// the client the pointer is currently handed to, tracked from the
    /// capture event stream rather than inferred from connection state
    active_capture: Option<ClientHandle>,
    /// reports that arrived before their connection was registered
    pending_peer_info: HashMap<SocketAddr, lan_mouse_ipc::IncomingInfo>,
    /// when the service started, for the uptime readout
    started: Instant,
    /// names of the backends actually in use
    capture_backend_name: String,
    emulation_backend_name: String,
    /// when a frontend is streaming logs: how often to poll and where we are
    log_stream: Option<(tokio::time::Interval, usize)>,
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
    /// when this peer last sent an event
    last_seen: Instant,
    /// events received since it connected
    received: u64,
    /// what the peer reported about itself, if it sent a `PeerInfo`
    info: Option<lan_mouse_ipc::IncomingInfo>,
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        // listener + connection
        let listener =
            LanMouseListener::new(config.port(), cert.clone(), authorized_keys.clone()).await?;
        let conn = LanMouseConnection::new(cert.clone(), client_manager.clone());
        // global hotkeys: these must fire while the pointer is still local, so
        // they read evdev directly instead of going through the capture backend
        let hotkeys = Hotkeys::new(&config, conn.clone());

        // input capture + emulation
        let capture_backend_name = config
            .capture_backend()
            .map(|b| b.to_string())
            .unwrap_or_else(|| "default".to_owned());
        let emulation_backend_name = config
            .emulation_backend()
            .map(|b| b.to_string())
            .unwrap_or_else(|| "default".to_owned());
        let capture_backend = config.capture_backend().map(|b| b.into());
        // `Capture` owns the connection handle that carries `recv_rx`; the
        // service keeps a clone for sending and preconnecting. A clone has no
        // receive side, so handing the clone to the capture task would panic on
        // its first `recv()`.
        let capture = Capture::new(
            capture_backend,
            conn,
            config.release_bind(),
            config.remap_rules(),
        );
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let port = config.port();
        let config_restart_interval = config.restart_interval();
        let service = Self {
            config,
            capture,
            hotkeys,
            emulation,
            frontend_listener,
            resolver,
            authorized_keys,
            public_key_fingerprint,
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            next_trigger_handle: 0,
            paused: false,
            restart_interval: config_restart_interval,
            last_restart: Instant::now(),
            should_exit: false,
            pending_pause: None,
            active_capture: None,
            pending_peer_info: Default::default(),
            started: Instant::now(),
            capture_backend_name: capture_backend_name.clone(),
            emulation_backend_name: emulation_backend_name.clone(),
            log_stream: None,
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }

        // A `sleep` inside `select!` would be recreated every iteration and
        // effectively never fire; an interval keeps its schedule.
        let mut runtime_tick = tokio::time::interval(Duration::from_secs(1));
        runtime_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                request = self.frontend_listener.next() => {
                    if request.is_none() {
                        // The last frontend went away: stop streaming so the
                        // pending-event queue cannot grow without a reader.
                        if self.log_stream.take().is_some() {
                            log::debug!("frontend closed; stopping log stream");
                        }
                        self.pending_frontend_events.clear();
                    }
                    self.handle_frontend_request(request);
                    if let Some(paused) = self.pending_pause.take() {
                        self.set_paused(paused).await;
                    }
                }
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => match event {
                    Some(event) => self.handle_emulation_event(event),
                    None => {
                        log::error!("the emulation task exited, shutting down");
                        break;
                    }
                },
                event = self.capture.event() => match event {
                    Some(event) => self.handle_capture_event(event),
                    None => {
                        log::error!("the capture task exited, shutting down");
                        break;
                    }
                },
                fire = self.hotkeys.next() => match fire {
                    crate::hotkey_task::HotkeyFire::Keys(keys) => {
                        self.hotkeys.send(crate::hotkey_task::HotkeyFire::Keys(keys), self.capture.is_active()).await
                    }
                    crate::hotkey_task::HotkeyFire::Action(action) => match action {
                        crate::hotkey_task::HotkeyAction::SwitchDevice => self.switch_device(),
                        crate::hotkey_task::HotkeyAction::ForceRelease => {
                            log::info!("force release (hotkey)");
                            self.capture.release();
                        }
                    },
                },
                event = self.resolver.event() => match event {
                    Some(event) => self.handle_resolver_event(event),
                    None => {
                        log::error!("the dns resolver task exited, shutting down");
                        break;
                    }
                },
                _ = self.config.changed() => self.handle_config_change(),
                _ = log_tick(&mut self.log_stream) => self.flush_log_stream(),
                _ = runtime_tick.tick() => self.publish_runtime(),
                _ = restart_tick(&self.restart_interval, self.last_restart) => {
                    log::info!("scheduled restart");
                    self.last_restart = Instant::now();
                    self.should_exit = true;
                }
                r = signal::ctrl_c() => {
                    if let Err(e) = r {
                        log::error!("failed to wait for CTRL+C: {e}");
                    }
                    break;
                }
            }
            if self.should_exit {
                log::info!("exiting for restart");
                break;
            }
        }

        log::info!("terminating service ...");
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;

        Ok(())
    }

    /// Send any records written since the last flush to the streaming frontend.
    fn flush_log_stream(&mut self) {
        let Some((_, cursor)) = self.log_stream.as_mut() else {
            return;
        };
        let (lines, new_cursor) = crate::logging::since(*cursor);
        *cursor = new_cursor;
        if !lines.is_empty() {
            self.notify_frontend(FrontendEvent::LogAppended(lines));
        }
    }

    /// Suspend or resume capture and emulation without exiting.
    ///
    /// Pausing releases the pointer and stops the backends from acting while
    /// leaving the process (and its IPC socket) alive. The backends cannot be
    /// destroyed and rebuilt in place: the capture task owns the only receive
    /// half of the connection, so a fresh `Capture` built from a clone would
    /// panic on its first `recv()`. Instead the existing instances are released
    /// and told to stand down.
    async fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if paused {
            log::info!("pausing: releasing the pointer and disabling backends");
            self.capture.release();
            self.capture.disable();
            self.emulation.disable();
        } else {
            log::info!("resuming capture and emulation");
            self.capture.reenable();
            self.emulation.reenable();
        }
        self.notify_frontend(FrontendEvent::Paused(paused));
        self.notify_frontend(FrontendEvent::CaptureStatus(if paused {
            Status::Disabled
        } else {
            Status::Enabled
        }));
        self.notify_frontend(FrontendEvent::EmulationStatus(if paused {
            Status::Disabled
        } else {
            Status::Enabled
        }));
    }

    /// Hand control away from the peer that currently holds it.
    ///
    /// The capture backends can only take the pointer *back*: `release` returns
    /// it to this machine. Moving it onto another peer is what the barrier does
    /// when the user crosses that edge. So a switch means "take it back here",
    /// and the next client is pre-warmed so the handshake is done by the time
    /// the pointer reaches its edge.
    ///
    /// With no peer holding the pointer there is nothing to switch, and saying
    /// so is better than a no-op that looks like it worked.
    fn switch_device(&mut self) {
        let Some(current) = self.capturing_client() else {
            self.notify_frontend(FrontendEvent::Error(
                "nothing to switch: the pointer is already on this machine".to_owned(),
            ));
            return;
        };

        log::info!("switch: taking the pointer back from client {current}");
        self.capture.release();

        // Warm the next active client so crossing to it is instant.
        let active = self.client_manager.active_clients();
        if let Some(next) = active.iter().copied().find(|h| *h != current) {
            self.capture.preconnect(next);
        }
        self.sync_frontend();
    }

    /// The client the pointer is currently handed to, if any.
    ///
    /// Taken from the capture event stream: a connected peer is not the same
    /// as a peer holding the pointer, and inferring it from `active_addr` made
    /// "switch" jump to the wrong machine whenever a connection was idle.
    fn capturing_client(&self) -> Option<ClientHandle> {
        self.active_capture
    }

    fn handle_frontend_request(&mut self, request: Option<Result<FrontendRequest, IpcError>>) {
        let request = match request {
            Some(Ok(r)) => r,
            Some(Err(e)) => return log::error!("error receiving request: {e}"),
            // No frontend connected. The caller has already stopped the log
            // stream and drained the queue; there is nothing to act on.
            None => return,
        };
        match request {
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config();
            }
            FrontendRequest::CreateClient {
                hostname,
                port,
                position,
                fix_ips,
                enter_hook,
                leave_hook,
            } => {
                let handle = self.add_client_with(ClientConfig {
                    hostname,
                    port,
                    pos: position,
                    fix_ips,
                    cmd: enter_hook,
                    leave_cmd: leave_hook,
                });
                // `UpdateHostname` starts a lookup; a client created with a
                // hostname must do the same, otherwise it never gets an address.
                if self
                    .client_manager
                    .get_state(handle)
                    .is_some_and(|(c, _)| c.hostname.is_some())
                {
                    self.resolve(handle);
                }
                self.save_config();
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle);
                self.save_config();
            }
            FrontendRequest::ReloadConfig => self.handle_config_change(),
            FrontendRequest::SetPaused(paused) => {
                // The handler is synchronous; queue the work for the main loop.
                self.pending_pause = Some(paused);
            }
            FrontendRequest::Restart => {
                log::info!("restart requested over IPC");
                // The supervisor restarts us; exiting cleanly lets it do so.
                self.should_exit = true;
            }
            FrontendRequest::SetRestartInterval(secs) => {
                self.restart_interval = secs.filter(|s| *s > 0).map(Duration::from_secs);
                self.last_restart = Instant::now();
                let secs = self.restart_interval.map(|d| d.as_secs());
                self.notify_frontend(FrontendEvent::RestartInterval(secs));
            }
            FrontendRequest::ForceRelease => {
                log::info!("force release requested");
                self.capture.release();
            }
            FrontendRequest::SwitchDevice => self.switch_device(),
            FrontendRequest::SetLogLevel(level) => match level {
                Some(name) => match crate::logging::set_level(&name) {
                    Ok(level) => {
                        self.notify_frontend(FrontendEvent::LogLevel(
                            crate::logging::level_name(level).to_owned(),
                        ));
                    }
                    Err(e) => self.notify_frontend(FrontendEvent::Error(e)),
                },
                None => self.notify_frontend(FrontendEvent::LogLevel(
                    crate::logging::level_name(crate::logging::current_level()).to_owned(),
                )),
            },
            FrontendRequest::EnumerateLogs(n) => {
                self.notify_frontend(FrontendEvent::Logs(crate::logging::recent(n)));
            }
            FrontendRequest::SubscribeLogs(interval_ms) => match interval_ms {
                // A frontend that reconnects (window reopened) restarts the
                // stream; the cursor is reset to "now" because it already
                // fetched the backlog with `EnumerateLogs`.
                Some(ms) => {
                    // Start from "now": the frontend already has the backlog
                    // from `EnumerateLogs`, so replaying it would duplicate.
                    let mut interval =
                        tokio::time::interval(Duration::from_millis(ms.max(100) as u64));
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    self.log_stream = Some((interval, crate::logging::total()));
                    log::debug!("log streaming enabled every {ms} ms");
                }
                None => {
                    self.log_stream = None;
                    log::debug!("log streaming disabled");
                }
            },
            FrontendRequest::RefreshPeerInfo(handle) => {
                log::info!("refreshing machine details for client {handle}");
                self.capture.request_peer_info(handle);
            }
            FrontendRequest::EnumerateServiceState => {
                self.notify_frontend(FrontendEvent::Paused(self.paused));
                let secs = self.restart_interval.map(|d| d.as_secs());
                self.notify_frontend(FrontendEvent::RestartInterval(secs));
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.update_fix_ips(handle, fix_ips);
                self.save_config();
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.update_hostname(handle, host);
                self.save_config();
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.update_port(handle, port);
                self.save_config();
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config();
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config();
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook);
                self.save_config();
            }
            FrontendRequest::UpdateLeaveHook(handle, leave_hook) => {
                self.update_leave_hook(handle, leave_hook);
                self.save_config();
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
            FrontendRequest::EnumerateHotkeys => self.notify_hotkeys(),
            FrontendRequest::AddHotkey(from, to) => self.add_hotkey(&from, &to),
            FrontendRequest::RemoveHotkey(idx) => self.remove_hotkey(idx),
            FrontendRequest::EnumerateVersion => self.notify_version(),
            FrontendRequest::SetAutostart(enabled) => self.set_autostart(enabled),
        }
    }

    /// Add a hotkey rule from human-readable key names (`"Shift+Alt+Z"`).
    fn add_hotkey(&mut self, from: &str, to: &str) {
        let from_keys = match parse_keys(from) {
            Ok(k) if !k.is_empty() => k,
            Ok(_) => return self.hotkey_error("no keys given"),
            Err(e) => return self.hotkey_error(&e),
        };
        let to_keys = match parse_keys(to) {
            Ok(k) if !k.is_empty() => k,
            Ok(_) => return self.hotkey_error("no target keys given"),
            Err(e) => return self.hotkey_error(&e),
        };

        let mut rules = self.config.hotkeys();
        if rules.iter().any(|r| r.from == from_keys && r.to == to_keys) {
            return self.notify_hotkeys();
        }
        rules.push(crate::hotkey_task::Hotkey {
            from: from_keys,
            to: to_keys,
            action: None,
            taps: 1,
        });
        self.apply_hotkeys(rules);
    }

    fn remove_hotkey(&mut self, idx: usize) {
        let mut rules = self.config.hotkeys();
        if idx >= rules.len() {
            return self.hotkey_error("no such hotkey");
        }
        rules.remove(idx);
        self.apply_hotkeys(rules);
    }

    /// Persist `rules`, push them to the live listener and refresh the UI.
    fn apply_hotkeys(&mut self, rules: Vec<crate::hotkey_task::Hotkey>) {
        self.config.set_hotkeys(rules.clone());
        self.hotkeys.set_hotkeys(rules);
        self.save_config();
        self.notify_hotkeys();
    }

    fn hotkey_error(&mut self, msg: &str) {
        log::warn!("hotkey: {msg}");
        self.notify_frontend(FrontendEvent::Error(msg.to_owned()));
        self.notify_hotkeys();
    }

    /// Send the current hotkey rules to the frontend.
    fn notify_hotkeys(&mut self) {
        let rules = self
            .config
            .hotkeys()
            .into_iter()
            .map(|h| (format_keys(&h.from), format_keys(&h.to)))
            .collect();
        self.notify_frontend(FrontendEvent::Hotkeys(rules));
    }

    fn notify_version(&mut self) {
        self.notify_frontend(FrontendEvent::Version {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            commit: crate::config::local_commit_string(),
            autostart: crate::autostart::is_enabled(),
        });
    }

    fn set_autostart(&mut self, enabled: bool) {
        match crate::autostart::set_enabled(enabled) {
            Ok(()) => {
                log::info!("autostart {}", if enabled { "enabled" } else { "disabled" });
            }
            Err(e) => {
                log::warn!("failed to change autostart: {e}");
                self.notify_frontend(FrontendEvent::Error(format!(
                    "could not change autostart: {e}"
                )));
            }
        }
        self.notify_frontend(FrontendEvent::Autostart(crate::autostart::is_enabled()));
    }

    fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
                leave_hook: c.leave_cmd,
            })
            .collect();
        self.config.set_clients(clients);
        self.config.set_port(self.port);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
    }

    fn handle_config_change(&mut self) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let Some((c, s)) = self.client_manager.get_state(handle) else {
                log::warn!("client {handle} disappeared while applying the config");
                continue;
            };
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        self.capture.set_remap_rules(self.config.remap_rules());
        self.hotkeys.reload(&self.config);
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        /* a port edited in the config file has to be applied right away,
         * otherwise it silently differs from the port we are listening on
         */
        let port = self.config.port();
        if port != self.port {
            log::info!(
                "config changed the listen port from {} to {port}",
                self.port
            );
            self.emulation.request_port_change(port);
        }
        self.sync_frontend();
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::PeerInfo { addr, info } => {
                log::debug!("inbound {}: {}", addr, crate::sysinfo::describe(&info));
                let flat = crate::sysinfo::flatten(&info);
                // The report usually arrives before `Entered` has registered the
                // connection, so hold it until there is a row to attach it to.
                match self
                    .incoming_conn_info
                    .values_mut()
                    .find(|i| i.addr == addr)
                {
                    Some(incoming) => {
                        incoming.info = Some(flat);
                        self.publish_incoming();
                    }
                    None => {
                        self.pending_peer_info.insert(addr, flat);
                    }
                }
            }
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => {
                // A peer entered this machine: it is now driving us.
                if let Some(incoming) = self
                    .incoming_conn_info
                    .values_mut()
                    .find(|i| i.addr == addr)
                {
                    incoming.last_seen = Instant::now();
                    incoming.received = incoming.received.saturating_add(1);
                }
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, pos, fingerprint);
                }
                self.publish_incoming();
            }
            EmulationEvent::Disconnected { addr } => {
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    /* otherwise the new port is gone after a restart */
                    self.save_config();
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify => self.capture.release(),
            EmulationEvent::Connected { addr, fingerprint } => {
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::PeerHello { addr, commit } => {
                // Map the peer's source addr back to its client handle
                // and stamp the commit. Skip if we don't have an
                // outgoing client configured for this peer (incoming-
                // only setup) — there's nowhere to display the version
                // in that case anyway.
                if let Some(handle) = self.client_manager.get_client(addr) {
                    self.client_manager.set_peer_commit(handle, Some(commit));
                    self.broadcast_client(handle);
                }
            }
        }
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation.send_leave_event(incoming.addr);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::ClientEntered(handle) => {
                log::info!("entering client {handle} ...");
                self.active_capture = Some(handle);
                self.spawn_hook_command(handle, HookKind::Enter);
            }
            ICaptureEvent::PeerInfo(handle) => {
                // Details arrived: re-broadcast the client state so the window
                // picks up the new fields.
                if let Some((config, state)) = self.client_manager.get_state(handle) {
                    self.notify_frontend(FrontendEvent::State(handle, config, state));
                }
            }
            ICaptureEvent::ReturnRequested(handle) => {
                // The pointer came back to this machine's own edge while a peer
                // was driving it. Tell that peer to release, otherwise it keeps
                // forwarding into a machine that stopped emulating and the
                // pointer never returns.
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    log::info!("pointer returned: asking {} to release", incoming.addr);
                    self.emulation.send_leave_event(incoming.addr);
                } else {
                    log::debug!("return requested for unknown handle {handle}");
                }
            }
            ICaptureEvent::ClientLeft(handle) => {
                log::info!("leaving client {handle} ...");
                if self.active_capture == Some(handle) {
                    self.active_capture = None;
                }
                self.spawn_hook_command(handle, HookKind::Leave);
            }
        }
    }

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    fn sync_frontend(&mut self) {
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    /// Publish live counters. These change while the service runs, so unlike the
    /// peer reports they are refreshed on a timer — one small message per
    /// second, and only while a frontend is listening.
    fn publish_runtime(&mut self) {
        let outbound = self
            .client_manager
            .active_clients()
            .into_iter()
            .filter(|h| self.client_manager.active_addr(*h).is_some())
            .count();
        let best_rtt = self
            .client_manager
            .clients()
            .iter()
            .filter_map(|(_, s)| s.rtt_ms)
            .min()
            .unwrap_or(0) as u32;
        let capturing_peer = self
            .active_capture
            .and_then(|h| self.client_manager.get_state(h))
            .and_then(|(c, _)| c.hostname)
            .unwrap_or_default();

        self.notify_frontend(FrontendEvent::Runtime(lan_mouse_ipc::RuntimeInfo {
            capture_backend: self.capture_backend_name.clone(),
            emulation_backend: self.emulation_backend_name.clone(),
            capture_alive: self.capture_status == Status::Enabled && !self.paused,
            emulation_alive: self.emulation_status == Status::Enabled && !self.paused,
            capturing: self.active_capture.is_some(),
            capturing_peer,
            port: self.port,
            uptime_secs: self.started.elapsed().as_secs(),
            paused: self.paused,
            inbound: self.incoming_conn_info.len(),
            outbound,
            best_rtt_ms: best_rtt,
        }));
    }

    /// Publish a snapshot of every inbound connection.
    fn publish_incoming(&mut self) {
        let active_addr = self
            .active_capture
            .and_then(|h| self.incoming_conn_info.get(&h).map(|i| i.addr));
        let infos: Vec<lan_mouse_ipc::IncomingInfo> = self
            .incoming_conn_info
            .values()
            .map(|i| {
                // Start from whatever the peer reported, then overlay the live
                // connection facts.
                let mut info = i.info.clone().unwrap_or_default();
                info.addr = i.addr;
                info.fingerprint = i.fingerprint.clone();
                info.pos = i.pos;
                info.idle_secs = i.last_seen.elapsed().as_secs();
                info.received = i.received;
                info.active = active_addr == Some(i.addr);
                info
            })
            .collect();
        self.notify_frontend(FrontendEvent::IncomingConnections(infos));
    }

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture.create(handle, pos, CaptureType::EnterOnly);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                pos,
                last_seen: Instant::now(),
                received: 0,
                // A report may have arrived before this connection was known.
                info: self.pending_peer_info.remove(&addr),
            },
        );
    }

    fn update_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let Some(incoming) = self
            .incoming_conn_info
            .values_mut()
            .find(|i| i.addr == addr)
        else {
            // No entry for this address: the initial `Enter` was lost or the
            // peer reconnected. Just register it instead of panicking.
            log::debug!("update for unknown incoming connection {addr}, adding it");
            self.add_incoming(addr, pos, fingerprint);
            return;
        };
        let mut changed = false;
        if incoming.fingerprint != fingerprint {
            incoming.fingerprint = fingerprint.clone();
            changed = true;
        }
        if incoming.pos != pos {
            incoming.pos = pos;
            changed = true;
        }
        if changed {
            self.remove_incoming(addr);
            self.add_incoming(addr, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn remove_incoming(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        // Bound the queue: a frontend that stops reading (window hidden, IPC
        // stalled) must not let a streaming event source grow it forever.
        // Dropping the oldest is safe — every event is a snapshot of state, and
        // the frontend re-syncs on reconnect.
        const MAX_PENDING: usize = 4096;
        if self.pending_frontend_events.len() >= MAX_PENDING {
            self.pending_frontend_events.pop_front();
        }
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys.write().expect("lock").insert(fp, desc);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    fn add_client(&mut self) {
        self.add_client_with(ClientConfig::default());
    }

    /// add a client with a known configuration in one step
    ///
    /// Splitting this into `Create` plus a sequence of `Update*` requests is
    /// racy: the `Created(handle)` answer is broadcast to every frontend, so
    /// two frontends creating a client at the same time send their updates for
    /// the wrong handle and leave half-configured clients behind.
    fn add_client_with(&mut self, config: ClientConfig) -> ClientHandle {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        self.client_manager.set_config(handle, config);
        match self.client_manager.get_state(handle) {
            Some((c, s)) => self.notify_frontend(FrontendEvent::Created(handle, c, s)),
            None => log::error!("client {handle} disappeared right after it was created"),
        }
        handle
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* Start the handshake now. The pointer may never reach this client,
             * but when it does the connection is usually already up, which
             * removes the round trip from the moment of crossing. */
            self.capture.preconnect(handle);
            /* notify capture and frontends */
            self.capture.create(handle, pos, CaptureType::Default);
            self.broadcast_client(handle);
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    fn remove_client(&mut self, handle: ClientHandle) {
        /* Only report `Deleted` for a client that actually existed: sending
         * it unconditionally hid failed deletions from the frontend (the CLI
         * waited for a confirmation that never distinguished success from a
         * no-op).
         */
        let Some((_, state)) = self.client_manager.remove_client(handle) else {
            log::warn!("cannot remove client {handle}: no such client");
            self.notify_frontend(FrontendEvent::NoSuchClient(handle));
            return;
        };
        if state.active {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    fn update_leave_hook(&mut self, handle: ClientHandle, leave_hook: Option<String>) {
        self.client_manager.set_leave_hook(handle, leave_hook);
        self.broadcast_client(handle);
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    fn spawn_hook_command(&self, handle: ClientHandle, kind: HookKind) {
        let cmd = match kind {
            HookKind::Enter => self.client_manager.get_enter_cmd(handle),
            HookKind::Leave => self.client_manager.get_leave_cmd(handle),
        };
        let Some(cmd) = cmd else { return };
        tokio::task::spawn_local(async move {
            log::info!("spawning {kind} hook for client {handle}");
            let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute {kind} hook for client {handle}: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{kind} hook for client {handle} ({cmd}) exited successfully");
                    } else {
                        log::warn!("{kind} hook for client {handle} ({cmd}) exited with {s}");
                    }
                }
                Err(e) => log::warn!("{kind} hook for client {handle} ({cmd}): {e}"),
            }
        });
    }
}

#[derive(Clone, Copy, Debug)]
enum HookKind {
    Enter,
    Leave,
}

impl std::fmt::Display for HookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookKind::Enter => f.write_str("enter"),
            HookKind::Leave => f.write_str("leave"),
        }
    }
}

/// Render scancodes as the human-readable names used by the GUI (`"Shift+Alt+Z"`).
fn format_keys(keys: &[u32]) -> String {
    keys.iter()
        .map(|&k| match input_event::scancode::Linux::try_from(k) {
            Ok(s) => format!("{s:?}"),
            Err(_) => format!("0x{k:x}"),
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Parse `"Shift+Alt+Z"` into scancodes.
///
/// Names follow the `scancode::Linux` variants so the GUI, the config file and
/// the logs all use one vocabulary. A leading `Key` may be omitted, and the
/// side modifiers accept both `Shift` and `LeftShift`.
fn parse_keys(spec: &str) -> Result<Vec<u32>, String> {
    use input_event::scancode::Linux;

    let mut out = Vec::new();
    for raw in spec.split('+') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let canonical = canonical_key_name(name);
        // `serde_json` handles a bare string; `toml` would expect a document.
        match serde_json::from_value::<Linux>(serde_json::Value::String(canonical)) {
            Ok(k) => out.push(k as u32),
            Err(_) => return Err(format!("unknown key `{name}`")),
        }
    }
    Ok(out)
}

/// Map a user-written key name to the `scancode::Linux` variant name.
///
/// Accepts the full variant (`KeyLeftShift`), a bare letter (`z`) and the
/// short spellings people actually type (`Shift`, `Ctrl`, `Alt`, `Super`).
fn canonical_key_name(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "shift" | "lshift" | "leftshift" => return "KeyLeftShift".into(),
        "rshift" | "rightshift" => return "KeyRightShift".into(),
        "ctrl" | "control" | "lctrl" | "leftctrl" => return "KeyLeftCtrl".into(),
        "rctrl" | "rightctrl" => return "KeyRightCtrl".into(),
        "alt" | "lalt" | "leftalt" => return "KeyLeftAlt".into(),
        "ralt" | "rightalt" | "altgr" => return "KeyRightalt".into(),
        "super" | "meta" | "win" | "lmeta" | "leftmeta" => return "KeyLeftMeta".into(),
        "rmeta" | "rightmeta" => return "KeyRightmeta".into(),
        _ => {}
    }

    let stripped = name
        .strip_prefix("Key")
        .or_else(|| name.strip_prefix("key"))
        .unwrap_or(name);

    // single character: `z` -> `KeyZ`
    if stripped.chars().count() == 1 {
        return format!("Key{}", stripped.to_ascii_uppercase());
    }

    // already a full variant such as `LeftShift`: capitalise the first letter
    // and keep the rest, which is exactly how the variants are spelled.
    let mut chars = stripped.chars();
    match chars.next() {
        Some(first) => format!("Key{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// Resolve on the next log-stream tick, or never when no frontend is streaming.
async fn log_tick(stream: &mut Option<(tokio::time::Interval, usize)>) {
    match stream {
        Some((interval, _)) => {
            interval.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// Resolve when the periodic restart interval elapses.
///
/// Never resolves when no interval is configured, so the `select!` arm stays
/// idle instead of spinning.
async fn restart_tick(interval: &Option<Duration>, last: Instant) {
    match interval {
        Some(interval) => {
            let elapsed = last.elapsed();
            let remaining = interval.saturating_sub(elapsed);
            tokio::time::sleep(remaining).await
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod key_parsing_tests {
    use super::*;

    #[test]
    fn parses_modifier_chord() {
        let keys = parse_keys("Shift+Alt+Z").expect("parse");
        assert_eq!(keys.len(), 3, "expected 3 keys, got {keys:?}");
    }

    #[test]
    fn parses_bare_letter() {
        let keys = parse_keys("Z").expect("parse");
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn round_trips_through_format() {
        let keys = parse_keys("Shift+Alt+Z").expect("parse");
        let text = format_keys(&keys);
        let again = parse_keys(&text).expect("reparse");
        assert_eq!(keys, again);
    }
}
