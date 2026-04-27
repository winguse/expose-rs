use expose_common::{Frame, TcpWriterCmd, FRAME_CLOSE, FRAME_DATA};
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

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    tunnel_tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    conn_map: Mutex<HashMap<u32, mpsc::Sender<TcpWriterCmd>>>,
    next_id: AtomicU32,
}

impl AppState {
    fn new() -> Self {
        AppState {
            tunnel_tx: Mutex::new(None),
            conn_map: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
        }
    }

    fn next_conn_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Run the expose-rs server on an already-bound listener.
///
/// Accepts connections forever (or until the task is cancelled/aborted).
/// The tunnel client must connect to `ws://<addr>/<secret_token>`.
pub async fn run_server(listener: TcpListener, secret_token: String) {
    let addr = listener
        .local_addr()
        .unwrap_or_else(|_| "?".parse().unwrap());
    let state = Arc::new(AppState::new());
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

// ── Dispatch: tunnel vs proxy ─────────────────────────────────────────────────

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

// ── Tunnel handler ────────────────────────────────────────────────────────────

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
    let (tunnel_tx, mut tunnel_rx) = mpsc::channel::<Vec<u8>>(512);

    {
        let mut guard = state.tunnel_tx.lock().await;
        *guard = Some(tunnel_tx);
        state.conn_map.lock().await.clear();
    }

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

    {
        let mut guard = state.tunnel_tx.lock().await;
        *guard = None;
        state.conn_map.lock().await.clear();
    }

    writer_task.abort();
}

async fn dispatch_from_tunnel(frame: Frame, state: &AppState) {
    match frame.frame_type {
        FRAME_DATA => {
            let map = state.conn_map.lock().await;
            if let Some(tx) = map.get(&frame.conn_id) {
                let _ = tx.send(TcpWriterCmd::Payload(frame.payload)).await;
            } else {
                warn!("DATA for unknown conn_id {}", frame.conn_id);
            }
        }
        FRAME_CLOSE => {
            let map = state.conn_map.lock().await;
            if let Some(tx) = map.get(&frame.conn_id) {
                let _ = tx.send(TcpWriterCmd::ShutdownWrite).await;
            }
        }
        other => {
            warn!("Unexpected frame type {:#04x} from tunnel", other);
        }
    }
}

// ── Proxy handler ─────────────────────────────────────────────────────────────

async fn handle_proxy(stream: TcpStream, state: Arc<AppState>) {
    let tunnel_tx = {
        let guard = state.tunnel_tx.lock().await;
        match guard.as_ref() {
            Some(tx) => tx.clone(),
            None => {
                warn!("No tunnel client connected; dropping incoming connection");
                return;
            }
        }
    };

    let conn_id = state.next_conn_id();

    let (data_tx, data_rx) = mpsc::channel::<TcpWriterCmd>(64);
    state.conn_map.lock().await.insert(conn_id, data_tx);

    if tunnel_tx.send(Frame::open(conn_id).encode()).await.is_err() {
        state.conn_map.lock().await.remove(&conn_id);
        return;
    }

    info!("Proxying connection {} to tunnel", conn_id);

    let (mut tcp_rx, mut tcp_tx) = stream.into_split();
    let tunnel_tx_clone = tunnel_tx.clone();

    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let frame = Frame::data(conn_id, buf[..n].to_vec()).encode();
                    if tunnel_tx_clone.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error!("TCP read error for conn {}: {}", conn_id, e);
                    break;
                }
            }
        }
        let _ = tunnel_tx_clone.send(Frame::close(conn_id).encode()).await;
    });

    let writer = tokio::spawn(async move {
        let mut rx = data_rx;
        let mut write_half_closed = false;
        while let Some(cmd) = rx.recv().await {
            match cmd {
                TcpWriterCmd::Payload(data) => {
                    if tcp_tx.write_all(&data).await.is_err() {
                        break;
                    }
                }
                TcpWriterCmd::ShutdownWrite => {
                    let _ = tcp_tx.shutdown().await;
                    write_half_closed = true;
                    break;
                }
            }
        }
        if !write_half_closed {
            let _ = tcp_tx.shutdown().await;
        }
    });

    let (read_res, write_res) = tokio::join!(reader, writer);
    let _ = (read_res, write_res);

    state.conn_map.lock().await.remove(&conn_id);
    info!("Proxied connection {} closed", conn_id);
}
