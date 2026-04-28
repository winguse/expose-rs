use expose_common::{
    acquire_permit, apply_flow_ack, semaphore_for_limit, Frame, ACK_BATCH_SIZE, FRAME_ACK,
    FRAME_CLOSE, FRAME_DATA,
};
pub use expose_common::{CapacityConfig, DEFAULT_MAX_PENDING_MESSAGES_PER_CONNECTION};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Mutex},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, warn};

// ── Tunnel state ─────────────────────────────────────────────────────────────

/// Holds the active tunnel sender together with a monotone session counter.
///
/// The session counter is incremented each time a new tunnel WebSocket connects.
/// Cleanup code checks that the session hasn't advanced before clearing the
/// sender, preventing a race where stale cleanup from an old tunnel clobbers
/// the newly-registered sender of a replacement tunnel.
struct TunnelState {
    session: u64,
    tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

struct AppState {
    tunnel: Mutex<TunnelState>,
    conn_map: Mutex<HashMap<u32, ConnEntry>>,
    next_id: AtomicU32,
    config: CapacityConfig,
}

struct ConnEntry {
    tx: mpsc::UnboundedSender<ProxyMsg>,
    remote_closed: bool,
    /// Visitor → tunnel: credits until the client ACKs after writing to upstream.
    inflight_to_tunnel: Option<Arc<tokio::sync::Semaphore>>,
}

enum ProxyMsg {
    Data(Vec<u8>),
    Close,
}

impl AppState {
    fn new(config: CapacityConfig) -> Self {
        AppState {
            tunnel: Mutex::new(TunnelState {
                session: 0,
                tx: None,
            }),
            conn_map: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            config,
        }
    }

    fn next_conn_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Run the expose-rs server on an already-bound listener.
pub async fn run_server(listener: TcpListener, secret_token: String) {
    run_server_with_channel_config(listener, secret_token, CapacityConfig::default()).await;
}

pub async fn run_server_with_channel_config(
    listener: TcpListener,
    secret_token: String,
    config: CapacityConfig,
) {
    let addr = listener
        .local_addr()
        .unwrap_or_else(|_| "?".parse().unwrap());
    let state = Arc::new(AppState::new(config));
    let tunnel_prefix = format!("GET /{} ", secret_token);

    info!("expose-server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                info!("New connection from {}", peer_addr);
                let state = Arc::clone(&state);
                let tunnel_prefix = tunnel_prefix.clone();
                tokio::spawn(async move {
                    handle_connection(stream, state, &tunnel_prefix).await;
                });
            }
            Err(e) => error!("Accept error: {}", e),
        }
    }
}

async fn handle_connection(stream: TcpStream, state: Arc<AppState>, tunnel_prefix: &str) {
    let mut buf = [0u8; 512];
    let n = match stream.peek(&mut buf).await {
        Ok(n) => n,
        Err(e) => {
            error!("Peek error: {}", e);
            return;
        }
    };

    let is_tunnel = std::str::from_utf8(&buf[..n])
        .map(|s| s.starts_with(tunnel_prefix))
        .unwrap_or(false);

    if is_tunnel {
        handle_tunnel(stream, state).await;
    } else {
        handle_proxy(stream, state).await;
    }
}

