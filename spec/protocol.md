# Protocol Design

The server and client communicate over a **WebSocket** connection (for TLS and proxy traversal support), using **binary WebSocket frames** only. Each binary WebSocket message carries exactly one tunnel frame.

## Binary Frame Format

All integers are big-endian.

```
+──────────────────+──────────────+────────────────────+──────────────────+
│  conn_id  (4 B)  │  type  (1 B) │  payload_len  (4 B) │  payload  (N B)  │
+──────────────────+──────────────+────────────────────+──────────────────+
```

Total header size: **9 bytes**.

### Field definitions

| Field         | Size   | Description                                    |
|---------------|--------|------------------------------------------------|
| `conn_id`     | 4 B    | Unique connection identifier (assigned by server, wraps around) |
| `type`        | 1 B    | Frame type (see below)                         |
| `payload_len` | 4 B    | Length of the payload in bytes                 |
| `payload`     | N B    | Raw bytes (empty for OPEN/CLOSE, 4 bytes for ACK) |

## Frame Types

| Value  | Name    | Direction          | Description                                           |
|--------|---------|--------------------|-------------------------------------------------------|
| `0x01` | `OPEN`  | Server → Client    | A new TCP connection has been accepted by the server. |
| `0x02` | `DATA`  | Bidirectional      | Raw TCP bytes for the identified connection.          |
| `0x03` | `CLOSE` | Bidirectional      | The TCP connection has been (or should be) closed.   |
| `0x04` | `ACK`   | Bidirectional      | Acknowledge that `DATA` frames were written to TCP.  |

### ACK payload

ACK payload is a 4-byte big-endian unsigned integer (`u32`) representing the
number of DATA messages that were successfully written to the local TCP stream
for `conn_id` since the previous ACK.

The sender of DATA decrements its per-connection in-flight counter only when it
receives ACK from the peer, which enables per-connection back pressure without
coupling unrelated streams.

## Flow

```
External client          expose-server          expose-client         Upstream TCP
      │                       │                       │                     │
      │──── TCP connect ──────▶│                       │                     │
      │                       │── OPEN(conn_id=1) ────▶│                     │
      │                       │                       │──── TCP connect ────▶│
      │──── bytes ────────────▶│                       │                     │
      │                       │── DATA(conn_id=1, …) ─▶│                     │
      │                       │                       │──── bytes ──────────▶│
      │                       │                       │◀─── bytes ───────────│
      │                       │◀─ DATA(conn_id=1, …) ──│                     │
      │◀─── bytes ─────────────│                       │                     │
      │──── TCP close ─────────▶│                       │                     │
      │                       │── CLOSE(conn_id=1) ───▶│                     │
      │                       │                       │──── TCP close ───────▶│
```

## Design properties

- **Protocol neutral**: the server and client relay raw TCP byte streams; neither inspects the application protocol (HTTP, WebSocket, gRPC, SSH, etc.).
- **Binary encoding**: no JSON, no base64; frames are compact binary.
- **Multiplexed**: many TCP connections are multiplexed over the single tunnel WebSocket.
- **Buffering**: the client buffers DATA frames that arrive while the upstream TCP connection is being established.
- **Single tunnel**: the server allows only one tunnel client at a time; a new connection atomically replaces the previous one.
