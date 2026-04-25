use std::io;

// ── Frame types ──────────────────────────────────────────────────────────────

/// Server → Client: a new TCP connection has been accepted.  Payload is empty.
pub const FRAME_OPEN: u8 = 0x01;
/// Bidirectional: raw TCP bytes.  Payload is the data.
pub const FRAME_DATA: u8 = 0x02;
/// Bidirectional: TCP connection closed.  Payload is empty.
pub const FRAME_CLOSE: u8 = 0x03;

// ── Wire layout ──────────────────────────────────────────────────────────────
//
//  +──────────────────+──────────────+──────────────────+──────────────────+
//  │  conn_id (4 B BE)│ type (1 B)   │  payload_len (4 B BE)│ payload (N B)  │
//  +──────────────────+──────────────+──────────────────+──────────────────+
//
//  Total header = 9 bytes.

pub const FRAME_HEADER_LEN: usize = 9;

// ── Frame ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Frame {
    pub conn_id: u32,
    pub frame_type: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn open(conn_id: u32) -> Self {
        Frame {
            conn_id,
            frame_type: FRAME_OPEN,
            payload: Vec::new(),
        }
    }

    pub fn data(conn_id: u32, payload: Vec<u8>) -> Self {
        Frame {
            conn_id,
            frame_type: FRAME_DATA,
            payload,
        }
    }

    pub fn close(conn_id: u32) -> Self {
        Frame {
            conn_id,
            frame_type: FRAME_CLOSE,
            payload: Vec::new(),
        }
    }

    /// Encode the frame into a byte buffer suitable for sending as a binary WebSocket message.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + self.payload.len());
        buf.extend_from_slice(&self.conn_id.to_be_bytes());
        buf.push(self.frame_type);
        buf.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode a frame from a byte slice.
    pub fn decode(bytes: &[u8]) -> Result<Self, io::Error> {
        if bytes.len() < FRAME_HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame too short: {} bytes", bytes.len()),
            ));
        }
        let conn_id = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let frame_type = bytes[4];
        let payload_len = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;
        if bytes.len() < FRAME_HEADER_LEN + payload_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame payload truncated",
            ));
        }
        let payload = bytes[FRAME_HEADER_LEN..FRAME_HEADER_LEN + payload_len].to_vec();
        Ok(Frame {
            conn_id,
            frame_type,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_open() {
        let f = Frame::open(42);
        let enc = f.encode();
        let dec = Frame::decode(&enc).unwrap();
        assert_eq!(dec.conn_id, 42);
        assert_eq!(dec.frame_type, FRAME_OPEN);
        assert!(dec.payload.is_empty());
    }

    #[test]
    fn roundtrip_data() {
        let payload = b"hello world".to_vec();
        let f = Frame::data(7, payload.clone());
        let enc = f.encode();
        let dec = Frame::decode(&enc).unwrap();
        assert_eq!(dec.conn_id, 7);
        assert_eq!(dec.frame_type, FRAME_DATA);
        assert_eq!(dec.payload, payload);
    }

    #[test]
    fn roundtrip_close() {
        let f = Frame::close(1);
        let enc = f.encode();
        let dec = Frame::decode(&enc).unwrap();
        assert_eq!(dec.conn_id, 1);
        assert_eq!(dec.frame_type, FRAME_CLOSE);
        assert!(dec.payload.is_empty());
    }

    #[test]
    fn decode_too_short() {
        assert!(Frame::decode(&[0u8; 4]).is_err());
    }
}
