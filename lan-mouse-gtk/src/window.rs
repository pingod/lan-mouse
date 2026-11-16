mod imp;

use std::collections::HashMap;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::{Object, clone};
use gtk::{
    NoSelection, gio,
    glib::{self, closure_local},
};

use lan_mouse_ipc::{
    ClientConfig, ClientHandle, ClientState, DEFAULT_PORT, FrontendRequest, FrontendRequestWriter,
    Position,
};

use crate::{
    arrangement::{self, Canvas, Machine, MachineRequest},
    authorization_window::AuthorizationWindow,
    fingerprint_window::FingerprintWindow,
    key_object::KeyObject,
    key_row::KeyRow,
};

use super::{client_object::ClientObject, client_row::ClientRow};

#[cfg(target_os = "macos")]
fn set_button_content_label(button: &gtk::Button, label: &str) {
    // The Reenable/Grant/Relaunch button wraps its icon+label in an
    // AdwButtonContent (see window.ui). Walk into it and swap the label
    // rather than GtkButton::set_label, which would replace the content
    // widget and drop the icon.
    if let Some(content) = button.child().and_downcast::<adw::ButtonContent>() {
        content.set_label(label);
    }
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub(super) fn new(app: &adw::Application, conn: FrontendRequestWriter) -> Self {
        let window: Self = Object::builder().property("application", app).build();
        window
            .imp()
            .frontend_request_writer
            .borrow_mut()
            .replace(conn);
        window
    }

    fn clients(&self) -> gio::ListStore {
        self.imp()
            .clients
            .borrow()
            .clone()
            .expect("Could not get clients")
    }

    fn authorized(&self) -> gio::ListStore {
        self.imp()
            .authorized
            .borrow()
            .clone()
            .expect("Could not get authorized")
    }

    fn client_by_idx(&self, idx: u32) -> Option<ClientObject> {
        self.clients().item(idx).map(|o| o.downcast().unwrap())
    }

    fn authorized_by_idx(&self, idx: u32) -> Option<KeyObject> {
        self.authorized().item(idx).map(|o| o.downcast().unwrap())
    }

    fn row_by_idx(&self, idx: i32) -> Option<ClientRow> {
        self.imp()
            .client_list
            .get()
            .row_at_index(idx)
            .map(|o| o.downcast().expect("expected ClientRow"))
    }

    fn setup_authorized(&self) {
        let store = gio::ListStore::new::<KeyObject>();
        self.imp().authorized.replace(Some(store));
        let selection_model = NoSelection::new(Some(self.authorized()));
        self.imp().authorized_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let key_obj = obj.downcast_ref().expect("object of type `KeyObject`");
                    let row = window.create_key_row(key_obj);
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[weak]
                            window,
                            move |row: KeyRow| {
                                if let Some(key_obj) = window.authorized_by_idx(row.index() as u32)
                                {
                                    window.request_fingerprint_remove(key_obj.get_fingerprint());
                                }
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        )
    }

    fn setup_clients(&self) {
        let model = gio::ListStore::new::<ClientObject>();
        self.imp().clients.replace(Some(model));

        let selection_model = NoSelection::new(Some(self.clients()));
        self.imp().client_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let client_object = obj
                        .downcast_ref()
                        .expect("Expected object of type `ClientObject`.");
                    let row = window.create_client_row(client_object);
                    row.connect_closure(
                        "request-hostname-change",
                        false,
                        closure_local!(
                            #[weak]
                            window,
                            move |row: ClientRow, hostname: String| {
                                log::debug!("request-hostname-change");
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let hostname = Some(hostname).filter(|s| !s.is_empty());
                                    /* changed in response to FrontendEvent
                                     * -> do not request additional update */
                                    window.request(FrontendRequest::UpdateHostname(
                                        client.handle(),
                                        hostname,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-port-change",
                        false,
                        closure_local!(
                            #[weak]
                            window,
                            move |row: ClientRow, port: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::UpdatePort(
                                        client.handle(),
                                        port as u16,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-activate",
                        false,
                        closure_local!(
                            #[weak]
                            window,
                            move |row: ClientRow, active: bool| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    log::debug!(
                                        "request: {} client",
                                        if active { "activating" } else { "deactivating" }
                                    );
                                    window.request(FrontendRequest::Activate(
                                        client.handle(),
                                        active,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[weak]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::Delete(client.handle()));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-dns",
                        false,
                        closure_local!(
                            #[weak]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::ResolveDns(
                                        client.get_data().handle,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-hooks",
                        false,
                        closure_local!(
                            #[weak]
                            window,
                            move |_row: ClientRow, handle: u64, enter: String, leave: String| {
                                window.open_hooks_dialog(handle, enter, leave);
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-position-change",
                        false,
                        closure_local!(
                            #[weak]
                            window,
                            move |row: ClientRow, pos_idx: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let position = match pos_idx {
                                        0 => Position::Left,
                                        1 => Position::Right,
                                        2 => Position::Top,
                                        _ => Position::Bottom,
                                    };
                                    window.request(FrontendRequest::UpdatePosition(
                                        client.handle(),
                                        position,
                                    ));
                                }
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        );
    }

    fn setup_icon(&self) {
        self.set_icon_name(Some("de.feschber.LanMouse"));
    }

    /// workaround for a bug in libadwaita that shows an ugly line beneath
    /// the last element if a placeholder is set.
    /// https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/6308
    fn update_placeholder_visibility(&self) {
        let visible = self.clients().n_items() == 0;
        let placeholder = self.imp().client_placeholder.get();
        self.imp().client_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn update_auth_placeholder_visibility(&self) {
        let visible = self.authorized().n_items() == 0;
        let placeholder = self.imp().authorized_placeholder.get();
        self.imp().authorized_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn create_client_row(&self, client_object: &ClientObject) -> ClientRow {
        let row = ClientRow::new(client_object);
        row.bind(client_object);
        row
    }

    fn create_key_row(&self, key_object: &KeyObject) -> KeyRow {
        let row = KeyRow::new();
        row.bind(key_object);
        row
    }

    /// Update the visual arrangement from the client store. Called from every
    /// place that can change a peer (a new snapshot entry, a position change or
    /// an activation), so the canvas never shows a stale edge.
    ///
    /// The canvas is built once and then only fed new data: a rebuild on every
    /// broadcast destroyed the widget the pointer was pressed into, which killed
    /// a drag in the middle of a gesture and reset the zoom.
    fn refresh_arrangement(&self) {
        let machines = self
            .clients()
            .iter::<ClientObject>()
            .filter_map(|client| client.ok())
            .map(|client| {
                let data = client.get_data();
                /* read the ranges before `data.hostname` is moved out of the
                 * struct by `Option::filter`, which borrows the rest */
                let (edge, peer_edge, peer_size) =
                    (data.edge(), data.peer_edge(), data.peer_size());
                Machine {
                    handle: client.handle(),
                    peer_picture: input_event::Arrangement::decode(&data.peer_arrangement),
                    peer_monitors: arrangement::peer_monitors(&data.peer_displays),
                    display: (!data.display.is_empty()).then(|| data.display.clone()),
                    hostname: data
                        .hostname
                        .filter(|hostname| !hostname.is_empty())
                        .unwrap_or_else(|| "unknown host".to_string()),
                    position: data.position.parse().unwrap_or_default(),
                    active: data.active,
                    edge,
                    peer_edge,
                    peer_size,
                }
            })
            .collect::<Vec<_>>();

        let imp = self.imp();
        let local_hostname = imp.hostname_label.text().to_string();
        let switch_options = imp.switch_options.borrow().clone();
        let displays = imp.displays.borrow().clone();
        /* The one line that says whether the canvas has the numbers it needs. A
         * peer that never reported a size is drawn at a derived one, which looks
         * like a layout bug until you can see that the size was `None`. */
        /* info, not debug: whether the canvas laid itself out at all is the first
         * question a window that never changes asks, and the default level is info -
         * so a debug line here was invisible in exactly the case it was needed. */
        log::info!(
            "arrangement: local {:?} at {:?}, {} machine(s): {}",
            local_hostname,
            displays,
            machines.len(),
            machines
                .iter()
                .map(|machine| format!(
                    "{} {:?} {:?}/{:?} size {:?}",
                    machine.hostname,
                    machine.position,
                    machine.edge,
                    machine.peer_edge,
                    machine.peer_size
                ))
                .collect::<Vec<_>>()
                .join("; "),
        );

        /* The window's own log ring is per process and only readable from the
         * window, so a headless check of what the canvas decided has nowhere to look.
         * This writes it where a shell can see it, only when asked for by path. */
        if let Some(path) = std::env::var_os("LAN_MOUSE_CANVAS_TRACE") {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let peer_pictures = machines
                    .iter()
                    .filter_map(|machine| machine.peer_picture.as_ref())
                    .map(|picture| format!("rev {} of {}", picture.revision, picture.author))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(
                    file,
                    "refresh: local={local_hostname:?} machines={} peer_pictures=[{peer_pictures}]",
                    machines.len()
                );
            }
        }

        let mut canvas = imp.canvas.borrow_mut();
        let canvas = canvas.get_or_insert_with(|| {
            Canvas::build(
                &imp.arrangement_box,
                clone!(
                    #[weak(rename_to = window)]
                    self,
                    move |request| window.handle_arrangement_request(request)
                ),
            )
        });
        canvas.update(&local_hostname, &machines, &displays, &switch_options);
    }

    /// Turn what the canvas decided into a request for the daemon.
    ///
    /// One request per link: the daemon takes the edge and both ranges together,
    /// so a move does not leave the old ranges registered on the new edge in
    /// between.
    fn handle_arrangement_request(&self, request: MachineRequest) {
        match request {
            MachineRequest::Publish(arrangement) => {
                self.request(FrontendRequest::PublishArrangement { arrangement });
            }
            MachineRequest::Move {
                handle,
                position,
                display,
                edge,
                peer_edge,
            } => {
                log::debug!("request: client {handle} on the {position} edge {edge}");
                self.request(FrontendRequest::UpdateLink {
                    handle,
                    position,
                    display,
                    edge,
                    peer_edge,
                });
            }
            MachineRequest::AutoArrange(updates) => {
                for (handle, position, display, edge, peer_edge) in updates {
                    self.request(FrontendRequest::UpdateLink {
                        handle,
                        position,
                        display,
                        edge,
                        peer_edge,
                    });
                }
            }
        }
    }

    pub(super) fn new_client(
        &self,
        handle: ClientHandle,
        client: ClientConfig,
        state: ClientState,
    ) {
        // Hooks are not part of `ClientObject`, so keep them until the row that
        // the list model creates for this object exists.
        let hooks = (client.cmd.clone(), client.leave_cmd.clone());
        let client = ClientObject::new(handle, client, state.clone());
        self.clients().append(&client);
        self.update_placeholder_visibility();
        if let Some(row) = self.row_for_handle(handle) {
            row.set_hooks(hooks.0, hooks.1);
        }

        self.update_dns_state(handle, !state.ips.is_empty());
        self.update_active_device_row();
        self.refresh_arrangement();
    }

    pub(super) fn update_client_list(
        &self,
        clients: Vec<(ClientHandle, ClientConfig, ClientState)>,
    ) {
        /* Drop rows that are not part of this snapshot: `Enumerate` is the
         * authoritative client list, and only upserting left rows of deleted
         * clients behind (they could never be selected again and re-added
         * the config on the next edit).
         */
        let handles = clients
            .iter()
            .map(|(handle, _, _)| *handle)
            .collect::<Vec<_>>();
        let stale = self
            .clients()
            .iter::<ClientObject>()
            .filter_map(|client| client.ok().map(|client| client.handle()))
            .filter(|handle| !handles.contains(handle))
            .collect::<Vec<_>>();
        for handle in stale {
            log::debug!("removing client {handle}: no longer known to the daemon");
            self.delete_client(handle);
        }

        for (handle, client, state) in clients {
            if self.client_idx(handle).is_some() {
                self.update_client_config(handle, client);
                self.update_client_state(handle, state);
            } else {
                self.new_client(handle, client, state);
            }
        }
        /* `Enumerate` is the snapshot a window starts from, and the per-client
         * updates above only refresh the canvas when a *field* changed. A window that
         * opened while the peer's picture was already stored therefore drew nothing:
         * no field changed afterwards, so nothing ever asked the canvas to lay itself
         * out. */
        self.refresh_arrangement();
    }

    /// Render the daemon's hotkey rules.
    pub(super) fn set_hotkeys(&self, rules: Vec<(String, String)>) {
        let list = self.imp().hotkey_list.get();
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }

        for (idx, (from, to)) in rules.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(from.as_str())
                .subtitle(format!("sends {to}"))
                .build();

            let edit = gtk::Button::builder()
                .icon_name("document-edit-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("change the keys of this hotkey")
                .build();
            let window = self.clone();
            let (edit_from, edit_to) = (from.clone(), to.clone());
            edit.connect_clicked(move |_| {
                window.open_hotkey_dialog(Some((idx, edit_from.clone(), edit_to.clone())));
            });
            row.add_suffix(&edit);

            let delete = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("remove this hotkey")
                .build();
            let window = self.clone();
            delete.connect_clicked(move |_| {
                window.request(FrontendRequest::RemoveHotkey(idx));
            });
            row.add_suffix(&delete);

            list.append(&row);
        }

        self.imp()
            .hotkey_placeholder_row
            .set_visible(rules.is_empty());
    }

    /// Render the daemon's chord remapping rules.
    pub(super) fn set_remap_rules(&self, rules: Vec<(String, String)>) {
        let list = self.imp().remap_list.get();
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }

        for (idx, (from, to)) in rules.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(from.as_str())
                .subtitle(format!("sends {to}"))
                .build();

            let edit = gtk::Button::builder()
                .icon_name("document-edit-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("change the keys of this remapping rule")
                .build();
            let window = self.clone();
            let (edit_from, edit_to) = (from.clone(), to.clone());
            edit.connect_clicked(move |_| {
                window.open_remap_dialog(Some((idx, edit_from.clone(), edit_to.clone())));
            });
            row.add_suffix(&edit);

            let delete = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("remove this remapping rule")
                .build();
            let window = self.clone();
            delete.connect_clicked(move |_| {
                window.request(FrontendRequest::RemoveRemap(idx));
            });
            row.add_suffix(&delete);

            list.append(&row);
        }

        self.imp()
            .remap_placeholder_row
            .set_visible(rules.is_empty());
    }

    /// Show the keys that pull the pointer back to this machine.
    pub(super) fn set_release_bind(&self, bind: &str) {
        let entry = self.imp().release_bind_entry.get();
        // Do not fight the user while they are typing a new combination.
        if !entry.has_focus() {
            entry.set_text(bind);
        }
    }

    pub(super) fn request_release_bind(&self, bind: String) {
        self.request(FrontendRequest::UpdateReleaseBind(bind));
    }

    /// Show the edge-crossing thresholds on the Screens page.
    ///
    /// They change what a crossing means: an edge that needs a second hit, or
    /// whose corners are blocked, is not reachable the way the canvas alone
    /// suggests, so the canvas states them instead of leaving them in the config
    /// file.
    pub(super) fn set_switch_options(&self, options: &lan_mouse_ipc::SwitchOptions) {
        self.imp().switch_options.replace(options.clone());
        self.refresh_arrangement();
    }

    /// Ask the daemon for the hotkey rules, version and autostart state.
    pub(super) fn request_initial_state(&self) {
        self.request(FrontendRequest::EnumerateHotkeys);
        self.request(FrontendRequest::EnumerateRemap);
        self.request(FrontendRequest::EnumerateVersion);
        self.request(FrontendRequest::EnumerateServiceState);
        self.request_logs();
    }

    pub(super) fn set_version(&self, version: &str, commit: &str) {
        self.imp()
            .version_row
            .set_subtitle(&format!("{version} ({commit})"));
    }

    pub(super) fn set_autostart(&self, enabled: bool) {
        let imp = self.imp();
        imp.autostart_updating.set(true);
        imp.autostart_switch.set_active(enabled);
        imp.autostart_updating.set(false);
    }

    pub(super) fn request_autostart(&self, enabled: bool) {
        self.request(FrontendRequest::SetAutostart(enabled));
    }

    pub(super) fn set_paused(&self, paused: bool) {
        let imp = self.imp();
        imp.pause_updating.set(true);
        imp.pause_switch.set_active(paused);
        imp.pause_updating.set(false);
    }

    pub(super) fn request_pause(&self, paused: bool) {
        self.request(FrontendRequest::SetPaused(paused));
    }

    pub(super) fn request_restart(&self) {
        self.request(FrontendRequest::Restart);
    }

    pub(super) fn request_reload(&self) {
        self.request(FrontendRequest::ReloadConfig);
    }

    pub(super) fn set_log_level(&self, level: &str) {
        let imp = self.imp();
        imp.log_level_combo.set_subtitle(&level_label(level));

        // Mirror the daemon's answer into the menu without echoing it back: the
        // requested level can be clamped (for example by the build's max level),
        // so the widget has to show what is actually in effect.
        let combo = imp.log_level_combo.get();
        let Some(position) = level_position(level) else {
            return;
        };
        if combo.selected() == position {
            return;
        }
        imp.log_level_syncing.set(true);
        combo.set_selected(position);
        imp.log_level_syncing.set(false);
    }

    pub(super) fn set_logs(&self, lines: Vec<String>) {
        let imp = self.imp();
        // Trailing newline: the stream appends whole lines, and without it the
        // first streamed record would be glued onto the last backlog line.
        let text = if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        };
        imp.log_buffer.borrow().set_text(&text);
        // A refresh replaces the whole view, so it always lands on the tail.
        imp.log_follow.set(true);
        scroll_logs_to_end(&imp.log_view.get());
    }

    /// Load the backlog once, then keep streaming.
    ///
    /// Subscribing again would restart the daemon's cursor and silently drop
    /// whatever was written between the last flush and the new subscription, so
    /// only the first call enables the stream.
    pub(super) fn request_logs(&self) {
        self.request(FrontendRequest::EnumerateLogs(500));
        self.request(FrontendRequest::SetLogLevel(None));
        if !self.imp().log_streaming.replace(true) {
            // 250 ms keeps the view live without flooding the IPC socket; the
            // daemon coalesces whatever was written in between.
            self.request(FrontendRequest::SubscribeLogs(Some(250)));
        }
    }

    /// Append streamed records, trimming the view so it cannot grow forever.
    pub(super) fn append_logs(&self, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        let imp = self.imp();
        // Decided before the insert: appending grows the buffer, which would
        // make every append look like the view had been scrolled away.
        let follow = imp.log_follow.get();
        let buffer = imp.log_buffer.borrow();
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, &format!("{}\n", lines.join("\n")));

        // Keep roughly the same window the daemon retains.
        const MAX_LINES: i32 = 2000;
        if buffer.line_count() > MAX_LINES {
            let mut start = buffer.start_iter();
            let mut cut = buffer
                .iter_at_line(buffer.line_count() - MAX_LINES)
                .unwrap_or(start);
            buffer.delete(&mut start, &mut cut);
        }
        drop(buffer);

        if follow {
            scroll_logs_to_end(&imp.log_view.get());
        }
    }

    /// Drop the displayed records and stop the stream while the window is
    /// hidden, so a background window costs nothing.
    pub(super) fn clear_logs(&self) {
        let imp = self.imp();
        imp.log_buffer.borrow().set_text("");
        imp.log_follow.set(true);
    }

    pub(super) fn request_restart_interval(&self, seconds: u64) {
        let interval = if seconds == 0 { None } else { Some(seconds) };
        self.request(FrontendRequest::SetRestartInterval(interval));
    }

    pub(super) fn request_log_level(&self, level: &str) {
        self.request(FrontendRequest::SetLogLevel(Some(level.to_owned())));
    }

    /// Ask every connected peer to re-send its machine details.
    pub(super) fn refresh_peer_details(&self) {
        let store = self.imp().clients.borrow().clone();
        if let Some(store) = store {
            for i in 0..store.n_items() {
                if let Some(obj) = store.item(i).and_downcast::<ClientObject>() {
                    self.request(FrontendRequest::RefreshPeerInfo(obj.handle()));
                }
            }
        }
        // Reload the backlog too, so the view shows what arrived.
        self.request(FrontendRequest::Enumerate());
    }

    pub(super) fn request_force_release(&self) {
        self.request(FrontendRequest::ForceRelease);
    }

    pub(super) fn request_switch_device(&self) {
        self.request(FrontendRequest::SwitchDevice);
    }

    pub(super) fn set_restart_interval(&self, secs: Option<u64>) {
        self.imp()
            .restart_interval_entry
            .set_text(&secs.map(|s| s.to_string()).unwrap_or_default());
    }

    /// Open the hotkey editor.
    ///
    /// `existing` carries the index and the current keys when an existing rule
    /// is being changed; `None` adds a new one.
    pub(super) fn open_hotkey_dialog(&self, existing: Option<(usize, String, String)>) {
        self.open_chord_dialog(ChordTarget::Hotkey, existing);
    }

    /// Open the chord remapping editor.
    ///
    /// `existing` carries the index and the current keys when an existing rule
    /// is being changed; `None` adds a new one.
    pub(super) fn open_remap_dialog(&self, existing: Option<(usize, String, String)>) {
        self.open_chord_dialog(ChordTarget::Remap, existing);
    }

    /// Edit the enter/leave hook commands of one client.
    ///
    /// Both commands are sent in a single request: two separate updates would
    /// make the daemon broadcast an intermediate state in which only one of
    /// them is applied. An empty field clears that hook.
    pub(super) fn open_hooks_dialog(&self, handle: ClientHandle, enter: String, leave: String) {
        let dialog = gtk::Dialog::builder()
            .transient_for(self)
            .modal(true)
            .title("Edit hooks")
            .build();

        let enter_entry = gtk::Entry::builder()
            .placeholder_text("e.g. notify-send \"entered the other machine\"")
            .text(enter)
            .build();
        let leave_entry = gtk::Entry::builder()
            .placeholder_text("e.g. notify-send \"back on this machine\"")
            .text(leave)
            .build();

        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 8);
        box_.set_margin_top(12);
        box_.set_margin_bottom(12);
        box_.set_margin_start(12);
        box_.set_margin_end(12);

        box_.append(
            &gtk::Label::builder()
                .label("Shell commands run on this machine when the pointer enters or leaves this client. They are passed to `sh -c`, so quotes and pipes work.")
                .xalign(0.0)
                .wrap(true)
                .build(),
        );
        box_.append(
            &gtk::Label::builder()
                .label("When the pointer enters")
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build(),
        );
        box_.append(&enter_entry);
        box_.append(
            &gtk::Label::builder()
                .label("When the pointer leaves")
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build(),
        );
        box_.append(&leave_entry);
        box_.append(
            &gtk::Label::builder()
                .label("Leave a field empty to run nothing.")
                .xalign(0.0)
                .wrap(true)
                .css_classes(["dim-label"])
                .build(),
        );

        dialog.content_area().append(&box_);
        dialog.add_button("Cancel", gtk::ResponseType::Cancel);
        dialog.add_button("Save", gtk::ResponseType::Accept);
        dialog.set_default_response(gtk::ResponseType::Accept);

        let window = self.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let enter = enter_entry.text().to_string();
                let leave = leave_entry.text().to_string();
                window.request(FrontendRequest::UpdateHooks {
                    handle,
                    enter,
                    leave,
                });
            }
            dialog.close();
        });
        dialog.present();
    }

    /// Prompt for `from` and `to`, then add or update the rule.
    ///
    /// Record a chord by pressing it, instead of typing key names by hand.
    /// Hand-typed names are easy to get wrong (`KeyLeftAlt` vs `LeftAlt`), and
    /// a mistyped rule silently never fires. Capturing the actual keystroke
    /// removes that class of mistake. Hotkeys and remapping rules are stored in
    /// the same `+`-joined key-name form, so both editors share this dialog.
    fn open_chord_dialog(&self, target: ChordTarget, existing: Option<(usize, String, String)>) {
        let editing = existing.is_some();
        let dialog = gtk::Dialog::builder()
            .transient_for(self)
            .modal(true)
            .title(target.title(editing))
            .build();

        let from_entry = gtk::Entry::builder()
            .placeholder_text(target.chord_placeholder())
            .text(
                existing
                    .as_ref()
                    .map(|(_, from, _)| from.as_str())
                    .unwrap_or_default(),
            )
            .build();
        let to_entry = gtk::Entry::builder()
            .placeholder_text(target.to_placeholder())
            .text(
                existing
                    .as_ref()
                    .map(|(_, _, to)| to.as_str())
                    .unwrap_or_else(|| target.to_placeholder()),
            )
            .build();
        let recorder = gtk::Label::builder()
            .label("Click Record, then press the combination")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();

        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 8);
        box_.set_margin_top(12);
        box_.set_margin_bottom(12);
        box_.set_margin_start(12);
        box_.set_margin_end(12);

        box_.append(
            &gtk::Label::builder()
                .label(target.chord_hint())
                .xalign(0.0)
                .wrap(true)
                .build(),
        );

        let from_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        from_row.append(&from_entry);
        let record_button = gtk::ToggleButton::builder()
            .label("Record")
            .tooltip_text("capture the next key combination you press")
            .build();
        from_row.append(&record_button);
        box_.append(&from_row);
        box_.append(&recorder);

        box_.append(
            &gtk::Label::builder()
                .label(target.to_label())
                .xalign(0.0)
                .build(),
        );
        box_.append(&to_entry);

        // Capture keys while recording. `capture` phase runs before the entry
        // sees them, so a chord does not also get typed into the field.
        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let from_entry = from_entry.clone();
            let recorder = recorder.clone();
            let record_button = record_button.clone();
            controller.connect_key_pressed(move |_, keyval, _code, state| {
                if !record_button.is_active() {
                    return glib::Propagation::Proceed;
                }
                let Some(chord) = chord_from_key(keyval, state) else {
                    return glib::Propagation::Stop;
                };
                from_entry.set_text(&chord);
                recorder.set_text(if editing {
                    "Captured — click Save"
                } else {
                    "Captured — click Add"
                });
                record_button.set_active(false);
                glib::Propagation::Stop
            });
        }
        dialog.add_controller(controller);

        dialog.content_area().append(&box_);
        dialog.add_button("Cancel", gtk::ResponseType::Cancel);
        dialog.add_button(
            if editing { "Save" } else { "Add" },
            gtk::ResponseType::Accept,
        );
        dialog.set_default_response(gtk::ResponseType::Accept);

        let window = self.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let from = from_entry.text().to_string();
                let to = to_entry.text().to_string();
                if !from.trim().is_empty() {
                    let request = match existing {
                        Some((idx, _, _)) => target.update_request(idx, from, to),
                        None => target.add_request(from, to),
                    };
                    window.request(request);
                }
            }
            dialog.close();
        });
        dialog.present();
    }

    pub(super) fn update_port(&self, port: u16, msg: Option<String>) {
        if let Some(msg) = msg {
            self.show_toast(msg.as_str());
        }
        self.imp().set_port(port);
    }

    fn client_idx(&self, handle: ClientHandle) -> Option<usize> {
        self.clients()
            .iter::<ClientObject>()
            .position(|c| c.ok().map(|c| c.handle() == handle).unwrap_or_default())
    }

    pub(super) fn delete_client(&self, handle: ClientHandle) {
        let Some(idx) = self.client_idx(handle) else {
            log::warn!("could not find client with handle {handle}");
            return;
        };

        self.clients().remove(idx as u32);
        if self.clients().n_items() == 0 {
            self.update_placeholder_visibility();
        }
        self.refresh_arrangement();
    }

    pub(super) fn update_client_config(&self, handle: ClientHandle, client: ClientConfig) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        row.set_hostname(client.hostname.clone());
        row.set_port(client.port);
        row.set_position(client.pos);
        // The hook commands are part of the client configuration, so the row
        // shows them after every broadcast — including another frontend's edit.
        row.set_hooks(client.cmd, client.leave_cmd);
        /* The canvas reads the link off the ClientObject, so the ranges have to
         * land there: a change made by the CLI or the config file would otherwise
         * not show up until the row was rebuilt.
         *
         * Only the ranges. The hostname and the position are written by
         * `row.set_hostname`/`set_position` above, which block the row's own
         * `changed` handlers while they do it. Writing those two properties here
         * as well would go through the property binding instead, which does not
         * block anything: the entry would fire `changed`, the window would ask the
         * daemon to set the hostname, the daemon would broadcast, and this function
         * would write the property again - a loop that rewrote the config file
         * hundreds of times a second.
         */
        if let Some(object) = self.client_object_for_handle(handle) {
            object.set_property("display", client.display.clone().unwrap_or_default());
            object.set_property("edge-start", client.edge.start() as u32);
            object.set_property("edge-end", client.edge.end() as u32);
            object.set_property("peer-edge-start", client.peer_edge.start() as u32);
            object.set_property("peer-edge-end", client.peer_edge.end() as u32);
        }
        self.refresh_arrangement();
    }

    pub(super) fn update_client_state(&self, handle: ClientHandle, state: ClientState) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        let Some(client_object) = self.client_object_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };

        /* activation state */
        row.set_active(state.active);
        row.set_peer_details(&state);

        /* dns state */
        client_object.set_resolving(state.resolving);

        row.set_status(&state);

        self.update_dns_state(handle, !state.ips.is_empty());
        let ips = state
            .ips
            .into_iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>();
        client_object.set_ips(ips);

        /* peer build version (drives the version-match indicator) */
        client_object.set_property(
            "peer-commit",
            crate::client_object::peer_commit_to_string(state.peer_commit),
        );
        /* the peer's screen size, which the canvas draws its card to */
        client_object.set_property("peer-displays", state.peer_displays);
        /* The picture the peer publishes is the one this window may have to draw
         * instead of its own, so a change in it is a change in the canvas - not just
         * in a row on the Devices page. */
        let arrangement_changed =
            client_object.property::<String>("peer-arrangement") != state.peer_arrangement;
        client_object.set_property("peer-arrangement", state.peer_arrangement);
        row.refresh_version_status();
        self.update_active_device_row();
        if arrangement_changed {
            self.refresh_arrangement();
        }
    }

    fn client_object_for_handle(&self, handle: ClientHandle) -> Option<ClientObject> {
        self.client_idx(handle)
            .and_then(|i| self.client_by_idx(i as u32))
    }

    fn row_for_handle(&self, handle: ClientHandle) -> Option<ClientRow> {
        self.client_idx(handle)
            .and_then(|i| self.row_by_idx(i as i32))
    }

    fn update_dns_state(&self, handle: ClientHandle, resolved: bool) {
        if let Some(client_row) = self.row_for_handle(handle) {
            client_row.set_dns_state(resolved);
        }
    }

    fn request_port_change(&self) {
        let text = self.imp().port_entry.get().text();
        let text = text.trim();
        /* an empty entry means "use the default port", anything else has to
         * be a valid port number. Silently falling back to the default port
         * (as `unwrap_or(DEFAULT_PORT)` did) changed the port of a running
         * session on a typo.
         */
        let port = if text.is_empty() {
            DEFAULT_PORT
        } else {
            match text.parse::<u16>() {
                Ok(port) => port,
                Err(_) => {
                    self.show_toast(&format!("invalid port: {text}"));
                    return;
                }
            }
        };
        self.request(FrontendRequest::ChangePort(port));
    }

    fn request_capture(&self) {
        self.request(FrontendRequest::EnableCapture);
    }

    fn request_emulation(&self) {
        self.request(FrontendRequest::EnableEmulation);
    }

    fn request_client_create(&self) {
        self.request(FrontendRequest::Create);
    }

    fn open_fingerprint_dialog(&self, fp: Option<String>) {
        let window = FingerprintWindow::new(fp);
        window.set_transient_for(Some(self));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[weak(rename_to = parent)]
                self,
                move |w: FingerprintWindow, desc: String, fp: String| {
                    parent.request_fingerprint_add(desc, fp);
                    w.close();
                }
            ),
        );
        window.present();
    }

    fn request_fingerprint_add(&self, desc: String, fp: String) {
        self.request(FrontendRequest::AuthorizeKey(desc, fp));
    }

    fn request_fingerprint_remove(&self, fp: String) {
        self.request(FrontendRequest::RemoveAuthorizedKey(fp));
    }

    fn request(&self, request: FrontendRequest) {
        let mut requester = self.imp().frontend_request_writer.borrow_mut();
        let Some(requester) = requester.as_mut() else {
            log::error!("frontend connection is not available");
            return;
        };
        if let Err(e) = requester.request(request) {
            log::error!("error sending message: {e}");
        };
    }

    pub(super) fn show_toast(&self, msg: &str) {
        let toast = adw::Toast::new(msg);
        self.add_toast(toast);
    }

    pub(super) fn add_toast(&self, toast: adw::Toast) {
        let toast_overlay = &self.imp().toast_overlay;
        toast_overlay.add_toast(toast);
    }

    pub(super) fn set_capture(&self, active: bool) {
        self.imp().capture_active.replace(active);
        self.update_capture_emulation_status();
    }

    pub(super) fn set_emulation(&self, active: bool) {
        self.imp().emulation_active.replace(active);
        self.update_capture_emulation_status();
    }

    #[cfg(target_os = "macos")]
    pub(super) fn refresh_capture_emulation_status(&self) {
        self.update_capture_emulation_status();
    }

    /// Render the live service counters and the backend rows.
    pub(super) fn set_runtime(&self, info: lan_mouse_ipc::RuntimeInfo) {
        let imp = self.imp();

        imp.peers_row.set_subtitle(&format!(
            "{} outbound · {} inbound",
            info.outbound, info.inbound
        ));
        imp.latency_row.set_subtitle(&if info.best_rtt_ms > 0 {
            format!("{} ms", info.best_rtt_ms)
        } else {
            "no peer responding".to_owned()
        });
        imp.uptime_row
            .set_subtitle(&format_duration(info.uptime_secs));
        imp.port_row.set_subtitle(&info.port.to_string());

        /* The arrangement canvas draws every monitor to scale with this, which is
         * what makes an edge range mean something on the picture. `displays` is the
         * per-monitor report; `screen_width`/`screen_height` are only its bounding
         * box, kept for a daemon that predates the per-monitor field. */
        let displays = if info.displays.is_empty() {
            /* an older daemon: fall back to the one box it knows */
            if info.screen_width > 0 && info.screen_height > 0 {
                format!(
                    "screen:0,0:{}x{}:normal:1",
                    info.screen_width, info.screen_height
                )
            } else {
                String::new()
            }
        } else {
            info.displays.clone()
        };
        if *imp.displays.borrow() != displays {
            *imp.displays.borrow_mut() = displays;
            self.refresh_arrangement();
        }

        // Traffic totals live on the clients; sum them here so the row is
        // filled even when the runtime snapshot has no counter of its own.
        let (sent, recv) = imp
            .clients
            .borrow()
            .clone()
            .map(|store| {
                let mut s = 0u64;
                let mut r = 0u64;
                for i in 0..store.n_items() {
                    if let Some(obj) = store.item(i).and_downcast::<ClientObject>() {
                        s += obj.property::<u64>("forwarded");
                        r += obj.property::<u64>("received");
                    }
                }
                (s, r)
            })
            .unwrap_or((0, 0));
        imp.traffic_row
            .set_subtitle(&format!("{sent} sent · {recv} received"));

        // Backends: name what is actually running, not just on/off.
        let capture_ok = info.capture_alive;
        imp.capture_status_row.set_subtitle(&if info.paused {
            "paused".to_owned()
        } else if capture_ok {
            format!("{} · running", info.capture_backend)
        } else {
            format!("{} · not available", info.capture_backend)
        });
        imp.capture_status_row
            .set_css_classes(if capture_ok { &[] } else { &["warning"] });
        imp.input_capture_button
            .set_visible(!capture_ok && !info.paused);

        let emulation_ok = info.emulation_alive;
        imp.emulation_status_row.set_subtitle(&if info.paused {
            "paused".to_owned()
        } else if emulation_ok {
            format!("{} · running", info.emulation_backend)
        } else {
            format!("{} · not available", info.emulation_backend)
        });
        imp.emulation_status_row
            .set_css_classes(if emulation_ok { &[] } else { &["warning"] });
        imp.input_emulation_button
            .set_visible(!emulation_ok && !info.paused);

        // "Controlling" is a fact from the capture stream, not a guess.
        imp.active_device_row.set_subtitle(&if info.capturing {
            if info.capturing_peer.is_empty() {
                "a peer holds the pointer".to_owned()
            } else {
                format!("{} holds the pointer", info.capturing_peer)
            }
        } else {
            "this machine".to_owned()
        });
    }

    /// Render the inbound connection list and the latency summary.
    pub(super) fn set_incoming(&self, infos: Vec<lan_mouse_ipc::IncomingInfo>) {
        let imp = self.imp();
        let list = imp.incoming_list.get();
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        imp.incoming_placeholder.set_visible(infos.is_empty());

        for info in &infos {
            list.append(&Self::incoming_row(info));
        }

        let active = infos.iter().find(|i| i.active);
        imp.latency_row.set_subtitle(&match active {
            Some(i) => format!("{} is driving · {}s idle", i.addr, i.idle_secs),
            None => "no inbound traffic".to_owned(),
        });
    }

    /// One expandable row per inbound peer, with everything it reported.
    fn incoming_row(info: &lan_mouse_ipc::IncomingInfo) -> adw::ExpanderRow {
        let title = if info.hostname.is_empty() {
            info.addr.to_string()
        } else {
            format!("{} ({})", info.hostname, info.addr)
        };
        let state = if info.active { "driving" } else { "connected" };
        let row = adw::ExpanderRow::builder()
            .title(title)
            .subtitle(format!(
                "{state} · from {} · {} events",
                info.pos, info.received
            ))
            .build();

        // Grouped with caption rows: thirteen flat entries pushed everything
        // else off the page, and a nested expander is not allowed here.
        add_group(&row, "System");
        add_detail(&row, "Operating system", &info.os);
        add_detail(&row, "OS version", &info.os_version);
        add_detail(&row, "Kernel", &info.kernel);
        add_detail(&row, "Architecture", &info.arch);
        add_detail(&row, "Displays", &info.displays);

        add_group(&row, "Network");
        add_detail(&row, "Address", &info.addr.to_string());
        add_detail(&row, "Entry side", &info.pos.to_string());
        add_detail(&row, "Events received", &info.received.to_string());
        add_detail(&row, "Idle", &format!("{}s", info.idle_secs));
        add_detail(&row, "Certificate", &info.fingerprint);

        add_group(&row, "Build");
        add_detail(&row, "Hostname", &info.hostname);
        add_detail(&row, "lan-mouse version", &info.app_version);
        add_detail(&row, "Build commit", &info.commit);
        row
    }

    /// Show which machine the pointer is currently on, plus the best peer RTT.
    fn update_active_device_row(&self) {
        let imp = self.imp();
        let store = imp.clients.borrow().clone();
        let mut controlling: Option<String> = None;
        let mut best_rtt: Option<u64> = None;

        if let Some(store) = store {
            for i in 0..store.n_items() {
                let Some(obj) = store.item(i).and_downcast::<ClientObject>() else {
                    continue;
                };
                let name = obj
                    .property::<Option<String>>("hostname")
                    .unwrap_or_else(|| "peer".to_owned());
                if obj.property::<bool>("connected") {
                    controlling.get_or_insert(name.clone());
                }
                let rtt = obj.property::<u32>("rtt-ms");
                if rtt > 0 {
                    best_rtt = Some(best_rtt.map_or(rtt as u64, |b: u64| b.min(rtt as u64)));
                }
            }
        }

        // Activity group: how many peers are connected and how much has moved.
        let mut connected = 0usize;
        let mut forwarded = 0u64;
        let mut received = 0u64;
        if let Some(store) = imp.clients.borrow().clone() {
            for i in 0..store.n_items() {
                let Some(obj) = store.item(i).and_downcast::<ClientObject>() else {
                    continue;
                };
                if obj.property::<bool>("connected") {
                    connected += 1;
                }
                forwarded += obj.property::<u64>("forwarded");
                received += obj.property::<u64>("received");
            }
        }
        imp.peers_row.set_subtitle(&if connected == 0 {
            "no peer connected".to_owned()
        } else {
            format!("{connected} connected")
        });
        imp.traffic_row
            .set_subtitle(&format!("{forwarded} sent · {received} received"));

        match controlling {
            Some(name) => {
                imp.active_device_row
                    .set_subtitle(&format!("{name} is driving this machine"));
            }
            None => {
                let subtitle = match best_rtt {
                    Some(rtt) => format!("this machine · peer rtt {rtt} ms"),
                    None => "this machine".to_owned(),
                };
                imp.active_device_row.set_subtitle(&subtitle);
            }
        }
    }

    fn update_capture_emulation_status(&self) {
        let capture = self.imp().capture_active.get();
        let emulation = self.imp().emulation_active.get();

        #[cfg(target_os = "macos")]
        {
            // On macOS, capture and emulation share the same TCC gate
            // (Accessibility). Collapse to a single warning row —
            // emulation_status_row stays hidden and capture_status_row
            // doubles as the shared status indicator. Its text and
            // button mutate based on whether we're waiting for AX or
            // waiting for the user to relaunch the app.
            let anything_off = !capture || !emulation;
            self.imp().emulation_status_row.set_visible(false);
            self.imp().capture_status_row.set_visible(anything_off);
            self.imp().capture_emulation_group.set_visible(anything_off);

            if anything_off {
                self.update_macos_warning_row_text();
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            // Always visible: a healthy backend is information too, and hiding
            // the whole group left the Status page looking empty.
            self.imp().capture_emulation_group.set_visible(true);
            self.imp().capture_status_row.set_visible(true);
            self.imp().emulation_status_row.set_visible(true);

            let capture_title = if capture {
                "input capture"
            } else {
                "input capture is disabled"
            };
            self.imp().capture_status_row.set_title(capture_title);
            self.imp()
                .capture_status_row
                .set_css_classes(if capture { &[] } else { &["warning"] });
            self.imp().input_capture_button.set_visible(!capture);

            let emulation_title = if emulation {
                "input emulation"
            } else {
                "input emulation is disabled"
            };
            self.imp().emulation_status_row.set_title(emulation_title);
            self.imp()
                .emulation_status_row
                .set_css_classes(if emulation { &[] } else { &["warning"] });
            self.imp().input_emulation_button.set_visible(!emulation);
        }
    }

    #[cfg(target_os = "macos")]
    fn update_macos_warning_row_text(&self) {
        let row = &self.imp().capture_status_row;
        let button = &self.imp().input_capture_button;

        if crate::macos_privacy::accessibility_granted() {
            // AX granted but capture/emulation still off → the daemon
            // subprocess bailed at startup and needs a fresh process to
            // re-initialize with the new grant in place.
            row.set_title("relaunch required");
            row.set_subtitle("Accessibility granted — restart to activate capture and emulation");
            set_button_content_label(button, "Relaunch");
        } else {
            // AX missing → send the user to System Settings.
            row.set_title("input capture is disabled");
            row.set_subtitle("grant Accessibility permission to enable");
            set_button_content_label(button, "Grant");
        }
    }

    pub(super) fn set_authorized_keys(&self, fingerprints: HashMap<String, String>) {
        let authorized = self.authorized();
        // clear list
        authorized.remove_all();
        /* Hashmap iteration order is random, so the list used to reorder
         * itself on every refresh. Sort it to keep the rows stable.
         */
        let mut fingerprints = fingerprints.into_iter().collect::<Vec<_>>();
        fingerprints.sort();
        // insert fingerprints
        for (fingerprint, description) in fingerprints {
            let key_obj = KeyObject::new(description, fingerprint);
            authorized.append(&key_obj);
        }
        self.update_auth_placeholder_visibility();
    }

    pub(super) fn set_pk_fp(&self, fingerprint: &str) {
        self.imp().fingerprint_row.set_subtitle(fingerprint);
    }

    pub(super) fn request_authorization(&self, fingerprint: &str) {
        if let Some(w) = self.imp().authorization_window.borrow_mut().take() {
            w.close();
        }
        let window = AuthorizationWindow::new(fingerprint);
        window.set_transient_for(Some(self));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[weak(rename_to = parent)]
                self,
                move |w: AuthorizationWindow, fp: String| {
                    w.close();
                    parent.open_fingerprint_dialog(Some(fp));
                }
            ),
        );
        window.connect_closure(
            "cancel-clicked",
            false,
            closure_local!(move |w: AuthorizationWindow| {
                w.close();
            }),
        );
        window.present();
        self.imp().authorization_window.replace(Some(window));
    }
}

