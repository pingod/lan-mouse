use crate::capture_test::TestCaptureArgs;
use crate::emulation_test::TestEmulationArgs;
use clap::{Parser, Subcommand, ValueEnum};
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env::{self, VarError};
use std::fmt::Display;
use std::fs::{self, File};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::{collections::HashSet, io, time::Duration};
use thiserror::Error;
use toml;
use toml_edit::{self, DocumentMut, Item};

use lan_mouse_cli::CliArgs;
use lan_mouse_ipc::{DEFAULT_PORT, EdgeRange, Position, SwitchCorner, SwitchOptions};

use input_event::scancode::{
    self,
    Linux::{KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift},
};

use shadow_rs::shadow;

shadow!(build);

/// Local build's 8-byte ASCII short commit hash, suitable for use
/// in [`lan_mouse_proto::ProtoEvent::Hello`]. Pads with `'?'` if
/// shadow_rs returns an unexpected length so the field is always
/// well-formed on the wire.
pub fn local_commit() -> [u8; 8] {
    let bytes = build::SHORT_COMMIT.as_bytes();
    let mut out = [b'?'; 8];
    let n = bytes.len().min(8);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

/// Short commit hash of this build, for the GUI's version line.
pub fn local_commit_string() -> String {
    build::SHORT_COMMIT.to_owned()
}

const CONFIG_FILE_NAME: &str = "config.toml";
const CERT_FILE_NAME: &str = "lan-mouse.pem";

fn default_path() -> Result<PathBuf, VarError> {
    #[cfg(unix)]
    let default_path = {
        let xdg_config_home =
            env::var("XDG_CONFIG_HOME").unwrap_or(format!("{}/.config", env::var("HOME")?));
        format!("{xdg_config_home}/lan-mouse/")
    };

    #[cfg(not(unix))]
    let default_path = {
        let app_data =
            env::var("LOCALAPPDATA").unwrap_or(format!("{}/.config", env::var("USERPROFILE")?));
        format!("{app_data}\\lan-mouse\\")
    };
    Ok(PathBuf::from(default_path))
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct ConfigToml {
    capture_backend: Option<CaptureBackend>,
    emulation_backend: Option<EmulationBackend>,
    port: Option<u16>,
    release_bind: Option<Vec<scancode::Linux>>,
    cert_path: Option<PathBuf>,
    clients: Option<Vec<TomlClient>>,
    authorized_fingerprints: Option<HashMap<String, String>>,
    remap: Option<Vec<TomlRule>>,
    hotkey: Option<Vec<TomlHotkey>>,
    /// restart the service every N seconds; `None` disables it
    restart_interval: Option<u64>,
    /// milliseconds within which the edge has to be hit twice
    switch_double_tap: Option<u32>,
    /// radius in pixels of the blocked corners
    switch_corner_size: Option<u32>,
    /// corners the pointer may not leave the screen through
    switch_corners: Option<Vec<SwitchCorner>>,
}

/// A global hotkey: pressing `from` while typing locally sends `to` to the peer.
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
struct TomlHotkey {
    from: Vec<scancode::Linux>,
    #[serde(default)]
    to: Vec<scancode::Linux>,
    /// local action to run instead of sending keys
    #[serde(default)]
    action: Option<TomlHotkeyAction>,
    /// presses required: 1 = chord (default), 2 = double-tap
    #[serde(default = "default_taps")]
    taps: u8,
}

fn default_taps() -> u8 {
    1
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum TomlHotkeyAction {
    /// hand the pointer to the next device, or back to this one
    SwitchDevice,
    /// release the pointer without moving it
    ForceRelease,
}

/// A chord rule: pressing all keys in `from` emits `to` on the peer.
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
struct TomlRule {
    from: Vec<scancode::Linux>,
    to: Vec<scancode::Linux>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
struct TomlClient {
    hostname: Option<String>,
    host_name: Option<String>,
    ips: Option<Vec<IpAddr>>,
    port: Option<u16>,
    position: Option<Position>,
    activate_on_startup: Option<bool>,
    enter_hook: Option<String>,
    leave_hook: Option<String>,
    /// segment of the local edge this client is reached through
    ///
    /// Written as `[start, end]` fractions of the edge, the way Synergy writes
    /// `right(0,60) = other`. Absent means the whole edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edge: Option<EdgeRange>,
    /// segment of this client's facing edge the cursor lands in
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peer_edge: Option<EdgeRange>,
}

impl ConfigToml {
    fn new(path: &Path) -> Result<ConfigToml, ConfigError> {
        let config = fs::read_to_string(path)?;
        Ok(toml::from_str::<_>(&config)?)
    }
}

#[derive(Parser, Debug)]
#[command(author, version=build::CLAP_LONG_VERSION, about, long_about = None)]
struct Args {
    /// the listen port for lan-mouse
    #[arg(short, long)]
    port: Option<u16>,

    /// non-default config file location
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// capture backend override
    #[arg(long)]
    capture_backend: Option<CaptureBackend>,

    /// emulation backend override
    #[arg(long)]
    emulation_backend: Option<EmulationBackend>,

    /// path to non-default certificate location
    #[arg(long)]
    cert_path: Option<PathBuf>,

    /// subcommands
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// test input emulation
    TestEmulation(TestEmulationArgs),
    /// test input capture
    TestCapture(TestCaptureArgs),
    /// Lan Mouse commandline interface
    Cli(CliArgs),
    /// run in daemon mode
    Daemon,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum CaptureBackend {
    #[cfg(libei_capture)]
    #[serde(rename = "input-capture-portal")]
    InputCapturePortal,
    #[cfg(layer_shell_capture)]
    #[serde(rename = "layer-shell")]
    LayerShell,
    #[cfg(x11_capture)]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl Display for CaptureBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei_capture)]
            CaptureBackend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell_capture)]
            CaptureBackend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11_capture)]
            CaptureBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            CaptureBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => write!(f, "MacOS"),
            CaptureBackend::Dummy => write!(f, "dummy"),
        }
    }
}

