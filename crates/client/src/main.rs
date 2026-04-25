use clap::Parser;
use expose_common::{Frame, FRAME_CLOSE, FRAME_DATA, FRAME_OPEN};
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, Mutex},
    time::sleep,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(author, version, about = "expose-rs client — protocol-neutral TCP tunnel")]
struct Args {
    /// Server WebSocket URL (e.g. ws://host:8080/secret-token or wss://host/secret-token).
    #[arg(long)]
    server: String,

    /// Upstream TCP address to forward connections to (e.g. localhost:3000).
    #[arg(long)]
    upstream: String,
}

// ── Connection state on the client side ──────────────────────────────────────

/// State of a proxied connection.
enum ConnEntry {
    /// TCP connection to upstream is being established; DATA frames are buffered.
    Connecting(Vec<Vec<u8>>),
    /// TCP connection is ready; DATA frames go directly into the channel.
    Connected(mpsc::Sender<Vec<u8>>),
}

type ConnMap = Arc<Mutex<HashMap<u32, ConnEntry>>>;

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "expose_client=info".into()),
        )
        .init();

    let args = Args::parse();
    let mut backoff = Duration::from_secs(1);

    loop {
        info!("Connecting to server: {}", args.server);
        match connect_async(&args.server).await {
            Ok((ws, _)) => {
                backoff = Duration::from_secs(1);
                info!("Connected to server");
                run_client(ws, args.upstream.clone()).await;
                warn!("Disconnected from server, reconnecting...");
            }
            Err(e) => {
                error!("Failed to connect: {}", e);
            }
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

// ── Main client loop ──────────────────────────────────────────────────────────

async fn run_client(
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    upstream: String,
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(512);

    // Writer task: send outgoing binary frames to the server.
    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    let conn_map: ConnMap = Arc::new(Mutex::new(HashMap::new()));

    // Main receive loop.
    while let Some(msg_result) = ws_rx.next().await {
        match msg_result {
            Ok(Message::Binary(bytes)) => match Frame::decode(&bytes) {
                Ok(frame) => {
                    handle_frame(frame, out_tx.clone(), upstream.clone(), Arc::clone(&conn_map))
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

    // Tear down all connections.
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

            // Register as "Connecting" immediately so DATA frames arriving before the TCP
            // handshake completes are buffered rather than dropped.
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

                        let (data_tx, data_rx) = mpsc::channel::<Vec<u8>>(64);

                        // Flush buffered data and transition state.
                        let buffered = {
                            let mut map = conn_map_clone.lock().await;
                            match map.get_mut(&conn_id) {
                                Some(ConnEntry::Connecting(buf)) => {
                                    let drained = std::mem::take(buf);
                                    *map.get_mut(&conn_id).unwrap() =
                                        ConnEntry::Connected(data_tx.clone());
                                    drained
                                }
                                _ => {
                                    // Entry was removed while we were connecting — abort.
                                    return;
                                }
                            }
                        };

                        // Forward any data that arrived during connection setup.
                        for chunk in buffered {
                            let _ = data_tx.send(chunk).await;
                        }

                        proxy_conn(conn_id, stream, data_rx, out_tx_clone, conn_map_clone)
                            .await;
                    }
                    Err(e) => {
                        error!(
                            "Failed to connect to upstream {} for conn {}: {}",
                            upstream, conn_id, e
                        );
                        conn_map_clone.lock().await.remove(&conn_id);
                        // Notify server that this connection failed.
                        let _ = out_tx_clone.send(Frame::close(conn_id).encode()).await;
                    }
                }
            });
        }

        FRAME_DATA => {
            let mut map = conn_map.lock().await;
            match map.get_mut(&frame.conn_id) {
                Some(ConnEntry::Connected(tx)) => {
                    let _ = tx.send(frame.payload).await;
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

        other => {
            warn!("Unexpected frame type {:#04x} from server", other);
        }
    }
}

// ── Per-connection TCP proxy ──────────────────────────────────────────────────

/// Relay data between the upstream TCP connection and the tunnel.
async fn proxy_conn(
    conn_id: u32,
    stream: TcpStream,
    mut data_rx: mpsc::Receiver<Vec<u8>>,
    out_tx: mpsc::Sender<Vec<u8>>,
    conn_map: ConnMap,
) {
    let (mut tcp_rx, mut tcp_tx) = stream.into_split();
    let out_tx_clone = out_tx.clone();

    // Task: upstream → tunnel (forward received bytes as DATA frames).
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let frame = Frame::data(conn_id, buf[..n].to_vec()).encode();
                    if out_tx_clone.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error!("Read from upstream error for conn {}: {}", conn_id, e);
                    break;
                }
            }
        }
        // Notify server that upstream closed.
        let _ = out_tx_clone.send(Frame::close(conn_id).encode()).await;
    });

    // Task: tunnel → upstream (write DATA frames to the upstream TCP socket).
    let writer = tokio::spawn(async move {
        while let Some(data) = data_rx.recv().await {
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

    conn_map.lock().await.remove(&conn_id);
    info!("Upstream connection for conn {} closed", conn_id);
}
