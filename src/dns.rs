use std::{collections::HashMap, io, net::IpAddr, time::Duration};

use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::net::lookup_host;
use tokio::task::{JoinHandle, spawn_local};

use tokio_util::sync::CancellationToken;

use lan_mouse_ipc::ClientHandle;

pub(crate) struct DnsResolver {
    cancellation_token: CancellationToken,
    task: Option<JoinHandle<()>>,
    request_tx: Sender<DnsRequest>,
    event_rx: Receiver<DnsEvent>,
}

struct DnsRequest {
    handle: ClientHandle,
    hostname: String,
}

pub(crate) enum DnsEvent {
    Resolving(ClientHandle),
    Resolved(ClientHandle, String, io::Result<Vec<IpAddr>>),
}

struct DnsTask {
    request_rx: Receiver<DnsRequest>,
    event_tx: Sender<DnsEvent>,
    cancellation_token: CancellationToken,
    active_tasks: HashMap<ClientHandle, JoinHandle<()>>,
}

impl DnsResolver {
    pub(crate) fn new() -> io::Result<Self> {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let dns_task = DnsTask {
            active_tasks: Default::default(),
            request_rx,
            event_tx,
            cancellation_token: cancellation_token.clone(),
        };
        let task = Some(spawn_local(dns_task.run()));
        Ok(Self {
            cancellation_token,
            task,
            event_rx,
            request_tx,
        })
    }

    pub(crate) fn resolve(&self, handle: ClientHandle, hostname: String) {
        let request = DnsRequest { handle, hostname };
        /* the task only goes away during shutdown and this binary is built
         * with `panic = "abort"`, so a failed send must not panic
         */
        if self.request_tx.send(request).is_err() {
            log::debug!("dns task is gone, dropping the resolve request");
        }
    }

    /// Wait for the next event; `None` means the resolver task has exited.
    pub(crate) async fn event(&mut self) -> Option<DnsEvent> {
        self.event_rx.recv().await
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        if let Some(task) = self.task.take() {
            if let Err(e) = task.await {
                log::warn!("dns resolver task failed: {e}");
            }
        }
    }
}

impl DnsTask {
    async fn run(mut self) {
        let cancellation_token = self.cancellation_token.clone();
        tokio::select! {
            _ = self.do_dns() => {},
            _ = cancellation_token.cancelled() => {},
        }
    }

    async fn do_dns(&mut self) {
        while let Some(dns_request) = self.request_rx.recv().await {
            let DnsRequest { handle, hostname } = dns_request;

            /* abort previous dns task */
            let previous_task = self.active_tasks.remove(&handle);
            if let Some(task) = previous_task {
                if !task.is_finished() {
                    task.abort();
                }
            }

            if self.event_tx.send(DnsEvent::Resolving(handle)).is_err() {
                // the receiver is gone, so nobody is waiting for results
                return;
            }

            /* spawn task for dns request */
            let event_tx = self.event_tx.clone();
            let cancellation_token = self.cancellation_token.clone();

            let task = tokio::task::spawn_local(async move {
                tokio::select! {
                    result = resolve_hostname(&hostname) => {
                        // A failed lookup is usually transient: mDNS/Avahi may
                        // not have announced the peer yet, or the machine is
                        // still booting. Retry with backoff instead of leaving
                        // the client permanently unreachable (it used to stay
                        // dead until the user re-saved the config).
                        let mut attempt = 0u32;
                        let mut result = result;
                        while result.is_err() {
                            let delay = retry_delay(attempt);
                            log::debug!(
                                "retrying dns lookup for {hostname} in {delay:?} (attempt {})",
                                attempt + 1
                            );
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                _ = cancellation_token.cancelled() => return,
                            }
                            result = resolve_hostname(&hostname).await;
                            attempt += 1;
                        }
                        if event_tx
                            .send(DnsEvent::Resolved(handle, hostname, result))
                            .is_err()
                        {
                            log::debug!("dns event receiver is gone");
                        }
                    }
                    _ = cancellation_token.cancelled() => {},
                }
            });
            self.active_tasks.insert(handle, task);
        }
    }
}

/// Backoff between DNS retries: 1s, 2s, 4s, then every 8s.
fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(3))
}

/// Resolve `hostname` via the operating system's full name-resolution
/// stack (`getaddrinfo` on Unix, GetAddrInfoEx on Windows). This walks
/// `/etc/nsswitch.conf` on Linux — picking up mDNS via Avahi, /etc/hosts,
/// and DNS — and uses Bonjour for `.local` names on macOS. Pure-DNS
/// resolvers like hickory miss all of those, which is why a Bonjour
/// hostname (e.g. `JKMBP-M4-Max.local`) wouldn't resolve before.
///
/// Port `0` is a placeholder — `lookup_host` requires `host:port` but we
/// only care about the IPs at this stage; the actual port is appended at
/// connection time.
async fn resolve_hostname(hostname: &str) -> io::Result<Vec<IpAddr>> {
    let addrs = lookup_host((hostname, 0)).await?;
    Ok(addrs.map(|sa| sa.ip()).collect())
}