impl From<CaptureBackend> for input_capture::Backend {
    fn from(backend: CaptureBackend) -> Self {
        match backend {
            #[cfg(libei_capture)]
            CaptureBackend::InputCapturePortal => Self::InputCapturePortal,
            #[cfg(layer_shell_capture)]
            CaptureBackend::LayerShell => Self::LayerShell,
            #[cfg(x11_capture)]
            CaptureBackend::X11 => Self::X11,
            #[cfg(windows)]
            CaptureBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => Self::MacOs,
            CaptureBackend::Dummy => Self::Dummy,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum EmulationBackend {
    #[cfg(wlroots_emulation)]
    #[serde(rename = "wlroots")]
    Wlroots,
    #[cfg(libei_emulation)]
    #[serde(rename = "libei")]
    Libei,
    #[cfg(rdp_emulation)]
    #[serde(rename = "xdp")]
    Xdp,
    #[cfg(x11_emulation)]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl From<EmulationBackend> for input_emulation::Backend {
    fn from(backend: EmulationBackend) -> Self {
        match backend {
            #[cfg(wlroots_emulation)]
            EmulationBackend::Wlroots => Self::Wlroots,
            #[cfg(libei_emulation)]
            EmulationBackend::Libei => Self::Libei,
            #[cfg(rdp_emulation)]
            EmulationBackend::Xdp => Self::Xdp,
            #[cfg(x11_emulation)]
            EmulationBackend::X11 => Self::X11,
            #[cfg(windows)]
            EmulationBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => Self::MacOs,
            EmulationBackend::Dummy => Self::Dummy,
        }
    }
}

impl Display for EmulationBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(wlroots_emulation)]
            EmulationBackend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei_emulation)]
            EmulationBackend::Libei => write!(f, "libei"),
            #[cfg(rdp_emulation)]
            EmulationBackend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11_emulation)]
            EmulationBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            EmulationBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => write!(f, "macos"),
            EmulationBackend::Dummy => write!(f, "dummy"),
        }
    }
}

