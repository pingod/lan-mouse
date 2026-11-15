use clap::{Args, Parser, Subcommand};
use futures::StreamExt;

use std::{net::IpAddr, time::Duration};
use thiserror::Error;

use lan_mouse_ipc::{
    ClientHandle, ConnectionError, FrontendEvent, FrontendRequest, IpcError, Position,
    connect_async,
};

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
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
        }) => {
            tx.request(FrontendRequest::Create).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Created(handle, _, _) = e? {
                    if let Some(hostname) = hostname {
                        tx.request(FrontendRequest::UpdateHostname(handle, Some(hostname)))
                            .await?;
                    }
                    if let Some(port) = port {
                        tx.request(FrontendRequest::UpdatePort(handle, port))
                            .await?;
                    }
                    if let Some(ips) = ips {
                        tx.request(FrontendRequest::UpdateFixIps(handle, ips))
                            .await?;
                    }
                    if let Some(enter_hook) = enter_hook {
                        tx.request(FrontendRequest::UpdateEnterHook(handle, Some(enter_hook)))
                            .await?;
                    }
                    if let Some(leave_hook) = leave_hook {
                        tx.request(FrontendRequest::UpdateLeaveHook(handle, Some(leave_hook)))
                            .await?;
                    }
                    break;
                }
            }
        }
        CliSubcommand::RemoveClient { id } => tx.request(FrontendRequest::Delete(id)).await?,
        CliSubcommand::Activate { id } => tx.request(FrontendRequest::Activate(id, true)).await?,
        CliSubcommand::Deactivate { id } => {
            tx.request(FrontendRequest::Activate(id, false)).await?
        }
        CliSubcommand::List => {
            tx.request(FrontendRequest::Enumerate()).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Enumerate(clients) = e? {
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
                        println!(
                            "id {handle}: {host}:{port} ({pos}) active: {active}, {health}, ips: {ips:?}"
                        );
                    }
                    break;
                }
            }
        }
        CliSubcommand::SetHost { id, host } => {
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?
        }
        CliSubcommand::SetPort { id, port } => {
            tx.request(FrontendRequest::UpdatePort(id, port)).await?
        }
        CliSubcommand::SetPosition { id, pos } => {
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?
        }
        CliSubcommand::SetIps { id, ips } => {
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
