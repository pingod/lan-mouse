use std::{
    cell::RefCell,
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use slab::Slab;

use lan_mouse_ipc::{ClientConfig, ClientHandle, ClientState, EdgeRange, Position};

use crate::config::ConfigClient;

#[derive(Clone, Default)]
pub struct ClientManager {
    clients: Rc<RefCell<Slab<(ClientConfig, ClientState)>>>,
}

impl ClientManager {
    /// get all clients
    pub fn clients(&self) -> Vec<(ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(_, c)| c.clone())
            .collect::<Vec<_>>()
    }

    pub fn add_with_config(&self, config_client: ConfigClient) -> ClientHandle {
        let config = ClientConfig {
            hostname: config_client.hostname,
            fix_ips: config_client.ips.into_iter().collect(),
            port: config_client.port,
            pos: config_client.pos,
            cmd: config_client.enter_hook,
            leave_cmd: config_client.leave_hook,
            edge: config_client.edge,
            peer_edge: config_client.peer_edge,
        };
        let state = ClientState {
            active: config_client.active,
            ips: HashSet::from_iter(config.fix_ips.iter().cloned()),
            ..Default::default()
        };
        let handle = self.add_client();
        self.set_config(handle, config);
        self.set_state(handle, state);
        handle
    }

    /// add a new client to this manager
    pub fn add_client(&self) -> ClientHandle {
        self.clients.borrow_mut().insert(Default::default()) as ClientHandle
    }

    /// set the config of the given client
    pub fn set_config(&self, handle: ClientHandle, config: ClientConfig) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(handle as usize) {
            *c = config;
        }
    }

    /// set the state of the given client
    pub fn set_state(&self, handle: ClientHandle, state: ClientState) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            *s = state;
        }
    }

    /// activate the given client
    /// returns, whether the client was activated
    pub fn activate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(handle as usize) {
            Some((_, s)) if !s.active => {
                s.active = true;
                true
            }
            _ => false,
        }
    }

    /// deactivate the given client
    /// returns, whether the client was deactivated
    pub fn deactivate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(handle as usize) {
            Some((_, s)) if s.active => {
                s.active = false;
                true
            }
            _ => false,
        }
    }

    /// find a client by its address
    pub fn get_client(&self, addr: SocketAddr) -> Option<ClientHandle> {
        // since there shouldn't be more than a handful of clients at any given
        // time this is likely faster than using a HashMap
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (_, s))| {
                if s.active && s.ips.contains(&addr.ip()) {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    /// get the client at the given position
    ///
    /// Only correct when the caller has no crossing point: with several active
    /// clients on one edge this returns the first in list order, which is what
    /// decides who keeps an overlapping range. Use
    /// [`ClientManager::client_at_fraction`] to route an actual crossing.
    pub fn client_at(&self, pos: Position) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (c, s))| {
                if s.active && c.pos == pos {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    /// the active client whose edge range contains a crossing at `fraction`
    ///
    /// `fraction` counts from the left or top end of the edge, exactly like the
    /// payload of the `Enter` protocol event. A crossing outside every range
    /// reaches nobody: the pointer hits a wall, the way Synergy refuses to
    /// switch when `getNeighbor` finds no link covering that point.
    pub fn client_at_fraction(&self, pos: Position, fraction: f64) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (c, s))| {
                if s.active && c.pos == pos && c.edge.contains(fraction) {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    /// every active client on the given edge, in list order
    pub fn clients_at(&self, pos: Position) -> Vec<(ClientHandle, ClientConfig)> {
        self.clients
            .borrow()
            .iter()
            .filter(|(_, (c, s))| s.active && c.pos == pos)
            .map(|(k, (c, _))| (k as ClientHandle, c.clone()))
            .collect()
    }

    /// the ranges the active clients claim on the given edge, in list order
    pub fn edges_at(&self, pos: Position) -> Vec<(ClientHandle, EdgeRange)> {
        self.clients
            .borrow()
            .iter()
            .filter(|(_, (c, s))| s.active && c.pos == pos)
            .map(|(k, (c, _))| (k as ClientHandle, c.edge))
            .collect()
    }

    pub(crate) fn get_hostname(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow_mut()
            .get_mut(handle as usize)
            .and_then(|(c, _)| c.hostname.clone())
    }

    /// get the position of the corresponding client
    pub(crate) fn get_pos(&self, handle: ClientHandle) -> Option<Position> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.pos)
    }

    /// get the edge range of the corresponding client
    pub(crate) fn get_edge(&self, handle: ClientHandle) -> Option<(EdgeRange, EdgeRange)> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| (c.edge, c.peer_edge))
    }

    /// set both edge ranges of a client
    ///
    /// Returns whether anything changed, so the caller can avoid tearing down
    /// and rebuilding the capture for a no-op edit.
    pub(crate) fn set_edge(
        &self,
        handle: ClientHandle,
        edge: EdgeRange,
        peer_edge: EdgeRange,
    ) -> bool {
        let mut clients = self.clients.borrow_mut();
        let Some((config, _)) = clients.get_mut(handle as usize) else {
            return false;
        };
        if config.edge == edge && config.peer_edge == peer_edge {
            return false;
        }
        config.edge = edge;
        config.peer_edge = peer_edge;
        true
    }

    /// remove a client from the list
    pub fn remove_client(&self, client: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        // remove id from occupied ids
        self.clients.borrow_mut().try_remove(client as usize)
    }

    /// get the config & state of the given client
    pub fn get_state(&self, handle: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        self.clients.borrow().get(handle as usize).cloned()
    }

    /// get the current config & state of all clients
    pub fn get_client_states(&self) -> Vec<(ClientHandle, ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(k, v)| (k as ClientHandle, v.0.clone(), v.1.clone()))
            .collect()
    }

    /// update the fix ips of the client
    pub fn set_fix_ips(&self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.fix_ips = fix_ips
        }
        self.update_ips(handle);
    }

    /// update the dns-ips of the client
    pub fn set_dns_ips(&self, handle: ClientHandle, dns_ips: Vec<IpAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.dns_ips = dns_ips
        }
        self.update_ips(handle);
    }

    fn update_ips(&self, handle: ClientHandle) {
        if let Some((c, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.ips = c
                .fix_ips
                .iter()
                .cloned()
                .chain(s.dns_ips.iter().cloned())
                .collect::<HashSet<_>>();
        }
    }

    /// update the hostname of the given client
    /// this automatically clears the active ip address and ips from dns
    pub fn set_hostname(&self, handle: ClientHandle, hostname: Option<String>) -> bool {
        let mut clients = self.clients.borrow_mut();
        let Some((c, s)) = clients.get_mut(handle as usize) else {
            return false;
        };

        // hostname changed
        if c.hostname != hostname {
            c.hostname = hostname;
            s.active_addr = None;
            s.dns_ips.clear();
            drop(clients);
            self.update_ips(handle);
            true
        } else {
            false
        }
    }

    /// update the port of the client
    pub(crate) fn set_port(&self, handle: ClientHandle, port: u16) {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.port != port => {
                c.port = port;
                s.active_addr = s.active_addr.map(|a| SocketAddr::new(a.ip(), port));
            }
            _ => {}
        };
    }

    /// update the position of the client
    /// returns true, if a change in capture position is required (pos changed & client is active)
    pub(crate) fn set_pos(&self, handle: ClientHandle, pos: Position) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.pos != pos => {
                log::info!("update pos {handle} {} -> {}", c.pos, pos);
                c.pos = pos;
                s.active
            }
            _ => false,
        }
    }

    /// update the enter hook command of the client
    pub(crate) fn set_enter_hook(&self, handle: ClientHandle, enter_hook: Option<String>) {
        if let Some((c, _s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.cmd = enter_hook;
        }
    }

    /// update the leave hook command of the client
    pub(crate) fn set_leave_hook(&self, handle: ClientHandle, leave_hook: Option<String>) {
        if let Some((c, _s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.leave_cmd = leave_hook;
        }
    }

    /// set resolving status of the client
    pub(crate) fn set_resolving(&self, handle: ClientHandle, status: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.resolving = status;
        }
    }

    /// get the enter hook command
    pub(crate) fn get_enter_cmd(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(c, _)| c.cmd.clone())
    }

    /// get the leave hook command
    pub(crate) fn get_leave_cmd(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(c, _)| c.leave_cmd.clone())
    }

    /// returns all clients that are currently registered
    pub(crate) fn registered_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .map(|(h, _)| h as ClientHandle)
            .collect()
    }

    /// returns all clients that are currently active
    pub(crate) fn active_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .filter(|(_, (_, s))| s.active)
            .map(|(h, _)| h as ClientHandle)
            .collect()
    }

    pub(crate) fn set_active_addr(&self, handle: ClientHandle, addr: Option<SocketAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.active_addr = addr;
        }
    }

    pub(crate) fn set_alive(&self, handle: ClientHandle, alive: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.alive = alive;
        }
    }

    /// Record the outcome of a ping/pong exchange.
    ///
    /// `rtt` is `None` when the peer answered without a usable timing sample;
    /// the timestamp is always refreshed so the UI can show staleness.
    pub(crate) fn set_health(&self, handle: ClientHandle, rtt: Option<Duration>, connected: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            if let Some(rtt) = rtt {
                s.rtt_ms = Some(rtt.as_millis() as u64);
            }
            s.connected = connected;
            s.last_seen = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or_default(),
            );
        }
    }

    /// Store what the peer reported about itself.
    ///
    /// Flattened into `ClientState` so the IPC layer does not depend on the
    /// protocol crate.
    pub(crate) fn set_peer_info(&self, handle: ClientHandle, info: lan_mouse_proto::PeerInfo) {
        use lan_mouse_proto::info_slot;
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.peer_os = info.get(info_slot::OS).to_owned();
            s.peer_os_version = info.get(info_slot::OS_VERSION).to_owned();
            s.peer_kernel = info.get(info_slot::KERNEL).to_owned();
            s.peer_arch = info.get(info_slot::ARCH).to_owned();
            s.peer_hostname = info.get(info_slot::HOSTNAME).to_owned();
            s.peer_app_version = info.get(info_slot::APP_VERSION).to_owned();
            s.peer_commit_short = info.get(info_slot::COMMIT).to_owned();
            s.peer_displays = info.get(info_slot::DISPLAYS).to_owned();
        }
    }

    pub(crate) fn set_peer_commit(&self, handle: ClientHandle, commit: Option<[u8; 8]>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.peer_commit = commit;
        }
    }

    /// Handle of the first active client, if any. Used by global hotkeys,
    /// which are not tied to a capture barrier.
    pub(crate) fn first_active_client(&self) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .enumerate()
            .find(|(_, (_, s))| s.1.active)
            .map(|(i, _)| i as ClientHandle)
    }

    pub(crate) fn active_addr(&self, handle: ClientHandle) -> Option<SocketAddr> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(_, s)| s.active_addr)
    }

    /// Count an event forwarded to `handle`.
    pub(crate) fn record_forwarded(&self, handle: ClientHandle) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.forwarded = s.forwarded.saturating_add(1);
        }
    }

    /// Count an event received from `handle`.
    pub(crate) fn record_received(&self, handle: ClientHandle) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.received = s.received.saturating_add(1);
        }
    }

    /// Mark a client as no longer connected, keeping its last-seen time.
    pub(crate) fn set_disconnected(&self, handle: ClientHandle) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.connected = false;
            s.rtt_ms = None;
        }
    }

    /// Find the client currently associated with `addr`.
    pub(crate) fn handle_for_addr(&self, addr: SocketAddr) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .find(|(_, (_, s))| s.active_addr == Some(addr))
            .map(|(i, _)| i as ClientHandle)
    }

    pub(crate) fn alive(&self, handle: ClientHandle) -> bool {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, s)| s.alive)
            .unwrap_or(false)
    }

    pub(crate) fn get_port(&self, handle: ClientHandle) -> Option<u16> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.port)
    }

    pub(crate) fn get_ips(&self, handle: ClientHandle) -> Option<HashSet<IpAddr>> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, s)| s.ips.clone())
    }
}
