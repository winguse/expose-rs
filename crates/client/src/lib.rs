use expose_common::{
    acquire_permit, apply_flow_ack, semaphore_for_limit, Frame, FRAME_ACK, FRAME_CLOSE, FRAME_DATA,
    FRAME_OPEN, FRAME_WRITE_ERROR,
};
pub use expose_common::{
    CapacityConfig, HeartbeatConfig, ACK_BATCH_SIZE, DEFAULT_HEARTBEAT_INTERVAL_SECS,
    DEFAULT_HEARTBEAT_MAX_MISSED, DEFAULT_MAX_PENDING_MESSAGES_PER_CONNECTION,
};
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
    net::TcpStream,
    sync::{mpsc, watch, Mutex},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

struct ConnEntry {
    tx: mpsc::UnboundedSender<ProxyMsg>,
    remote_closed: bool,
    /// Upstream → tunnel: credits until the server ACKs after writing to the visitor.
    inflight_to_server: Option<Arc<tokio::sync::Semaphore>>,
    /// Signal sent to the local reader when a FRAME_WRITE_ERROR is received from
    /// the peer.  The reader selects on this to stop gracefully without abort.
    peer_write_error: watch::Sender<bool>,
}

type ConnMap = Arc<Mutex<HashMap<u32, ConnEntry>>>;

