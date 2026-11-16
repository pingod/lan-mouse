use std::{
    collections::{HashMap, HashSet},
    env::VarError,
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};
use thiserror::Error;

#[cfg(unix)]
use std::{
    env,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

// Frontends talk about edge ranges through `ClientConfig` and the requests
// below, so the type is re-exported: a frontend only needs this crate.
pub use input_event::edge::EdgeRange;

mod connect;
mod connect_async;
mod listen;

pub use connect::{FrontendEventReader, FrontendRequestWriter, connect, try_connect};
pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::AsyncFrontendListener;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error(transparent)]
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("connection timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    AlreadyRunning,
    #[error("failed to bind lan-mouse socket: `{0}`")]
    Bind(io::Error),
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error occured: `{0}`")]
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Listen(#[from] IpcListenerCreationError),
}

pub const DEFAULT_PORT: u16 = 4242;

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Position::Right,
            Position::Right => Position::Left,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Top,
        }
    }
}

#[derive(Debug, Error)]
#[error("not a valid position: {pos}")]
pub struct PositionParseError {
    pos: String,
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            _ => Err(PositionParseError { pos: s.into() }),
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Position::Left => "left",
                Position::Right => "right",
                Position::Top => "top",
                Position::Bottom => "bottom",
            }
        )
    }
}

impl TryFrom<&str> for Position {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// hostname of this client
    pub hostname: Option<String>,
    /// fix ips, determined by the user
    pub fix_ips: Vec<IpAddr>,
    /// both active_addr and addrs can be None / empty so port needs to be stored seperately
    pub port: u16,
    /// position of a client on screen
    pub pos: Position,
    /// enter hook
    pub cmd: Option<String>,
    /// leave hook
    pub leave_cmd: Option<String>,
    /// segment of the local edge the pointer crosses to reach this client
    ///
    /// Several clients may share one edge as long as their ranges are disjoint;
    /// a crossing outside every range hits a wall. [`EdgeRange::FULL`] is what
    /// a client had before ranges existed, and what an older configuration
    /// without the key decodes to.
    #[serde(default = "full_edge")]
    pub edge: EdgeRange,
    /// segment of this client's facing edge the cursor lands in
    ///
    /// The crossing point is mapped from [`Self::edge`] onto this range, the way
    /// Synergy maps a link's source interval onto its destination interval, so
    /// two screens of different sizes keep the cursor where the user left it.
    #[serde(default = "full_edge")]
    pub peer_edge: EdgeRange,
}

/// The whole edge, for messages written by a frontend that predates edge ranges.
fn full_edge() -> EdgeRange {
    EdgeRange::FULL
}

/// Parse the size out of a peer's display report.
///
/// A report is `"<width>x<height>"`, or the older `"<n> displays, <w>x<h>"`, or
/// empty for a peer that never told us anything. Only the size is interesting:
/// it is what the arrangement canvas draws the peer's screen to scale with, and
/// without it the cards stay equal sized and an edge range says nothing visually.
pub fn parse_screen_size(report: &str) -> Option<(u32, u32)> {
    let token = report
        .split(|c: char| c == ',' || c.is_whitespace())
        .find(|part| part.contains('x'))?;
    let (width, height) = token.split_once('x')?;
    let width = width.trim().parse::<u32>().ok()?;
    let height = height.trim().parse::<u32>().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
            leave_cmd: None,
            edge: EdgeRange::FULL,
            peer_edge: EdgeRange::FULL,
        }
    }
}

