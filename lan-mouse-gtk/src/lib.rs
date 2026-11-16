mod arrangement;
mod authorization_window;
mod client_object;
mod client_row;
mod fingerprint_window;
mod key_object;
mod key_row;
#[cfg(target_os = "macos")]
mod macos_privacy;

/// Whether the process must exit non-zero so launchd restarts the job after a
/// permission grant. Always false off macOS.
#[cfg(target_os = "macos")]
pub fn exit_non_zero_for_relaunch() -> bool {
    macos_privacy::exit_non_zero_for_relaunch()
}

#[cfg(not(target_os = "macos"))]
pub fn exit_non_zero_for_relaunch() -> bool {
    false
}
#[cfg(target_os = "macos")]
mod macos_status_item;
mod window;

use std::{env, str, sync::OnceLock, time::Duration};

use gtk::CssProvider;
use window::Window;

/// Local build's commit hash, set once by [`run`] before the GTK
/// main loop starts. Read by per-row UI to compare against each
/// peer's [`lan_mouse_ipc::ClientState::peer_commit`] for the
/// soft-warn version-mismatch indicator.
pub(crate) static LOCAL_COMMIT: OnceLock<[u8; 8]> = OnceLock::new();

/// Convenience: returns the local commit as an 8-char ASCII string,
/// or a placeholder if unset (which would indicate a programmer
/// error since [`run`] always sets it).
pub(crate) fn local_commit_str() -> String {
    LOCAL_COMMIT
        .get()
        .and_then(|c| std::str::from_utf8(c).ok())
        .unwrap_or("????????")
        .to_string()
}

use lan_mouse_ipc::FrontendEvent;

use adw::Application;
use gtk::{IconTheme, gdk::Display, glib::clone, prelude::*};
use gtk::{gio, glib, prelude::ApplicationExt};

use self::client_object::ClientObject;
use self::key_object::KeyObject;

use thiserror::Error;

/// how often to retry connecting to the daemon socket while it is starting up
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// how many times to retry before giving up (500 * 100ms = 50s of patience)
const CONNECT_ATTEMPTS: usize = 500;

#[derive(Error, Debug)]
pub enum GtkError {
    #[error("gtk frontend exited with non zero exit code: {0}")]
    NonZeroExitCode(i32),
    #[error("could not connect to the lan-mouse daemon: {0}")]
    DaemonUnavailable(String),
}

/// Set when the frontend has to quit because of a startup error. The GTK main
/// loop only returns an [`glib::ExitCode`], so the reason is stashed here and
/// turned into a [`GtkError`] by [`run`].
static STARTUP_ERROR: OnceLock<String> = OnceLock::new();

pub fn run(local_commit: [u8; 8]) -> Result<(), GtkError> {
    log::debug!("running gtk frontend");
    LOCAL_COMMIT
        .set(local_commit)
        .expect("local_commit set once");

    #[cfg(windows)]
    let ret = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024) // https://gitlab.gnome.org/GNOME/gtk/-/commit/52dbb3f372b2c3ea339e879689c1de535ba2c2c3 -> caused crash on windows
        .name("gtk".into())
        .spawn(gtk_main)
        .unwrap()
        .join()
        .unwrap();
    #[cfg(not(windows))]
    let ret = gtk_main();

    if let Some(e) = STARTUP_ERROR.get() {
        return Err(GtkError::DaemonUnavailable(e.clone()));
    }

    match ret {
        glib::ExitCode::SUCCESS => Ok(()),
        e => Err(GtkError::NonZeroExitCode(e.value())),
    }
}

fn gtk_main() -> glib::ExitCode {
    #[cfg(target_os = "macos")]
    {
        configure_macos_bundle_environment();
        install_macos_gtk_log_filter();
    }

    gio::resources_register_include!("lan-mouse.gresource").expect("Failed to register resources.");

    let app = Application::builder()
        .application_id("de.feschber.LanMouse")
        // Allow several instances: the GTK single-instance handshake goes over
        // D-Bus, which a bare Wayland session in a test sandbox may not have.
        // The daemon already enforces one IPC listener, so a second GUI just
        // connects to it.
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_startup(|app| {
        load_css();
        load_icons();
        setup_actions(app);
        setup_menu(app);
    });
    app.connect_activate(build_ui);

    let args: Vec<&'static str> = vec![];
    app.run_with_args(&args)
}

