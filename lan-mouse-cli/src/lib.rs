use clap::{Args, Parser, Subcommand};
use futures::StreamExt;

use std::{net::IpAddr, time::Duration};
use thiserror::Error;

use lan_mouse_ipc::{
    AsyncFrontendEventReader, AsyncFrontendRequestWriter, ClientHandle, ConnectionError,
    DEFAULT_PORT, EdgeRange, FrontendEvent, FrontendRequest, IpcError, Position, connect_async,
};

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
    /// the service reported an error
    #[error("service error: {0}")]
    Service(String),
    /// the service does not know the given client
    #[error("no such client: {0}")]
    NoSuchClient(ClientHandle),
    /// the service closed the connection before it answered
    #[error("the service closed the connection before answering")]
    ConnectionClosed,
}

#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(name = "lan-mouse-cli", about = "LanMouse CLI interface")]
pub struct CliArgs {
    #[command(subcommand)]
    command: CliSubcommand,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
struct Client {
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    ips: Option<Vec<IpAddr>>,
    #[arg(long)]
    enter_hook: Option<String>,
    #[arg(long)]
    leave_hook: Option<String>,
    /// which edge of this screen the client sits behind
    #[arg(long)]
    position: Option<Position>,
    /// the local monitor this link is attached to, by name (see `screens`)
    ///
    /// Scopes `--edge` to that monitor's edge instead of the bounding box around
    /// every monitor. Needed on a desk whose monitors have unequal heights, where
    /// part of the box's edge belongs to a screen the link does not touch.
    #[arg(long)]
    display: Option<String>,
    /// the part of that edge the pointer crosses to reach it, as
    /// `start,end` fractions of the edge (0.0-1.0)
    ///
    /// Several clients can share one edge as long as their ranges do not
    /// overlap. Absent means the whole edge.
    #[arg(long)]
    edge: Option<EdgeRange>,
    /// the part of the client's facing edge the cursor lands in, same form as
    /// `--edge`
    #[arg(long)]
    peer_edge: Option<EdgeRange>,
}

#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
enum CliSubcommand {
    /// add a new client
    AddClient(Client),
    /// remove an existing client
    RemoveClient { id: ClientHandle },
    /// activate a client
    Activate { id: ClientHandle },
    /// deactivate a client
    Deactivate { id: ClientHandle },
    /// list configured clients
    List,
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// Publish an arrangement as if the canvas had drawn it.
    ///
    /// The window normally does this; the command exists to check the transport
    /// without one, which is the difference between "the picture never arrived" and
    /// "the picture arrived and was not used".
    PublishArrangement {
        /// the encoded arrangement, see `Arrangement::encode`
        arrangement: String,
    },
    /// set the part of its edge a client is reached through, and the part of the
    /// client's edge the cursor lands in
    SetEdge {
        id: ClientHandle,
        /// the local monitor to attach the link to; omit to keep the current one
        #[arg(long)]
        display: Option<String>,
        /// `start,end` fractions of this screen's edge, e.g. `0.0,0.6`
        #[arg(long)]
        edge: EdgeRange,
        /// `start,end` fractions of the client's facing edge
        #[arg(long, default_value = "0.0,1.0")]
        peer_edge: EdgeRange,
    },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
    /// re-enable capture
    EnableCapture,
    /// re-enable emulation
    EnableEmulation,
    /// authorize a public key
    AuthorizeKey {
        description: String,
        sha256_fingerprint: String,
    },
    /// deauthorize a public key
    RemoveAuthorizedKey { sha256_fingerprint: String },
    /// save configuration to file
    SaveConfig,
    /// re-read the configuration file now
    ReloadConfig,
    /// suspend input capture and emulation without quitting
    Pause,
    /// resume after [`Self::Pause`]
    Resume,
    /// exit so the service manager starts a fresh process
    Restart,
    /// restart every N seconds; 0 disables
    SetRestartInterval { seconds: u64 },
    /// show pause state, restart schedule and per-client connection health
    Status,
    /// release the pointer from the remote machine
    ForceRelease,
    /// hand control to the next machine, or back to this one
    Switch,
    /// show recent service log records
    Logs {
        #[arg(default_value_t = 100)]
        lines: usize,
    },
    /// change the log level at runtime
    SetLogLevel { level: String },
    /// ask a peer to send its machine details again
    RefreshPeerInfo { id: ClientHandle },
    /// show the screen layout: this screen's size and who covers which part of
    /// every edge
    Screens,
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    execute(args.command).await?;
    Ok(())
}

async fn execute(cmd: CliSubcommand) -> Result<(), CliError> {
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    match cmd {
        CliSubcommand::AddClient(Client {
            hostname,
            port,
            ips,
            enter_hook,
            leave_hook,
            position,
            display,
            edge,
            peer_edge,
        }) => {
            // Create the client with its whole configuration in one request:
            // `Create` followed by `Update*` requests is racy because the
            // `Created` answer is broadcast to every connected frontend, so
            // two concurrent `add-client` runs send their updates for the
            // wrong handle.
            let port = port.unwrap_or(DEFAULT_PORT);
            let fix_ips = ips.unwrap_or_default();
            tx.request(FrontendRequest::CreateClient {
                display,
                hostname: hostname.clone(),
                port,
                position: position.unwrap_or_default(),
                fix_ips: fix_ips.clone(),
                enter_hook,
                leave_hook,
                edge: edge.unwrap_or(EdgeRange::FULL),
                peer_edge: peer_edge.unwrap_or(EdgeRange::FULL),
            })
            .await?;
            loop {
                let event = rx.next().await.ok_or(CliError::ConnectionClosed)??;
                match event {
                    // Match on the configuration we sent so a `Created` event
                    // of a concurrent `add-client` is not mistaken for ours.
                    FrontendEvent::Created(handle, config, state)
                        if config.hostname == hostname
                            && config.port == port
                            && config.fix_ips == fix_ips =>
                    {
                        println!(
                            "id {handle}: {}:{} ({:?}) active: {}",
                            config.hostname.as_deref().unwrap_or("unknown"),
                            config.port,
                            config.pos,
                            state.active
                        );
                        break;
                    }
                    FrontendEvent::Error(msg) => return Err(CliError::Service(msg)),
                    FrontendEvent::NoSuchClient(handle) => {
                        return Err(CliError::NoSuchClient(handle));
                    }
                    // ignore unrelated events while waiting for the client
                    _ => {}
                }
            }
        }
        CliSubcommand::RemoveClient { id } => {
            tx.request(FrontendRequest::Delete(id)).await?;
            loop {
                match rx.next().await.ok_or(CliError::ConnectionClosed)?? {
                    FrontendEvent::Deleted(handle) if handle == id => break,
                    FrontendEvent::NoSuchClient(handle) if handle == id => {
                        return Err(CliError::NoSuchClient(handle));
                    }
                    FrontendEvent::Error(msg) => return Err(CliError::Service(msg)),
                    _ => {}
                }
            }
        }
        CliSubcommand::Activate { id } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::Activate(id, true)).await?
        }
        CliSubcommand::Deactivate { id } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::Activate(id, false)).await?
        }
        CliSubcommand::List => {
            tx.request(FrontendRequest::Enumerate()).await?;
            loop {
                let event = rx.next().await.ok_or(CliError::ConnectionClosed)??;
                match event {
                    FrontendEvent::Error(msg) => return Err(CliError::Service(msg)),
                    FrontendEvent::NoSuchClient(handle) => {
                        return Err(CliError::NoSuchClient(handle));
                    }
                    FrontendEvent::Enumerate(clients) => {
                        for (handle, config, state) in clients {
                            let host = config.hostname.unwrap_or("unknown".to_owned());
                            let port = config.port;
                            let pos = config.pos;
                            let active = state.active;
                            let ips = state.ips;
                            let health = match (state.connected, state.rtt_ms) {
                                (true, Some(rtt)) => format!("connected, rtt {rtt} ms"),
                                (true, None) => "connected".to_owned(),
                                (false, _) => "not connected".to_owned(),
                            };
                            /* the range is only spelled out when it is not the
                             * whole edge, so a plain two-machine setup still
                             * reads the same as it always did */
                            let placement = if config.edge == EdgeRange::FULL {
                                pos.to_string()
                            } else {
                                format!("{pos} {edge}", edge = config.edge)
                            };
                            println!(
                                "id {handle}: {host}:{port} ({placement}) active: {active}, {health}, ips: {ips:?}"
                            );
                            /* The picture the peer published, which is what this
                             * side is supposed to be drawing. Printed rather than
                             * summarised because whether it arrived at all is the
                             * question a mismatched pair of windows asks. */
                            if !state.peer_arrangement.is_empty() {
                                println!("    peer arrangement: {}", state.peer_arrangement);
                            }
                            if !state.peer_displays.is_empty() {
                                println!("    peer displays: {}", state.peer_displays);
                            }
                        }
                        break;
                    }
                    // ignore unrelated events while waiting for the list
                    _ => {}
                }
            }
        }
        CliSubcommand::SetHost { id, host } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?
        }
        CliSubcommand::SetPort { id, port } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::UpdatePort(id, port)).await?
        }
        CliSubcommand::SetPosition { id, pos } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?
        }
        CliSubcommand::PublishArrangement { arrangement } => {
            tx.request(FrontendRequest::PublishArrangement { arrangement })
                .await?;
            println!("published");
        }
        CliSubcommand::SetEdge {
            id,
            display,
            edge,
            peer_edge,
        } => {
            /* the edge is only half of a link: the position has to travel with
             * it, so the daemon gets one request instead of two half-saved
             * states */
            let (position, current_display) = client_link(&mut rx, &mut tx, id).await?;
            /* `--display` is optional and omitting it must not detach the link
             * from its monitor, so the current value is carried over */
            let display = display.or(current_display);
            tx.request(FrontendRequest::UpdateLink {
                handle: id,
                position,
                display,
                edge,
                peer_edge,
            })
            .await?
        }
        CliSubcommand::SetIps { id, ips } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::UpdateFixIps(id, ips)).await?
        }
        CliSubcommand::EnableCapture => tx.request(FrontendRequest::EnableCapture).await?,
        CliSubcommand::EnableEmulation => tx.request(FrontendRequest::EnableEmulation).await?,
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => {
            tx.request(FrontendRequest::AuthorizeKey(
                description,
                sha256_fingerprint,
            ))
            .await?
        }
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            tx.request(FrontendRequest::RemoveAuthorizedKey(sha256_fingerprint))
                .await?
        }
        CliSubcommand::SaveConfig => tx.request(FrontendRequest::SaveConfiguration).await?,
        CliSubcommand::ReloadConfig => tx.request(FrontendRequest::ReloadConfig).await?,
        CliSubcommand::Pause => tx.request(FrontendRequest::SetPaused(true)).await?,
        CliSubcommand::Resume => tx.request(FrontendRequest::SetPaused(false)).await?,
        CliSubcommand::Restart => tx.request(FrontendRequest::Restart).await?,
        CliSubcommand::SetRestartInterval { seconds } => {
            let interval = if seconds == 0 { None } else { Some(seconds) };
            tx.request(FrontendRequest::SetRestartInterval(interval))
                .await?
        }
        CliSubcommand::ForceRelease => tx.request(FrontendRequest::ForceRelease).await?,
        CliSubcommand::RefreshPeerInfo { id } => {
            tx.request(FrontendRequest::RefreshPeerInfo(id)).await?
        }
        CliSubcommand::Switch => tx.request(FrontendRequest::SwitchDevice).await?,
        CliSubcommand::Logs { lines } => {
            tx.request(FrontendRequest::EnumerateLogs(lines)).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Logs(records) = e? {
                    for record in records {
                        println!("{record}");
                    }
                    break;
                }
            }
        }
        CliSubcommand::SetLogLevel { level } => {
            tx.request(FrontendRequest::SetLogLevel(Some(level)))
                .await?;
            while let Some(e) = rx.next().await {
                match e? {
                    FrontendEvent::LogLevel(level) => {
                        println!("log level: {level}");
                        break;
                    }
                    FrontendEvent::Error(e) => {
                        eprintln!("{e}");
                        break;
                    }
                    _ => {}
                }
            }
        }
        CliSubcommand::Screens => {
            tx.request(FrontendRequest::Enumerate()).await?;
            let mut clients: Option<Vec<_>> = None;
            let mut runtime: Option<lan_mouse_ipc::RuntimeInfo> = None;
            while clients.is_none() || runtime.is_none() {
                match rx.next().await.ok_or(CliError::ConnectionClosed)?? {
                    FrontendEvent::Enumerate(c) => clients = Some(c),
                    FrontendEvent::Runtime(info) => runtime = Some(info),
                    FrontendEvent::Error(msg) => return Err(CliError::Service(msg)),
                    _ => {}
                }
            }
            let clients = clients.unwrap_or_default();
            let runtime = runtime.expect("collected above");
            print_screens(
                runtime.screen_width,
                runtime.screen_height,
                &clients,
                runtime.capturing_peer.as_str(),
            );
        }
        CliSubcommand::Status => {
            tx.request(FrontendRequest::Enumerate()).await?;
            tx.request(FrontendRequest::EnumerateServiceState).await?;
            let mut paused: Option<bool> = None;
            let mut interval: Option<Option<u64>> = None;
            let mut clients: Option<Vec<_>> = None;
            while let Some(e) = rx.next().await {
                match e? {
                    FrontendEvent::Paused(p) => paused = Some(p),
                    FrontendEvent::RestartInterval(s) => interval = Some(s),
                    FrontendEvent::Enumerate(c) => clients = Some(c),
                    _ => {}
                }
                if paused.is_some() && interval.is_some() && clients.is_some() {
                    break;
                }
            }
            println!(
                "service: {}",
                match paused {
                    Some(true) => "paused",
                    Some(false) => "running",
                    None => "unknown",
                }
            );
            match interval.flatten() {
                Some(secs) => println!("scheduled restart: every {secs}s"),
                None => println!("scheduled restart: off"),
            }
            for (handle, config, state) in clients.unwrap_or_default() {
                let host = config.hostname.unwrap_or_else(|| "unknown".to_owned());
                println!("id {handle}: {host}:{} ({})", config.port, config.pos);
                println!(
                    "  enabled: {}, connected: {}, alive: {}, resolving: {}",
                    state.active, state.connected, state.alive, state.resolving
                );
                if let Some(addr) = state.active_addr {
                    println!("  address: {addr}");
                }
                if let Some(rtt) = state.rtt_ms {
                    println!("  round trip: {rtt} ms");
                }
                // What the peer reported about itself.
                let detail = |label: &str, value: &str| {
                    if !value.is_empty() {
                        println!("  {label}: {value}");
                    }
                };
                detail("os", &state.peer_os);
                detail("os version", &state.peer_os_version);
                detail("kernel", &state.peer_kernel);
                detail("arch", &state.peer_arch);
                detail("hostname", &state.peer_hostname);
                detail("version", &state.peer_app_version);
                detail("commit", &state.peer_commit_short);
                detail("displays", &state.peer_displays);
                if let Some(seen) = state.last_seen {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(seen);
                    println!("  last seen: {}s ago", now.saturating_sub(seen));
                }
                if state.has_pressed_keys {
                    println!("  keys held on peer");
                }
            }
        }
    }
    Ok(())
}

