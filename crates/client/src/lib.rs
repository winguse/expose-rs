use expose_common::{Frame, FRAME_CLOSE, FRAME_DATA, FRAME_HALF_CLOSE, FRAME_OPEN};
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, Mutex},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

// ── Connection state ──────────────────────────────────────────────────────────

enum ConnEntry {
    Connecting(Vec<Vec<u8>>),
    Connected(mpsc::UnboundedSender<Vec<u8>>),
}

type ConnMap = Arc<Mutex<HashMap<u32, ConnEntry>>>;

// ── Public entry points ───────────────────────────────────────────────────────

/// Connect to the expose-rs server once and relay connections to `upstream`.
///
/// Returns when the WebSocket connection to the server is closed (or fails).
/// Does **not** reconnect; the caller is responsible for retry logic.
pub async fn run_client_once(server_url: String, upstream: String) {
    match connect_async(&server_url).await {
        Ok((ws, _)) => {
            info!("Connected to server at {}", server_url);
            run_client(ws, upstream).await;
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
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(512);

    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                break;
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
    out_tx: mpsc::Sender<Vec<u8>>,
    upstream: String,
    conn_map: ConnMap,
) {
    match frame.frame_type {
        FRAME_OPEN => {
            let conn_id = frame.conn_id;

            conn_map
                .lock()
                .await
                .insert(conn_id, ConnEntry::Connecting(Vec::new()));

            let out_tx_clone = out_tx.clone();
            let conn_map_clone = Arc::clone(&conn_map);

            tokio::spawn(async move {
                match TcpStream::connect(&upstream).await {
                    Ok(stream) => {
                        info!("Opened upstream connection for conn {}", conn_id);

                        let (data_tx, data_rx) = mpsc::unbounded_channel::<Vec<u8>>();

                        let buffered = {
                            let mut map = conn_map_clone.lock().await;
                            match map.get_mut(&conn_id) {
                                Some(ConnEntry::Connecting(buf)) => {
                                    let drained = std::mem::take(buf);
                                    *map.get_mut(&conn_id).unwrap() =
                                        ConnEntry::Connected(data_tx.clone());
                                    drained
                                }
                                _ => return,
                            }
                        };

                        for chunk in buffered {
                            let _ = data_tx.send(chunk);
                        }

                        proxy_conn(conn_id, stream, data_rx, out_tx_clone, conn_map_clone).await;
                    }
                    Err(e) => {
                        error!(
                            "Failed to connect to upstream {} for conn {}: {}",
                            upstream, conn_id, e
                        );
                        conn_map_clone.lock().await.remove(&conn_id);
                        let _ = out_tx_clone.send(Frame::close(conn_id).encode()).await;
                    }
                }
            });
        }

        FRAME_DATA => {
            let mut map = conn_map.lock().await;
            match map.get_mut(&frame.conn_id) {
                Some(ConnEntry::Connected(tx)) => {
                    // UnboundedSender::send is synchronous — never blocks the dispatch loop.
                    let _ = tx.send(frame.payload);
                }
                Some(ConnEntry::Connecting(buf)) => {
                    buf.push(frame.payload);
                }
                None => {
                    warn!("DATA for unknown conn_id {}", frame.conn_id);
                }
            }
        }

        FRAME_CLOSE => {
            conn_map.lock().await.remove(&frame.conn_id);
        }

        FRAME_HALF_CLOSE => {
            // The remote downstream finished sending; drop the data sender so the
            // client-side writer shuts down its write half to the upstream.
            // The client-side reader keeps running to forward any remaining upstream
            // data back through the tunnel (e.g. iperf3 server-side result JSON).
            conn_map.lock().await.remove(&frame.conn_id);
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
    mut data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    out_tx: mpsc::Sender<Vec<u8>>,
    conn_map: ConnMap,
) {
    let (mut tcp_rx, mut tcp_tx) = stream.into_split();
    let out_tx_clone = out_tx.clone();

    let mut reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        let mut normal_exit = true;
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let frame = Frame::data(conn_id, buf[..n].to_vec()).encode();
                    if out_tx_clone.send(frame).await.is_err() {
                        normal_exit = false;
                        break;
                    }
                }
                Err(e) => {
                    error!("Read from upstream error for conn {}: {}", conn_id, e);
                    normal_exit = false;
                    break;
                }
            }
        }
        // On a normal EOF send FRAME_HALF_CLOSE so the remote shuts down its write
        // half while keeping the other direction alive.  On error send FRAME_CLOSE
        // to tear down the whole connection immediately.
        let close_frame = if normal_exit {
            Frame::half_close(conn_id).encode()
        } else {
            Frame::close(conn_id).encode()
        };
        let _ = out_tx_clone.send(close_frame).await;
        normal_exit
    });

    let mut writer = tokio::spawn(async move {
        while let Some(data) = data_rx.recv().await {
            if tcp_tx.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = tcp_tx.shutdown().await;
    });

    tokio::select! {
        r = &mut reader => {
            let normal_exit = r.unwrap_or(false);
            if normal_exit {
                // Reader done normally (upstream EOF, FRAME_HALF_CLOSE sent).
                // Wait for the writer to drain the other direction and finish.
                let _ = writer.await;
            } else {
                // Reader errored (FRAME_CLOSE sent); abort the writer immediately.
                writer.abort();
                let _ = writer.await;
            }
        }
        _ = &mut writer => {
            // Writer finished (data channel closed after receiving FRAME_HALF_CLOSE
            // or FRAME_CLOSE from the remote, or a write error).
            // Keep the reader running so the other direction can complete.
            let _ = reader.await;
        }
    }

    conn_map.lock().await.remove(&conn_id);
    info!("Upstream connection for conn {} closed", conn_id);
}
