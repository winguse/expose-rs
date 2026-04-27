use expose_common::{Frame, FRAME_CLOSE, FRAME_DATA};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Mutex},
    time::{sleep, Duration},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, warn};

// ── App state ─────────────────────────────────────────────────────────────────

pub const DEFAULT_MAX_INFLIGHT_TO_TUNNEL_PER_CONNECTION: usize = 256;
pub const DEFAULT_MAX_INFLIGHT_FROM_TUNNEL_PER_CONNECTION: usize = 256;
const INFLIGHT_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug)]
pub struct CapacityConfig {
    pub max_inflight_to_tunnel_per_connection: usize,
    pub max_inflight_from_tunnel_per_connection: usize,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        CapacityConfig {
            max_inflight_to_tunnel_per_connection: DEFAULT_MAX_INFLIGHT_TO_TUNNEL_PER_CONNECTION,
            max_inflight_from_tunnel_per_connection:
                DEFAULT_MAX_INFLIGHT_FROM_TUNNEL_PER_CONNECTION,
        }
    }
}

struct AppState {
    tunnel_tx: Mutex<Option<mpsc::UnboundedSender<TunnelMsg>>>,
    conn_map: Mutex<HashMap<u32, ConnEntry>>,
    next_id: AtomicU32,
    config: CapacityConfig,
}

struct ConnEntry {
    tx: mpsc::UnboundedSender<ProxyMsg>,
    remote_closed: bool,
    queued_from_tunnel: Arc<AtomicUsize>,
}

enum ProxyMsg {
    Data(Vec<u8>),
    Close,
}

enum TunnelMsg {
    Frame(Vec<u8>),
    Data {
        bytes: Vec<u8>,
        inflight_counter: Arc<AtomicUsize>,
    },
}

fn decrement_counter(counter: &AtomicUsize) {
    let mut current = counter.load(Ordering::Acquire);
    while current > 0 {
        match counter.compare_exchange_weak(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(next) => current = next,
        }
    }
}

async fn wait_for_slot(counter: &AtomicUsize, max_inflight: usize) {
    if max_inflight == 0 {
        return;
    }
    while counter.load(Ordering::Acquire) >= max_inflight {
        sleep(INFLIGHT_WAIT_POLL_INTERVAL).await;
    }
}