/// Make sure the daemon knows the given client before sending a request for it.
///
/// The one-shot commands used to exit successfully even for unknown client
/// handles because the daemon silently ignores requests for clients that do
/// Print the screen layout the daemon is working with.
///
/// The point is to answer "why does the pointer not get there" without a
/// display: which part of each edge is claimed, which part is a wall, and
/// whether a client was squeezed out of the range it asked for. Only enabled
/// clients are resolved, because only those have a barrier registered.
fn print_screens(
    width: u32,
    height: u32,
    clients: &[(
        ClientHandle,
        lan_mouse_ipc::ClientConfig,
        lan_mouse_ipc::ClientState,
    )],
    capturing_peer: &str,
) {
    if width > 0 && height > 0 {
        println!("this screen: {width}x{height}");
    } else {
        /* Without a size the canvas cannot draw to scale and the corner
         * thresholds cannot be located, so it is worth saying out loud. */
        println!("this screen: size unknown to the capture backend");
    }
    if !capturing_peer.is_empty() {
        println!("the pointer is on: {capturing_peer}");
    }

    for position in [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ] {
        let on_edge: Vec<_> = clients
            .iter()
            .filter(|(_, config, _)| config.pos == position)
            .collect();
        println!("\n{position}:");
        if on_edge.is_empty() {
            println!("  nothing configured");
            continue;
        }
        /* Only links on the same monitor compete for a stretch: a link above one
         * screen and a link above another are not narrowed by each other, and
         * their ranges are fractions of different edges. */
        let mut groups: Vec<Option<String>> = Vec::new();
        for (_, config, _) in &on_edge {
            if !groups.contains(&config.display) {
                groups.push(config.display.clone());
            }
        }
        for display in &groups {
            let group: Vec<_> = on_edge
                .iter()
                .filter(|(_, config, _)| config.display == *display)
                .collect();
            let enabled = group
                .iter()
                .filter(|(_, _, state)| state.active)
                .map(|(handle, config, _)| (*handle, config.edge))
                .collect::<Vec<_>>();
            let effective =
                EdgeRange::resolve(&enabled.iter().map(|(_, range)| *range).collect::<Vec<_>>());
            let reached = effective.iter().flatten().copied().collect::<Vec<_>>();
            if let Some(display) = display.as_deref() {
                println!("  {display}:");
            }
            for (handle, config, state) in group.iter().map(|(h, c, s)| (*h, c.clone(), s.active)) {
                let host = config
                    .hostname
                    .clone()
                    .unwrap_or_else(|| format!("client {handle}"));
                let claim = enabled
                    .iter()
                    .position(|(other, _)| *other == handle)
                    .and_then(|index| effective[index]);
                let note = if !state {
                    "disabled".to_owned()
                } else {
                    match claim {
                        None => "UNREACHABLE: another client already holds all of it".to_owned(),
                        Some(claim) if claim != config.edge => {
                            format!("narrowed to what is left of {}", config.edge)
                        }
                        Some(_) => "ok".to_owned(),
                    }
                };
                let shown = if state {
                    claim
                        .map(|range| range.to_string())
                        .unwrap_or_else(|| "-".to_owned())
                } else {
                    config.edge.to_string()
                };
                println!("  id {handle}: {host} {shown}  [{note}]");
                if config.peer_edge != EdgeRange::FULL {
                    println!("             lands in its {}", config.peer_edge);
                }
            }
            for gap in EdgeRange::gaps(&reached) {
                println!("  {gap} of the {position} edge is a wall");
            }
        }
    }
}

