# Protocol Design

All messages are sent as WebSocket **text frames**, JSON-encoded.

## Message Types

### Server → Client: HttpRequest

Sent when a new HTTP request arrives at the server that needs to be proxied.

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "type": "HttpRequest",
  "method": "GET",
  "path": "/some/path?query=value",
  "headers": {
    "host": ["example.com"],
    "accept": ["*/*"]
  },
  "body": null
}
```

`body` is base64-encoded or `null` for bodyless requests.

### Client → Server: HttpResponse

Sent in response to an HttpRequest (non-streaming).

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "type": "HttpResponse",
  "status": 200,
  "headers": {
    "content-type": ["text/html"]
  },
  "body": "PGh0bWw+..."
}
```

### Client → Server: HttpResponseChunk (streaming first frame)

Sent when the upstream response is streaming (e.g., SSE). Carries status and headers.

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "type": "HttpResponseChunk",
  "status": 200,
  "headers": {
    "content-type": ["text/event-stream"]
  }
}
```

### Client → Server: HttpResponseBodyChunk (streaming data frame)

Subsequent frames carrying streamed body data.

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "type": "HttpResponseBodyChunk",
  "data": "ZGF0YTogaGVsbG8K",
  "done": false
}
```

When `done` is `true`, `data` is empty and the stream is complete.

### Server → Client: WsOpen

Sent when an incoming HTTP connection is a WebSocket upgrade request.

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "type": "WsOpen",
  "path": "/ws-path",
  "headers": {
    "sec-websocket-protocol": ["chat"]
  }
}
```

### Server → Client / Client → Server: WsData

Carries a WebSocket message frame in either direction.

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "type": "WsData",
  "data": "aGVsbG8=",
  "is_binary": false
}
```

### Server → Client / Client → Server: WsClose

Signals WebSocket connection closure.

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "type": "WsClose"
}
```

## Multiplexing

Multiple HTTP requests and WebSocket sessions are multiplexed over the single control WebSocket. The `id` field uniquely identifies each request/session, allowing concurrent in-flight requests.
