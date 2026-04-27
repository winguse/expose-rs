//! End-to-end integration tests for expose-rs.
//!
//! Each test spins up:
//!  1. A simple TCP echo server (the "upstream" / downstream service).
//!  2. An expose-server on a random OS-assigned port.
//!  3. An expose-client connecting the server to the echo upstream.
//!
//! Then it connects raw TCP to the expose-server port and verifies that data
//! flows correctly through the full tunnel.

use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Spawn a simple TCP echo server.  Returns its bound address.
async fn start_echo_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        let (mut rx, mut tx) = stream.into_split();
                        tokio::io::copy(&mut rx, &mut tx).await.ok();
                    });
                }
                Err(_) => break,
            }
        }
    });
    addr
}

/// Spawn expose-server + expose-client pointing at `upstream`.
/// Returns the server's listening address and the two task handles (so the
/// caller can abort them when the test is done).
async fn start_tunnel(
    upstream: std::net::SocketAddr,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let secret = "integration-secret".to_string();

    // Bind the server listener on an OS-assigned port.
    let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();

    let server_task = tokio::spawn(expose_server::run_server(server_listener, secret.clone()));

    // Give the server a moment to enter its accept loop.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ws_url = format!("ws://{}/{}", server_addr, secret);
    let client_task = tokio::spawn(expose_client::run_client_once(ws_url, upstream.to_string()));

    (server_addr, server_task, client_task)
}

/// Poll until a full echo round-trip through the tunnel succeeds, or panic
/// after a timeout.  This ensures the WebSocket handshake and first connection
/// are ready before we start the real test.
async fn wait_until_tunnel_ready(server_addr: std::net::SocketAddr) {
    let probe = b"probe";
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!("Tunnel did not become ready within 5 s");
        }

        if let Ok(Ok(mut stream)) =
            timeout(Duration::from_millis(100), TcpStream::connect(server_addr)).await
        {
            let _ = stream.write_all(probe).await;
            let mut buf = [0u8; 5];
            if timeout(Duration::from_millis(200), stream.read_exact(&mut buf))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false)
                && &buf == probe
            {
                return;
            }
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// A single TCP connection sends a short message and receives the echo back.
#[tokio::test]
async fn test_basic_echo() {
    let upstream = start_echo_server().await;
    let (server_addr, server_task, client_task) = start_tunnel(upstream).await;

    wait_until_tunnel_ready(server_addr).await;

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    let payload = b"hello, expose-rs!";
    stream.write_all(payload).await.unwrap();

    let mut buf = vec![0u8; payload.len()];
    timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
        .await
        .expect("read timed out")
        .expect("read failed");

    assert_eq!(buf, payload);

    server_task.abort();
    client_task.abort();
}

/// Several concurrent connections all flow through the same tunnel simultaneously.
#[tokio::test]
async fn test_concurrent_connections() {
    let upstream = start_echo_server().await;
    let (server_addr, server_task, client_task) = start_tunnel(upstream).await;

    wait_until_tunnel_ready(server_addr).await;

    const N: usize = 10;
    let mut handles = Vec::with_capacity(N);

    for i in 0..N {
        let addr = server_addr;
        handles.push(tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let payload = format!("connection-{i}");
            stream.write_all(payload.as_bytes()).await.unwrap();

            let mut buf = vec![0u8; payload.len()];
            timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
                .await
                .expect("read timed out")
                .expect("read failed");

            assert_eq!(buf, payload.as_bytes());
        }));
    }

    for h in handles {
        h.await.expect("concurrent connection task panicked");
    }

    server_task.abort();
    client_task.abort();
}

/// Sends data larger than a single TCP segment (several hundred kilobytes) to
/// exercise multi-chunk forwarding.
#[tokio::test]
async fn test_large_data() {
    let upstream = start_echo_server().await;
    let (server_addr, server_task, client_task) = start_tunnel(upstream).await;

    wait_until_tunnel_ready(server_addr).await;

    // 512 KiB of data.
    let payload: Vec<u8> = (0u32..).map(|i| (i % 251) as u8).take(512 * 1024).collect();

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    stream.write_all(&payload).await.unwrap();

    // Read back exactly as many bytes as we sent (the echo server mirrors the
    // data) without closing the write side first.
    let mut received = vec![0u8; payload.len()];
    timeout(Duration::from_secs(15), stream.read_exact(&mut received))
        .await
        .expect("read timed out")
        .expect("read failed");

    assert_eq!(received, payload);

    server_task.abort();
    client_task.abort();
}

/// Closing the local write-half (TCP FIN) must not discard data still flowing
/// back from upstream on the read-half.
#[tokio::test]
async fn test_half_close_still_receives_response() {
    let upstream = start_echo_server().await;
    let (server_addr, server_task, client_task) = start_tunnel(upstream).await;

    wait_until_tunnel_ready(server_addr).await;

    let payload = b"half-close-check".to_vec();
    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    stream.write_all(&payload).await.unwrap();
    stream.shutdown().await.unwrap();

    let mut received = vec![0u8; payload.len()];
    timeout(Duration::from_secs(5), stream.read_exact(&mut received))
        .await
        .expect("read timed out")
        .expect("read failed");

    assert_eq!(received, payload);

    server_task.abort();
    client_task.abort();
}

/// After a connection closes (EOF), the tunnel correctly handles a new
/// connection on the same port.
#[tokio::test]
async fn test_sequential_connections() {
    let upstream = start_echo_server().await;
    let (server_addr, server_task, client_task) = start_tunnel(upstream).await;

    wait_until_tunnel_ready(server_addr).await;

    for round in 0..5u32 {
        let mut stream = TcpStream::connect(server_addr).await.unwrap();
        let payload = format!("round-{round}");
        stream.write_all(payload.as_bytes()).await.unwrap();

        let mut buf = vec![0u8; payload.len()];
        timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
            .await
            .unwrap_or_else(|_| panic!("round {round}: read timed out"))
            .unwrap_or_else(|e| panic!("round {round}: read error: {e}"));

        assert_eq!(buf, payload.as_bytes(), "round {round}");
    }

    server_task.abort();
    client_task.abort();
}
