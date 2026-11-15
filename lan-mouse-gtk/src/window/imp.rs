use std::cell::{Cell, RefCell};

use adw::subclass::prelude::*;
use adw::{ActionRow, PreferencesGroup, ToastOverlay, prelude::*};
use glib::subclass::InitializingObject;
use gtk::glib::clone;
use gtk::{Button, CompositeTemplate, Entry, Image, Label, ListBox, gdk, gio, glib};

use lan_mouse_ipc::{DEFAULT_PORT, FrontendRequestWriter};

use crate::authorization_window::AuthorizationWindow;

#[derive(CompositeTemplate, Default)]
#[template(resource = "/de/feschber/LanMouse/window.ui")]
pub struct Window {
    #[template_child]
    pub authorized_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub fingerprint_row: TemplateChild<ActionRow>,
    #[template_child]
    pub port_edit_apply: TemplateChild<Button>,
    #[template_child]
    pub port_edit_cancel: TemplateChild<Button>,
    #[template_child]
    pub client_list: TemplateChild<ListBox>,
    #[template_child]
    pub client_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub port_entry: TemplateChild<Entry>,
    #[template_child]
    pub hostname_copy_icon: TemplateChild<Image>,
    #[template_child]
    pub hostname_label: TemplateChild<Label>,
    #[template_child]
    pub toast_overlay: TemplateChild<ToastOverlay>,
    #[template_child]
    pub capture_emulation_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub capture_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub emulation_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub input_emulation_button: TemplateChild<Button>,
    #[template_child]
    pub input_capture_button: TemplateChild<Button>,
    #[template_child]
    pub authorized_list: TemplateChild<ListBox>,
    #[template_child]
    pub hotkey_list: TemplateChild<ListBox>,
    #[template_child]
    pub hotkey_placeholder_row: TemplateChild<ActionRow>,
    #[template_child]
    pub version_row: TemplateChild<ActionRow>,
    #[template_child]
    pub autostart_switch: TemplateChild<gtk::Switch>,
    #[template_child]
    pub pause_switch: TemplateChild<gtk::Switch>,
    #[template_child]
    pub restart_interval_entry: TemplateChild<gtk::Entry>,
    #[template_child]
    pub log_level_label: TemplateChild<gtk::Label>,
    #[template_child]
    pub log_view: TemplateChild<gtk::TextView>,
    #[template_child]
    pub log_level_combo: TemplateChild<adw::ComboRow>,
    #[template_child]
    pub main_stack: TemplateChild<adw::ViewStack>,
    #[template_child]
    pub active_device_row: TemplateChild<ActionRow>,
    #[template_child]
    pub peers_row: TemplateChild<ActionRow>,
    #[template_child]
    pub traffic_row: TemplateChild<ActionRow>,
    pub clients: RefCell<Option<gio::ListStore>>,
    pub authorized: RefCell<Option<gio::ListStore>>,
    pub frontend_request_writer: RefCell<Option<FrontendRequestWriter>>,
    pub port: Cell<u16>,
    pub capture_active: Cell<bool>,
    pub emulation_active: Cell<bool>,
    pub authorization_window: RefCell<Option<AuthorizationWindow>>,
    /// set while the autostart switch is being updated from the daemon
    pub autostart_updating: Cell<bool>,
    /// set while the pause switch is being updated from the daemon
    pub pause_updating: Cell<bool>,
    pub log_buffer: RefCell<gtk::TextBuffer>,
    /// set once the daemon started streaming; subscribing twice would reset its
    /// cursor and lose the records in between
    pub log_streaming: Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for Window {
    // `NAME` needs to match `class` attribute of template
    const NAME: &'static str = "LanMouseWindow";
    const ABSTRACT: bool = false;

    type Type = super::Window;
    type ParentType = adw::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

#[gtk::template_callbacks]
impl Window {
    #[template_callback]
    fn handle_add_hotkey_pressed(&self, _button: &Button) {
        self.obj().open_hotkey_dialog();
    }

    /// `notify::active` delivers a `GParamSpec`, not the switch itself.
    #[template_callback]
    fn handle_autostart_toggled(&self, _param: glib::ParamSpec) {
        // Setting the state from the daemon also emits `notify::active`;
        // only forward real user toggles.
        if self.autostart_updating.get() {
            return;
        }
        let active = self.autostart_switch.is_active();
        self.obj().request_autostart(active);
    }

    #[template_callback]
    fn handle_force_release_pressed(&self, _button: &Button) {
        self.obj().request_force_release();
    }

    #[template_callback]
    fn handle_switch_device_pressed(&self, _button: &Button) {
        self.obj().request_switch_device();
    }

    #[template_callback]
    fn handle_log_refresh_pressed(&self, _button: &Button) {
        self.obj().request_logs();
    }

    #[template_callback]
    fn handle_log_clear_pressed(&self, _button: &Button) {
        self.obj().clear_logs();
    }

    #[template_callback]
    fn handle_restart_interval_apply(&self, _button: &Button) {
        let text = self.restart_interval_entry.text();
        let seconds = text.trim().parse::<u64>().unwrap_or(0);
        self.obj().request_restart_interval(seconds);
    }

    #[template_callback]
    fn handle_pause_toggled(&self, _param: glib::ParamSpec) {
        if self.pause_updating.get() {
            return;
        }
        let paused = self.pause_switch.is_active();
        self.obj().request_pause(paused);
    }

    #[template_callback]
    fn handle_restart_pressed(&self, _button: &Button) {
        self.obj().request_restart();
    }

    #[template_callback]
    fn handle_reload_pressed(&self, _button: &Button) {
        self.obj().request_reload();
    }

    #[template_callback]
    fn handle_add_client_pressed(&self, _button: &Button) {
        self.obj().request_client_create();
    }

    #[template_callback]
    fn handle_copy_hostname(&self, _: &Button) {
        if let Ok(hostname) = hostname::get() {
            let display = gdk::Display::default().unwrap();
            let clipboard = display.clipboard();
            clipboard.set_text(hostname.to_str().expect("hostname: invalid utf8"));
            let icon = self.hostname_copy_icon.clone();
            icon.set_icon_name(Some("emblem-ok-symbolic"));
            icon.set_css_classes(&["success"]);
            glib::spawn_future_local(clone!(
                #[weak]
                icon,
                async move {
                    glib::timeout_future_seconds(1).await;
                    icon.set_icon_name(Some("edit-copy-symbolic"));
                    icon.set_css_classes(&[]);
                }
            ));
        }
    }

    #[template_callback]
    fn handle_copy_fingerprint(&self, button: &Button) {
        let fingerprint: String = self.fingerprint_row.property("subtitle");
        let display = gdk::Display::default().unwrap();
        let clipboard = display.clipboard();
        clipboard.set_text(&fingerprint);
        button.set_icon_name("emblem-ok-symbolic");
        button.set_css_classes(&["success"]);
        glib::spawn_future_local(clone!(
            #[weak]
            button,
            async move {
                glib::timeout_future_seconds(1).await;
                button.set_icon_name("edit-copy-symbolic");
                button.set_css_classes(&[]);
            }
        ));
    }

    #[template_callback]
    fn handle_port_changed(&self, _entry: &Entry) {
        self.port_edit_apply.set_visible(true);
        self.port_edit_cancel.set_visible(true);
    }

    #[template_callback]
    fn handle_port_edit_apply(&self) {
        self.obj().request_port_change();
    }

    #[template_callback]
    fn handle_port_edit_cancel(&self) {
        log::debug!("cancel port edit");
        self.port_entry
            .set_text(self.port.get().to_string().as_str());
        self.port_edit_apply.set_visible(false);
        self.port_edit_cancel.set_visible(false);
    }

    #[template_callback]
    fn handle_emulation(&self) {
        // On macOS the emulation_status_row is hidden — capture_status_row
        // acts as the shared warning (see update_capture_emulation_status).
        // This handler still fires for the non-macOS platforms where the
        // emulation row is distinct.
        self.obj().request_emulation();
    }

    #[template_callback]
    fn handle_capture(&self) {
        #[cfg(target_os = "macos")]
        {
            use crate::macos_privacy;
            if macos_privacy::accessibility_granted() {
                // AX granted but the row is still visible => the daemon
                // subprocess bailed before AX was in place and needs a
                // fresh process.
                log::info!("capture row clicked in relaunch-required state");
                let Some(app) = self.obj().application() else {
                    log::error!("failed to relaunch: window has no application");
                    self.obj().show_toast("Failed to relaunch Lan Mouse");
                    return;
                };
                match macos_privacy::relaunch_bundle() {
                    // Quit through the normal path either way: `main` reaps
                    // the daemon and only then picks the exit status launchd
                    // needs to restart the job.
                    Ok(()) => app.quit(),
                    Err(error) => {
                        log::error!("failed to schedule app relaunch: {error}");
                        self.obj().show_toast("Failed to relaunch Lan Mouse");
                    }
                }
                return;
            }
            log::info!("capture row clicked in AX-missing state, opening pane");
            macos_privacy::open_accessibility_settings();
        }
        self.obj().request_capture();
    }

    #[template_callback]
    fn handle_add_cert_fingerprint(&self, _button: &Button) {
        self.obj().open_fingerprint_dialog(None);
    }

    pub fn set_port(&self, port: u16) {
        self.port.set(port);
        if port == DEFAULT_PORT {
            self.port_entry.set_text("");
        } else {
            self.port_entry.set_text(format!("{port}").as_str());
        }
        self.port_edit_apply.set_visible(false);
        self.port_edit_cancel.set_visible(false);
    }
}

impl ObjectImpl for Window {
    fn constructed(&self) {
        if let Ok(hostname) = hostname::get() {
            self.hostname_label
                .set_text(hostname.to_str().expect("hostname: invalid utf8"));
        }
        self.parent_constructed();
        self.set_port(DEFAULT_PORT);
        let obj = self.obj();
        obj.setup_icon();
        obj.setup_clients();
        obj.setup_authorized();

        // Log the page structure once so a broken UI file is visible in the
        // logs instead of only as a blank window.
        {
            let stack = self.main_stack.get();
            let model = stack.pages();
            let names: Vec<String> = (0..model.n_items())
                .filter_map(|i| model.item(i))
                .filter_map(|o| o.downcast::<adw::ViewStackPage>().ok())
                .map(|p| p.name().map(|n| n.to_string()).unwrap_or_default())
                .collect();
            log::info!("ui pages: {names:?}");
        }

        // The log view owns its buffer; wire it up once and let
        // `set_logs` write into it.
        self.log_view.set_buffer(Some(&*self.log_buffer.borrow()));
        let combo = self.log_level_combo.clone();
        let weak = obj.downgrade();
        combo.connect_selected_notify(move |row| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let Some(level) = row
                .selected_item()
                .and_then(|item| item.downcast::<gtk::StringObject>().ok())
            else {
                return;
            };
            window.request_log_level(&level.string());
        });
    }
}

impl WidgetImpl for Window {}
impl WindowImpl for Window {}
impl ApplicationWindowImpl for Window {}
impl AdwApplicationWindowImpl for Window {}