pub type ClientHandle = u64;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientState {
    /// events should be sent to and received from the client
    pub active: bool,
    /// `active` address of the client, used to send data to.
    /// This should generally be the socket address where data
    /// was last received from.
    pub active_addr: Option<SocketAddr>,
    /// tracks whether or not the client is available for emulation
    pub alive: bool,
    /// ips from dns
    pub dns_ips: Vec<IpAddr>,
    /// all ip addresses associated with a particular client
    /// e.g. Laptops usually have at least an ethernet and a wifi port
    /// which have different ip addresses
    pub ips: HashSet<IpAddr>,
    /// client has pressed keys
    pub has_pressed_keys: bool,
    /// dns resolving in progress
    pub resolving: bool,
    /// Peer's build short commit hash from the [`Hello`] proto
    /// event. `None` means we haven't received a Hello yet — either
    /// the connection is fresh, or the peer is on an older build
    /// that predates the Hello event. The frontend uses this to
    /// soft-warn on version mismatch.
    pub peer_commit: Option<[u8; 8]>,
    /// round trip time of the last ping/pong exchange, in milliseconds
    pub rtt_ms: Option<u64>,
    /// when this client was last heard from, as seconds since the Unix epoch
    pub last_seen: Option<u64>,
    /// the client has an established (D)TLS connection right now
    pub connected: bool,
    /// what the peer reported about itself: OS, version, kernel, arch,
    /// hostname, app version, commit and display summary. Empty fields mean the
    /// peer is on a build that predates the exchange.
    pub peer_os: String,
    pub peer_os_version: String,
    pub peer_kernel: String,
    pub peer_arch: String,
    pub peer_hostname: String,
    pub peer_app_version: String,
    pub peer_commit_short: String,
    pub peer_displays: String,
    /// input events forwarded to this client since the connection started
    pub forwarded: u64,
    /// input events received from this client since the connection started
    pub received: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    /// a client was created
    Created(ClientHandle, ClientConfig, ClientState),
    /// no such client
    NoSuchClient(ClientHandle),
    /// state changed
    State(ClientHandle, ClientConfig, ClientState),
    /// the client was deleted
    Deleted(ClientHandle),
    /// new port, reason of failure (if failed)
    PortChanged(u16, Option<String>),
    /// list of all clients, used for initial state synchronization
    Enumerate(Vec<(ClientHandle, ClientConfig, ClientState)>),
    /// an error occured
    Error(String),
    /// service is paused: capture and emulation are torn down but the process
    /// keeps running and its IPC socket stays reachable
    Paused(bool),
    /// periodic restart interval in seconds, `None` when disabled
    RestartInterval(Option<u64>),
    /// one or more edge ranges were narrowed because several clients claim the
    /// same part of an edge; the message says which and what is in effect
    ///
    /// Synergy refuses to save such a configuration at all ("overlapping
    /// range"). lan-mouse keeps every client reachable by letting the earlier
    /// one win and reporting the result, because the arrangement canvas and the
    /// config file are edited independently here.
    EdgeConflict(String),
    /// current edge-crossing thresholds
    SwitchOptions(SwitchOptions),
    /// the active log level
    LogLevel(String),
    /// buffered log records, oldest first
    Logs(Vec<String>),
    /// records appended to a live stream
    LogAppended(Vec<String>),
    /// a full snapshot of every inbound connection
    IncomingConnections(Vec<IncomingInfo>),
    /// live backend and runtime counters
    Runtime(RuntimeInfo),
    /// capture status
    CaptureStatus(Status),
    /// emulation status
    EmulationStatus(Status),
    /// authorized public key fingerprints have been updated
    AuthorizedUpdated(HashMap<String, String>),
    /// public key fingerprint of this device
    PublicKeyFingerprint(String),
    /// new device connected
    DeviceConnected {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// incoming device entered the screen
    DeviceEntered {
        fingerprint: String,
        addr: SocketAddr,
        pos: Position,
    },
    /// incoming disconnected
    IncomingDisconnected(SocketAddr),
    /// failed connection attempt (approval for fingerprint required)
    ConnectionAttempt { fingerprint: String },
    /// current global hotkey rules
    Hotkeys(Vec<(String, String)>),
    /// current chord remapping rules, as human-readable key names
    Remap(Vec<(String, String)>),
    /// keys that pull the pointer back, as human-readable key names
    ReleaseBind(String),
    /// version information of this build
    Version {
        version: String,
        commit: String,
        /// whether the autostart entry is currently installed
        autostart: bool,
    },
    /// whether autostart is installed (sent after a toggle)
    Autostart(bool),
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum FrontendRequest {
    /// activate/deactivate client
    Activate(ClientHandle, bool),
    /// add a new client
    Create,
    /// add a new client together with its full configuration
    ///
    /// `Create` has to be followed by `UpdateHostname`/`UpdatePort`/
    /// `UpdatePosition`/`UpdateFixIps`, and the answers to those are broadcast
    /// to every connected frontend: two frontends creating a client at the
    /// same time therefore apply each other's `Created(handle)` to the wrong
    /// handle and leave half-configured clients behind. Sending the whole
    /// configuration at once makes creating a client atomic, so a single
    /// request is all a caller needs (the resulting handle is looked up with
    /// `Enumerate`).
    CreateClient {
        /// hostname of the new client
        hostname: Option<String>,
        /// port of the new client
        port: u16,
        /// position of the new client on screen
        position: Position,
        /// fix ips, determined by the user
        fix_ips: Vec<IpAddr>,
        /// command to run when the pointer enters this client
        enter_hook: Option<String>,
        /// command to run when the pointer leaves this client
        leave_hook: Option<String>,
        /// segment of the local edge the pointer crosses to reach it
        #[serde(default = "full_edge")]
        edge: EdgeRange,
        /// segment of the client's facing edge the cursor lands in
        #[serde(default = "full_edge")]
        peer_edge: EdgeRange,
    },
    /// change the listen port (recreate udp listener)
    ChangePort(u16),
    /// remove a client
    Delete(ClientHandle),
    /// request an enumeration of all clients
    Enumerate(),
    /// resolve dns
    ResolveDns(ClientHandle),
    /// update hostname
    UpdateHostname(ClientHandle, Option<String>),
    /// update port
    UpdatePort(ClientHandle, u16),
    /// update position
    UpdatePosition(ClientHandle, Position),
    /// set both edge ranges of one client at once
    ///
    /// The two ranges describe one link and are always edited together (the
    /// arrangement canvas drags them as one rectangle), so a single request is
    /// used: sending [`FrontendRequest::UpdatePosition`] and then two range
    /// updates would broadcast half-saved states to every frontend.
    ///
    /// The daemon resolves overlaps against the other clients on that edge, so
    /// the range that ends up in effect can be narrower than the one requested;
    /// it reports that with [`FrontendEvent::EdgeConflict`].
    UpdateEdge {
        /// the client whose link is edited
        handle: ClientHandle,
        /// segment of the local edge the pointer crosses to reach it
        edge: EdgeRange,
        /// segment of the client's facing edge the cursor lands in
        peer_edge: EdgeRange,
    },
    /// update fix-ips
    UpdateFixIps(ClientHandle, Vec<IpAddr>),
    /// request reenabling input capture
    EnableCapture,
    /// request reenabling input emulation
    EnableEmulation,
    /// synchronize all state
    Sync,
    /// authorize fingerprint (description, fingerprint)
    AuthorizeKey(String, String),
    /// remove fingerprint (fingerprint)
    RemoveAuthorizedKey(String),
    /// change the hook command
    UpdateEnterHook(u64, Option<String>),
    /// change the leave hook command
    UpdateLeaveHook(u64, Option<String>),
    /// replace both hook commands of one client at once
    ///
    /// Sending [`FrontendRequest::UpdateEnterHook`] and
    /// [`FrontendRequest::UpdateLeaveHook`] separately makes the daemon
    /// broadcast two intermediate states, so a frontend could briefly show
    /// half of what the editor saved. An empty string clears that hook.
    UpdateHooks {
        /// the client whose hooks are edited
        handle: ClientHandle,
        /// command run when the pointer enters this client
        enter: String,
        /// command run when the pointer leaves this client
        leave: String,
    },
    /// save config file
    SaveConfiguration,
    /// request the current hotkey rules
    EnumerateHotkeys,
    /// add a global hotkey (from, to) as human-readable key names
    AddHotkey(String, String),
    /// replace the global hotkey at this index with (from, to) key names
    UpdateHotkey(usize, String, String),
    /// remove the global hotkey with this index
    RemoveHotkey(usize),
    /// replace the keys that pull the pointer back while it is on a peer
    UpdateReleaseBind(String),
    /// request the current chord remapping rules
    EnumerateRemap,
    /// add a remap rule (from, to) as human-readable key names
    AddRemap(String, String),
    /// replace the remap rule at this index with (from, to) key names
    UpdateRemap(usize, String, String),
    /// remove the remap rule with this index
    RemoveRemap(usize),
    /// request version information
    EnumerateVersion,
    /// enable or disable starting with the session
    SetAutostart(bool),
    /// re-read the configuration file now
    ReloadConfig,
    /// suspend or resume input capture and emulation without exiting
    SetPaused(bool),
    /// exit so the supervisor (systemd / launchd) starts a fresh process
    Restart,
    /// schedule a periodic restart; `None` disables it
    SetRestartInterval(Option<u64>),
    /// request the pause state and restart schedule
    EnumerateServiceState,
    /// replace the edge-crossing thresholds
    SetSwitchOptions(SwitchOptions),
    /// request the current edge-crossing thresholds
    EnumerateSwitchOptions,
    /// release the pointer from whatever client currently holds it
    ForceRelease,
    /// hand control to the next client (or back to this machine)
    SwitchDevice,
    /// change the log level at runtime; `None` keeps the current level
    SetLogLevel(Option<String>),
    /// request the last N log records
    EnumerateLogs(usize),
    /// stream new records as they are written; `None` stops the stream
    SubscribeLogs(Option<usize>),
    /// re-ask a peer for its machine details
    RefreshPeerInfo(ClientHandle),
}

/// How close to an edge the pointer has to behave before it may cross.
///
/// These are the parts of Synergy's `section: options` that lan-mouse can
/// implement from a crossing alone: a hit can require a second hit, and the
/// corners can be blocked. `switchDelay` cannot be expressed because a barrier
/// reports the crossing rather than the dwell, and the `switchNeedsShift` /
/// `switchNeedsControl` / `switchNeedsAlt` family cannot be read on Wayland,
/// where the compositor does not expose the modifiers of a pointer this process
/// does not own.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct SwitchOptions {
    /// milliseconds within which the same edge has to be hit twice; `0` lets a
    /// single crossing through
    ///
    /// This is Synergy's `switchDoubleTap`. Its `switchDelay` has no lan-mouse
    /// equivalent: a delay needs to watch the pointer *dwell* on the edge, and a
    /// barrier reports only the crossing - once it fires the pointer is already
    /// gone, so there is nothing to wait for.
    #[serde(default)]
    pub double_tap_ms: u32,
    /// radius in pixels of the corners the pointer may not leave through;
    /// `0` disables the corners
    #[serde(default)]
    pub corner_size: u32,
    /// which corners are blocked; empty when none are
    #[serde(default)]
    pub corners: Vec<SwitchCorner>,
}

/// A corner of the reachable screen area.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SwitchCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl SwitchCorner {
    /// The spelling used in the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            SwitchCorner::TopLeft => "top-left",
            SwitchCorner::TopRight => "top-right",
            SwitchCorner::BottomLeft => "bottom-left",
            SwitchCorner::BottomRight => "bottom-right",
        }
    }
}

