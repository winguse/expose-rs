# Server Specification

## CLI

```
expose-server [OPTIONS]

Options:
  --host <HOST>               Listen host [default: 0.0.0.0]
  --port <PORT>               Listen port [default: 8080]
  --secret-token <TOKEN>      Secret token path segment [required]
  --max-pending-messages-per-connection <N>      Max DATA frames proxied TCP→tunnel per connection until client ACK (written to upstream TCP) [default: 256]
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

Only one hop per direction applies the window:

- **Visitor → tunnel → client → upstream**: The **server** limits how many `DATA` frames it reads from the proxied TCP and sends on the tunnel until the client sends `ACK` after writing to upstream TCP.
- **Upstream → client → tunnel → visitor**: The **client** limits how many `DATA` frames it reads from upstream and sends until the server sends `ACK` after writing to the visitor TCP.

Each `DATA` consumes one credit (`acquire` + `forget`); the peer returns exactly that many credits with `ACK(conn_id, count)` after successfully writing to its local TCP. The client and server each keep a pending window and `ACK_BATCH_SIZE` (64) as in `expose_common`.

## Error Handling

- If no tunnel client is connected when a proxied connection arrives, the TCP connection is dropped immediately.
- If the tunnel client disconnects, all active proxied connections are cleaned up.
