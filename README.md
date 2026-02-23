# HTTP Socks5 Bridge proxy

An app that tunnels TCP connections over HTTP polling, split into three components that can run on different machines. The only requirement is HTTP connectivity between them.

## ⚠️ WARNING: THIS IS A TOY PROJECT ⚠️

This was vibe-coded in a single chat session with an AI for fun and learning. **Do not use this for anything.** Not for production. Not for staging.

## Components

| Component | Language | Role |
|-----------|----------|------|
| **http-bridge-client** | Rust | Local SOCKS5 proxy. Accepts connections from browsers/curl, translates them into HTTP API calls |
| **bridge_server.py** | Python | Central relay server. Buffers data between the SOCKS5 side and the TCP side via a simple HTTP API |
| **http-bridge-server** | Rust | TCP connector. Auto-discovers sessions, opens real TCP connections to targets, relays data through the bridge |

## Full end-to-end flow now

```bash
# Terminal 1: Python bridge server
python bridge_server.py

# Terminal 2: http-bridge-server (just sits and watches for new sessions)
cd http-bridge-server && cargo run

# Terminal 3: SOCKS5 proxy
cd http-bridge-client && cargo run

# Terminal 4: test
curl --socks5 127.0.0.1:1080 http://example.com
```

## How It Works

1. **Firefox** connects to the local SOCKS5 proxy and requests a connection to `example.com:443`
2. **http-bridge-client** calls `POST /session/create` on the bridge server with the target address
3. **bridge_server.py** creates a new session and returns a unique `session_id`
4. **http-bridge-server** periodically polls `GET /debug/sessions`, discovers the new session
5. **http-bridge-server** opens a real TCP connection to `example.com:443`
6. Data flows bidirectionally:
   - **Browser → SOCKS5 → HTTP POST /debug/{id}/send → bridge → POST /session/{id}/read → tcp-http-bridge → TCP → remote**
   - **Remote → TCP → tcp-http-bridge → POST /session/{id}/write → bridge → GET /debug/{id}/recv → SOCKS5 → browser**