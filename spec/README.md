# expose-rs

A tool to expose any local TCP-based service to the internet through a protocol-neutral reverse tunnel.

## Architecture

expose-rs consists of two binaries:

- **expose-server**: Runs on a public server. Accepts raw TCP connections and forwards their byte streams through the tunnel WebSocket to the client.
- **expose-client**: Runs locally. Receives TCP byte streams from the server over the tunnel and forwards them to the upstream service.

Neither binary inspects or parses application-level protocols (HTTP, WebSocket, gRPC, SSH, etc.). The tunnel is completely protocol neutral.

## Quick Start

### Server

```bash
expose-server --host 0.0.0.0 --port 8080 --secret-token mysecret
```

### Client

```bash
expose-client --server ws://yourserver:8080/mysecret --upstream localhost:3000
```

For TLS between client and server (server is behind a TLS-terminating reverse proxy):

```bash
expose-client --server wss://yourserver/mysecret --upstream localhost:3000
```

## Features

- **Protocol neutral**: works with any TCP-based protocol (HTTP/1.1, HTTP/2, WebSocket, SSE, gRPC, SSH, …)
- **Binary tunnel frames**: compact binary encoding, no JSON or base64 overhead
- **Multiplexed**: many simultaneous connections over a single WebSocket tunnel
- **Single-tunnel model**: new tunnel client replaces the previous one
- **TLS support**: client→server tunnel supports wss:// via native-tls
- **Reconnection**: exponential backoff (1 s → 60 s max)
