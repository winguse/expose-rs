use axum::{
    body::Body,
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        FromRequestParts, State,
    },
    http::{HeaderName, HeaderValue, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use bytes::Bytes;
use expose_common::{decode_base64, encode_base64, Headers, TunnelMessage};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    time::timeout,
};
use tracing::{error, info, warn};
use uuid::Uuid;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about = "expose-rs server")]
struct Args {
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    #[arg(long, default_value_t = 8080)]
    port: u16,

    #[arg(long)]
    secret_token: String,
}

type ClientTx = mpsc::Sender<TunnelMessage>;

struct StreamChunk {
    data: String,
    done: bool,
}

enum ResponseKind {
    Full {
        status: u16,
        headers: Headers,
        body: Option<String>,
    },
    Streaming {
        status: u16,
        headers: Headers,
        body_rx: mpsc::Receiver<StreamChunk>,
    },
}

/// A frame relayed for an incoming WebSocket session (server-side browser → tunnel client).
struct IncomingWsFrame {
    /// Base64-encoded payload.
    data: String,
    is_binary: bool,
    close: bool,
}

struct SharedClientState {
    response_waiters: HashMap<String, oneshot::Sender<ResponseKind>>,
    body_streams: HashMap<String, mpsc::Sender<StreamChunk>>,
    /// Sessions for incoming WebSocket connections being proxied to the tunnel client.
    /// Key = session id. Value = sender to forward frames from the tunnel back to the browser WS.
    ws_sessions: HashMap<String, mpsc::Sender<IncomingWsFrame>>,
}

struct AppState {
    client: Mutex<Option<ClientHandle>>,
}

struct ClientHandle {
    tx: ClientTx,
    shared: Arc<Mutex<SharedClientState>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "expose_server=info".into()),
        )
        .init();

    let args = Args::parse();

    let state = Arc::new(AppState {
        client: Mutex::new(None),
    });

    let token_path = format!("/{}", args.secret_token);

    let app = Router::new()
        .route(&token_path, get(tunnel_ws_handler))
        .fallback(proxy_handler)
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .expect("Invalid host/port");

    info!("expose-server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ── Tunnel WebSocket handler ─────────────────────────────────────────────────

async fn tunnel_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_tunnel_ws(socket, state))
}

async fn handle_tunnel_ws(socket: WebSocket, state: Arc<AppState>) {
    info!("Tunnel client connected");

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (client_tx, mut client_rx) = mpsc::channel::<TunnelMessage>(256);

    let shared = Arc::new(Mutex::new(SharedClientState {
        response_waiters: HashMap::new(),
        body_streams: HashMap::new(),
        ws_sessions: HashMap::new(),
    }));

    {
        let mut guard = state.client.lock().await;
        guard.take();
        *guard = Some(ClientHandle {
            tx: client_tx.clone(),
            shared: Arc::clone(&shared),
        });
    }

    let shared_for_writer = Arc::clone(&shared);

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(text) => {
                    if ws_tx
                        .send(WsMessage::Text(text.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => error!("Serialize error: {}", e),
            }
        }
        let mut s = shared_for_writer.lock().await;
        s.response_waiters.clear();
        s.body_streams.clear();
        s.ws_sessions.clear();
    });

    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            WsMessage::Text(text) => match serde_json::from_str::<TunnelMessage>(&text) {
                Ok(tunnel_msg) => {
                    dispatch_tunnel_message(tunnel_msg, Arc::clone(&shared)).await;
                }
                Err(e) => warn!("Bad tunnel message: {}", e),
            },
            WsMessage::Close(_) => break,
            _ => {}
        }
    }

    info!("Tunnel client disconnected");

    {
        let mut guard = state.client.lock().await;
        guard.take();
    }

    writer_task.abort();
}