/// Replace the managed keys of `new_config` in the file at `path`.
///
/// Comments, key order and every key this program does not know about are
/// kept: the file is parsed into a document, the new values are inserted into
/// it, and the result is serialized again. If the existing file cannot be read
/// or parsed, the new configuration is written as is, so a broken file never
/// blocks saving.
fn merge_config(path: &Path, new_config: &str) -> String {
    let (Ok(existing), Ok(replacement)) =
        (fs::read_to_string(path), new_config.parse::<DocumentMut>())
    else {
        return new_config.to_string();
    };
    let Ok(mut doc) = existing.parse::<DocumentMut>() else {
        log::warn!("could not parse {path:?}, rewriting it");
        return new_config.to_string();
    };
    for (key, candidate) in replacement.iter() {
        let mut item = candidate.clone();
        let Some((_, existing)) = doc.get_key_value_mut(key) else {
            /* a new key: just add it */
            doc.insert(key, item);
            continue;
        };
        /* the decor of a value or of the first table of an array of tables
         * holds the comments the user wrote around it
         */
        match (&*existing, &item) {
            (Item::Value(previous), Item::Value(_)) => {
                let decor = previous.decor().clone();
                if let Item::Value(value) = &mut item {
                    *value.decor_mut() = decor;
                }
            }
            (Item::Table(previous), Item::Table(_)) => {
                let decor = previous.decor().clone();
                if let Item::Table(table) = &mut item {
                    *table.decor_mut() = decor;
                }
            }
            (Item::ArrayOfTables(previous), Item::ArrayOfTables(_)) => {
                if let Some(previous) = previous.iter().next() {
                    let decor = previous.decor().clone();
                    if let Some(current) = item
                        .as_array_of_tables_mut()
                        .and_then(|tables| tables.iter_mut().next())
                    {
                        *current.decor_mut() = decor;
                    }
                }
            }
            _ => {}
        }
        *existing = item;
    }
    doc.to_string()
}

pub struct Config {
    /// command line arguments
    args: Args,
    /// path to the certificate file used
    cert_path: PathBuf,
    /// path to the config file used
    config_path: PathBuf,
    /// path to config directory (parent of above)
    config_dir: PathBuf,
    /// the (optional) toml config and it's path
    config_toml: Option<ConfigToml>,
    // filesystem watcher
    watcher: notify::RecommendedWatcher,
    // channel for filesystem events
    watch_rx: tokio::sync::mpsc::Receiver<Result<notify::Event, notify::Error>>,
}

#[derive(Clone, Debug)]
pub struct ConfigClient {
    pub ips: HashSet<IpAddr>,
    pub hostname: Option<String>,
    pub port: u16,
    pub pos: Position,
    pub active: bool,
    pub enter_hook: Option<String>,
    pub leave_hook: Option<String>,
    /// segment of the local edge this client is reached through
    pub edge: EdgeRange,
    /// segment of this client's facing edge the cursor lands in
    pub peer_edge: EdgeRange,
}

impl From<TomlClient> for ConfigClient {
    fn from(toml: TomlClient) -> Self {
        let active = toml.activate_on_startup.unwrap_or(false);
        let enter_hook = toml.enter_hook;
        let leave_hook = toml.leave_hook;
        let hostname = toml.hostname;
        let ips = HashSet::from_iter(toml.ips.into_iter().flatten());
        let port = toml.port.unwrap_or(DEFAULT_PORT);
        let pos = toml.position.unwrap_or_default();
        Self {
            ips,
            hostname,
            port,
            pos,
            active,
            enter_hook,
            leave_hook,
            edge: toml.edge.unwrap_or(EdgeRange::FULL),
            peer_edge: toml.peer_edge.unwrap_or(EdgeRange::FULL),
        }
    }
}

