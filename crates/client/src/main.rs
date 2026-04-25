use clap::Parser;
use expose_common::{decode_base64, encode_base64, Headers, TunnelMessage};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderName, HeaderValue};
use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, Mutex},
    time::sleep,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use url::Url;

#[derive(Parser, Debug)]
#[command(author, version, about = "expose-rs client")]
struct Args {
    /// Server WebSocket URL (e.g. ws://host:8080/secret or wss://host/secret)
    #[arg(long)]
    server: String,

    /// Upstream HTTP base URL (e.g. http://localhost:3000)
    #[arg(long)]
    upstream: String,
}

/// Shared sender for the control WebSocket connection.
type TunnelTx = mpsc::Sender<TunnelMessage>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "expose_client=info".into()),
        )
        .init();

    let args = Args::parse();
    let upstream = args.upstream.trim_end_matches('/').to_string();

    let mut backoff = Duration::from_secs(1);

    loop {
        info!("Connecting to server: {}", args.server);
        match connect_async(&args.server).await {
            Ok((ws_stream, _)) => {
                info!("Connected to server");
                backoff = Duration::from_secs(1);
                run_client(ws_stream, upstream.clone()).await;
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

async fn run_client(
    ws_stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    upstream: String,
) {
    let (mut ws_tx, mut ws_rx) = ws_stream.split();
    let (out_tx, mut out_rx) = mpsc::channel::<TunnelMessage>(256);

    // Writer task: send outgoing tunnel messages.
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(text) => {
                    if ws_tx
                        .send(Message::Text(text.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => error!("Serialize error: {}", e),
            }
        }
    });

    // WebSocket sessions: map from session id -> sender to the upstream ws forwarder.
    let ws_sessions: Arc<Mutex<HashMap<String, mpsc::Sender<WsFrame>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

    while let Some(msg_result) = ws_rx.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<TunnelMessage>(&text) {
                    Ok(tunnel_msg) => {
                        let out_tx = out_tx.clone();
                        let upstream = upstream.clone();
                        let http_client = http_client.clone();
                        let ws_sessions = Arc::clone(&ws_sessions);

                        tokio::spawn(async move {
                            handle_tunnel_message(
                                tunnel_msg,
                                out_tx,
                                upstream,
                                http_client,
                                ws_sessions,
                            )
                            .await;
                        });
                    }
                    Err(e) => warn!("Bad tunnel message: {}", e),
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                error!("WebSocket error: {}", e);
                break;
            }
        }
    }

    writer_task.abort();
}

struct WsFrame {
    data: String,
    is_binary: bool,
    close: bool,
}

async fn handle_tunnel_message(
    msg: TunnelMessage,
    out_tx: TunnelTx,
    upstream: String,
    http_client: reqwest::Client,
    ws_sessions: Arc<Mutex<HashMap<String, mpsc::Sender<WsFrame>>>>,
) {
    match msg {
        TunnelMessage::HttpRequest {
            id,
            method,
            path,
            headers,
            body,
        } => {
            handle_http_request(id, method, path, headers, body, out_tx, upstream, http_client)
                .await;
        }

        TunnelMessage::WsOpen { id, path, headers } => {
            handle_ws_open(id, path, headers, out_tx, upstream, ws_sessions).await;
        }

        TunnelMessage::WsData {
            id,
            data,
            is_binary,
        } => {
            let sessions = ws_sessions.lock().await;
            if let Some(tx) = sessions.get(&id) {
                let _ = tx
                    .send(WsFrame {
                        data,
                        is_binary,
                        close: false,
                    })
                    .await;
            }
        }

        TunnelMessage::WsClose { id } => {
            let mut sessions = ws_sessions.lock().await;
            if let Some(tx) = sessions.remove(&id) {
                let _ = tx
                    .send(WsFrame {
                        data: String::new(),
                        is_binary: false,
                        close: true,
                    })
                    .await;
            }
        }

        _ => {}
    }
}

async fn handle_http_request(
    id: String,
    method: String,
    path: String,
    req_headers: Headers,
    body: Option<String>,
    out_tx: TunnelTx,
    upstream: String,
    http_client: reqwest::Client,
) {
    let url = format!("{}{}", upstream, path);

    let method_parsed = match reqwest::Method::from_bytes(method.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            send_error(&id, 400, "Invalid method", &out_tx).await;
            return;
        }
    };

    let mut request_builder = http_client.request(method_parsed, &url);

    for (name, values) in &req_headers {
        // Skip hop-by-hop headers.
        if matches!(
            name.as_str(),
            "connection" | "upgrade" | "transfer-encoding" | "keep-alive"
                | "proxy-connection" | "proxy-authenticate" | "proxy-authorization"
                | "te" | "trailers"
        ) {
            continue;
        }
        if let Ok(header_name) = HeaderName::from_str(name) {
            for value in values {
                if let Ok(header_value) = HeaderValue::from_str(value) {
                    request_builder = request_builder.header(header_name.clone(), header_value);
                }
            }
        }
    }

    if let Some(b) = body {
        match decode_base64(&b) {
            Ok(bytes) => {
                request_builder = request_builder.body(bytes);
            }
            Err(e) => {
                warn!("Failed to decode request body: {}", e);
            }
        }
    }

    let response = match request_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            error!("Upstream request failed for {}: {}", url, e);
            send_error(&id, 502, "Upstream request failed", &out_tx).await;
            return;
        }
    };

    let status = response.status().as_u16();
    let mut resp_headers: Headers = HashMap::new();
    for (name, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            resp_headers
                .entry(name.as_str().to_lowercase())
                .or_default()
                .push(v.to_string());
        }
    }

    let is_streaming = resp_headers
        .get("content-type")
        .and_then(|v| v.first())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);

    if is_streaming {
        // Send header chunk first.
        let _ = out_tx
            .send(TunnelMessage::HttpResponseChunk {
                id: id.clone(),
                status,
                headers: resp_headers,
            })
            .await;

        // Stream body chunks.
        let mut stream = response.bytes_stream();
        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    let _ = out_tx
                        .send(TunnelMessage::HttpResponseBodyChunk {
                            id: id.clone(),
                            data: encode_base64(&chunk),
                            done: false,
                        })
                        .await;
                }
                Err(e) => {
                    error!("Streaming error for {}: {}", url, e);
                    break;
                }
            }
        }

        // Signal end of stream.
        let _ = out_tx
            .send(TunnelMessage::HttpResponseBodyChunk {
                id: id.clone(),
                data: String::new(),
                done: true,
            })
            .await;
    } else {
        // Read full body.
        let body_bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to read upstream response body: {}", e);
                send_error(&id, 502, "Failed to read upstream response", &out_tx).await;
                return;
            }
        };

        let body = if body_bytes.is_empty() {
            None
        } else {
            Some(encode_base64(&body_bytes))
        };

        let _ = out_tx
            .send(TunnelMessage::HttpResponse {
                id,
                status,
                headers: resp_headers,
                body,
            })
            .await;
    }
}