async fn handle_tunnel(stream: TcpStream, state: Arc<AppState>) {
    info!("Tunnel client connecting via WebSocket handshake");

    let ws = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket handshake failed: {}", e);
            return;
        }
    };

    info!("Tunnel client connected");

    let (mut ws_tx, mut ws_rx) = ws.split();
    let (tunnel_tx, mut tunnel_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Register this tunnel and record our session ID.
    // Incrementing the session counter atomically (under the lock) lets cleanup
    // distinguish itself from a newer tunnel that may have registered in the meantime.
    //
    // The counter is u64 and increments once per tunnel connection.  Overflow would
    // require ~1.8 × 10¹⁹ reconnections — practically impossible — but note that if
    // it ever did wrap around exactly to a previous value the stale-cleanup guard
    // would fail to protect that specific session.  The wrapping_add is intentional
    // to avoid a panic in debug builds.
    let my_session = {
        let mut guard = state.tunnel.lock().await;
        guard.session = guard.session.wrapping_add(1);
        let session = guard.session;
        guard.tx = Some(tunnel_tx);
        session
        // guard is dropped here — conn_map is cleared without holding the tunnel lock
    };
    state.conn_map.lock().await.clear();

    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = tunnel_rx.recv().await {
            if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    while let Some(msg_result) = ws_rx.next().await {
        match msg_result {
            Ok(Message::Binary(bytes)) => match Frame::decode(&bytes) {
                Ok(frame) => dispatch_from_tunnel(frame, &state).await,
                Err(e) => warn!("Bad tunnel frame: {}", e),
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                error!("Tunnel WebSocket error: {}", e);
                break;
            }
        }
    }

    info!("Tunnel client disconnected");

    // Only clear tunnel state if no newer tunnel has registered since us.
    // Without this check a stale cleanup from an old tunnel would set
    // tunnel_tx = None after a replacement tunnel has already written its sender
    // into the slot, permanently breaking the new tunnel.
    let was_active = {
        let mut guard = state.tunnel.lock().await;
        if guard.session == my_session {
            guard.tx = None;
            true
        } else {
            false
        }
    };
    if was_active {
        state.conn_map.lock().await.clear();
    }

    writer_task.abort();
}

async fn dispatch_from_tunnel(frame: Frame, state: &AppState) {
    match frame.frame_type {
        FRAME_DATA => {
            let tx = {
                let mut map = state.conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(entry) if !entry.remote_closed => Some(entry.tx.clone()),
                    _ => None,
                }
            };

            if let Some(tx) = tx {
                let _ = tx.send(ProxyMsg::Data(frame.payload));
            } else {
                warn!("DATA for unknown conn_id {}", frame.conn_id);
            }
        }
        FRAME_ACK => {
            let ack_count = match frame.ack_count() {
                Ok(count) => count,
                Err(e) => {
                    warn!("Bad ACK frame for conn {}: {}", frame.conn_id, e);
                    return;
                }
            };
            let sem = {
                let map = state.conn_map.lock().await;
                map.get(&frame.conn_id)
                    .and_then(|e| e.inflight_to_tunnel.clone())
            };
            apply_flow_ack(sem.as_deref(), ack_count);
        }
        FRAME_CLOSE => {
            let tx = {
                let mut map = state.conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(entry) if !entry.remote_closed => {
                        entry.remote_closed = true;
                        Some(entry.tx.clone())
                    }
                    _ => None,
                }
            };
            if let Some(tx) = tx {
                let _ = tx.send(ProxyMsg::Close);
            }
        }
        other => {
            warn!("Unexpected frame type {:#04x} from tunnel", other);
        }
    }
}

async fn handle_proxy(stream: TcpStream, state: Arc<AppState>) {
    let tunnel_tx = {
        let guard = state.tunnel.lock().await;
        match guard.tx.as_ref() {
            Some(tx) => tx.clone(),
            None => {
                warn!("No tunnel client connected; dropping incoming connection");
                return;
            }
        }
    };

    let conn_id = state.next_conn_id();
    let inflight_to_tunnel = semaphore_for_limit(state.config.max_pending_messages_per_connection);
    let (data_tx, data_rx) = mpsc::unbounded_channel::<ProxyMsg>();
    state.conn_map.lock().await.insert(
        conn_id,
        ConnEntry {
            tx: data_tx,
            remote_closed: false,
            inflight_to_tunnel: inflight_to_tunnel.clone(),
        },
    );

    if tunnel_tx.send(Frame::open(conn_id).encode()).is_err() {
        state.conn_map.lock().await.remove(&conn_id);
        return;
    }

    info!("Proxying connection {} to tunnel", conn_id);

    let (mut tcp_rx, mut tcp_tx) = stream.into_split();
    let tunnel_tx_clone = tunnel_tx.clone();

    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let permit = acquire_permit(&inflight_to_tunnel).await;
            match tcp_rx.read(&mut buf).await {
                Ok(0) => {
                    drop(permit);
                    break;
                }
                Ok(n) => {
                    let frame = Frame::data(conn_id, buf[..n].to_vec()).encode();
                    // Forget the permit only after the send succeeds so credits are
                    // not permanently lost when the channel has already closed.
                    if tunnel_tx_clone.send(frame).is_err() {
                        drop(permit);
                        break;
                    }
                    if let Some(p) = permit {
                        p.forget();
                    }
                }
                Err(e) => {
                    drop(permit);
                    error!("TCP read error for conn {}: {}", conn_id, e);
                    break;
                }
            }
        }
        let _ = tunnel_tx_clone.send(Frame::close(conn_id).encode());
    });

    let writer = tokio::spawn(async move {
        let mut rx = data_rx;
        let mut ack_batch: u32 = 0;
        while let Some(msg) = rx.recv().await {
            match msg {
                ProxyMsg::Data(payload) => {
                    if tcp_tx.write_all(&payload).await.is_err() {
                        break;
                    }
                    ack_batch = ack_batch.saturating_add(1);
                    if ack_batch >= ACK_BATCH_SIZE {
                        let _ = tunnel_tx.send(Frame::ack(conn_id, ack_batch).encode());
                        ack_batch = 0;
                    }
                }
                ProxyMsg::Close => break,
            }
        }
        if ack_batch > 0 {
            let _ = tunnel_tx.send(Frame::ack(conn_id, ack_batch).encode());
        }
        let _ = tcp_tx.shutdown().await;
    });

    let _ = tokio::join!(reader, writer);

    state.conn_map.lock().await.remove(&conn_id);
    info!("Proxied connection {} closed", conn_id);
}
