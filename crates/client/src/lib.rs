use expose_common::{Frame, FRAME_CLOSE, FRAME_DATA, FRAME_OPEN};
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

pub const DEFAULT_MAX_PENDING_UPSTREAM_MESSAGES_PER_CONNECTION: usize = 256;
pub const DEFAULT_MAX_PENDING_DOWNSTREAM_MESSAGES_PER_CONNECTION: usize = 256;

#[derive(Clone, Copy, Debug)]
pub struct CapacityConfig {
    pub max_pending_upstream_messages_per_connection: usize,
    pub max_pending_downstream_messages_per_connection: usize,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        CapacityConfig {
            max_pending_upstream_messages_per_connection:
                DEFAULT_MAX_PENDING_UPSTREAM_MESSAGES_PER_CONNECTION,
            max_pending_downstream_messages_per_connection:
                DEFAULT_MAX_PENDING_DOWNSTREAM_MESSAGES_PER_CONNECTION,
        }
    }
}

struct ConnEntry {
    tx: mpsc::UnboundedSender<ProxyMsg>,
    remote_closed: bool,
    inflight_from_server: Option<Arc<Semaphore>>,
}

type ConnMap = Arc<Mutex<HashMap<u32, ConnEntry>>>;

enum ProxyMsg {
    Data {
        payload: Vec<u8>,
        permit: Option<OwnedSemaphorePermit>,
    },
    Close,
}

enum WsOutMsg {
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

// ── Public entry points ───────────────────────────────────────────────────────

/// Connect to the expose-rs server once and relay connections to `upstream`.
///
/// Returns when the WebSocket connection to the server is closed (or fails).
/// Does **not** reconnect; the caller is responsible for retry logic.
pub async fn run_client_once(server_url: String, upstream: String) {
    run_client_once_with_channel_config(server_url, upstream, CapacityConfig::default()).await;
}

/// Connect to the expose-rs server once and relay connections to `upstream`,
/// using explicit per-connection in-flight limits.
pub async fn run_client_once_with_channel_config(
    server_url: String,
    upstream: String,
    channel_config: CapacityConfig,
) {
    match connect_async(&server_url).await {
        Ok((ws, _)) => {
            info!("Connected to server at {}", server_url);
            run_client(ws, upstream, channel_config).await;
        }
        Err(e) => {
            error!("Failed to connect to {}: {}", server_url, e);
        }
    }
}

// ── Internal loop ─────────────────────────────────────────────────────────────

async fn run_client(
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    upstream: String,
    channel_config: CapacityConfig,
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsOutMsg>();

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let (bytes, permit) = match msg {
                WsOutMsg::Frame(bytes) => (bytes, None),
                WsOutMsg::Data { bytes, permit } => (bytes, permit),
            };

            if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                drop(permit);
                break;
            }

            drop(permit);
        }
        let _ = ws_tx.close().await;
    });

    let conn_map: ConnMap = Arc::new(Mutex::new(HashMap::new()));

    while let Some(msg_result) = ws_rx.next().await {
        match msg_result {
            Ok(Message::Binary(bytes)) => match Frame::decode(&bytes) {
                Ok(frame) => {
                    handle_frame(
                        frame,
                        out_tx.clone(),
                        upstream.clone(),
                        Arc::clone(&conn_map),
                        channel_config,
                    )
                    .await;
                }
                Err(e) => warn!("Bad frame from server: {}", e),
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                error!("WebSocket error: {}", e);
                break;
            }
        }
    }

    conn_map.lock().await.clear();
    writer_task.abort();
}

// ── Frame dispatch ────────────────────────────────────────────────────────────