async fn dispatch_tunnel_message(msg: TunnelMessage, shared: Arc<Mutex<SharedClientState>>) {
    match msg {
        TunnelMessage::HttpResponse {
            id,
            status,
            headers,
            body,
        } => {
            let mut s = shared.lock().await;
            if let Some(tx) = s.response_waiters.remove(&id) {
                let _ = tx.send(ResponseKind::Full {
                    status,
                    headers,
                    body,
                });
            }
        }

        TunnelMessage::HttpResponseChunk {
            id,
            status,
            headers,
        } => {
            let (body_tx, body_rx) = mpsc::channel::<StreamChunk>(64);
            let mut s = shared.lock().await;
            if let Some(tx) = s.response_waiters.remove(&id) {
                s.body_streams.insert(id, body_tx);
                let _ = tx.send(ResponseKind::Streaming {
                    status,
                    headers,
                    body_rx,
                });
            }
        }

        TunnelMessage::HttpResponseBodyChunk { id, data, done } => {
            let mut s = shared.lock().await;
            if let Some(body_tx) = s.body_streams.get(&id) {
                let _ = body_tx.send(StreamChunk { data, done }).await;
                if done {
                    s.body_streams.remove(&id);
                }
            }
        }

        // Tunnel client is forwarding a WebSocket message from upstream → browser.
        TunnelMessage::WsData {
            id,
            data,
            is_binary,
        } => {
            let s = shared.lock().await;
            if let Some(tx) = s.ws_sessions.get(&id) {
                let _ = tx
                    .send(IncomingWsFrame {
                        data,
                        is_binary,
                        close: false,
                    })
                    .await;
            }
        }

        // Tunnel client signals upstream WebSocket closure.
        TunnelMessage::WsClose { id } => {
            let mut s = shared.lock().await;
            if let Some(tx) = s.ws_sessions.remove(&id) {
                let _ = tx
                    .send(IncomingWsFrame {
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

// ── Proxy handler (HTTP + incoming WebSocket) ────────────────────────────────

async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    // Extract parts before consuming request, so we can check for WS upgrade.
    let (mut parts, body) = req.into_parts();

    // Try WebSocket upgrade detection via FromRequestParts (only needs headers).
    match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws_upgrade) => {
            // Extract path and headers from parts (no Body needed for WS path/headers).
            let path = parts
                .uri
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| "/".to_string());
            let mut headers: Headers = HashMap::new();
            for (name, value) in &parts.headers {
                if let Ok(v) = value.to_str() {
                    headers
                        .entry(name.as_str().to_lowercase())
                        .or_default()
                        .push(v.to_string());
                }
            }
            handle_incoming_ws_upgrade(ws_upgrade, state, path, headers).await
        }
        Err(_) => {
            let req = Request::from_parts(parts, body);
            handle_http_proxy(state, req).await
        }
    }
}

/// Handle an incoming WebSocket upgrade by opening a WsOpen session through the tunnel.
async fn handle_incoming_ws_upgrade(
    ws_upgrade: WebSocketUpgrade,
    state: Arc<AppState>,
    path: String,
    headers: Headers,
) -> Response {
    let guard = state.client.lock().await;
    let Some(client) = guard.as_ref() else {
        return (StatusCode::BAD_GATEWAY, "No tunnel client connected").into_response();
    };

    let client_tx = client.tx.clone();
    let shared = Arc::clone(&client.shared);
    drop(guard);

    let id = Uuid::new_v4().to_string();

    let (browser_tx, mut browser_rx) = mpsc::channel::<IncomingWsFrame>(64);
    {
        let mut s = shared.lock().await;
        s.ws_sessions.insert(id.clone(), browser_tx);
    }

    if client_tx
        .send(TunnelMessage::WsOpen {
            id: id.clone(),
            path,
            headers,
        })
        .await
        .is_err()
    {
        let mut s = shared.lock().await;
        s.ws_sessions.remove(&id);
        return (StatusCode::BAD_GATEWAY, "Tunnel client disconnected").into_response();
    }

    let id_clone = id.clone();
    ws_upgrade.on_upgrade(move |browser_ws| async move {
        info!("Incoming WebSocket session {} opened", id_clone);

        let (mut browser_sink, mut browser_stream) = browser_ws.split();
        let client_tx_clone = client_tx.clone();
        let id_for_reader = id_clone.clone();

        // Browser → tunnel task.
        let browser_to_tunnel = tokio::spawn(async move {
            while let Some(Ok(msg)) = browser_stream.next().await {
                match msg {
                    WsMessage::Text(text) => {
                        let _ = client_tx_clone
                            .send(TunnelMessage::WsData {
                                id: id_for_reader.clone(),
                                data: encode_base64(text.as_bytes()),
                                is_binary: false,
                            })
                            .await;
                    }
                    WsMessage::Binary(bin) => {
                        let _ = client_tx_clone
                            .send(TunnelMessage::WsData {
                                id: id_for_reader.clone(),
                                data: encode_base64(&bin),
                                is_binary: true,
                            })
                            .await;
                    }
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
            let _ = client_tx_clone
                .send(TunnelMessage::WsClose {
                    id: id_for_reader,
                })
                .await;
        });

        // Tunnel → browser task.
        let tunnel_to_browser = tokio::spawn(async move {
            while let Some(frame) = browser_rx.recv().await {
                if frame.close {
                    break;
                }
                let payload = decode_base64(&frame.data).unwrap_or_default();
                let ws_msg = if frame.is_binary {
                    WsMessage::Binary(payload.into())
                } else {
                    WsMessage::Text(String::from_utf8_lossy(&payload).to_string().into())
                };
                if browser_sink.send(ws_msg).await.is_err() {
                    break;
                }
            }
            let _ = browser_sink.close().await;
        });

        tokio::select! {
            _ = browser_to_tunnel => {}
            _ = tunnel_to_browser => {}
        }

        // Clean up session entry if it wasn't already removed.
        shared.lock().await.ws_sessions.remove(&id_clone);
        info!("Incoming WebSocket session {} closed", id_clone);
    })
}

async fn handle_http_proxy(state: Arc<AppState>, req: Request<Body>) -> Response {
    let guard = state.client.lock().await;
    let Some(client) = guard.as_ref() else {
        return (StatusCode::BAD_GATEWAY, "No tunnel client connected").into_response();
    };

    let client_tx = client.tx.clone();
    let shared = Arc::clone(&client.shared);
    drop(guard);

    let id = Uuid::new_v4().to_string();
    let method = req.method().to_string();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let mut headers: Headers = HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers
                .entry(name.as_str().to_lowercase())
                .or_default()
                .push(v.to_string());
        }
    }

    let body_bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let body = if body_bytes.is_empty() {
        None
    } else {
        Some(encode_base64(&body_bytes))
    };

    let (resp_tx, resp_rx) = oneshot::channel::<ResponseKind>();
    {
        let mut s = shared.lock().await;
        s.response_waiters.insert(id.clone(), resp_tx);
    }

    let tunnel_msg = TunnelMessage::HttpRequest {
        id: id.clone(),
        method,
        path: path_and_query,
        headers,
        body,
    };

    if client_tx.send(tunnel_msg).await.is_err() {
        let mut s = shared.lock().await;
        s.response_waiters.remove(&id);
        return (StatusCode::BAD_GATEWAY, "Tunnel client disconnected").into_response();
    }

    match timeout(Duration::from_secs(30), resp_rx).await {
        Ok(Ok(ResponseKind::Full {
            status,
            headers,
            body,
        })) => build_full_response(status, headers, body),

        Ok(Ok(ResponseKind::Streaming {
            status,
            headers,
            body_rx,
        })) => build_streaming_response(status, headers, body_rx),

        Ok(Err(_)) => {
            (StatusCode::BAD_GATEWAY, "Tunnel client disconnected mid-request").into_response()
        }
        Err(_) => {
            let mut s = shared.lock().await;
            s.response_waiters.remove(&id);
            (StatusCode::GATEWAY_TIMEOUT, "Upstream timeout").into_response()
        }
    }
}

fn build_full_response(status: u16, headers: Headers, body: Option<String>) -> Response {
    let status_code =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let body_bytes: Bytes = body
        .as_deref()
        .map(|b| decode_base64(b).unwrap_or_default())
        .unwrap_or_default()
        .into();

    let mut builder = Response::builder().status(status_code);
    for (name, values) in &headers {
        for value in values {
            if let (Ok(n), Ok(v)) = (HeaderName::from_str(name), HeaderValue::from_str(value)) {
                builder = builder.header(n, v);
            }
        }
    }

    builder
        .body(Body::from(body_bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn build_streaming_response(
    status: u16,
    headers: Headers,
    body_rx: mpsc::Receiver<StreamChunk>,
) -> Response {
    let status_code =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let stream = futures_util::stream::unfold(body_rx, |mut rx| async move {
        let chunk = rx.recv().await?;
        if chunk.done {
            return None;
        }
        let data = decode_base64(&chunk.data).unwrap_or_default();
        Some((Ok::<Bytes, std::io::Error>(Bytes::from(data)), rx))
    });

    let mut builder = Response::builder().status(status_code);
    for (name, values) in &headers {
        for value in values {
            if let (Ok(n), Ok(v)) = (HeaderName::from_str(name), HeaderValue::from_str(value)) {
                builder = builder.header(n, v);
            }
        }
    }

    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
