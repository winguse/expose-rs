use expose_common::{Frame, FRAME_ACK, FRAME_CLOSE, FRAME_DATA};
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
    sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, warn};

pub const DEFAULT_MAX_UNACKED_TO_TUNNEL_FRAMES_PER_CONNECTION: usize = 256;
const ACK_BATCH_SIZE: u32 = 64;

#[derive(Clone, Copy, Debug)]
pub struct CapacityConfig {
    pub max_unacked_to_tunnel_frames_per_connection: usize,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        CapacityConfig {
            max_unacked_to_tunnel_frames_per_connection:
                DEFAULT_MAX_UNACKED_TO_TUNNEL_FRAMES_PER_CONNECTION,
        }
    }
}

struct AppState {
    tunnel_tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    conn_map: Mutex<HashMap<u32, ConnEntry>>,
    next_id: AtomicU32,
    config: CapacityConfig,
}

struct ConnEntry {
    tx: mpsc::UnboundedSender<ProxyMsg>,
    remote_closed: bool,
    unacked_to_tunnel: Option<Arc<Semaphore>>,
    pending_to_tunnel: Arc<AtomicUsize>,
}

enum ProxyMsg {
    Data(Vec<u8>),
    Close,
}

fn semaphore_for_limit(limit: usize) -> Option<Arc<Semaphore>> {
    if limit == 0 {
        None
    } else {
        Some(Arc::new(Semaphore::new(limit)))
    }
}

async fn acquire_credit(limit: &Option<Arc<Semaphore>>) -> Option<OwnedSemaphorePermit> {
    match limit {
        Some(sem) => sem.clone().acquire_owned().await.ok(),
        None => None,
    }
}

fn release_acked_credits(pending: &AtomicUsize, sem: Option<&Semaphore>, ack_count: u32) {
    if ack_count == 0 {
        return;
    }
    let mut current = pending.load(Ordering::Acquire);
    let mut released = 0usize;
    let requested = ack_count as usize;
    loop {
        if current == 0 {
            break;
        }
        let to_release = current.min(requested);
        match pending.compare_exchange_weak(
            current,
            current - to_release,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                released = to_release;
                break;
            }
            Err(next) => current = next,
        }
    }
    if released > 0 {
        if let Some(sem) = sem {
            sem.add_permits(released);
        }
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
    let (tunnel_tx, mut tunnel_rx) = mpsc::unbounded_channel::<Vec<u8>>();

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
            release_connection_credits(state, frame.conn_id, ack_count).await;
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
    let unacked_to_tunnel =
        semaphore_for_limit(state.config.max_unacked_to_tunnel_frames_per_connection);
    let pending_to_tunnel = Arc::new(AtomicUsize::new(0));
    let (data_tx, data_rx) = mpsc::unbounded_channel::<ProxyMsg>();
    state.conn_map.lock().await.insert(
        conn_id,
        ConnEntry {
            tx: data_tx,
            remote_closed: false,
            unacked_to_tunnel: unacked_to_tunnel.clone(),
            pending_to_tunnel: Arc::clone(&pending_to_tunnel),
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
            let permit = acquire_credit(&unacked_to_tunnel).await;
            match tcp_rx.read(&mut buf).await {
                Ok(0) => {
                    drop(permit);
                    break;
                }
                Ok(n) => {
                    if let Some(permit) = permit {
                        permit.forget();
                        pending_to_tunnel.fetch_add(1, Ordering::AcqRel);
                    }
                    let frame = Frame::data(conn_id, buf[..n].to_vec()).encode();
                    if tunnel_tx_clone.send(frame).is_err() {
                        release_acked_credits(&pending_to_tunnel, unacked_to_tunnel.as_deref(), 1);
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

async fn release_connection_credits(state: &AppState, conn_id: u32, ack_count: u32) {
    if ack_count == 0 {
        return;
    }

    let (pending, sem) = {
        let map = state.conn_map.lock().await;
        match map.get(&conn_id) {
            Some(entry) => (
                Arc::clone(&entry.pending_to_tunnel),
                entry.unacked_to_tunnel.clone(),
            ),
            None => return,
        }
    };

    release_acked_credits(&pending, sem.as_deref(), ack_count);
}