async fn handle_ws_open(
    id: String,
    path: String,
    req_headers: Headers,
    out_tx: TunnelTx,
    upstream: String,
    ws_sessions: Arc<Mutex<HashMap<String, mpsc::Sender<WsFrame>>>>,
) {
    // Build the upstream WebSocket URL.
    let upstream_ws_url = {
        let base = upstream.replace("http://", "ws://").replace("https://", "wss://");
        format!("{}{}", base, path)
    };

    let upstream_url = match Url::parse(&upstream_ws_url) {
        Ok(u) => u,
        Err(e) => {
            error!("Invalid upstream WS URL {}: {}", upstream_ws_url, e);
            let _ = out_tx
                .send(TunnelMessage::WsClose { id })
                .await;
            return;
        }
    };

    // Build the WebSocket request with headers.
    let mut ws_request = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
        .uri(upstream_url.as_str());
    for (name, values) in &req_headers {
        if matches!(name.as_str(), "connection" | "upgrade" | "sec-websocket-key" | "sec-websocket-version") {
            continue;
        }
        for value in values {
            ws_request = ws_request.header(name.as_str(), value.as_str());
        }
    }
    let ws_request = match ws_request.body(()) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to build WS request: {}", e);
            let _ = out_tx.send(TunnelMessage::WsClose { id }).await;
            return;
        }
    };

    let upstream_ws = match connect_async(ws_request).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            error!("Failed to connect upstream WS {}: {}", upstream_ws_url, e);
            let _ = out_tx.send(TunnelMessage::WsClose { id }).await;
            return;
        }
    };

    info!("Upstream WebSocket opened: {}", upstream_ws_url);

    let (mut upstream_tx, mut upstream_rx) = upstream_ws.split();
    let (frame_tx, mut frame_rx) = mpsc::channel::<WsFrame>(64);

    ws_sessions.lock().await.insert(id.clone(), frame_tx);

    let id_for_reader = id.clone();
    let out_tx_for_reader = out_tx.clone();

    // Task: upstream → server (forward upstream WS messages to tunnel).
    let upstream_to_server = tokio::spawn(async move {
        while let Some(msg_result) = upstream_rx.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    let _ = out_tx_for_reader
                        .send(TunnelMessage::WsData {
                            id: id_for_reader.clone(),
                            data: encode_base64(text.as_bytes()),
                            is_binary: false,
                        })
                        .await;
                }
                Ok(Message::Binary(bin)) => {
                    let _ = out_tx_for_reader
                        .send(TunnelMessage::WsData {
                            id: id_for_reader.clone(),
                            data: encode_base64(&bin),
                            is_binary: true,
                        })
                        .await;
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        let _ = out_tx_for_reader
            .send(TunnelMessage::WsClose {
                id: id_for_reader,
            })
            .await;
    });

    // Task: server → upstream (forward tunnel WsData frames to upstream WS).
    let server_to_upstream = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if frame.close {
                break;
            }
            let ws_msg = if frame.is_binary {
                match decode_base64(&frame.data) {
                    Ok(bytes) => Message::Binary(bytes.into()),
                    Err(_) => continue,
                }
            } else {
                match decode_base64(&frame.data) {
                    Ok(bytes) => Message::Text(
                        String::from_utf8_lossy(&bytes).to_string().into(),
                    ),
                    Err(_) => continue,
                }
            };
            if upstream_tx.send(ws_msg).await.is_err() {
                break;
            }
        }
        let _ = upstream_tx.close().await;
    });

    // Wait for either task to finish, then clean up.
    tokio::select! {
        _ = upstream_to_server => {}
        _ = server_to_upstream => {}
    }

    ws_sessions.lock().await.remove(&id);
    info!("WebSocket session {} closed", id);
}

async fn send_error(id: &str, status: u16, message: &str, out_tx: &TunnelTx) {
    let body = encode_base64(message.as_bytes());
    let _ = out_tx
        .send(TunnelMessage::HttpResponse {
            id: id.to_string(),
            status,
            headers: HashMap::from([(
                "content-type".to_string(),
                vec!["text/plain".to_string()],
            )]),
            body: Some(body),
        })
        .await;
}