impl std::str::FromStr for SwitchCorner {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "top-left" => Ok(SwitchCorner::TopLeft),
            "top-right" => Ok(SwitchCorner::TopRight),
            "bottom-left" => Ok(SwitchCorner::BottomLeft),
            "bottom-right" => Ok(SwitchCorner::BottomRight),
            other => Err(format!("not a corner: `{other}`")),
        }
    }
}

impl SwitchOptions {
    /// Whether no threshold is active, for logging and for the frontend hint.
    pub fn has_no_thresholds(&self) -> bool {
        self.double_tap_ms == 0 && self.corner_size == 0
    }
}

/// Live state of the running service.
///
/// Everything here changes while it runs, unlike [`IncomingInfo`] which
/// describes an install.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// capture backend in use, empty when unavailable
    pub capture_backend: String,
    /// emulation backend in use
    pub emulation_backend: String,
    /// capture is enabled and the backend is alive
    pub capture_alive: bool,
    /// emulation is enabled and the backend is alive
    pub emulation_alive: bool,
    /// pointer is currently handed to a peer
    pub capturing: bool,
    /// peer that holds the pointer, empty when local
    pub capturing_peer: String,
    /// configured listen port
    pub port: u16,
    /// seconds since the service started
    pub uptime_secs: u64,
    /// whether capture or emulation is paused by the user
    pub paused: bool,
    /// number of active inbound connections
    pub inbound: usize,
    /// number of connected outbound clients
    pub outbound: usize,
    /// best round-trip time across peers, in milliseconds; 0 when unknown
    pub best_rtt_ms: u32,
    /// width of the reachable screen area in pixels; 0 when the backend does
    /// not report one. The arrangement canvas needs it to draw the local screen
    /// to scale, which is what makes an edge range mean something visually.
    #[serde(default)]
    pub screen_width: u32,
    /// height of the reachable screen area in pixels; 0 when unknown
    #[serde(default)]
    pub screen_height: u32,
}

