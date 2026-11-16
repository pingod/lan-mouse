use std::cell::RefCell;

use glib::Properties;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use lan_mouse_ipc::ClientHandle;

use super::ClientData;

#[derive(Properties, Default)]
#[properties(wrapper_type = super::ClientObject)]
pub struct ClientObject {
    #[property(name = "handle", get, set, type = ClientHandle, member = handle)]
    #[property(name = "hostname", get, set, type = Option<String>, member = hostname)]
    #[property(name = "port", get, set, type = u32, member = port, maximum = u16::MAX as u32)]
    #[property(name = "active", get, set, type = bool, member = active)]
    #[property(name = "position", get, set, type = String, member = position)]
    #[property(name = "resolving", get, set, type = bool, member = resolving)]
    #[property(name = "ips", get, set, type = Vec<String>, member = ips)]
    #[property(name = "peer-commit", get, set, type = Option<String>, member = peer_commit)]
    #[property(name = "connected", get, set, type = bool, member = connected)]
    #[property(name = "rtt-ms", get, set, type = u32, member = rtt_ms, maximum = u32::MAX)]
    #[property(name = "last-seen", get, set, type = u64, member = last_seen)]
    #[property(name = "forwarded", get, set, type = u64, member = forwarded)]
    #[property(name = "received", get, set, type = u64, member = received)]
    /// the local monitor this link is attached to, empty when machine-level
    #[property(name = "display", get, set, type = String, member = display)]
    #[property(name = "edge-start", get, set, type = u32, member = edge_start, maximum = 1000)]
    #[property(name = "edge-end", get, set, type = u32, member = edge_end, maximum = 1000)]
    #[property(name = "peer-edge-start", get, set, type = u32, member = peer_edge_start, maximum = 1000)]
    #[property(name = "peer-edge-end", get, set, type = u32, member = peer_edge_end, maximum = 1000)]
    /// the peer's own display report, verbatim: `WxH` or the older
    /// `N displays, WxH`. The arrangement canvas reads the size out of it.
    #[property(name = "peer-displays", get, set, type = String, member = peer_displays)]
    /// the arrangement the peer is drawing, verbatim; empty when it sent none
    #[property(name = "peer-arrangement", get, set, type = String, member = peer_arrangement)]
    pub data: RefCell<ClientData>,
}

#[glib::object_subclass]
impl ObjectSubclass for ClientObject {
    const NAME: &'static str = "ClientObject";
    type Type = super::ClientObject;
}

#[glib::derived_properties]
impl ObjectImpl for ClientObject {}
