use clap::Parser;
use expose_common::{Frame, FRAME_CLOSE, FRAME_DATA};
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

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(author, version, about = "expose-rs server — protocol-neutral TCP tunnel")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Port to listen on.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Secret token; the tunnel client must connect to ws(s)://<host>:<port>/<secret-token>.
    #[arg(long)]
    secret_token: String,
}

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    /// Channel to send binary frames to the currently connected tunnel client.
    tunnel_tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Map from connection id → channel for writing data back to the TCP client.
    conn_map: Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>,
    /// Monotonically increasing connection id counter.
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

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "expose_server=info".into()),
        )
        .init();

    let args = Args::parse();
    let addr = format!("{}:{}", args.host, args.port);
    let state = Arc::new(AppState::new());

    // The tunnel client announces itself with an HTTP GET for /<secret-token>.
    // We detect this by peeking at the first bytes of each incoming TCP stream.
    let tunnel_prefix = format!("GET /{} ", args.secret_token);

    let listener = TcpListener::bind(&addr).await.expect("Failed to bind");
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
    // Peek at the first bytes to decide whether this is the tunnel client.
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

/// Perform the WebSocket handshake with the tunnel client and multiplex frames.
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

    // Register this tunnel, replacing any previous one.
    {
        let mut guard = state.tunnel_tx.lock().await;
        *guard = Some(tunnel_tx);
        // Drop existing connections — they'll get errors when writing.
        state.conn_map.lock().await.clear();
    }

    // Writer task: drain tunnel_rx → WebSocket binary frames.
    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = tunnel_rx.recv().await {
            if ws_tx.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    // Main loop: receive binary frames from tunnel client and dispatch to connections.
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

    // Unregister tunnel and clear all connections.
    {
        let mut guard = state.tunnel_tx.lock().await;
        *guard = None;
        state.conn_map.lock().await.clear();
    }

    writer_task.abort();
}

/// Process a frame arriving from the tunnel client (DATA or CLOSE).
async fn dispatch_from_tunnel(frame: Frame, state: &AppState) {
    match frame.frame_type {
        FRAME_DATA => {
            let map = state.conn_map.lock().await;
            if let Some(tx) = map.get(&frame.conn_id) {
                let _ = tx.send(frame.payload).await;
            } else {
                warn!("DATA for unknown conn_id {}", frame.conn_id);
            }
        }
        FRAME_CLOSE => {
            state.conn_map.lock().await.remove(&frame.conn_id);
        }
        other => {
            warn!("Unexpected frame type {:#04x} from tunnel", other);
        }
    }
}

// ── Proxy handler ─────────────────────────────────────────────────────────────

/// Forward a raw TCP connection to the tunnel client.
async fn handle_proxy(stream: TcpStream, state: Arc<AppState>) {
    // Grab a snapshot of the tunnel sender (if any).
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

    // Register the connection with a channel for receiving data from tunnel.
    let (data_tx, data_rx) = mpsc::channel::<Vec<u8>>(64);
    state.conn_map.lock().await.insert(conn_id, data_tx);

    // Notify tunnel client that a new connection has been opened.
    if tunnel_tx.send(Frame::open(conn_id).encode()).await.is_err() {
        state.conn_map.lock().await.remove(&conn_id);
        return;
    }

    info!("Proxying connection {} to tunnel", conn_id);

    let (mut tcp_rx, mut tcp_tx) = stream.into_split();
    let tunnel_tx_clone = tunnel_tx.clone();

    // Task: TCP client → tunnel (forward bytes as DATA frames).
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) => break, // EOF
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
        // Signal connection closure to tunnel client.
        let _ = tunnel_tx_clone.send(Frame::close(conn_id).encode()).await;
    });

    // Task: tunnel → TCP client (write DATA from tunnel back to the TCP client).
    let writer = tokio::spawn(async move {
        let mut rx = data_rx;
        while let Some(data) = rx.recv().await {
            if tcp_tx.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = tcp_tx.shutdown().await;
    });

    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }

    state.conn_map.lock().await.remove(&conn_id);
    info!("Proxied connection {} closed", conn_id);
}
