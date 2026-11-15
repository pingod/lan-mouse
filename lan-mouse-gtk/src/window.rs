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
    authorization_window::AuthorizationWindow, fingerprint_window::FingerprintWindow,
    key_object::KeyObject, key_row::KeyRow,
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
                            #[strong]
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
                            #[strong]
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
                            #[strong]
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
                            #[strong]
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
                            #[strong]
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
                            #[strong]
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
                        "request-position-change",
                        false,
                        closure_local!(
                            #[strong]
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

    pub(super) fn new_client(
        &self,
        handle: ClientHandle,
        client: ClientConfig,
        state: ClientState,
    ) {
        let client = ClientObject::new(handle, client, state.clone());
        self.clients().append(&client);
        self.update_placeholder_visibility();

        self.update_dns_state(handle, !state.ips.is_empty());
        self.update_active_device_row();
    }

    pub(super) fn update_client_list(
        &self,
        clients: Vec<(ClientHandle, ClientConfig, ClientState)>,
    ) {
        for (handle, client, state) in clients {
            if self.client_idx(handle).is_some() {
                self.update_client_config(handle, client);
                self.update_client_state(handle, state);
            } else {
                self.new_client(handle, client, state);
            }
        }
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

    /// Ask the daemon for the hotkey rules, version and autostart state.
    pub(super) fn request_initial_state(&self) {
        self.request(FrontendRequest::EnumerateHotkeys);
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
        self.imp().log_level_label.set_text(level);
    }

    pub(super) fn set_logs(&self, lines: Vec<String>) {
        let imp = self.imp();
        imp.log_buffer.borrow().set_text(&lines.join("\n"));
        // Keep the newest record visible.
        let view = imp.log_view.get();
        let mut end = view.buffer().end_iter();
        view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
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

        // Follow the tail unless the user scrolled up to read something.
        let view = imp.log_view.get();
        let at_bottom = view
            .vadjustment()
            .map(|adj| adj.value() + adj.page_size() >= adj.upper() - 4.0)
            .unwrap_or(true);
        if at_bottom {
            let mut end = imp.log_buffer.borrow().end_iter();
            view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
        }
    }

    /// Drop the displayed records and stop the stream while the window is
    /// hidden, so a background window costs nothing.
    pub(super) fn clear_logs(&self) {
        self.imp().log_buffer.borrow().set_text("");
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

    /// Prompt for `from` and `to`, then add the rule.
    /// Record a chord by pressing it, instead of typing key names by hand.
    ///
    /// Hand-typed names are easy to get wrong (`KeyLeftAlt` vs `LeftAlt`), and
    /// a mistyped rule silently never fires. Capturing the actual keystroke
    /// removes that class of mistake.
    pub(super) fn open_hotkey_dialog(&self) {
        let dialog = gtk::Dialog::builder()
            .transient_for(self)
            .modal(true)
            .title("Add hotkey")
            .build();

        let from_entry = gtk::Entry::builder()
            .placeholder_text("press the keys, or type e.g. Shift+Alt+Z")
            .build();
        let to_entry = gtk::Entry::builder()
            .placeholder_text("Z")
            .text("Z")
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
                .label("Local keys — sent to the other machine, your own keystroke still works")
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
                .label("Keys sent to the peer")
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
                recorder.set_text("Captured — click Add");
                record_button.set_active(false);
                glib::Propagation::Stop
            });
        }
        dialog.add_controller(controller);

        dialog.content_area().append(&box_);
        dialog.add_button("Cancel", gtk::ResponseType::Cancel);
        dialog.add_button("Add", gtk::ResponseType::Accept);
        dialog.set_default_response(gtk::ResponseType::Accept);

        let window = self.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let from = from_entry.text().to_string();
                let to = to_entry.text().to_string();
                if !from.trim().is_empty() {
                    window.request(FrontendRequest::AddHotkey(from, to));
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
    }

    pub(super) fn update_client_config(&self, handle: ClientHandle, client: ClientConfig) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        row.set_hostname(client.hostname);
        row.set_port(client.port);
        row.set_position(client.pos);
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
        row.refresh_version_status();
        self.update_active_device_row();
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
        let port = self
            .imp()
            .port_entry
            .get()
            .text()
            .as_str()
            .parse::<u16>()
            .unwrap_or(DEFAULT_PORT);
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
                #[strong(rename_to = parent)]
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
        let requester = requester.as_mut().unwrap();
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
                #[strong(rename_to = parent)]
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
}