impl AppState {
    fn new(config: CapacityConfig) -> Self {
        AppState {
            tunnel_tx: Mutex::new(None),
            conn_map: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            config,
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
    run_server_with_channel_config(listener, secret_token, CapacityConfig::default()).await;
}

/// Same as [`run_server`], but allows tuning per-connection in-flight limits.
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
    let (tunnel_tx, mut tunnel_rx) = mpsc::unbounded_channel::<TunnelMsg>();

    {
        let mut guard = state.tunnel_tx.lock().await;
        *guard = Some(tunnel_tx);
        state.conn_map.lock().await.clear();
    }

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = tunnel_rx.recv().await {
            let (bytes, inflight_counter) = match msg {
                TunnelMsg::Frame(bytes) => (bytes, None),
                TunnelMsg::Data {
                    bytes,
                    inflight_counter,
                } => (bytes, Some(inflight_counter)),
            };

            if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                if let Some(counter) = inflight_counter {
                    decrement_counter(&counter);
                }
                break;
            }

            if let Some(counter) = inflight_counter {
                decrement_counter(&counter);
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
            let limit = state.config.max_inflight_from_tunnel_per_connection;
            let decision = {
                let mut map = state.conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(entry) if entry.remote_closed => DispatchDataDecision::Drop,
                    Some(entry)
                        if limit > 0
                            && entry.queued_from_tunnel.load(Ordering::Acquire) >= limit =>
                    {
                        entry.remote_closed = true;
                        DispatchDataDecision::Overflow {
                            tx: entry.tx.clone(),
                        }
                    }
                    Some(entry) => {
                        entry.queued_from_tunnel.fetch_add(1, Ordering::AcqRel);
                        DispatchDataDecision::Forward {
                            tx: entry.tx.clone(),
                            queued_from_tunnel: Arc::clone(&entry.queued_from_tunnel),
                        }
                    }
                    None => DispatchDataDecision::Unknown,
                }
            };

            match decision {
                DispatchDataDecision::Forward {
                    tx,
                    queued_from_tunnel,
                } => {
                    if tx.send(ProxyMsg::Data(frame.payload)).is_err() {
                        decrement_counter(&queued_from_tunnel);
                    }
                }
                DispatchDataDecision::Overflow { tx } => {
                    warn!(
                        "conn {} exceeded max in-flight from tunnel limit; closing connection",
                        frame.conn_id
                    );
                    let _ = tx.send(ProxyMsg::Close);
                    if let Some(tunnel_tx) = state.tunnel_tx.lock().await.as_ref().cloned() {
                        let _ =
                            tunnel_tx.send(TunnelMsg::Frame(Frame::close(frame.conn_id).encode()));
                    }
                }
                DispatchDataDecision::Drop => {}
                DispatchDataDecision::Unknown => {
                    warn!("DATA for unknown conn_id {}", frame.conn_id);
                }
            }
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

enum DispatchDataDecision {
    Forward {
        tx: mpsc::UnboundedSender<ProxyMsg>,
        queued_from_tunnel: Arc<AtomicUsize>,
    },
    Overflow {
        tx: mpsc::UnboundedSender<ProxyMsg>,
    },
    Drop,
    Unknown,
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
    let queued_from_tunnel = Arc::new(AtomicUsize::new(0));
    let (data_tx, data_rx) = mpsc::unbounded_channel::<ProxyMsg>();
    state.conn_map.lock().await.insert(
        conn_id,
        ConnEntry {
            tx: data_tx,
            remote_closed: false,
            queued_from_tunnel: Arc::clone(&queued_from_tunnel),
        },
    );

    if tunnel_tx
        .send(TunnelMsg::Frame(Frame::open(conn_id).encode()))
        .is_err()
    {
        state.conn_map.lock().await.remove(&conn_id);
        return;
    }

    info!("Proxying connection {} to tunnel", conn_id);

    let (mut tcp_rx, mut tcp_tx) = stream.into_split();
    let tunnel_tx_clone = tunnel_tx.clone();
    let to_tunnel_inflight = Arc::new(AtomicUsize::new(0));
    let to_tunnel_inflight_reader = Arc::clone(&to_tunnel_inflight);
    let max_to_tunnel = state.config.max_inflight_to_tunnel_per_connection;

    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            wait_for_slot(&to_tunnel_inflight_reader, max_to_tunnel).await;

            match tcp_rx.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    to_tunnel_inflight_reader.fetch_add(1, Ordering::AcqRel);
                    let msg = TunnelMsg::Data {
                        bytes: Frame::data(conn_id, buf[..n].to_vec()).encode(),
                        inflight_counter: Arc::clone(&to_tunnel_inflight_reader),
                    };
                    if tunnel_tx_clone.send(msg).is_err() {
                        decrement_counter(&to_tunnel_inflight_reader);
                        break;
                    }
                }
                Err(e) => {
                    error!("TCP read error for conn {}: {}", conn_id, e);
                    break;
                }
            }
        }
        let _ = tunnel_tx_clone.send(TunnelMsg::Frame(Frame::close(conn_id).encode()));
    });

    let queued_from_tunnel_writer = Arc::clone(&queued_from_tunnel);
    let writer = tokio::spawn(async move {
        let mut rx = data_rx;
        while let Some(msg) = rx.recv().await {
            match msg {
                ProxyMsg::Data(data) => {
                    decrement_counter(&queued_from_tunnel_writer);
                    if tcp_tx.write_all(&data).await.is_err() {
                        break;
                    }
                }
                ProxyMsg::Close => break,
            }
        }
        let _ = tcp_tx.shutdown().await;
    });

    let _ = tokio::join!(reader, writer);

    state.conn_map.lock().await.remove(&conn_id);
    info!("Proxied connection {} closed", conn_id);
}
