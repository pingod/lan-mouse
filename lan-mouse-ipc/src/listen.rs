use futures::{Future, Stream, StreamExt};
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    io::ErrorKind,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio_stream::wrappers::LinesStream;

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::TcpListener;
#[cfg(windows)]
use tokio::net::TcpStream;

use crate::{FrontendEvent, FrontendRequest, IpcError, IpcListenerCreationError};

pub struct AsyncFrontendListener {
    #[cfg(windows)]
    listener: TcpListener,
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
    /* Both vectors are index aligned: `tx_streams[i]` writes to the
     * connection whose requests `line_streams[i]` reads. They must stay
     * aligned, so the writer of a broken connection is only marked as `None`
     * here and removed together with its reader in `poll_next`.
     */
    #[cfg(unix)]
    line_streams: Vec<LinesStream<BufReader<ReadHalf<UnixStream>>>>,
    #[cfg(windows)]
    line_streams: Vec<LinesStream<BufReader<ReadHalf<TcpStream>>>>,
    #[cfg(unix)]
    tx_streams: Vec<Option<WriteHalf<UnixStream>>>,
    #[cfg(windows)]
    tx_streams: Vec<Option<WriteHalf<TcpStream>>>,
    /* Backoff after an accept error (e.g. EMFILE): without it a persistent
     * error would spin the run loop at 100% CPU.
     */
    accept_retry: Option<Pin<Box<tokio::time::Sleep>>>,
    /* stream that is polled first, so a busy connection cannot starve others */
    next_stream: usize,
}

/// Delay before retrying `accept` after an error.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

impl AsyncFrontendListener {
    pub async fn new() -> Result<Self, IpcListenerCreationError> {
        #[cfg(unix)]
        let (socket_path, listener) = {
            let socket_path = crate::default_socket_path()?;

            /* The socket lives below $XDG_RUNTIME_DIR (or ~/Library/Caches on
             * macOS). Neither is guaranteed to exist, and binding a socket in
             * a missing directory fails, so create it first.
             */
            if let Some(parent) = socket_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    log::warn!("could not create the socket directory {parent:?}: {e}");
                }
            }

            if socket_path.exists() {
                // try to connect to see if some other instance
                // of lan-mouse is already running
                match UnixStream::connect(&socket_path).await {
                    // connected -> lan-mouse is already running
                    Ok(_) => return Err(IpcListenerCreationError::AlreadyRunning),
                    /* The connection was refused (nobody is listening) or the
                     * socket file is stale, so it can safely be removed. Any
                     * other error (e.g. EACCES, or EAGAIN because the listen
                     * backlog of a running instance is full) must not delete
                     * the socket of a running daemon.
                     */
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::ConnectionRefused | ErrorKind::NotFound
                        ) =>
                    {
                        log::debug!("{socket_path:?}: {e} - removing left behind socket");
                        let _ = std::fs::remove_file(&socket_path);
                    }
                    Err(e) => {
                        log::error!("could not connect to the socket at {socket_path:?}: {e}");
                        return Err(IpcListenerCreationError::Bind(e));
                    }
                }
            }
            let listener = match UnixListener::bind(&socket_path) {
                Ok(ls) => ls,
                // some other lan-mouse instance has bound the socket in the meantime
                Err(e) if e.kind() == ErrorKind::AddrInUse => {
                    return Err(IpcListenerCreationError::AlreadyRunning);
                }
                Err(e) => return Err(IpcListenerCreationError::Bind(e)),
            };
            (socket_path, listener)
        };

        #[cfg(windows)]
        let listener = match TcpListener::bind("127.0.0.1:5252").await {
            Ok(ls) => ls,
            // some other lan-mouse instance has bound the socket in the meantime
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                return Err(IpcListenerCreationError::AlreadyRunning);
            }
            Err(e) => return Err(IpcListenerCreationError::Bind(e)),
        };

        let adapter = Self {
            listener,
            #[cfg(unix)]
            socket_path,
            line_streams: vec![],
            tx_streams: vec![],
            accept_retry: None,
            next_stream: 0,
        };

        Ok(adapter)
    }

    pub async fn broadcast(&mut self, notify: FrontendEvent) {
        // encode event
        let mut json = serde_json::to_string(&notify).unwrap();
        json.push('\n');

        // TODO do simultaneously
        for slot in self.tx_streams.iter_mut() {
            let Some(tx) = slot.as_mut() else {
                // this connection already failed
                continue;
            };
            /* `write` may write only part of the buffer, which would corrupt
             * the newline delimited stream for the frontend (it reads lines
             * and would glue the next event onto a truncated one).
             */
            if tx.write_all(json.as_bytes()).await.is_err() {
                // keep the vectors aligned, `poll_next` removes the pair
                *slot = None;
            }
        }
    }
}