/// Which rule list the shared chord dialog edits.
///
/// Hotkeys and remapping rules are both stored as `+`-joined key names and both
/// live in `config.toml`, so they differ only in wording and in which requests
/// the dialog sends.
#[derive(Clone, Copy)]
enum ChordTarget {
    /// `[[hotkey]]`: fire a key on the peer while you keep typing here.
    Hotkey,
    /// `[[remap]]`: rewrite a chord into different keys before it is sent.
    Remap,
}

impl ChordTarget {
    fn title(self, editing: bool) -> &'static str {
        match (self, editing) {
            (ChordTarget::Hotkey, true) => "Edit hotkey",
            (ChordTarget::Hotkey, false) => "Add hotkey",
            (ChordTarget::Remap, true) => "Edit remap rule",
            (ChordTarget::Remap, false) => "Add remap rule",
        }
    }

    fn chord_placeholder(self) -> &'static str {
        match self {
            ChordTarget::Hotkey => "press the keys, or type e.g. Shift+Alt+Z",
            ChordTarget::Remap => "press the chord to intercept, e.g. Shift+Alt+Q",
        }
    }

    fn chord_hint(self) -> &'static str {
        match self {
            ChordTarget::Hotkey => {
                "Local keys — sent to the other machine, your own keystroke still works"
            }
            ChordTarget::Remap => {
                "Keys to intercept — the peer receives the keys below instead of these"
            }
        }
    }

    fn to_label(self) -> &'static str {
        match self {
            ChordTarget::Hotkey => "Keys sent to the peer",
            ChordTarget::Remap => "Keys sent instead",
        }
    }

    fn to_placeholder(self) -> &'static str {
        match self {
            ChordTarget::Hotkey => "Z",
            ChordTarget::Remap => "Q",
        }
    }

    fn add_request(self, from: String, to: String) -> FrontendRequest {
        match self {
            ChordTarget::Hotkey => FrontendRequest::AddHotkey(from, to),
            ChordTarget::Remap => FrontendRequest::AddRemap(from, to),
        }
    }

    fn update_request(self, idx: usize, from: String, to: String) -> FrontendRequest {
        match self {
            ChordTarget::Hotkey => FrontendRequest::UpdateHotkey(idx, from, to),
            ChordTarget::Remap => FrontendRequest::UpdateRemap(idx, from, to),
        }
    }
}

