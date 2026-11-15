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
    pub(super) fn open_hotkey_dialog(&self) {
        let dialog = gtk::Dialog::builder()
            .transient_for(self)
            .modal(true)
            .title("Add hotkey")
            .build();

        let from_entry = gtk::Entry::builder()
            .placeholder_text("Shift+Alt+Z")
            .text("Shift+Alt+Z")
            .build();
        let to_entry = gtk::Entry::builder()
            .placeholder_text("Z")
            .text("Z")
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
        box_.append(&from_entry);
        box_.append(
            &gtk::Label::builder()
                .label("Keys sent to the peer")
                .xalign(0.0)
                .build(),
        );
        box_.append(&to_entry);

        dialog.content_area().append(&box_);
        dialog.add_button("Cancel", gtk::ResponseType::Cancel);
        dialog.add_button("Add", gtk::ResponseType::Accept);
        dialog.set_default_response(gtk::ResponseType::Accept);

        let window = self.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let from = from_entry.text().to_string();
                let to = to_entry.text().to_string();
                window.request(FrontendRequest::AddHotkey(from, to));
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
            self.imp().capture_status_row.set_visible(!capture);
            self.imp().emulation_status_row.set_visible(!emulation);
            self.imp()
                .capture_emulation_group
                .set_visible(!capture || !emulation);
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
