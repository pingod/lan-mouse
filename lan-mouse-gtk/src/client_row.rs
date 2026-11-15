mod imp;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, Object};

use lan_mouse_ipc::{DEFAULT_PORT, Position};

use super::ClientObject;

glib::wrapper! {
    pub struct ClientRow(ObjectSubclass<imp::ClientRow>)
    @extends gtk::ListBoxRow, gtk::Widget, adw::PreferencesRow, adw::ExpanderRow,
    @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl ClientRow {
    pub fn new(client_object: &ClientObject) -> Self {
        let client_row: Self = Object::builder().build();
        client_row
            .imp()
            .client_object
            .borrow_mut()
            .replace(client_object.clone());
        client_row
    }

    pub fn bind(&self, client_object: &ClientObject) {
        let mut bindings = self.imp().bindings.borrow_mut();

        // bind client active to switch state
        let active_binding = client_object
            .bind_property("active", &self.imp().enable_switch.get(), "state")
            .sync_create()
            .build();

        // bind client active to switch position
        let switch_position_binding = client_object
            .bind_property("active", &self.imp().enable_switch.get(), "active")
            .sync_create()
            .build();

        // bind hostname to hostname edit field
        let hostname_binding = client_object
            .bind_property("hostname", &self.imp().hostname.get(), "text")
            .transform_to(|_, v: Option<String>| {
                if let Some(hostname) = v {
                    Some(hostname)
                } else {
                    Some("".to_string())
                }
            })
            .sync_create()
            .build();

        // bind hostname to title
        let title_binding = client_object
            .bind_property("hostname", self, "title")
            .transform_to(|_, v: Option<String>| {
                /* The expander row renders its title as plain text (it is not
                 * created with `use-markup`), so Pango markup would show up
                 * verbatim. Use a plain placeholder instead.
                 */
                v.or(Some("no hostname!".to_string()))
            })
            .sync_create()
            .build();

        // bind port to port edit field
        let port_binding = client_object
            .bind_property("port", &self.imp().port.get(), "text")
            .transform_to(|_, v: u32| {
                if v == DEFAULT_PORT as u32 {
                    Some("".to_string())
                } else {
                    Some(v.to_string())
                }
            })
            .sync_create()
            .build();

        /* The subtitle shows both the port and the peer version status. A
         * single property binding can only mirror one of the two sources, and
         * binding the port here meant that every `refresh_version_status()`
         * (or a later port change) overwrote the other one. Compose it in
         * `refresh_version_status()` and re-render on both changes instead.
         */
        let subtitle_handlers = [
            client_object.connect_notify_local(
                Some("port"),
                glib::clone!(
                    #[weak(rename_to = row)]
                    self,
                    move |_, _| row.refresh_version_status()
                ),
            ),
            client_object.connect_notify_local(
                Some("peer-commit"),
                glib::clone!(
                    #[weak(rename_to = row)]
                    self,
                    move |_, _| row.refresh_version_status()
                ),
            ),
        ];
        self.imp()
            .subtitle_handlers
            .borrow_mut()
            .extend(subtitle_handlers);

        // bind position to selected position
        let position_binding = client_object
            .bind_property("position", &self.imp().position.get(), "selected")
            .transform_to(|_, v: String| match v.as_str() {
                "right" => Some(1u32),
                "top" => Some(2u32),
                "bottom" => Some(3u32),
                _ => Some(0u32),
            })
            .sync_create()
            .build();

        // bind resolving status to spinner visibility
        let resolve_binding = client_object
            .bind_property(
                "resolving",
                &self.imp().dns_loading_indicator.get(),
                "spinning",
            )
            .sync_create()
            .build();

        // bind ips to tooltip-text
        let ip_binding = client_object
            .bind_property("ips", &self.imp().dns_button.get(), "tooltip-text")
            .transform_to(|_, ips: Vec<String>| {
                if ips.is_empty() {
                    Some("no ip addresses associated with this client".into())
                } else {
                    Some(ips.join("\n"))
                }
            })
            .sync_create()
            .build();

        bindings.push(active_binding);
        bindings.push(switch_position_binding);
        bindings.push(hostname_binding);
        bindings.push(title_binding);
        bindings.push(port_binding);
        bindings.push(position_binding);
        bindings.push(resolve_binding);
        bindings.push(ip_binding);

        // Render the initial collapsed subtitle from whatever
        // peer_commit the ClientObject was created with. Subsequent
        // changes are pushed by `Window::update_client_state` calling
        // `refresh_version_status` after writing the new property.
        self.refresh_version_status();
    }

    pub fn unbind(&self) {
        for binding in self.imp().bindings.borrow_mut().drain(..) {
            binding.unbind();
        }
        let client_object = self.imp().client_object.borrow().clone();
        if let Some(client_object) = client_object {
            for handler in self.imp().subtitle_handlers.borrow_mut().drain(..) {
                client_object.disconnect(handler);
            }
        }
    }

    pub fn set_active(&self, active: bool) {
        self.imp().set_active(active);
    }

    pub fn set_hostname(&self, hostname: Option<String>) {
        self.imp().set_hostname(hostname);
    }

    pub fn set_port(&self, port: u16) {
        self.imp().set_port(port);
    }

    pub fn set_position(&self, pos: Position) {
        self.imp().set_pos(pos);
    }

    /// Describe the connection in one line: reachability, round-trip time and
    /// how recently the peer was heard from.
    pub fn set_status(&self, state: &lan_mouse_ipc::ClientState) {
        let row = &self.imp().status_row;
        let mut parts: Vec<String> = Vec::new();

        parts.push(if state.connected {
            "connected".to_string()
        } else if state.active {
            "not connected".to_string()
        } else {
            "disabled".to_string()
        });

        if let Some(addr) = state.active_addr {
            parts.push(format!("via {addr}"));
        }
        if let Some(rtt) = state.rtt_ms {
            parts.push(format!("rtt {rtt} ms"));
        }
        if !state.alive {
            parts.push("peer cannot emulate input".to_string());
        }
        if state.resolving {
            parts.push("resolving".to_string());
        }
        if state.has_pressed_keys {
            parts.push("keys held".to_string());
        }
        if let Some(last_seen) = state.last_seen {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(last_seen);
            parts.push(format!("last seen {}s ago", now.saturating_sub(last_seen)));
        }

        row.set_subtitle(&parts.join(" · "));
    }

    /// Fill the expander with everything the peer reported about itself.
    ///
    /// Rebuilt on every state update: the daemon only sends a peer report once
    /// per connection, so this runs a handful of times, and rebuilding avoids
    /// stale rows when a peer reconnects with different details.
    pub fn set_peer_details(&self, state: &lan_mouse_ipc::ClientState) {
        // Drop previously added rows, keeping the template rows.
        let mut child = self.first_child();
        while let Some(widget) = child {
            let next = widget.next_sibling();
            if widget
                .downcast_ref::<adw::ActionRow>()
                .is_some_and(|r| r.css_classes().iter().any(|c| c == "peer-detail"))
            {
                self.remove(&widget);
            }
            child = next;
        }

        // Grouped: a flat list of every field made the expander taller than the
        // window and buried the settings below it.
        let group = |title: &str| {
            let caption = adw::ActionRow::builder()
                .title(title)
                .css_classes(["heading", "dim-label", "peer-detail"])
                .activatable(false)
                .build();
            self.add_row(&caption);
        };
        let add = |label: &str, value: &str| {
            if value.is_empty() {
                return;
            }
            let row = adw::ActionRow::builder()
                .title(label)
                .subtitle(value)
                .build();
            row.add_css_class("peer-detail");
            self.add_row(&row);
        };

        group("System");
        add("Operating system", &state.peer_os);
        add("OS version", &state.peer_os_version);
        add("Kernel", &state.peer_kernel);
        add("Architecture", &state.peer_arch);
        add("Displays", &state.peer_displays);

        group("Build");
        add("Hostname", &state.peer_hostname);
        add("lan-mouse version", &state.peer_app_version);
        add("Build commit", &state.peer_commit_short);
    }

    pub fn set_dns_state(&self, resolved: bool) {
        self.imp().set_dns_state(resolved);
    }

    /// Recompute the collapsed subtitle (Pango markup) based on the
    /// current `peer-commit` property and the local build's commit.
    /// Soft-warn semantics: a missing or mismatched peer commit
    /// surfaces as orange text but never blocks traffic. Called by
    /// the window after `update_client_state` writes the new
    /// `peer-commit`. The dns-status icon is left to its existing
    /// `set_dns_state` handler so the two indicators don't fight
    /// for the same CSS class.
    pub fn refresh_version_status(&self) {
        /* Clone the object out of the RefCell: `set_subtitle()` below notifies
         * handlers that borrow it again.
         */
        let client_object = self.imp().client_object.borrow().clone();
        let peer: Option<String> = client_object
            .as_ref()
            .and_then(|co| co.property::<Option<String>>("peer-commit"));
        let port: u32 = client_object
            .as_ref()
            .map(|co| co.property::<u32>("port"))
            .unwrap_or(DEFAULT_PORT as u32);
        let local = crate::local_commit_str();
        let status = match peer.as_deref() {
            None => "peer version: unknown".to_string(),
            Some(p) if p == local.as_str() => format!("peer version: {p} · matched"),
            Some(p) => format!("peer version: {p} · ours: {local}"),
        };
        self.remove_css_class("peer-mismatch");
        self.remove_css_class("peer-match");
        self.remove_css_class("peer-unknown");
        match peer.as_deref() {
            Some(p) if p == local.as_str() => self.add_css_class("peer-match"),
            Some(_) => self.add_css_class("peer-mismatch"),
            None => self.add_css_class("peer-unknown"),
        };
        /* plain text: the peer-* CSS classes do the coloring, markup would be
         * rendered verbatim by the expander row.
         */
        self.set_subtitle(&format!("{port} · {status}"));
    }
}
