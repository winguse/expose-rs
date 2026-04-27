# Client Specification

## CLI

```
expose-client [OPTIONS]

Options:
  --server <URL>        Tunnel server WebSocket URL (e.g. ws://host:8080/secret or wss://host/secret) [required]
  --upstream <ADDR>     Upstream TCP address to forward connections to (e.g. localhost:3000) [required]
```

## Behavior

1. Connects to the server tunnel endpoint via WebSocket (supports ws:// and wss://).
2. Receives binary tunnel frames and dispatches them.
3. Reconnects on disconnect with exponential backoff (1 s → 2 s → 4 s … max 60 s).

## OPEN Frame Handling

When an `OPEN` frame is received for `conn_id`:

1. Register the connection immediately as `Connecting` (buffering subsequent DATA frames).
2. Spawn a task to `TcpStream::connect(upstream)`.
3. On success: flush buffered DATA, transition state to `Connected`, relay data bidirectionally.
4. On failure: remove entry, send `CLOSE` frame back to server.

## DATA Frame Handling

- If connection is `Connecting`: append payload to buffer.
- If connection is `Connected`: send payload into the upstream writer channel.
- If connection is unknown: log a warning and discard.
- After data is successfully written to upstream TCP, send an `ACK` frame back to
  the server indicating how many `DATA` frames were applied to the socket.

## CLOSE Frame Handling

Remove the connection entry from the map (the writer task will notice the channel is closed and shut down the upstream TCP socket).

## Per-Connection Flow

For each upstream TCP connection:

- **Reader task**: read from upstream TCP, send `DATA` frames to server, send
  `CLOSE` on EOF/error.
- **Writer task**: receive `DATA` payloads, write to upstream TCP, emit `ACK`
  frames after successful writes, shutdown on channel close.

## Error Handling

- Upstream connection failure: send `CLOSE` back to server.
- Tunnel WebSocket error: log and reconnect with backoff.
- All connection entries are cleared on tunnel disconnect.