impl From<ConfigClient> for TomlClient {
    fn from(client: ConfigClient) -> Self {
        let hostname = client.hostname;
        let host_name = None;
        let mut ips = client.ips.into_iter().collect::<Vec<_>>();
        ips.sort();
        let ips = Some(ips);
        let port = if client.port == DEFAULT_PORT {
            None
        } else {
            Some(client.port)
        };
        let position = Some(client.pos);
        let activate_on_startup = if client.active { Some(true) } else { None };
        let enter_hook = client.enter_hook;
        let leave_hook = client.leave_hook;
        /* the whole edge is what every client had before ranges existed, so it
         * is not written: a config file only carries what the user chose
         */
        let edge = (client.edge != EdgeRange::FULL).then_some(client.edge);
        let peer_edge = (client.peer_edge != EdgeRange::FULL).then_some(client.peer_edge);
        Self {
            hostname,
            host_name,
            ips,
            port,
            position,
            activate_on_startup,
            enter_hook,
            leave_hook,
            edge,
            peer_edge,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Var(#[from] VarError),
    #[error(transparent)]
    Watcher(#[from] notify::Error),
}

const DEFAULT_RELEASE_KEYS: [scancode::Linux; 4] =
    [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt];

impl Config {
    pub fn new() -> Result<Self, ConfigError> {
        let args = Args::parse();

        // --config <file> overrules default location
        let config_path = args
            .config
            .clone()
            .unwrap_or(default_path()?.join(CONFIG_FILE_NAME));
        let config_dir = config_path
            .parent()
            .expect("config directory")
            .to_path_buf();

        // Ensure the config directory exists and write a default config file
        // if none is present. Runs on every Config::new(), regardless of which
        // entry path (GUI main, spawned daemon, CLI, test commands) we're on,
        // so a fresh Mac never hits "No such file or directory" on config.toml
        // and notify::Watcher (which requires the dir to exist on macOS
        // FSEvents and some Linux backends) has a concrete path to watch.
        fs::create_dir_all(&config_dir)?;
        if !config_path.exists() {
            let default_toml = toml::to_string_pretty(&ConfigToml::default())
                .expect("default ConfigToml serialization cannot fail");
            fs::write(&config_path, default_toml)?;
        }

        let config_toml = match ConfigToml::new(&config_path) {
            Err(e) => {
                log::warn!("{config_path:?}: {e}");
                log::warn!("Continuing without config file ...");
                None
            }
            Ok(c) => Some(c),
        };

        // --cert-path <file> overrules default location
        let cert_path = args
            .cert_path
            .clone()
            .or(config_toml.as_ref().and_then(|c| c.cert_path.clone()))
            .unwrap_or(default_path()?.join(CERT_FILE_NAME));

        let (tx, watch_rx) = tokio::sync::mpsc::channel(16);
        let watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.blocking_send(res);
            },
            notify::Config::default(),
        )?;
        let mut config = Config {
            args,
            cert_path,
            config_path,
            config_dir,
            config_toml,
            watcher,
            watch_rx,
        };
        config.watch()?;
        Ok(config)
    }

    fn watch(&mut self) -> Result<(), notify::Error> {
        self.watcher
            .watch(&self.config_dir, notify::RecursiveMode::NonRecursive)?;
        Ok(())
    }

    fn unwatch(&mut self) -> Result<(), notify::Error> {
        self.watcher.unwatch(&self.config_dir)?;
        Ok(())
    }

    pub async fn changed(&mut self) -> Result<(), notify::Error> {
        loop {
            let Some(event) = self.watch_rx.recv().await else {
                /* The watcher is gone; keep waiting instead of returning
                 * immediately, which would spin the service event loop.
                 */
                log::error!("config file watcher stopped");
                std::future::pending::<()>().await;
                continue;
            };
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    // e.g. the OS ran out of inotify watches
                    log::warn!("error watching the config file: {e}");
                    continue;
                }
            };
            if event.paths.contains(&self.config_path)
                && matches!(
                    event.kind,
                    EventKind::Create(_)
                        | EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Remove(_)
                )
                && self.read_from_disk()?
            {
                return Ok(());
            }
        }
    }