enum ProxyMsg {
    Data(Vec<u8>),
    Close,
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Connect to the expose-rs server once and relay connections to `upstream`.
///
/// Returns when the WebSocket connection to the server is closed (or fails).
/// Does **not** reconnect; the caller is responsible for retry logic.
pub async fn run_client_once(server_url: String, upstream: String) {
    run_client_once_with_channel_config(
        server_url,
        upstream,
        CapacityConfig::default(),
        HeartbeatConfig::default(),
    )
    .await;
}

/// Connect to the expose-rs server once and relay connections to `upstream`,
/// using explicit per-connection in-flight limits and heartbeat settings.
pub async fn run_client_once_with_channel_config(
    server_url: String,
    upstream: String,
    channel_config: CapacityConfig,
    heartbeat_config: HeartbeatConfig,
) {
    match connect_async(&server_url).await {
        Ok((ws, _)) => {
            info!("Connected to server at {}", server_url);
            run_client(ws, upstream, channel_config, heartbeat_config).await;
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
    heartbeat_config: HeartbeatConfig,
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    // Single outgoing channel for all messages (ACKs, CLOSE, DATA, Ping).
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    let conn_map: ConnMap = Arc::new(Mutex::new(HashMap::new()));

    // ── Heartbeat setup ───────────────────────────────────────────────────────
    // `missed_pongs` counts how many pings have been sent without a matching pong.
    // Reset to 0 on every Pong received; incremented by the heartbeat task before
    // each ping.  If it exceeds `max_missed` the task signals a close.
    let missed_pongs = Arc::new(AtomicU32::new(0));
    let (hb_close_tx, hb_close_rx) = watch::channel(false);

    let heartbeat_enabled = !heartbeat_config.interval.is_zero();
    if heartbeat_enabled {
        let out_tx_hb = out_tx.clone();
        let missed_pongs_hb = Arc::clone(&missed_pongs);
        let hb_close_tx_hb = hb_close_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(heartbeat_config.interval);
            ticker.tick().await; // skip the immediate first tick
            loop {
                ticker.tick().await;
                let missed = missed_pongs_hb.fetch_add(1, Ordering::Relaxed) + 1;
                if missed > heartbeat_config.max_missed {
                    warn!(
                        "Heartbeat timeout: {} consecutive missed pongs (max {}), closing connection",
                        missed, heartbeat_config.max_missed
                    );
                    let _ = hb_close_tx_hb.send(true);
                    break;
                }
                if out_tx_hb.send(Message::Ping(vec![])).is_err() {
                    break;
                }
            }
        });
    }

    let mut hb_close_rx = hb_close_rx;

    loop {
        tokio::select! {
            _ = hb_close_rx.changed(), if heartbeat_enabled => {
                if *hb_close_rx.borrow() {
                    break;
                }
            }
            msg_result = ws_rx.next() => {
                match msg_result {
                    Some(Ok(Message::Binary(bytes))) => match Frame::decode(&bytes) {
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
                    Some(Ok(Message::Pong(_))) => {
                        missed_pongs.store(0, Ordering::Relaxed);
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        error!("WebSocket error: {}", e);
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    conn_map.lock().await.clear();
    writer_task.abort();
}

// ── Frame dispatch ────────────────────────────────────────────────────────────

async fn handle_frame(
    frame: Frame,
    out_tx: mpsc::UnboundedSender<Message>,
    upstream: String,
    conn_map: ConnMap,
    channel_config: CapacityConfig,
) {
    match frame.frame_type {
        FRAME_OPEN => {
            let conn_id = frame.conn_id;
            let inflight_to_server =
                semaphore_for_limit(channel_config.max_pending_messages_per_connection);
            let (data_tx, data_rx) = mpsc::unbounded_channel::<ProxyMsg>();
            let (peer_write_error_tx, peer_write_error_rx) = watch::channel(false);
            conn_map.lock().await.insert(
                conn_id,
                ConnEntry {
                    tx: data_tx,
                    remote_closed: false,
                    inflight_to_server: inflight_to_server.clone(),
                    peer_write_error: peer_write_error_tx,
                },
            );

            let out_tx_clone = out_tx.clone();
            let conn_map_clone = Arc::clone(&conn_map);
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
                            inflight_to_server,
                            peer_write_error_rx,
                        )
                        .await;
                    }
                    Err(e) => {
                        error!(
                            "Failed to connect to upstream {} for conn {}: {}",
                            upstream, conn_id, e
                        );
                        conn_map_clone.lock().await.remove(&conn_id);
                        let _ = out_tx_clone
                            .send(Message::Binary(Frame::close(conn_id).encode()));
                    }
                }
            });
        }

        FRAME_ACK => {
            let count = match frame.ack_count() {
                Ok(c) => c,
                Err(e) => {
                    warn!("Bad ACK frame for conn {}: {}", frame.conn_id, e);
                    return;
                }
            };
            let sem = {
                let map = conn_map.lock().await;
                map.get(&frame.conn_id)
                    .and_then(|e| e.inflight_to_server.clone())
            };
            apply_flow_ack(sem.as_deref(), count);
        }

        FRAME_DATA => {
            let conn = {
                let mut map = conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(ConnEntry {
                        tx, remote_closed, ..
                    }) => {
                        if *remote_closed {
                            None
                        } else {
                            Some(tx.clone())
                        }
                    }
                    None => {
                        warn!("DATA for unknown conn_id {}", frame.conn_id);
                        None
                    }
                }
            };
            if let Some(tx) = conn {
                let _ = tx.send(ProxyMsg::Data(frame.payload));
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

        FRAME_WRITE_ERROR => {
            // The server could not write to the visitor.
            // * Signal our local reader to stop gracefully (via watch channel).
            // * Do NOT remove conn_id from conn_map here; cleanup happens after
            //   both tasks finish in proxy_conn.
            let map = conn_map.lock().await;
            if let Some(entry) = map.get(&frame.conn_id) {
                let _ = entry.peer_write_error.send(true);
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
    out_tx: mpsc::UnboundedSender<Message>,
    conn_map: ConnMap,
    // Semaphore passed in directly so the reader does not need to lock conn_map
    // on every iteration just to retrieve an immutable Arc clone.
    inflight_to_server: Option<Arc<tokio::sync::Semaphore>>,
    // Watch receiver signalled when a FRAME_WRITE_ERROR arrives from the server.
    peer_write_error_rx: watch::Receiver<bool>,
) {
    let (mut tcp_rx, mut tcp_tx) = stream.into_split();
    let out_tx_clone = out_tx.clone();
    let mut write_ack_count: u32 = 0;

    let reader = tokio::spawn(async move {
        let mut peer_write_error_rx = peer_write_error_rx;
        let mut buf = vec![0u8; 16 * 1024];
        let mut stopped_by_peer_error = false;
        'reader: loop {
            // Wait for a flow-control permit, but also watch for a peer write
            // error signal so we don't block here indefinitely when the server
            // stops sending ACKs after its own write error.
            let permit;
            tokio::select! {
                _ = peer_write_error_rx.changed() => {
                    stopped_by_peer_error = true;
                    break 'reader;
                }
                p = acquire_permit(&inflight_to_server) => { permit = p; }
            }

            // Read from upstream TCP, but also watch for the peer write error
            // signal so we exit gracefully without aborting the task.
            tokio::select! {
                _ = peer_write_error_rx.changed() => {
                    drop(permit);
                    stopped_by_peer_error = true;
                    break 'reader;
                }
                result = tcp_rx.read(&mut buf) => {
                    match result {
                        Ok(0) => {
                            drop(permit);
                            break 'reader;
                        }
                        Ok(n) => {
                            let bytes = Message::Binary(Frame::data(conn_id, buf[..n].to_vec()).encode());
                            // Forget the permit only after the send succeeds so credits
                            // are not permanently lost when the channel has already closed.
                            if out_tx_clone.send(bytes).is_err() {
                                drop(permit);
                                break 'reader;
                            }
                            if let Some(p) = permit {
                                p.forget();
                            }
                        }
                        Err(e) => {
                            drop(permit);
                            error!("Read from upstream error for conn {}: {}", conn_id, e);
                            break 'reader;
                        }
                    }
                }
            }
        }
        // Only send FRAME_CLOSE on a normal read EOF / error.  When stopped by a
        // peer write-error signal the peer already knows — sending FRAME_CLOSE
        // would be confusing and redundant.
        if !stopped_by_peer_error {
            let _ = out_tx_clone.send(Message::Binary(Frame::close(conn_id).encode()));
        }
    });

    let writer = tokio::spawn(async move {
        while let Some(msg) = data_rx.recv().await {
            match msg {
                ProxyMsg::Data(payload) => {
                    if let Err(e) = tcp_tx.write_all(&payload).await {
                        error!(
                            "Upstream write error for conn {}: {}; notifying server",
                            conn_id, e
                        );
                        // Signal the peer with FRAME_WRITE_ERROR.  We intentionally
                        // do NOT abort the reader or remove conn_id from conn_map
                        // here — the reader keeps running and conn_map is cleaned up
                        // after both tasks finish.
                        let _ = out_tx.send(Message::Binary(Frame::write_error(conn_id).encode()));
                        break;
                    }
                    write_ack_count = write_ack_count.saturating_add(1);
                    if write_ack_count >= ACK_BATCH_SIZE {
                        let _ = out_tx.send(Message::Binary(Frame::ack(conn_id, write_ack_count).encode()));
                        write_ack_count = 0;
                    }
                }
                ProxyMsg::Close => break,
            }
        }
        if write_ack_count > 0 {
            let _ = out_tx.send(Message::Binary(Frame::ack(conn_id, write_ack_count).encode()));
        }
        let _ = tcp_tx.shutdown().await;
    });

    let _ = tokio::join!(reader, writer);

    conn_map.lock().await.remove(&conn_id);
    info!("Upstream connection for conn {} closed", conn_id);
}