#[cfg(unix)]
impl Drop for AsyncFrontendListener {
    fn drop(&mut self) {
        log::debug!("remove socket: {:?}", self.socket_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Stream for AsyncFrontendListener {
    type Item = Result<FrontendRequest, IpcError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // wait out the backoff before trying to accept again
        if let Some(retry) = self.accept_retry.as_mut() {
            if retry.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.accept_retry = None;
        }

        /* Poll the connections one by one so a connection that failed can be
         * dropped instead of being polled (and failing) again on every poll.
         * The scan starts at the connection after the one that produced the
         * last request, so a busy connection cannot starve the others.
         */
        let mut to_drop: Vec<usize> = Vec::new();
        let mut request: Option<Result<FrontendRequest, IpcError>> = None;
        let len = self.line_streams.len();
        if len > 0 {
            let start = self.next_stream % len;
            for k in 0..len {
                let i = (start + k) % len;
                match self.line_streams[i].poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(line))) => {
                        request = Some(
                            serde_json::from_str::<FrontendRequest>(line.as_str())
                                .map_err(IpcError::from),
                        );
                        self.next_stream = i + 1;
                        break;
                    }
                    Poll::Ready(Some(Err(e))) => {
                        log::debug!("dropping the frontend connection: {e}");
                        to_drop.push(i);
                    }
                    Poll::Ready(None) => {
                        log::debug!("frontend connection closed");
                        to_drop.push(i);
                    }
                    Poll::Pending => {}
                }
            }
        }
        // drop the failed connections; the vector stays index aligned
        to_drop.sort_unstable();
        for i in to_drop.into_iter().rev() {
            self.line_streams.remove(i);
            self.tx_streams.remove(i);
            self.next_stream = self.next_stream.saturating_sub(1);
        }
        if self.line_streams.is_empty() {
            self.next_stream = 0;
        }
        if let Some(request) = request {
            return Poll::Ready(Some(request));
        }

        let mut sync = false;
        loop {
            match self.listener.poll_accept(cx) {
                Poll::Ready(Ok((stream, _))) => {
                    let (rx, tx) = tokio::io::split(stream);
                    let buf_reader = BufReader::new(rx);
                    let lines = buf_reader.lines();
                    let lines = LinesStream::new(lines);
                    self.line_streams.push(lines);
                    self.tx_streams.push(Some(tx));
                    sync = true;
                }
                /* An accept error (e.g. EMFILE) used to end the loop and
                 * return Pending without registering a waker, so the frontend
                 * listener never woke up again and the daemon stopped
                 * accepting connections. Retry after a short delay instead, so
                 * a persistent error does not spin the run loop. The sleep is
                 * polled once here, because it only registers its waker when it
                 * is polled, and `break` (instead of `return`) still reports a
                 * connection that was accepted earlier in this poll.
                 */
                Poll::Ready(Err(e)) => {
                    log::warn!("failed to accept a frontend connection: {e}");
                    let mut retry = Box::pin(tokio::time::sleep(ACCEPT_RETRY_DELAY));
                    let _ = retry.as_mut().poll(cx);
                    self.accept_retry = Some(retry);
                    break;
                }
                Poll::Pending => break,
            }
        }
        if sync {
            Poll::Ready(Some(Ok(FrontendRequest::Sync)))
        } else {
            Poll::Pending
        }
    }
}