async fn handle_frame(
    frame: Frame,
    out_tx: mpsc::UnboundedSender<WsOutMsg>,
    upstream: String,
    conn_map: ConnMap,
    channel_config: CapacityConfig,
) {
    match frame.frame_type {
        FRAME_OPEN => {
            let conn_id = frame.conn_id;
            let inflight_from_server =
                semaphore_for_limit(channel_config.max_pending_downstream_messages_per_connection);
            let (data_tx, data_rx) = mpsc::unbounded_channel::<ProxyMsg>();
            conn_map.lock().await.insert(
                conn_id,
                ConnEntry {
                    tx: data_tx.clone(),
                    remote_closed: false,
                    inflight_from_server: inflight_from_server.clone(),
                },
            );

            let out_tx_clone = out_tx.clone();
            let conn_map_clone = Arc::clone(&conn_map);
            let max_pending_upstream = channel_config.max_pending_upstream_messages_per_connection;

            tokio::spawn(async move {
                match TcpStream::connect(&upstream).await {
                    Ok(stream) => {
                        info!("Opened upstream connection for conn {}", conn_id);
                        if !conn_map_clone.lock().await.contains_key(&conn_id) {
                            return;
                        }

                        proxy_conn(
                            conn_id,
                            stream,
                            data_rx,
                            out_tx_clone,
                            conn_map_clone,
                            max_pending_upstream,
                        )
                        .await;
                    }
                    Err(e) => {
                        error!(
                            "Failed to connect to upstream {} for conn {}: {}",
                            upstream, conn_id, e
                        );
                        conn_map_clone.lock().await.remove(&conn_id);
                        let _ = out_tx_clone.send(WsOutMsg::Frame(Frame::close(conn_id).encode()));
                    }
                }
            });
        }

        FRAME_DATA => {
            let conn = {
                let mut map = conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(ConnEntry {
                        tx,
                        remote_closed,
                        inflight_from_server,
                    }) => {
                        if *remote_closed {
                            None
                        } else {
                            Some((tx.clone(), inflight_from_server.clone()))
                        }
                    }
                    None => {
                        warn!("DATA for unknown conn_id {}", frame.conn_id);
                        None
                    }
                }
            };
            if let Some((tx, inflight_from_server)) = conn {
                let permit = acquire_permit(&inflight_from_server).await;
                let _ = tx.send(ProxyMsg::Data {
                    payload: frame.payload,
                    permit,
                });
            }
        }

        FRAME_CLOSE => {
            let tx = {
                let mut map = conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(ConnEntry {
                        tx, remote_closed, ..
                    }) if !*remote_closed => {
                        *remote_closed = true;
                        Some(tx.clone())
                    }
                    Some(ConnEntry { .. }) => None,
                    None => None,
                }
            };
            if let Some(tx) = tx {
                let _ = tx.send(ProxyMsg::Close);
            }
        }

        other => {
            warn!("Unexpected frame type {:#04x} from server", other);
        }
    }
}

// ── Per-connection TCP proxy ──────────────────────────────────────────────────

async fn proxy_conn(
    conn_id: u32,
    stream: TcpStream,
    mut data_rx: mpsc::UnboundedReceiver<ProxyMsg>,
    out_tx: mpsc::UnboundedSender<WsOutMsg>,
    conn_map: ConnMap,
    max_pending_upstream_messages_per_connection: usize,
) {
    let (mut tcp_rx, mut tcp_tx) = stream.into_split();
    let out_tx_clone = out_tx.clone();
    let inflight_to_server = semaphore_for_limit(max_pending_upstream_messages_per_connection);

    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let permit = acquire_permit(&inflight_to_server).await;

            match tcp_rx.read(&mut buf).await {
                Ok(0) => {
                    drop(permit);
                    break;
                }
                Ok(n) => {
                    let msg = WsOutMsg::Data {
                        bytes: Frame::data(conn_id, buf[..n].to_vec()).encode(),
                        permit,
                    };
                    if out_tx_clone.send(msg).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    drop(permit);
                    error!("Read from upstream error for conn {}: {}", conn_id, e);
                    break;
                }
            }
        }
        let _ = out_tx_clone.send(WsOutMsg::Frame(Frame::close(conn_id).encode()));
    });

    let writer = tokio::spawn(async move {
        while let Some(msg) = data_rx.recv().await {
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

    conn_map.lock().await.remove(&conn_id);
    info!("Upstream connection for conn {} closed", conn_id);
}