#[cfg(target_os = "macos")]
fn install_macos_gtk_log_filter() {
    glib::log_set_writer_func(|level, fields| {
        if level == glib::LogLevel::Warning && is_gtk_theme_parser_warning(fields) {
            return glib::LogWriterOutput::Handled;
        }

        glib::log_writer_default(level, fields)
    });
}

#[cfg(target_os = "macos")]
fn is_gtk_theme_parser_warning(fields: &[glib::LogField<'_>]) -> bool {
    let mut domain = None;
    let mut message = None;

    for field in fields {
        match field.key() {
            "GLIB_DOMAIN" => domain = field.value_str(),
            "MESSAGE" => message = field.value_str(),
            _ => {}
        }
    }

    domain == Some("Gtk")
        && message.is_some_and(|message| message.starts_with("Theme parser warning: gtk.css:"))
}

#[cfg(target_os = "macos")]
fn configure_macos_bundle_environment() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let Some(contents) = exe
        .parent()
        .and_then(|dir| dir.parent())
        .map(std::path::Path::to_owned)
    else {
        return;
    };

    let share = contents.join("Resources").join("share");
    if !share.exists() {
        return;
    }

    let schemas = share.join("glib-2.0").join("schemas");
    if schemas.exists() {
        env::set_var("GSETTINGS_SCHEMA_DIR", schemas);
    }

    env::set_var("XDG_DATA_DIRS", &share);
    env::set_var(
        "GTK_DATA_PREFIX",
        contents.join("Resources").to_string_lossy().as_ref(),
    );
}

fn load_css() {
    let provider = CssProvider::default();
    provider.load_from_resource("de/feschber/LanMouse/style.css");
    gtk::style_context_add_provider_for_display(
        &Display::default().expect("Could not connect to a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn load_icons() {
    let display = &Display::default().expect("Could not connect to a display.");
    let icon_theme = IconTheme::for_display(display);
    icon_theme.add_resource_path("/de/feschber/LanMouse/icons");
}

// Add application actions
fn setup_actions(app: &adw::Application) {
    // Quit action
    // This is important on macOS, where users expect a File->Quit action with a Cmd+Q shortcut.
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| {
            app.quit();
        }
    });
    app.add_action(&quit_action);
}

// Set up a global menu
//
// Currently this is used only on macOS
fn setup_menu(app: &adw::Application) {
    let menu = gio::Menu::new();

    let file_menu = gio::Menu::new();
    file_menu.append(Some("Quit"), Some("app.quit"));
    menu.append_submenu(Some("_File"), &file_menu);

    app.set_menubar(Some(&menu))
}

