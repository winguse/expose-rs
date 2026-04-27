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
    sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, warn};

pub const DEFAULT_MAX_INFLIGHT_TO_TUNNEL_PER_CONNECTION: usize = 256;
pub const DEFAULT_MAX_INFLIGHT_FROM_TUNNEL_PER_CONNECTION: usize = 256;

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
    inflight_from_tunnel: Option<Arc<Semaphore>>,
}

enum ProxyMsg {
    Data {
        payload: Vec<u8>,
        permit: Option<OwnedSemaphorePermit>,
    },
    Close,
}

enum TunnelMsg {
    Frame(Vec<u8>),
    Data {
        bytes: Vec<u8>,
        permit: Option<OwnedSemaphorePermit>,
    },
}

fn semaphore_for_limit(limit: usize) -> Option<Arc<Semaphore>> {
    if limit == 0 {
        None
    } else {
        Some(Arc::new(Semaphore::new(limit)))
    }
}

async fn acquire_permit(limit: &Option<Arc<Semaphore>>) -> Option<OwnedSemaphorePermit> {
    match limit {
        Some(sem) => sem.clone().acquire_owned().await.ok(),
        None => None,
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
    let (tunnel_tx, mut tunnel_rx) = mpsc::unbounded_channel::<TunnelMsg>();

    {
        let mut guard = state.tunnel_tx.lock().await;
        *guard = Some(tunnel_tx);
        state.conn_map.lock().await.clear();
    }

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = tunnel_rx.recv().await {
            match msg {
                TunnelMsg::Frame(bytes) => {
                    if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                TunnelMsg::Data { bytes, permit } => {
                    let send_result = ws_tx.send(Message::Binary(bytes)).await;
                    drop(permit);
                    if send_result.is_err() {
                        break;
                    }
                }
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
            let conn = {
                let mut map = state.conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(entry) if !entry.remote_closed => {
                        Some((entry.tx.clone(), entry.inflight_from_tunnel.clone()))
                    }
                    _ => None,
                }
            };

            let Some((tx, limit)) = conn else {
                warn!("DATA for unknown conn_id {}", frame.conn_id);
                return;
            };

            let permit = acquire_permit(&limit).await;
            let _ = tx.send(ProxyMsg::Data {
                payload: frame.payload,
                permit,
            });
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
    let inflight_from_tunnel =
        semaphore_for_limit(state.config.max_inflight_from_tunnel_per_connection);
    let (data_tx, data_rx) = mpsc::unbounded_channel::<ProxyMsg>();
    state.conn_map.lock().await.insert(
        conn_id,
        ConnEntry {
            tx: data_tx,
            remote_closed: false,
            inflight_from_tunnel,
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
    let inflight_to_tunnel =
        semaphore_for_limit(state.config.max_inflight_to_tunnel_per_connection);

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
                    let msg = TunnelMsg::Data {
                        bytes: Frame::data(conn_id, buf[..n].to_vec()).encode(),
                        permit,
                    };
                    if tunnel_tx_clone.send(msg).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    drop(permit);
                    error!("TCP read error for conn {}: {}", conn_id, e);
                    break;
                }
            }
        }
        let _ = tunnel_tx_clone.send(TunnelMsg::Frame(Frame::close(conn_id).encode()));
    });

    let writer = tokio::spawn(async move {
        let mut rx = data_rx;
        while let Some(msg) = rx.recv().await {
            match msg {
                ProxyMsg::Data { payload, permit } => {
                    let write_result = tcp_tx.write_all(&payload).await;
                    drop(permit);
                    if write_result.is_err() {
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
