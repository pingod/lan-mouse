mod imp;

use adw::subclass::prelude::*;
use gtk::glib::{self, Object};

use lan_mouse_ipc::{ClientConfig, ClientHandle, ClientState, EdgeRange};

glib::wrapper! {
    pub struct ClientObject(ObjectSubclass<imp::ClientObject>);
}

impl ClientObject {
    pub fn new(handle: ClientHandle, client: ClientConfig, state: ClientState) -> Self {
        Object::builder()
            .property("handle", handle)
            .property("hostname", client.hostname)
            .property("port", client.port as u32)
            .property("position", client.pos.to_string())
            .property("active", state.active)
            .property(
                "ips",
                state
                    .ips
                    .iter()
                    .map(|ip| ip.to_string())
                    .collect::<Vec<_>>(),
            )
            .property("resolving", state.resolving)
            .property("peer-commit", peer_commit_to_string(state.peer_commit))
            .property("connected", state.connected)
            .property("rtt-ms", state.rtt_ms.unwrap_or(0) as u32)
            .property("last-seen", state.last_seen.unwrap_or(0))
            .property("forwarded", state.forwarded)
            .property("received", state.received)
            .property("display", client.display.clone().unwrap_or_default())
            .property("edge-start", client.edge.start() as u32)
            .property("edge-end", client.edge.end() as u32)
            .property("peer-edge-start", client.peer_edge.start() as u32)
            .property("peer-edge-end", client.peer_edge.end() as u32)
            .property("peer-displays", state.peer_displays)
            .build()
    }

    pub fn get_data(&self) -> ClientData {
        self.imp().data.borrow().clone()
    }
}

/// Render the 8-byte ASCII commit hash carried in
/// [`lan_mouse_ipc::ClientState::peer_commit`] as a `String`. `None`
/// in → `None` out (peer hasn't sent a Hello yet, or speaks an older
/// proto).
pub fn peer_commit_to_string(commit: Option<[u8; 8]>) -> Option<String> {
    commit.and_then(|c| std::str::from_utf8(&c).ok().map(str::to_string))
}

#[derive(Default, Clone)]
pub struct ClientData {
    pub handle: ClientHandle,
    pub hostname: Option<String>,
    pub port: u32,
    pub active: bool,
    pub position: String,
    pub resolving: bool,
    pub ips: Vec<String>,
    pub peer_commit: Option<String>,
    pub connected: bool,
    /// Round-trip time in milliseconds; `0` means unknown.
    pub rtt_ms: u32,
    /// Unix timestamp of the last contact; `0` means unknown.
    pub last_seen: u64,
    pub forwarded: u64,
    pub received: u64,
    /// the local monitor this link is attached to, empty when machine-level
    pub display: String,
    /// segment of the local edge this client is reached through, in permille
    pub edge_start: u32,
    /// first permille after that segment
    pub edge_end: u32,
    /// segment of this client's facing edge the cursor lands in, in permille
    pub peer_edge_start: u32,
    /// first permille after that segment
    pub peer_edge_end: u32,
    /// the peer's display report, see the `peer-displays` property
    pub peer_displays: String,
}

impl ClientData {
    /// The segment of the local edge this client is reached through.
    ///
    /// A range is stored as two permille numbers because that is what the GTK
    /// properties can carry; an impossible pair (empty or out of bounds) means
    /// the client was never configured with one, so the whole edge is assumed -
    /// which is exactly the behaviour before ranges existed.
    pub fn edge(&self) -> EdgeRange {
        EdgeRange::new(self.edge_start as u16, self.edge_end as u16).unwrap_or(EdgeRange::FULL)
    }

    /// The segment of the peer's facing edge the cursor lands in.
    pub fn peer_edge(&self) -> EdgeRange {
        EdgeRange::new(self.peer_edge_start as u16, self.peer_edge_end as u16)
            .unwrap_or(EdgeRange::FULL)
    }

    /// The peer's screen size in pixels, when it reported one.
    ///
    /// `None` for a peer on a build that never described itself, which the canvas
    /// draws as a card sized from the ranges instead of a wrong number.
    pub fn peer_size(&self) -> Option<(f64, f64)> {
        lan_mouse_ipc::parse_screen_size(&self.peer_displays)
            .map(|(width, height)| (width as f64, height as f64))
    }
}