fn build_ui(app: &Application) {
    // If a window already exists (re-activation), just present it
    if let Some(window) = app.windows().first() {
        window.present();
        return;
    }

    log::debug!("connecting to lan-mouse-socket");
    let (frontend_rx, frontend_tx) = match lan_mouse_ipc::try_connect() {
        Ok(conn) => conn,
        Err(e) => {
            /* The caller already started the daemon as a child process, so do
             * not spawn a second one here: both would race for the same
             * socket. Wait for the daemon to come up instead — but bail out
             * after a while, because `lan_mouse_ipc::connect` retries
             * forever and this runs on the GTK main thread (the GUI would
             * hang with no window at all if the daemon cannot start).
             */
            log::warn!("could not connect to daemon ({e}), waiting for it to start");
            let mut connected = None;
            for attempt in 0..CONNECT_ATTEMPTS {
                std::thread::sleep(CONNECT_RETRY_INTERVAL);
                match lan_mouse_ipc::try_connect() {
                    Ok(conn) => {
                        connected = Some(conn);
                        break;
                    }
                    Err(e) => log::debug!("still not connected (attempt {attempt}): {e}"),
                }
            }
            let Some(conn) = connected else {
                let msg = format!(
                    "no answer from the daemon after {}s",
                    (CONNECT_ATTEMPTS as f32 * CONNECT_RETRY_INTERVAL.as_secs_f32()) as u32
                );
                log::error!("could not connect to the lan-mouse daemon: {msg}");
                /* Quit through the application instead of `process::exit`:
                 * the caller spawned the daemon and terminates it once the
                 * GTK loop returns, so exiting here would orphan it.
                 */
                let _ = STARTUP_ERROR.set(msg);
                app.quit();
                return;
            };
            conn
        }
    };
    log::debug!("connected to lan-mouse-socket");

    let (sender, receiver) = async_channel::bounded(10);
    /* Carries a fresh writer after the daemon came back. Separate from the event
     * channel because it is not an event: the window swaps its connection and asks for
     * the state again, and nothing about it is a snapshot of the daemon's state. */
    let (conn_tx, conn_rx) = async_channel::bounded(1);
    /* The window needs a writer to send with, and the reader thread needs it back to
     * keep reading: the pair is not cloneable, so it is passed along the same channel
     * that carries a reconnected one. */
    let _ = conn_tx.send_blocking(frontend_tx);

    gio::spawn_blocking(move || {
        let mut reader = frontend_rx;
        loop {
            while let Some(e) = reader.next_event() {
                match e {
                    // the receiver is gone once the window is closed
                    Ok(e) => {
                        if sender.send_blocking(e).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        log::warn!("frontend read failed: {e}");
                        break;
                    }
                }
            }

            /* The daemon went away - it is restarted on every upgrade, and the window
             * used to quit here, which meant a deploy closed whatever the user had
             * open. Wait for it to come back and carry on with the same widgets. */
            log::warn!("lost the daemon connection, waiting for it to come back");
            loop {
                std::thread::sleep(CONNECT_RETRY_INTERVAL);
                match lan_mouse_ipc::try_connect() {
                    Ok((new_reader, new_writer)) => {
                        reader = new_reader;
                        /* hand the new writer to the window; if it cannot be
                         * delivered the window is gone */
                        if conn_tx.send_blocking(new_writer).is_err() {
                            return;
                        }
                        break;
                    }
                    Err(_) => continue,
                }
            }
            log::info!("reconnected to the lan-mouse daemon");
        }
    });

    let window = Window::new(app, conn_rx.recv_blocking().expect("the first writer"));
    // Ask the daemon for the hotkey rules, version and autostart state as soon
    // as the window exists, so the UI is populated before the first event.
    window.request_initial_state();
    #[cfg(target_os = "macos")]
    {
        window.connect_close_request(|window| {
            window.set_visible(false);
            glib::Propagation::Stop
        });
        macos_status_item::setup(app, &window);
        // First-launch TCC prompts. No-op when already granted.
        macos_privacy::fire_initial_prompts();
        // Watch the Accessibility grant continuously for the lifetime
        // of the process. On a grant, swap the warning row into its
        // "relaunch required" state (the daemon subprocess already
        // bailed and can't recover without a restart). On a REVOKE,
        // quit immediately — an active CGEventTap at
        // HeadInsertEventTap can wedge system input if the process
        // lingers after losing AX, and forcing the process to exit is
        // the only bulletproof way to guarantee the kernel tears the
        // tap down.
        let window_weak = window.downgrade();
        let app_weak = app.downgrade();
        macos_privacy::watch_accessibility_state(move |change| match change {
            macos_privacy::AccessibilityChange::Granted => {
                if let Some(window) = window_weak.upgrade() {
                    window.present();
                    window.refresh_capture_emulation_status();
                }
            }
            macos_privacy::AccessibilityChange::Revoked => {
                log::warn!("Accessibility revoked — quitting to avoid wedging system input");
                if let Some(app) = app_weak.upgrade() {
                    app.quit();
                }
            }
        });
    }

    glib::spawn_future_local(clone!(
        #[weak]
        window,
        #[weak]
        app,
        async move {
            loop {
                /* A reconnected writer arrives here; the reader thread keeps the
                 * event channel open across the gap, so this is the only place the
                 * window learns that it has to swap connections. Selected rather than
                 * polled: the loop would otherwise sit in `recv` and miss it. */
                /* One channel of either kind: a state event, or the writer of a
                 * connection that came back. Selected rather than polled - the loop
                 * would otherwise sit in `recv` and miss the reconnect. */
                /* boxed, because a `FrontendEvent` is much larger than a writer and
                 * every iteration would otherwise reserve for the largest variant */
                enum Incoming {
                    Event(Box<FrontendEvent>),
                    Writer(Box<lan_mouse_ipc::FrontendRequestWriter>),
                    Gone,
                }
                let incoming = futures::future::select(
                    Box::pin(async {
                        match receiver.recv().await {
                            Ok(event) => Incoming::Event(Box::new(event)),
                            Err(_) => Incoming::Gone,
                        }
                    }),
                    Box::pin(async {
                        match conn_rx.recv().await {
                            Ok(writer) => Incoming::Writer(Box::new(writer)),
                            Err(_) => Incoming::Gone,
                        }
                    }),
                );
                let notify = match incoming.await {
                    futures::future::Either::Left((Incoming::Event(event), _)) => *event,
                    futures::future::Either::Left((_, _)) => {
                        log::error!("lost connection to the lan-mouse daemon");
                        let _ =
                            STARTUP_ERROR.set("the connection to the daemon was lost".to_owned());
                        app.quit();
                        return;
                    }
                    futures::future::Either::Right((Incoming::Writer(writer), _)) => {
                        window.replace_connection(*writer);
                        continue;
                    }
                    futures::future::Either::Right((_, _)) => {
                        log::error!("lost connection to the lan-mouse daemon");
                        let _ =
                            STARTUP_ERROR.set("the connection to the daemon was lost".to_owned());
                        app.quit();
                        return;
                    }
                };
                match notify {
                    FrontendEvent::Created(handle, client, state) => {
                        window.new_client(handle, client, state)
                    }
                    FrontendEvent::Deleted(client) => window.delete_client(client),
                    FrontendEvent::State(handle, config, state) => {
                        window.update_client_config(handle, config);
                        window.update_client_state(handle, state);
                    }
                    FrontendEvent::NoSuchClient(_) => {}
                    FrontendEvent::Error(e) => window.show_toast(e.as_str()),
                    FrontendEvent::Enumerate(clients) => window.update_client_list(clients),
                    FrontendEvent::PortChanged(port, msg) => window.update_port(port, msg),
                    FrontendEvent::CaptureStatus(s) => window.set_capture(s.into()),
                    FrontendEvent::EmulationStatus(s) => window.set_emulation(s.into()),
                    FrontendEvent::AuthorizedUpdated(keys) => window.set_authorized_keys(keys),
                    FrontendEvent::PublicKeyFingerprint(fp) => window.set_pk_fp(&fp),
                    FrontendEvent::ConnectionAttempt { fingerprint } => {
                        window.request_authorization(&fingerprint);
                    }
                    FrontendEvent::DeviceConnected {
                        fingerprint: _,
                        addr,
                    } => {
                        window.show_toast(format!("device connected: {addr}").as_str());
                    }
                    FrontendEvent::DeviceEntered {
                        fingerprint: _,
                        addr,
                        pos,
                    } => {
                        window.show_toast(format!("device entered: {addr} ({pos})").as_str());
                    }
                    FrontendEvent::IncomingConnections(infos) => window.set_incoming(infos),
                    FrontendEvent::Runtime(info) => window.set_runtime(info),
                    FrontendEvent::IncomingDisconnected(addr) => {
                        window.show_toast(format!("{addr} disconnected").as_str());
                    }
                    FrontendEvent::Hotkeys(rules) => window.set_hotkeys(rules),
                    FrontendEvent::Remap(rules) => window.set_remap_rules(rules),
                    FrontendEvent::ReleaseBind(bind) => window.set_release_bind(&bind),
                    FrontendEvent::Version {
                        version,
                        commit,
                        autostart,
                    } => {
                        window.set_version(&version, &commit);
                        window.set_autostart(autostart);
                    }
                    FrontendEvent::Autostart(enabled) => window.set_autostart(enabled),
                    FrontendEvent::Paused(paused) => window.set_paused(paused),
                    FrontendEvent::RestartInterval(secs) => window.set_restart_interval(secs),
                    /* The daemon narrowed an edge range because two machines
                     * claim the same part of an edge. A toast is enough for
                     * now: the arrangement canvas that shows the coverage
                     * surfaces the same conflict in place.
                     */
                    FrontendEvent::EdgeConflict(msg) => window.show_toast(msg.as_str()),
                    FrontendEvent::SwitchOptions(options) => {
                        window.set_switch_options(&options);
                    }
                    FrontendEvent::LogLevel(level) => window.set_log_level(&level),
                    FrontendEvent::Logs(lines) => window.set_logs(lines),
                    FrontendEvent::LogAppended(lines) => window.append_logs(lines),
                }
            }
        }
    ));

    #[cfg(not(target_os = "macos"))]
    window.present();

    // On macOS, default to presenting the main window on every launch
    // so the user gets a visible confirmation that the app is running
    // — including the post-grant relaunch and normal Dock/Finder/`open`
    // launches. Opt out by setting `LAN_MOUSE_HIDDEN=1` in the
    // environment (useful for a LaunchAgent / login-item configuration
    // where the user wants the app to come up quietly into the menu
    // bar only, with no window on boot).
    #[cfg(target_os = "macos")]
    if env::var_os("LAN_MOUSE_HIDDEN").is_none() {
        window.present();
    }
}
