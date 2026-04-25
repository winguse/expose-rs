# expose-rs

A tool to expose a local web service to the internet through a reverse tunnel.

## Architecture

expose-rs consists of two binaries:

- **expose-server**: Runs on a public server, accepts WebSocket tunnel connections and proxies HTTP requests
- **expose-client**: Runs locally, connects to the server via WebSocket and proxies requests to your upstream service

## Quick Start

### Server

```bash
expose-server --host 0.0.0.0 --port 8080 --secret-token mysecret
```

### Client

```bash
expose-client --server ws://yourserver:8080/mysecret --upstream http://localhost:3000
```

## Features

- HTTP proxying (full request/response)
- WebSocket proxying (bidirectional relay)
- HTTP SSE / streaming support via chunked protocol
- Single-client model: new connection terminates previous
- Timeout handling for pending requests (30 seconds)
- Reconnection with exponential backoff
