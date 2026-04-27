use expose_common::{Frame, FRAME_CLOSE, FRAME_DATA, FRAME_OPEN};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, Mutex},
    time::{sleep, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

// ── Connection state ──────────────────────────────────────────────────────────

pub const DEFAULT_MAX_IN_FLIGHT_FRAMES_PER_CONNECTION: usize = 4096;
const INFLIGHT_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug)]
pub struct CapacityConfig {
    pub max_in_flight_frames_per_connection: usize,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        CapacityConfig {
            max_in_flight_frames_per_connection: DEFAULT_MAX_IN_FLIGHT_FRAMES_PER_CONNECTION,
        }
    }
}

enum ConnEntry {
    Connecting {
        buffered: Vec<Vec<u8>>,
        remote_closed: bool,
    },
    Connected {
        tx: mpsc::UnboundedSender<ProxyMsg>,
        remote_closed: bool,
        queued_to_upstream: Arc<AtomicUsize>,
    },
}

type ConnMap = Arc<Mutex<HashMap<u32, ConnEntry>>>;

enum ProxyMsg {
    Data(Vec<u8>),
    Close,
}

enum WsOutMsg {
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
            let (bytes, inflight_counter) = match msg {
                WsOutMsg::Frame(bytes) => (bytes, None),
                WsOutMsg::Data {
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
                        channel_config.max_in_flight_frames_per_connection,
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
    max_in_flight_frames_per_connection: usize,
) {
    match frame.frame_type {
        FRAME_OPEN => {
            let conn_id = frame.conn_id;

            conn_map.lock().await.insert(
                conn_id,
                ConnEntry::Connecting {
                    buffered: Vec::new(),
                    remote_closed: false,
                },
            );

            let out_tx_clone = out_tx.clone();
            let conn_map_clone = Arc::clone(&conn_map);

            tokio::spawn(async move {
                match TcpStream::connect(&upstream).await {
                    Ok(stream) => {
                        info!("Opened upstream connection for conn {}", conn_id);

                        let queued_to_upstream = Arc::new(AtomicUsize::new(0));
                        let (data_tx, data_rx) = mpsc::unbounded_channel::<ProxyMsg>();

                        let (buffered, remote_closed) = {
                            let mut map = conn_map_clone.lock().await;
                            match map.get_mut(&conn_id) {
                                Some(ConnEntry::Connecting {
                                    buffered,
                                    remote_closed,
                                }) => {
                                    let drained = std::mem::take(buffered);
                                    let remote_closed = *remote_closed;
                                    *map.get_mut(&conn_id).unwrap() = ConnEntry::Connected {
                                        tx: data_tx.clone(),
                                        remote_closed: false,
                                        queued_to_upstream: Arc::clone(&queued_to_upstream),
                                    };
                                    (drained, remote_closed)
                                }
                                _ => return,
                            }
                        };

                        for chunk in buffered {
                            wait_for_slot(&queued_to_upstream, max_in_flight_frames_per_connection)
                                .await;
                            queued_to_upstream.fetch_add(1, Ordering::AcqRel);
                            if data_tx.send(ProxyMsg::Data(chunk)).is_err() {
                                decrement_counter(&queued_to_upstream);
                                break;
                            }
                        }

                        if remote_closed {
                            let tx = {
                                let mut map = conn_map_clone.lock().await;
                                match map.get_mut(&conn_id) {
                                    Some(ConnEntry::Connected {
                                        tx, remote_closed, ..
                                    }) => {
                                        *remote_closed = true;
                                        Some(tx.clone())
                                    }
                                    _ => None,
                                }
                            };
                            if let Some(tx) = tx {
                                let _ = tx.send(ProxyMsg::Close);
                            }
                        }

                        proxy_conn(
                            conn_id,
                            stream,
                            data_rx,
                            out_tx_clone,
                            conn_map_clone,
                            queued_to_upstream,
                            max_in_flight_frames_per_connection,
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
            let mut payload = Some(frame.payload);
            let conn = {
                let mut map = conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(ConnEntry::Connected {
                        tx,
                        remote_closed,
                        queued_to_upstream,
                    }) => {
                        if *remote_closed {
                            None
                        } else {
                            Some((tx.clone(), Arc::clone(queued_to_upstream)))
                        }
                    }
                    Some(ConnEntry::Connecting { buffered, .. }) => {
                        buffered.push(payload.take().unwrap());
                        None
                    }
                    None => {
                        warn!("DATA for unknown conn_id {}", frame.conn_id);
                        None
                    }
                }
            };
            if let Some((tx, queued_to_upstream)) = conn {
                wait_for_slot(&queued_to_upstream, max_in_flight_frames_per_connection).await;
                queued_to_upstream.fetch_add(1, Ordering::AcqRel);
                if tx.send(ProxyMsg::Data(payload.take().unwrap())).is_err() {
                    decrement_counter(&queued_to_upstream);
                }
            }
        }

        FRAME_CLOSE => {
            let tx = {
                let mut map = conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(ConnEntry::Connected {
                        tx, remote_closed, ..
                    }) if !*remote_closed => {
                        *remote_closed = true;
                        Some(tx.clone())
                    }
                    Some(ConnEntry::Connected { .. }) => None,
                    Some(ConnEntry::Connecting { remote_closed, .. }) => {
                        *remote_closed = true;
                        None
                    }
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
    queued_to_upstream: Arc<AtomicUsize>,
    max_in_flight_frames_per_connection: usize,
) {
    let (mut tcp_rx, mut tcp_tx) = stream.into_split();
    let out_tx_clone = out_tx.clone();
    let to_tunnel_inflight = Arc::new(AtomicUsize::new(0));
    let to_tunnel_inflight_reader = Arc::clone(&to_tunnel_inflight);

    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            wait_for_slot(
                &to_tunnel_inflight_reader,
                max_in_flight_frames_per_connection,
            )
            .await;

            match tcp_rx.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    to_tunnel_inflight_reader.fetch_add(1, Ordering::AcqRel);
                    let msg = WsOutMsg::Data {
                        bytes: Frame::data(conn_id, buf[..n].to_vec()).encode(),
                        inflight_counter: Arc::clone(&to_tunnel_inflight_reader),
                    };
                    if out_tx_clone.send(msg).is_err() {
                        decrement_counter(&to_tunnel_inflight_reader);
                        break;
                    }
                }
                Err(e) => {
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
                ProxyMsg::Data(data) => {
                    decrement_counter(&queued_to_upstream);
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

    conn_map.lock().await.remove(&conn_id);
    info!("Upstream connection for conn {} closed", conn_id);
}