/// One device currently driving (or allowed to drive) this machine.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct IncomingInfo {
    /// peer address
    pub addr: SocketAddr,
    /// peer certificate fingerprint (SHA-256, colon separated)
    pub fingerprint: String,
    /// side of this screen the peer entered from
    pub pos: Position,
    /// seconds since this peer last sent an event
    pub idle_secs: u64,
    /// events received from this peer since it connected
    pub received: u64,
    /// whether this peer is the one currently holding the pointer
    pub active: bool,
    /// what the peer reported about itself; empty fields mean an older build
    pub os: String,
    pub os_version: String,
    pub kernel: String,
    pub arch: String,
    pub hostname: String,
    pub app_version: String,
    pub commit: String,
    pub displays: String,
}

impl Default for IncomingInfo {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            fingerprint: String::new(),
            pos: Position::default(),
            idle_secs: 0,
            received: 0,
            active: false,
            os: String::new(),
            os_version: String::new(),
            kernel: String::new(),
            arch: String::new(),
            hostname: String::new(),
            app_version: String::new(),
            commit: String::new(),
            displays: String::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
}

impl From<Status> for bool {
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled => false,
        }
    }
}

#[cfg(unix)]
const LAN_MOUSE_SOCKET_NAME: &str = "lan-mouse-socket.sock";

