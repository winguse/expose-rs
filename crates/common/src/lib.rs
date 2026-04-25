use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type Headers = HashMap<String, Vec<String>>;

/// Messages sent from Server → Client and Client → Server over the tunnel WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TunnelMessage {
    /// Server → Client: a new HTTP request to proxy.
    HttpRequest {
        id: String,
        method: String,
        path: String,
        headers: Headers,
        /// Base64-encoded body, or null for bodyless requests.
        body: Option<String>,
    },
    /// Client → Server: a complete HTTP response.
    HttpResponse {
        id: String,
        status: u16,
        headers: Headers,
        /// Base64-encoded body, or null.
        body: Option<String>,
    },
    /// Client → Server: first frame of a streaming response (SSE etc.), carries status + headers.
    HttpResponseChunk {
        id: String,
        status: u16,
        headers: Headers,
    },
    /// Client → Server: subsequent body chunk for a streaming response.
    HttpResponseBodyChunk {
        id: String,
        /// Base64-encoded chunk data.
        data: String,
        /// True when this is the final chunk (data will be empty).
        done: bool,
    },
    /// Server → Client: open a proxied WebSocket session.
    WsOpen {
        id: String,
        path: String,
        headers: Headers,
    },
    /// Bidirectional: a WebSocket message frame.
    WsData {
        id: String,
        /// Base64-encoded frame payload.
        data: String,
        is_binary: bool,
    },
    /// Bidirectional: close a WebSocket session.
    WsClose { id: String },
}

impl TunnelMessage {
    pub fn id(&self) -> &str {
        match self {
            TunnelMessage::HttpRequest { id, .. } => id,
            TunnelMessage::HttpResponse { id, .. } => id,
            TunnelMessage::HttpResponseChunk { id, .. } => id,
            TunnelMessage::HttpResponseBodyChunk { id, .. } => id,
            TunnelMessage::WsOpen { id, .. } => id,
            TunnelMessage::WsData { id, .. } => id,
            TunnelMessage::WsClose { id } => id,
        }
    }
}

pub fn encode_base64(data: &[u8]) -> String {
    STANDARD.encode(data)
}

pub fn decode_base64(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD.decode(s)
}
