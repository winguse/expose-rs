use expose_common::{Frame, FRAME_CLOSE, FRAME_DATA, FRAME_OPEN};
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
    Connecting {
        buffered: Vec<Vec<u8>>,
        remote_closed: bool,
    },
    Connected {
        tx: mpsc::UnboundedSender<ProxyMsg>,
        remote_closed: bool,
    },
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
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();

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
    out_tx: mpsc::UnboundedSender<Vec<u8>>,
    upstream: String,
    conn_map: ConnMap,
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
                                    };
                                    (drained, remote_closed)
                                }
                                _ => return,
                            }
                        };

                        for chunk in buffered {
                            let _ = data_tx.send(ProxyMsg::Data(chunk));
                        }

                        if remote_closed {
                            let tx = {
                                let mut map = conn_map_clone.lock().await;
                                match map.get_mut(&conn_id) {
                                    Some(ConnEntry::Connected { tx, remote_closed }) => {
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

                        proxy_conn(conn_id, stream, data_rx, out_tx_clone, conn_map_clone).await;
                    }
                    Err(e) => {
                        error!(
                            "Failed to connect to upstream {} for conn {}: {}",
                            upstream, conn_id, e
                        );
                        conn_map_clone.lock().await.remove(&conn_id);
                        let _ = out_tx_clone.send(Frame::close(conn_id).encode());
                    }
                }
            });
        }

        FRAME_DATA => {
            let mut payload = Some(frame.payload);
            let tx = {
                let mut map = conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(ConnEntry::Connected { tx, remote_closed }) => {
                        if *remote_closed {
                            None
                        } else {
                            Some(tx.clone())
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
            if let Some(tx) = tx {
                let _ = tx.send(ProxyMsg::Data(payload.take().unwrap()));
            }
        }

        FRAME_CLOSE => {
            let tx = {
                let mut map = conn_map.lock().await;
                match map.get_mut(&frame.conn_id) {
                    Some(ConnEntry::Connected { tx, remote_closed }) if !*remote_closed => {
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
    out_tx: mpsc::UnboundedSender<Vec<u8>>,
    conn_map: ConnMap,
) {
    let (mut tcp_rx, mut tcp_tx) = stream.into_split();
    let out_tx_clone = out_tx.clone();

    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let frame = Frame::data(conn_id, buf[..n].to_vec()).encode();
                    if out_tx_clone.send(frame).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error!("Read from upstream error for conn {}: {}", conn_id, e);
                    break;
                }
            }
        }
        let _ = out_tx_clone.send(Frame::close(conn_id).encode());
    });

    let writer = tokio::spawn(async move {
        while let Some(msg) = data_rx.recv().await {
            match msg {
                ProxyMsg::Data(data) => {
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
