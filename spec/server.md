# Server Specification

## CLI

```
expose-server [OPTIONS]

Options:
  --host <HOST>               Listen host [default: 0.0.0.0]
  --port <PORT>               Listen port [default: 8080]
  --secret-token <TOKEN>      Secret token path segment [required]
```

## Behavior

1. Starts an axum HTTP server on `<host>:<port>`.
2. `GET /<secret-token>` — WebSocket upgrade endpoint for tunnel clients.
3. All other routes — HTTP proxy to the connected tunnel client.

## Tunnel Client Management

- Only one tunnel client may be connected at a time.
- When a new client connects, the previous client connection is closed.
- The server maintains `Arc<Mutex<Option<ClientHandle>>>` shared state.

## Request Proxying

For every incoming HTTP request (non-tunnel):

1. Generate a UUID for the request.
2. Register a oneshot channel in the pending-requests map keyed by UUID.
3. Send an `HttpRequest` message to the client.
4. Await response on the oneshot channel with a 30-second timeout.
5. Return the resulting HTTP response (or 504 on timeout, 502 on no client).

## Streaming

When the client responds with `HttpResponseChunk`, the server begins streaming the body back to the original caller as it receives `HttpResponseBodyChunk` frames until `done: true`.

## WebSocket Proxying

When the incoming request contains a WebSocket upgrade header:

1. Send `WsOpen` to the client.
2. Relay `WsData` frames bidirectionally.
3. Send `WsClose` when either side closes.

## Error Handling

- 502 Bad Gateway when no client is connected.
- 504 Gateway Timeout when the request times out (30 s).
- Connection errors logged and cleaned up gracefully.