#[derive(Debug, Error)]
pub enum SocketPathError {
    #[error("could not determine $XDG_RUNTIME_DIR: `{0}`")]
    XdgRuntimeDirNotFound(VarError),
    #[error("could not determine $HOME: `{0}`")]
    HomeDirNotFound(VarError),
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let xdg_runtime_dir =
        env::var("XDG_RUNTIME_DIR").map_err(SocketPathError::XdgRuntimeDirNotFound)?;
    Ok(Path::new(xdg_runtime_dir.as_str()).join(LAN_MOUSE_SOCKET_NAME))
}

#[cfg(all(unix, target_os = "macos"))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let home = env::var("HOME").map_err(SocketPathError::HomeDirNotFound)?;
    Ok(Path::new(home.as_str())
        .join("Library")
        .join("Caches")
        .join(LAN_MOUSE_SOCKET_NAME))
}

/// Check if a lan-mouse service is already running by probing the IPC socket.
#[cfg(unix)]
pub fn is_service_running() -> bool {
    let Ok(socket_path) = default_socket_path() else {
        return false;
    };
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

/// Check if a lan-mouse service is already running by probing the IPC socket.
#[cfg(windows)]
pub fn is_service_running() -> bool {
    std::net::TcpStream::connect("127.0.0.1:5252").is_ok()
}

#[cfg(test)]
mod serialization_tests {
    use super::*;

    /// A `ClientConfig` written by a frontend that predates edge ranges must
    /// still decode: both ranges fall back to the whole edge, which is exactly
    /// the behaviour that frontend expected.
    #[test]
    fn client_config_without_edge_ranges_still_decodes() {
        let json = r#"{"hostname":"iridium","fix_ips":[],"port":4242,"pos":"right","cmd":null,"leave_cmd":null}"#;
        let config: ClientConfig = serde_json::from_str(json).expect("decode");
        assert_eq!(config.edge, EdgeRange::FULL);
        assert_eq!(config.peer_edge, EdgeRange::FULL);
    }

    #[test]
    fn edge_ranges_round_trip() {
        let config = ClientConfig {
            edge: EdgeRange::from_fractions(0.5, 1.0).expect("range"),
            peer_edge: EdgeRange::from_fractions(0.0, 0.25).expect("range"),
            ..ClientConfig::default()
        };
        let json = serde_json::to_string(&config).expect("encode");
        let back: ClientConfig = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, config);
        assert!(json.contains("[0.5,1.0]"), "{json}");
    }

    /// The daemon matches on these; a request from an older CLI has to decode
    /// with the full edge rather than being rejected.
    #[test]
    fn create_client_without_edge_ranges_still_decodes() {
        let json = r#"{"CreateClient":{"hostname":"a","port":4242,"position":"left","fix_ips":[],"enter_hook":null,"leave_hook":null}}"#;
        let request: FrontendRequest = serde_json::from_str(json).expect("decode");
        let FrontendRequest::CreateClient {
            edge, peer_edge, ..
        } = request
        else {
            panic!("wrong variant");
        };
        assert_eq!(edge, EdgeRange::FULL);
        assert_eq!(peer_edge, EdgeRange::FULL);
    }

    #[test]
    fn update_edge_round_trips() {
        let request = FrontendRequest::UpdateEdge {
            handle: 7,
            edge: EdgeRange::from_fractions(0.0, 0.4).expect("range"),
            peer_edge: EdgeRange::from_fractions(0.6, 1.0).expect("range"),
        };
        let json = serde_json::to_string(&request).expect("encode");
        let back: FrontendRequest = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, request);
    }

    /// A `RuntimeInfo` from an older service has no screen geometry; the canvas
    /// falls back to equal-sized cards rather than failing to decode.
    #[test]
    fn runtime_info_without_screen_size_still_decodes() {
        let json = r#"{
            "capture_backend": "libei",
            "emulation_backend": "wlroots",
            "capture_alive": true,
            "emulation_alive": true,
            "capturing": false,
            "capturing_peer": "",
            "port": 4242,
            "uptime_secs": 3,
            "paused": false,
            "inbound": 0,
            "outbound": 1,
            "best_rtt_ms": 4
        }"#;
        let back: RuntimeInfo =
            serde_json::from_str(json).unwrap_or_else(|e| panic!("could not decode: {e}"));
        assert_eq!(back.screen_width, 0);
        assert_eq!(back.screen_height, 0);
        assert_eq!(back.port, 4242);
    }

    #[test]
    fn switch_options_round_trip_and_defaults() {
        let json = r#"{"double_tap_ms":250}"#;
        let options: SwitchOptions = serde_json::from_str(json).expect("decode");
        assert_eq!(options.double_tap_ms, 250);
        assert_eq!(options.corner_size, 0);
        assert!(options.corners.is_empty());
        let encoded = serde_json::to_string(&options).expect("encode");
        let back: SwitchOptions = serde_json::from_str(&encoded).expect("re-decode");
        assert_eq!(back, options);
    }

    #[test]
    fn switch_corners_use_the_config_spelling() {
        let corner: SwitchCorner = "top-right".parse().expect("parse");
        assert_eq!(corner, SwitchCorner::TopRight);
        assert_eq!(corner.as_str(), "top-right");
        assert!("middle".parse::<SwitchCorner>().is_err());
        let json = serde_json::to_string(&vec![SwitchCorner::BottomLeft]).expect("encode");
        assert_eq!(json, r#"["bottom-left"]"#);
    }

    /// A peer running an older build reports `"2 displays, 2560x1440"`, and one
    /// that never answered reports nothing; both have to decode without the
    /// canvas mistaking a count for a width.
    #[test]
    fn screen_sizes_are_parsed_from_either_report_shape() {
        assert_eq!(parse_screen_size("2560x1440"), Some((2560, 1440)));
        assert_eq!(
            parse_screen_size("2 displays, 2560x1440"),
            Some((2560, 1440))
        );
        assert_eq!(parse_screen_size(""), None);
        assert_eq!(parse_screen_size("no idea"), None);
        assert_eq!(parse_screen_size("0x0"), None);
        // the count must not be read as the width
        assert_eq!(parse_screen_size("1 display"), None);
    }

    #[test]
    fn default_config_has_the_whole_edge() {
        assert_eq!(ClientConfig::default().edge, EdgeRange::FULL);
        assert_eq!(ClientConfig::default().peer_edge, EdgeRange::FULL);
        assert!(SwitchOptions::default().has_no_thresholds());
    }
}
