//! Tests for the frontend listener that multiplexes the GUI and the CLI over
//! a single unix socket.

#![cfg(unix)]

use std::{env, fs, path::PathBuf, time::Duration};

use futures::StreamExt;

use lan_mouse_ipc::{AsyncFrontendListener, FrontendEvent, FrontendRequest, default_socket_path};

use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
};

const TIMEOUT: Duration = Duration::from_secs(5);

/// The socket path is derived from `$XDG_RUNTIME_DIR`, which is a process wide
/// variable, so this test binary must not contain more than one test.
fn prepare_socket_dir() -> PathBuf {
    let dir = env::temp_dir().join(format!("lan-mouse-ipc-test-{}", std::process::id()));

    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("could not create the socket directory");

    env::set_var("XDG_RUNTIME_DIR", &dir);

    dir
}

async fn next_request(listener: &mut AsyncFrontendListener) -> FrontendRequest {
    tokio::time::timeout(TIMEOUT, listener.next())
        .await
        .expect("the listener did not report a request in time")
        .expect("the listener stream ended")
        .expect("the listener reported an error")
}

/// Waits for `expected`, skipping the `Sync` requests the listener emits for
/// newly accepted connections (one `Sync` covers every connection that was
/// accepted in the same poll, because the service pushes the whole state to
/// every connection when it handles one).
async fn expect_request(listener: &mut AsyncFrontendListener, expected: &FrontendRequest) {
    loop {
        let request = next_request(listener).await;

        if &request == expected {
            return;
        }

        assert_eq!(request, FrontendRequest::Sync, "unexpected request");
    }
}

async fn send_request<W: AsyncWrite + Unpin>(stream: &mut W, request: &FrontendRequest) {
    let mut line = serde_json::to_string(request).expect("could not serialize the request");
    line.push('\n');

    if let Err(e) = stream.write_all(line.as_bytes()).await {
        panic!("could not send the request: {e}");
    }
}

#[tokio::test]
async fn listener_survives_closed_connections_and_delivers_requests() {
    let dir = prepare_socket_dir();

    let mut listener = AsyncFrontendListener::new()
        .await
        .expect("could not create the listener");

    let socket_path = default_socket_path().expect("could not determine the socket path");

    let mut first = UnixStream::connect(&socket_path)
        .await
        .expect("could not connect the first frontend");

    let second = UnixStream::connect(&socket_path)
        .await
        .expect("could not connect the second frontend");

    /* every new connection is announced with a `Sync` request so the service
     * can push the current state to it.
     */
    assert_eq!(next_request(&mut listener).await, FrontendRequest::Sync);

    // requests of every connection are delivered
    send_request(&mut first, &FrontendRequest::Enumerate()).await;
    expect_request(&mut listener, &FrontendRequest::Enumerate()).await;

    /* broadcasts reach the frontends as one newline terminated line so the
     * frontends can read them with `Lines` (a partial write would glue the
     * next message onto a truncated line).
     */
    let mut second = BufReader::new(second);

    listener
        .broadcast(FrontendEvent::Error("first message".to_owned()))
        .await;

    let mut line = String::new();

    tokio::time::timeout(TIMEOUT, second.read_line(&mut line))
        .await
        .expect("the frontend did not receive the broadcast in time")
        .expect("could not read the broadcast");

    assert_eq!(line, "{\"Error\":\"first message\"}\n");

    send_request(&mut second.get_mut(), &FrontendRequest::SaveConfiguration).await;
    expect_request(&mut listener, &FrontendRequest::SaveConfiguration).await;

    // a connection that went away must not affect the remaining ones
    drop(second);

    send_request(&mut first, &FrontendRequest::ChangePort(4242)).await;
    expect_request(&mut listener, &FrontendRequest::ChangePort(4242)).await;

    /* broadcasting to the closed connection must not panic or take the other
     * connection down with it
     */
    listener
        .broadcast(FrontendEvent::Error("to a closed one".to_owned()))
        .await;

    send_request(&mut first, &FrontendRequest::Create).await;
    expect_request(&mut listener, &FrontendRequest::Create).await;

    drop(first);

    // new connections are still accepted after the old ones are gone
    let mut third = UnixStream::connect(&socket_path)
        .await
        .expect("could not connect the third frontend");

    assert_eq!(next_request(&mut listener).await, FrontendRequest::Sync);

    send_request(&mut third, &FrontendRequest::EnableCapture).await;
    expect_request(&mut listener, &FrontendRequest::EnableCapture).await;

    drop(third);
    drop(listener);

    let _ = fs::remove_dir_all(dir);
}
