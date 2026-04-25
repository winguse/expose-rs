# expose-rs

A protocol-neutral TCP reverse tunnel written in Rust.

Expose any local TCP-based service (HTTP, WebSocket, gRPC, SSH, …) to the internet through a public server — without the server or client needing to understand the application protocol.

## How it works

```
Internet → expose-server (public) ──[WebSocket tunnel]──► expose-client (local) → upstream service
```

The server accepts raw TCP connections and forwards their byte streams to the client over a binary-framed WebSocket tunnel. The client connects each stream to the upstream service. Neither side parses the application protocol.

## Quick Start

**Server** (on your public machine):

```bash
expose-server --host 0.0.0.0 --port 8080 --secret-token mysecret
```

**Client** (on the machine with the service to expose):

```bash
expose-client --server ws://yourserver:8080/mysecret --upstream localhost:3000
```

With TLS (server is behind nginx/caddy):

```bash
expose-client --server wss://yourserver/mysecret --upstream localhost:3000
```

## CLI reference

### expose-server

| Flag             | Default     | Description                   |
|------------------|-------------|-------------------------------|
| `--host`         | `0.0.0.0`   | Address to listen on          |
| `--port`         | `8080`      | Port to listen on             |
| `--secret-token` | (required)  | Secret path segment for tunnel |

### expose-client

| Flag         | Default    | Description                              |
|--------------|------------|------------------------------------------|
| `--server`   | (required) | Tunnel WebSocket URL (`ws://` or `wss://`) |
| `--upstream` | (required) | Upstream TCP address (`host:port`)       |

## Design

See [`spec/`](spec/) for full documentation:

- [`spec/README.md`](spec/README.md) — overview
- [`spec/protocol.md`](spec/protocol.md) — binary frame protocol
- [`spec/server.md`](spec/server.md) — server specification
- [`spec/client.md`](spec/client.md) — client specification