/// The edge, and the monitor, a client's link is currently attached to.
///
/// `set-edge` changes only the ranges, but the daemon takes the whole link in one
/// request, so the rest of it has to be read first rather than assumed.
async fn client_link(
    rx: &mut AsyncFrontendEventReader,
    tx: &mut AsyncFrontendRequestWriter,
    id: ClientHandle,
) -> Result<(Position, Option<String>), CliError> {
    tx.request(FrontendRequest::Enumerate()).await?;
    loop {
        match rx.next().await.ok_or(CliError::ConnectionClosed)?? {
            FrontendEvent::Error(msg) => return Err(CliError::Service(msg)),
            FrontendEvent::Enumerate(clients) => {
                return clients
                    .iter()
                    .find(|(handle, _, _)| *handle == id)
                    .map(|(_, config, _)| (config.pos, config.display.clone()))
                    .ok_or(CliError::NoSuchClient(id));
            }
            // unrelated notifications may arrive while waiting
            _ => {}
        }
    }
}

/// not exist. `Enumerate` returns the authoritative snapshot, so a missing
/// handle can be reported as an error.
async fn ensure_client_exists(
    rx: &mut AsyncFrontendEventReader,
    tx: &mut AsyncFrontendRequestWriter,
    id: ClientHandle,
) -> Result<(), CliError> {
    tx.request(FrontendRequest::Enumerate()).await?;
    loop {
        match rx.next().await.ok_or(CliError::ConnectionClosed)?? {
            FrontendEvent::Error(msg) => return Err(CliError::Service(msg)),
            FrontendEvent::Enumerate(clients) => {
                if clients.iter().any(|(handle, _, _)| *handle == id) {
                    return Ok(());
                }
                return Err(CliError::NoSuchClient(id));
            }
            // unrelated notifications may arrive while waiting
            _ => {}
        }
    }
}