/// Build a chord string (`Shift+Alt+Z`) from a key press.
///
/// Returns `None` when the press is a bare modifier: the user is still
/// assembling the combination, so there is nothing to record yet.
fn chord_from_key(keyval: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> Option<String> {
    use gtk::gdk::ModifierType;

    let mut parts: Vec<&str> = Vec::new();
    if state.contains(ModifierType::SHIFT_MASK) {
        parts.push("KeyLeftShift");
    }
    if state.contains(ModifierType::CONTROL_MASK) {
        parts.push("KeyLeftCtrl");
    }
    if state.contains(ModifierType::ALT_MASK) {
        parts.push("KeyLeftAlt");
    }
    if state.contains(ModifierType::SUPER_MASK) {
        parts.push("KeyLeftMeta");
    }

    // A modifier alone is not a complete chord.
    let is_modifier = matches!(
        keyval,
        gtk::gdk::Key::Shift_L
            | gtk::gdk::Key::Shift_R
            | gtk::gdk::Key::Control_L
            | gtk::gdk::Key::Control_R
            | gtk::gdk::Key::Alt_L
            | gtk::gdk::Key::Alt_R
            | gtk::gdk::Key::Super_L
            | gtk::gdk::Key::Super_R
            | gtk::gdk::Key::Meta_L
            | gtk::gdk::Key::Meta_R
            | gtk::gdk::Key::ISO_Level3_Shift
    );
    if is_modifier {
        return None;
    }

    let name = key_name(keyval)?;
    if !parts.is_empty() {
        return Some(format!("{}+{}", parts.join("+"), name));
    }
    Some(name.to_owned())
}

/// Map a GDK keyval to the config's key spelling.
///
/// The daemon accepts friendly names (`Z`, `LeftShift`) and canonicalises them,
/// so only the shape has to be right here.
fn key_name(keyval: gtk::gdk::Key) -> Option<&'static str> {
    let name = match keyval {
        gtk::gdk::Key::space => "Space",
        gtk::gdk::Key::Return => "Enter",
        gtk::gdk::Key::Tab => "Tab",
        gtk::gdk::Key::Escape => "Escape",
        gtk::gdk::Key::BackSpace => "Backspace",
        gtk::gdk::Key::Delete => "Delete",
        gtk::gdk::Key::Left => "Left",
        gtk::gdk::Key::Right => "Right",
        gtk::gdk::Key::Up => "Up",
        gtk::gdk::Key::Down => "Down",
        gtk::gdk::Key::Home => "Home",
        gtk::gdk::Key::End => "End",
        gtk::gdk::Key::Page_Up => "PageUp",
        gtk::gdk::Key::Page_Down => "PageDown",
        gtk::gdk::Key::F1 => "F1",
        gtk::gdk::Key::F2 => "F2",
        gtk::gdk::Key::F3 => "F3",
        gtk::gdk::Key::F4 => "F4",
        gtk::gdk::Key::F5 => "F5",
        gtk::gdk::Key::F6 => "F6",
        gtk::gdk::Key::F7 => "F7",
        gtk::gdk::Key::F8 => "F8",
        gtk::gdk::Key::F9 => "F9",
        gtk::gdk::Key::F10 => "F10",
        gtk::gdk::Key::F11 => "F11",
        gtk::gdk::Key::F12 => "F12",
        _ => return single_char_name(keyval),
    };
    Some(name)
}

