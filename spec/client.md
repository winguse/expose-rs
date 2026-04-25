# Client Specification

## CLI

```
expose-client [OPTIONS]

Options:
  --server <URL>      Server WebSocket URL (e.g. wss://host/secret) [required]
  --upstream <URL>    Upstream HTTP base URL (e.g. http://localhost:3000) [required]
```

## Behavior

1. Connects to the server WebSocket URL.
2. Loops receiving messages and dispatches each concurrently.
3. Reconnects on disconnect with exponential backoff (1 s → 2 s → 4 s … max 60 s).

## HttpRequest Handling

1. Reconstruct the full upstream URL from `--upstream` base + `path` from the message.
2. Issue an HTTP request using `reqwest`.
3. Inspect the response `content-type` header.
   - If `text/event-stream` (SSE) or any streaming response: use chunked protocol.
   - Otherwise: read full body, send `HttpResponse`.

## Streaming Response

1. Send `HttpResponseChunk` with status and headers.
2. Stream body chunks as `HttpResponseBodyChunk { data, done: false }`.
3. Send final `HttpResponseBodyChunk { data: "", done: true }`.

## WebSocket Proxying

When `WsOpen` is received:

1. Connect to upstream WebSocket at the given path.
2. Relay `WsData` frames in both directions using separate tasks.
3. Send `WsClose` to server when the upstream WebSocket closes.
4. Close upstream WebSocket when `WsClose` is received from server.

## Error Handling

- Log errors and send a 502 `HttpResponse` when upstream request fails.
- Backoff-based reconnection to server on connection loss.
