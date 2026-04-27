# Server Specification

## CLI

```
expose-server [OPTIONS]

Options:
  --host <HOST>               Listen host [default: 0.0.0.0]
  --port <PORT>               Listen port [default: 8080]
  --secret-token <TOKEN>      Secret token path segment [required]
  --max-inflight-to-tunnel-per-connection <N>    Max unacked DATA frames sent to client per connection [default: 256]
  --max-inflight-from-tunnel-per-connection <N>  Max unacked DATA frames received from client per connection [default: 256]
```

## Behavior

1. Opens a raw `TcpListener` on `<host>:<port>`.
2. For every incoming TCP connection, peeks at the first bytes to check whether it is the tunnel client.
   - **Tunnel client**: HTTP `GET /<secret-token> HTTP/1.x` WebSocket upgrade → perform WebSocket handshake, set up tunnel.
   - **Proxied connection**: any other byte stream → assign a `conn_id`, forward raw bytes through the tunnel.

## Tunnel Client Management

- Only one tunnel client may be connected at a time.
- When a new client connects, the previous client's sender is replaced and all tracked connections are cleared.
- The tunnel transport is a binary WebSocket connection (see `spec/protocol.md`).

## Proxied Connection Flow

For every non-tunnel TCP connection:

1. Assign a monotonically increasing `conn_id` (u32, wraps).
2. Send an `OPEN` frame to the tunnel client.
3. Spawn two tasks:
   - **Reader**: read from the TCP socket, send `DATA` frames through the tunnel.
   - **Writer**: receive `DATA` frames from the tunnel, write to the TCP socket.
4. When either side closes, send a `CLOSE` frame and clean up the connection entry.

## Per-Connection Backpressure

- The server enforces per-connection in-flight limits independently in each direction.
- Sending side consumes one credit per `DATA` frame sent.
- The credit is returned only when an `ACK(conn_id, count)` frame is received from the peer.
- `ACK` is emitted by the receiving side after bytes are successfully written into the local TCP stream.

## Error Handling

- If no tunnel client is connected when a proxied connection arrives, the TCP connection is dropped immediately.
- If the tunnel client disconnects, all active proxied connections are cleaned up.