    /// the command to run
    pub fn command(&self) -> Option<Command> {
        self.args.command.clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// public key fingerprints authorized for connection
    pub fn authorized_fingerprints(&self) -> HashMap<String, String> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.authorized_fingerprints.clone())
            .unwrap_or_default()
    }

    /// path to certificate
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// optional input-capture backend override
    pub fn capture_backend(&self) -> Option<CaptureBackend> {
        self.args
            .capture_backend
            .or(self.config_toml.as_ref().and_then(|c| c.capture_backend))
    }

    /// optional input-emulation backend override
    pub fn emulation_backend(&self) -> Option<EmulationBackend> {
        self.args
            .emulation_backend
            .or(self.config_toml.as_ref().and_then(|c| c.emulation_backend))
    }

    /// the port to use (initially)
    pub fn port(&self) -> u16 {
        self.args
            .port
            .or(self.config_toml.as_ref().and_then(|c| c.port))
            .unwrap_or(DEFAULT_PORT)
    }

    /// Edge-crossing thresholds: how close to an edge the pointer has to behave
    /// before it may cross to a peer.
    ///
    /// The lan-mouse equivalent of Synergy's `switchDelay`, `switchDoubleTap`,
    /// `switchCornerSize` and `switchCorners`. They are read once at startup and
    /// replaced at runtime when a frontend edits them.
    pub fn switch_options(&self) -> SwitchOptions {
        let toml = self.config_toml.as_ref();
        SwitchOptions {
            double_tap_ms: toml.and_then(|c| c.switch_double_tap).unwrap_or(0),
            corner_size: toml.and_then(|c| c.switch_corner_size).unwrap_or(0),
            corners: toml
                .and_then(|c| c.switch_corners.clone())
                .unwrap_or_default(),
        }
    }

    /// Store the edge-crossing thresholds.
    ///
    /// A value of `0` is written as absent rather than as `switch_delay = 0`:
    /// the keys are opt-in, and a config file full of zeros would look like the
    /// user had tuned them.
    pub fn set_switch_options(&mut self, options: &SwitchOptions) {
        let config = self.toml_mut();
        config.switch_double_tap = (options.double_tap_ms > 0).then_some(options.double_tap_ms);
        config.switch_corner_size = (options.corner_size > 0).then_some(options.corner_size);
        config.switch_corners = (!options.corners.is_empty()).then(|| options.corners.clone());
    }

    /// list of configured clients
    pub fn clients(&self) -> Vec<ConfigClient> {
        self.config_toml
            .as_ref()
            .map(|c| c.clients.clone())
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .map(From::<TomlClient>::from)
            .collect()
    }

    /// release bind for returning control to the host
    /// Periodic restart interval, if configured.
    ///
    /// Useful when the backends leak or the compositor's portal session goes
    /// stale over long uptimes; the supervisor (systemd / launchd) restarts the
    /// process and everything is rebuilt from the config file.
    pub fn restart_interval(&self) -> Option<Duration> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.restart_interval)
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs)
    }

    pub fn release_bind(&self) -> Vec<scancode::Linux> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.release_bind.clone())
            .unwrap_or(Vec::from_iter(DEFAULT_RELEASE_KEYS.iter().cloned()))
    }

    /// chord remapping rules applied to captured keyboard events
    pub(crate) fn remap_rules(&self) -> Vec<crate::remap::Rule> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.remap.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|r| crate::remap::Rule {
                from: r.from.into_iter().map(|k| k as u32).collect(),
                to: r.to.into_iter().map(|k| k as u32).collect(),
            })
            .collect()
    }

    /// global hotkeys that fire even while the pointer stays local
    ///
    /// Parsed on every platform so the GUI can display and edit the rules; only
    /// the Linux evdev listener acts on them.
    pub(crate) fn hotkeys(&self) -> Vec<crate::hotkey_task::Hotkey> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.hotkey.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|h| crate::hotkey_task::Hotkey {
                from: h.from.into_iter().map(|k| k as u32).collect(),
                to: h.to.into_iter().map(|k| k as u32).collect(),
                action: h.action.map(|a| match a {
                    TomlHotkeyAction::SwitchDevice => {
                        crate::hotkey_task::HotkeyAction::SwitchDevice
                    }
                    TomlHotkeyAction::ForceRelease => {
                        crate::hotkey_task::HotkeyAction::ForceRelease
                    }
                }),
                taps: h.taps.max(1),
            })
            .collect()
    }

    /// replace the persisted global hotkey rules
    pub(crate) fn set_hotkeys(&mut self, hotkeys: Vec<crate::hotkey_task::Hotkey>) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        let rules = hotkeys
            .into_iter()
            .map(|h| TomlHotkey {
                from: h
                    .from
                    .into_iter()
                    .filter_map(|k| scancode::Linux::try_from(k).ok())
                    .collect(),
                action: h.action.map(|a| match a {
                    crate::hotkey_task::HotkeyAction::SwitchDevice => {
                        TomlHotkeyAction::SwitchDevice
                    }
                    crate::hotkey_task::HotkeyAction::ForceRelease => {
                        TomlHotkeyAction::ForceRelease
                    }
                }),
                taps: h.taps,
                to: h
                    .to
                    .into_iter()
                    .filter_map(|k| scancode::Linux::try_from(k).ok())
                    .collect(),
            })
            .collect::<Vec<_>>();
        let cfg = self.config_toml.as_mut().expect("config");
        if rules.is_empty() {
            cfg.hotkey = None;
        } else {
            cfg.hotkey = Some(rules);
        }
    }

    /// replace the persisted keys that pull the pointer back
    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        self.toml_mut().release_bind = Some(bind);
    }

    /// replace the persisted chord remapping rules
    pub(crate) fn set_remap_rules(&mut self, rules: Vec<crate::remap::Rule>) {
        let rules = rules
            .into_iter()
            .map(|r| TomlRule {
                from: r
                    .from
                    .into_iter()
                    .filter_map(|k| scancode::Linux::try_from(k).ok())
                    .collect(),
                to: r
                    .to
                    .into_iter()
                    .filter_map(|k| scancode::Linux::try_from(k).ok())
                    .collect(),
            })
            .collect::<Vec<_>>();
        let cfg = self.toml_mut();
        // An empty list has to clear the section, not write `remap = []`:
        // otherwise the default rules would come back on the next reload.
        if rules.is_empty() {
            cfg.remap = None;
        } else {
            cfg.remap = Some(rules);
        }
    }

    /// set configured clients
    pub fn set_clients(&mut self, clients: Vec<ConfigClient>) {
        // An empty list is meaningful: deleting the last client has to be
        // persisted, otherwise the watcher reloads the old list and the
        // deleted client comes back.
        let config = self.toml_mut();
        config.clients = Some(clients.into_iter().map(|c| c.into()).collect::<Vec<_>>());
    }

    /// set authorized keys
    pub fn set_authorized_keys(&mut self, fingerprints: HashMap<String, String>) {
        self.toml_mut().authorized_fingerprints = Some(fingerprints);
    }

    /// store the currently used listen port so a runtime port change survives
    /// a restart
    pub fn set_port(&mut self, port: u16) {
        self.toml_mut().port = Some(port);
    }

    /// mutable access to the in-memory config, creating a default one if the
    /// config file could not be read
    fn toml_mut(&mut self) -> &mut ConfigToml {
        self.config_toml.get_or_insert_with(Default::default)
    }

    pub fn read_from_disk(&mut self) -> Result<bool, io::Error> {
        log::info!("reading config from {:?}", self.config_path);

        let current_config = fs::read_to_string(&self.config_path)?;
        let current_config = match current_config.parse::<DocumentMut>() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("{:?} {e}", self.config_path());
                return Ok(false);
            }
        };
        let mut changed = false;
        match toml_edit::de::from_document::<ConfigToml>(current_config) {
            Ok(current_config) => {
                changed = self
                    .config_toml
                    .as_ref()
                    .is_none_or(|c| c != &current_config);
                self.config_toml.replace(current_config);
            }
            Err(e) => log::warn!("{:?} {e}", self.config_path()),
        };
        if changed {
            log::info!("config changed");
        } else {
            log::info!("config unchanged");
        }
        Ok(changed)
    }

    pub fn write_back(&mut self) -> Result<(), io::Error> {
        log::info!("writing config to {:?}", self.config_path);
        /* the new config */
        let new_config = self.config_toml.clone().unwrap_or_default();
        let new_config = toml_edit::ser::to_string_pretty(&new_config).expect("config");
        /* keep the comments and the unknown keys the user wrote: only the
         * values this program manages are replaced
         */
        let new_config = merge_config(self.config_path(), &new_config);

        let _ = self.unwatch();
        /* write new config to file */
        if let Some(p) = self.config_path().parent() {
            fs::create_dir_all(p)?;
        }
        {
            let mut f = File::create(self.config_path())?;
            f.write_all(new_config.as_bytes())?;
            f.sync_all()?;
        }

        let _ = self.watch();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_comments_and_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("lan-mouse-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# leading comment\nport = 1234 # inline comment\n\n# a clients comment\n[[clients]]\nhostname = \"1.2.3.4\"\nport = 4242\nposition = \"top\"\n",
        )
        .expect("write config");

        let merged = merge_config(
            &path,
            "port = 4321\n[[clients]]\nhostname = \"1.2.3.4\"\nport = 4242\nposition = \"top\"\n",
        );
        let _ = std::fs::remove_dir_all(&dir);

        assert!(merged.contains("# leading comment"), "{merged}");
        assert!(merged.contains("# inline comment"), "{merged}");
        assert!(merged.contains("# a clients comment"), "{merged}");
        assert!(merged.contains("port = 4321"), "{merged}");
        /* the value is replaced, not duplicated */
        assert_eq!(merged.matches("port = ").count(), 2, "{merged}");
    }

    #[test]
    fn merge_keeps_unknown_keys() {
        let dir =
            std::env::temp_dir().join(format!("lan-mouse-merge-unknown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        std::fs::write(&path, "# keep me\nport = 1234\ncustom = \"value\"\n")
            .expect("write config");

        let merged = merge_config(&path, "port = 4321\n");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(merged.contains("custom = \"value\""), "{merged}");
        assert!(merged.contains("# keep me"), "{merged}");
        assert!(merged.contains("port = 4321"), "{merged}");
    }

    #[test]
    fn remap_rules_are_parsed_from_toml() {
        let toml = r#"
[[remap]]
from = ["KeyLeftShift", "KeyLeftAlt", "KeyS"]
to = ["KeyS"]

[[remap]]
from = ["KeyLeftMeta", "KeyC"]
to = ["KeyLeftCtrl", "KeyC"]
"#;
        let config: ConfigToml = toml::from_str(toml).expect("valid config");
        let remap = config.remap.expect("remap section");
        assert_eq!(remap.len(), 2);
        assert_eq!(
            remap[0].from,
            vec![
                scancode::Linux::KeyLeftShift,
                scancode::Linux::KeyLeftAlt,
                scancode::Linux::KeyS
            ]
        );
        assert_eq!(remap[0].to, vec![scancode::Linux::KeyS]);
        assert_eq!(
            remap[1].to,
            vec![scancode::Linux::KeyLeftCtrl, scancode::Linux::KeyC]
        );
    }

    #[test]
    fn config_without_remap_still_parses() {
        let config: ConfigToml = toml::from_str("port = 4242").expect("valid config");
        assert!(config.remap.is_none());
    }

    #[test]
    fn remap_round_trips_through_serialization() {
        let toml = r#"
[[remap]]
from = ["KeyLeftShift", "KeyLeftAlt", "KeyS"]
to = ["KeyS"]
"#;
        let config: ConfigToml = toml::from_str(toml).expect("valid config");
        let out = toml_edit::ser::to_string_pretty(&config).expect("serialize");
        let parsed: ConfigToml = toml::from_str(&out).expect("reparse");
        assert_eq!(parsed, config);
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;

    fn parse(text: &str) -> ConfigToml {
        toml::from_str(text).expect("valid config")
    }

    fn client(toml: &ConfigToml) -> ConfigClient {
        ConfigClient::from(toml.clients.clone().expect("clients")[0].clone())
    }

    fn config_client(edge: EdgeRange, peer_edge: EdgeRange) -> ConfigClient {
        ConfigClient {
            ips: HashSet::new(),
            hostname: Some("a".to_owned()),
            port: DEFAULT_PORT,
            pos: Position::Right,
            active: false,
            enter_hook: None,
            leave_hook: None,
            edge,
            peer_edge,
        }
    }

    /// A client written before edge ranges existed keeps the whole edge, which is
    /// exactly the behaviour that file produced.
    #[test]
    fn a_client_without_edge_keys_gets_the_whole_edge() {
        let config = parse("[[clients]]\nhostname = \"iridium\"\nposition = \"right\"\n");
        let raw = config.clients.as_ref().unwrap()[0].clone();
        assert_eq!(raw.edge, None);
        assert_eq!(raw.peer_edge, None);
        let client = ConfigClient::from(raw);
        assert_eq!(client.edge, EdgeRange::FULL);
        assert_eq!(client.peer_edge, EdgeRange::FULL);
    }

    #[test]
    fn edge_ranges_are_read_as_fractions_of_the_edge() {
        let config = parse(
            "[[clients]]\nhostname = \"a\"\nposition = \"right\"\nedge = [0.0, 0.6]\npeer_edge = [0.4, 1.0]\n",
        );
        let client = client(&config);
        assert_eq!(client.edge.fractions(), [0.0, 0.6]);
        assert_eq!(client.peer_edge.fractions(), [0.4, 1.0]);
    }

    fn write(client: &ConfigClient) -> String {
        let config = ConfigToml {
            clients: Some(vec![TomlClient::from(client.clone())]),
            ..ConfigToml::default()
        };
        toml_edit::ser::to_string_pretty(&config).expect("serialize")
    }

    fn read(text: &str) -> ConfigClient {
        ConfigClient::from(parse(text).clients.expect("clients")[0].clone())
    }

    /// Only what the user chose is written: a client that covers a whole edge
    /// must not gain two keys, or every existing config file would grow noise the
    /// first time the daemon saved it.
    #[test]
    fn a_full_edge_is_not_written_out() {
        let text = write(&config_client(EdgeRange::FULL, EdgeRange::FULL));
        assert!(!text.contains("edge"), "{text}");

        let half = EdgeRange::from_fractions(0.5, 1.0).expect("range");
        let text = write(&config_client(half, EdgeRange::FULL));
        assert!(text.contains("edge = ["), "{text}");
        assert!(text.contains("0.5") && text.contains("1.0"), "{text}");
        assert!(!text.contains("peer_edge"), "{text}");
    }

    #[test]
    fn edge_ranges_round_trip_through_toml() {
        let client = config_client(
            EdgeRange::from_fractions(0.225, 0.75).expect("range"),
            EdgeRange::from_fractions(0.0, 0.5).expect("range"),
        );
        let back = read(&write(&client));
        assert_eq!(back.edge, client.edge);
        assert_eq!(back.peer_edge, client.peer_edge);
        assert_eq!(back.hostname, client.hostname);
        assert_eq!(back.pos, client.pos);
    }

    #[test]
    fn an_invalid_edge_range_is_rejected_not_guessed() {
        let error =
            toml::from_str::<ConfigToml>("[[clients]]\nhostname = \"a\"\nedge = [0.6, 0.4]\n")
                .expect_err("an empty range must not parse");
        assert!(error.to_string().contains("empty"), "{error}");
        let error =
            toml::from_str::<ConfigToml>("[[clients]]\nhostname = \"a\"\nedge = [0.0, 1.5]\n")
                .expect_err("an out of bounds range must not parse");
        assert!(error.to_string().contains("outside"), "{error}");
    }

    #[test]
    fn switch_options_are_read_and_only_written_when_set() {
        let config = parse(
            "switch_double_tap = 250\nswitch_corner_size = 40\nswitch_corners = [\"top-left\"]\n",
        );
        assert_eq!(config.switch_double_tap, Some(250));
        assert_eq!(config.switch_corner_size, Some(40));
        assert_eq!(
            config.switch_corners.as_ref().unwrap(),
            &vec![SwitchCorner::TopLeft]
        );

        let text = toml_edit::ser::to_string_pretty(&config).expect("toml");
        assert!(text.contains("switch_double_tap = 250"), "{text}");
        assert!(text.contains("\"top-left\""), "{text}");

        let empty = ConfigToml::default();
        let text = toml_edit::ser::to_string_pretty(&empty).expect("toml");
        assert!(!text.contains("switch_"), "{text}");
    }

    #[test]
    fn a_config_without_switch_options_has_none() {
        let config: ConfigToml = toml::from_str("port = 4242").expect("valid config");
        assert_eq!(config.switch_double_tap, None);
        assert!(config.switch_corners.is_none());
    }

    #[test]
    fn switch_options_round_trip_through_toml() {
        let options = SwitchOptions {
            double_tap_ms: 120,
            corner_size: 30,
            corners: vec![SwitchCorner::BottomRight, SwitchCorner::TopLeft],
        };
        let config = ConfigToml {
            switch_double_tap: (options.double_tap_ms > 0).then_some(options.double_tap_ms),
            switch_corner_size: (options.corner_size > 0).then_some(options.corner_size),
            switch_corners: (!options.corners.is_empty()).then(|| options.corners.clone()),
            ..ConfigToml::default()
        };
        let text = toml_edit::ser::to_string_pretty(&config).expect("toml");
        let back = parse(&text);
        assert_eq!(back.switch_double_tap, Some(120));
        assert_eq!(back.switch_corners, Some(options.corners));
    }
}
