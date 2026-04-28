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

    let server_task = tokio::spawn(expose_server::run_server_with_channel_config(
        server_listener,
        secret.clone(),
        expose_server::CapacityConfig::default(),
    ));

    // Give the server a moment to enter its accept loop.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ws_url = format!("ws://{}/{}", server_addr, secret);
    let client_task = tokio::spawn(expose_client::run_client_once_with_channel_config(
        ws_url,
        upstream.to_string(),
        expose_client::CapacityConfig::default(),
    ));

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

/// Upstream that waits for the tunnel to half-close (FIN), then sends bytes.
/// Reproduces iperf-style flows where results travel after the client stops sending.
async fn start_half_close_upstream() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        let (mut rx, mut tx) = stream.into_split();
                        let mut sink = Vec::new();
                        let _ = rx.read_to_end(&mut sink).await;
                        let _ = tx.write_all(b"after-fin").await;
                        let _ = tx.shutdown().await;
                    });
                }
                Err(_) => break,
            }
        }
    });
    addr
}

/// Half-close from the proxied TCP side: peer sends FIN first; upstream must still
/// be able to deliver data back (iperf reverse / JSON results).
#[tokio::test]
async fn test_half_close_then_upstream_response() {
    let upstream = start_half_close_upstream().await;
    let (server_addr, server_task, client_task) = start_tunnel(upstream).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut saw_response = false;
    while tokio::time::Instant::now() < deadline && !saw_response {
        if let Ok(mut stream) = TcpStream::connect(server_addr).await {
            let _ = stream.shutdown().await;
            let mut buf = [0u8; 32];
            if let Ok(Ok(n)) = timeout(Duration::from_millis(400), stream.read(&mut buf)).await {
                if n > 0 && &buf[..n] == b"after-fin" {
                    saw_response = true;
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(
        saw_response,
        "expected upstream response after half-close (tunnel may not be ready)"
    );

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

// ── Tunnel-reconnect race regression ──────────────────────────────────────────

/// Regression test for the stale-cleanup race.
///
/// Sequence:
///   1. Client A connects → becomes the active tunnel.
///   2. Client B connects → B becomes the active tunnel (A is replaced).
///   3. Client A's task is aborted → A's WebSocket closes.
///   4. Server-side `handle_tunnel` for A detects the WS close and runs its
///      cleanup code.
///
/// BUG (before fix): Cleanup unconditionally set `tunnel_tx = None`, clobbering
/// B's sender and permanently breaking the tunnel for B.
///
/// FIXED: Cleanup compares the session counter stored when the tunnel registered;
/// if a newer tunnel already connected, cleanup does nothing to `tunnel_tx`.
#[tokio::test]
async fn test_tunnel_reconnect_cleanup_race() {
    let upstream = start_echo_server().await;

    let secret = "reconnect-race-secret".to_string();
    let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    let server_task = tokio::spawn(expose_server::run_server_with_channel_config(
        server_listener,
        secret.clone(),
        expose_server::CapacityConfig::default(),
    ));
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ws_url = format!("ws://{}/{}", server_addr, secret);

    // Phase 1: Client A connects and becomes the active tunnel.
    let client_a_task = tokio::spawn(expose_client::run_client_once_with_channel_config(
        ws_url.clone(),
        upstream.to_string(),
        expose_client::CapacityConfig::default(),
    ));
    wait_until_tunnel_ready(server_addr).await;

    // Phase 2: Client B connects, atomically replacing A as the active tunnel.
    let client_b_task = tokio::spawn(expose_client::run_client_once_with_channel_config(
        ws_url.clone(),
        upstream.to_string(),
        expose_client::CapacityConfig::default(),
    ));
    // Give B time to complete the WebSocket handshake with the server.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Phase 3: Abort A's task.  This drops A's WebSocket, causing the server's
    // handle_tunnel for A to see a WS error and run its cleanup code.
    client_a_task.abort();

    // Phase 4: Give the server time to process A's disconnect and run cleanup.
    // If the bug is present the cleanup sets tunnel_tx = None, breaking B's tunnel.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Phase 5: Verify B's tunnel is still operational.
    // wait_until_tunnel_ready panics if the tunnel is broken (regression path).
    wait_until_tunnel_ready(server_addr).await;

    // Extra echo round-trip to be sure.
    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    stream.write_all(b"post-reconnect").await.unwrap();
    let mut buf = vec![0u8; 14];
    timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
        .await
        .expect("read timed out after reconnect")
        .expect("read failed after reconnect");
    assert_eq!(&buf, b"post-reconnect");

    server_task.abort();
    client_b_task.abort();
}

// ── Upstream-refused ──────────────────────────────────────────────────────────

/// When the upstream service is not listening, the client sends CLOSE and the
/// visitor's TCP connection receives a clean EOF (read returns 0).
///
/// The visitor must send at least one byte so the server's `peek()` can classify
/// the connection as a proxy request and route it to `handle_proxy`.
#[tokio::test]
async fn test_upstream_refused() {
    // Bind a listener to get a free port, then drop it so the port is unoccupied.
    let dummy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let refused_addr = dummy.local_addr().unwrap();
    drop(dummy);

    let secret = "refused-secret".to_string();
    let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    let server_task = tokio::spawn(expose_server::run_server_with_channel_config(
        server_listener,
        secret.clone(),
        expose_server::CapacityConfig::default(),
    ));
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ws_url = format!("ws://{}/{}", server_addr, secret);
    let client_task = tokio::spawn(expose_client::run_client_once_with_channel_config(
        ws_url,
        refused_addr.to_string(),
        expose_client::CapacityConfig::default(),
    ));
    // Wait for the WebSocket handshake to complete.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The visitor must write at least one byte so the server's peek() can
    // classify the connection as a proxy request and proceed to handle_proxy.
    // Server sends OPEN → client fails to reach upstream → client sends CLOSE
    // → server shuts down the visitor's write half → read = 0.
    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    stream.write_all(b"x").await.unwrap();
    let mut buf = [0u8; 1];
    let n = timeout(Duration::from_secs(3), stream.read(&mut buf))
        .await
        .expect("read timed out when upstream is refused")
        .expect("read error when upstream is refused");
    assert_eq!(n, 0, "visitor stream should be closed when upstream is refused");

    server_task.abort();
    client_task.abort();
}

// ── Flow-control with a small window ─────────────────────────────────────────

/// Verify that the semaphore-based flow control does not deadlock or lose data
/// when the per-connection window is small (80 messages, just above the ACK
/// batch size of 64 to avoid deadlock while still exercising flow-control waits).
#[tokio::test]
async fn test_flow_control_small_limit() {
    let upstream = start_echo_server().await;

    // The flow-control window must be ≥ ACK_BATCH_SIZE (64) or the sender will
    // exhaust its permits before the receiver ever sends an ACK, causing a deadlock.
    // 80 sits just above that threshold and still forces multiple ACK round-trips.
    let limit = 80;
    let secret = "flow-secret".to_string();
    let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    let server_task = tokio::spawn(expose_server::run_server_with_channel_config(
        server_listener,
        secret.clone(),
        expose_server::CapacityConfig {
            max_pending_messages_per_connection: limit,
        },
    ));
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ws_url = format!("ws://{}/{}", server_addr, secret);
    let client_task = tokio::spawn(expose_client::run_client_once_with_channel_config(
        ws_url,
        upstream.to_string(),
        expose_client::CapacityConfig {
            max_pending_messages_per_connection: limit,
        },
    ));

    wait_until_tunnel_ready(server_addr).await;

    // 256 KiB — enough to exercise many flow-control cycles with this window.
    let payload: Vec<u8> = (0u32..).map(|i| (i % 251) as u8).take(256 * 1024).collect();

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    stream.write_all(&payload).await.unwrap();

    let mut received = vec![0u8; payload.len()];
    timeout(Duration::from_secs(30), stream.read_exact(&mut received))
        .await
        .expect("read timed out with small flow-control window")
        .expect("read failed with small flow-control window");

    assert_eq!(received, payload, "data must be intact under flow control");

    server_task.abort();
    client_task.abort();
}