/// Printable characters are named by themselves (`z` -> `KeyZ` upstream).
fn single_char_name(keyval: gtk::gdk::Key) -> Option<&'static str> {
    let c = keyval.to_unicode()?;
    let upper = c.to_ascii_uppercase();
    if !upper.is_ascii_alphanumeric() {
        return None;
    }
    // A small fixed table keeps the returned value `'static`; only the letters
    // and digits a hotkey realistically uses are covered.
    const NAMES: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let idx = NAMES.chars().position(|n| n == upper)?;
    Some(&NAMES[idx..idx + 1])
}

/// A caption row separating groups inside an expander.
fn add_group(row: &adw::ExpanderRow, title: &str) {
    let caption = adw::ActionRow::builder()
        .title(title)
        .css_classes(["heading", "dim-label"])
        .activatable(false)
        .build();
    row.add_row(&caption);
}

/// A labelled detail row, skipped when the peer did not report the field.
fn add_detail(row: &adw::ExpanderRow, label: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    row.add_row(
        &adw::ActionRow::builder()
            .title(label)
            .subtitle(value)
            .build(),
    );
}

/// Human-readable duration, e.g. `3d 4h 12m`.
fn format_duration(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {}s", secs % 60)
    } else {
        format!("{}s", secs)
    }
}

/// Put the newest record at the bottom of the viewport.
///
/// `scroll_to_iter` only guarantees the mark becomes visible, which leaves the
/// last line half cut off after a word-wrapped record; pinning the adjustment
/// on the next idle makes the bottom edge line up exactly.
fn scroll_logs_to_end(view: &gtk::TextView) {
    let mut end = view.buffer().end_iter();
    view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
    let view = view.clone();
    glib::idle_add_local_once(move || {
        if let Some(adjustment) = view.vadjustment() {
            adjustment
                .set_value((adjustment.upper() - adjustment.page_size()).max(adjustment.lower()));
        }
    });
}

/// The menu entries, in the order the `.ui` file lists them.
const LOG_LEVELS: [&str; 6] = ["off", "error", "warn", "info", "debug", "trace"];

/// Subtitle text for a level name reported by the daemon.
fn level_label(level: &str) -> String {
    format!("Active: {level}")
}

/// Index of `level` in the menu, so the widget can show the effective level.
fn level_position(level: &str) -> Option<u32> {
    LOG_LEVELS
        .iter()
        .position(|candidate| *candidate == level)
        .map(|index| index as u32)
}

#[cfg(test)]
mod chord_tests {
    use super::*;
    use gtk::gdk::{Key, ModifierType};

    #[test]
    fn letter_with_modifiers() {
        assert_eq!(
            chord_from_key(Key::z, ModifierType::SHIFT_MASK | ModifierType::ALT_MASK),
            Some("KeyLeftShift+KeyLeftAlt+Z".to_owned())
        );
    }

    #[test]
    fn bare_modifier_is_not_a_chord() {
        // The user is still assembling the combination.
        assert_eq!(chord_from_key(Key::Shift_L, ModifierType::SHIFT_MASK), None);
        assert_eq!(chord_from_key(Key::Alt_L, ModifierType::ALT_MASK), None);
    }

    #[test]
    fn single_key_without_modifiers() {
        assert_eq!(
            chord_from_key(Key::z, ModifierType::empty()),
            Some("Z".to_owned())
        );
    }

    #[test]
    fn named_keys_map_to_config_spelling() {
        assert_eq!(
            chord_from_key(Key::F5, ModifierType::CONTROL_MASK),
            Some("KeyLeftCtrl+F5".to_owned())
        );
        assert_eq!(
            chord_from_key(Key::space, ModifierType::empty()),
            Some("Space".to_owned())
        );
        assert_eq!(
            chord_from_key(Key::Page_Down, ModifierType::empty()),
            Some("PageDown".to_owned())
        );
    }

    #[test]
    fn punctuation_is_rejected_rather_than_misnamed() {
        // Better to record nothing than a name the daemon cannot resolve.
        assert_eq!(chord_from_key(Key::semicolon, ModifierType::empty()), None);
    }

    #[test]
    fn chord_target_routes_to_the_matching_request() {
        // The shared dialog must talk to the remap rule list, not the hotkey
        // list: the two are stored in different parts of config.toml.
        assert_eq!(
            ChordTarget::Remap.add_request("KeyLeftShift+Q".to_owned(), "Q".to_owned()),
            FrontendRequest::AddRemap("KeyLeftShift+Q".to_owned(), "Q".to_owned())
        );
        assert_eq!(
            ChordTarget::Remap.update_request(2, "KeyLeftShift+Q".to_owned(), "Q".to_owned()),
            FrontendRequest::UpdateRemap(2, "KeyLeftShift+Q".to_owned(), "Q".to_owned())
        );
        assert_eq!(
            ChordTarget::Hotkey.update_request(1, "A".to_owned(), "B".to_owned()),
            FrontendRequest::UpdateHotkey(1, "A".to_owned(), "B".to_owned())
        );
        assert_eq!(
            ChordTarget::Hotkey.add_request("A".to_owned(), "B".to_owned()),
            FrontendRequest::AddHotkey("A".to_owned(), "B".to_owned())
        );
    }
}
