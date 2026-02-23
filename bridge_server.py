#!/usr/bin/env python3
"""
HTTP Bridge Server for tcp-http-bridge Rust client.

Endpoints used by Rust client:
  POST /session/<id>/read   — client polls this to get data TO SEND to TCP
  POST /session/<id>/write  — client pushes data it READ from TCP

Session management:
  POST /session/create       — create a new session, returns session ID

Debug endpoints (use from curl):
  POST /debug/<id>/send     — inject data that the Rust client will pick up on next /read poll
  GET  /debug/<id>/recv     — see what the Rust client has written (data it read from TCP)
  GET  /debug/<id>/status   — show buffer sizes and session info
  GET  /debug/sessions      — list all active sessions

Usage:
  python bridge_server.py [--port 8080]

Examples with curl:
  # Create a new session
  curl -X POST http://localhost:8080/session/create -H 'Content-Type: application/json' -d '{"target_addr": "example.com", "target_port": 80}'

  # Send "hello" to the Rust client (it will write this to the TCP socket)
  curl -X POST http://localhost:8080/debug/324/send -H 'Content-Type: application/json' -d '{"text": "hello"}'

  # Send raw base64 data
  curl -X POST http://localhost:8080/debug/324/send -H 'Content-Type: application/json' -d '{"data": "aGVsbG8="}'

  # See what the Rust client has forwarded from TCP
  curl http://localhost:8080/debug/324/recv

  # See what the Rust client has forwarded, and clear the buffer
  curl http://localhost:8080/debug/324/recv?clear=1

  # Check session status
  curl http://localhost:8080/debug/324/status

  # List all sessions
  curl http://localhost:8080/debug/sessions
"""

import argparse
import base64
import json
import logging
import threading
import uuid
from collections import deque
from datetime import datetime, timezone
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse, parse_qs

logging.basicConfig(
    level=logging.DEBUG,
    format="%(asctime)s [%(levelname)s] %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("bridge")


class Session:
    """Holds two buffers per session:
    - to_client:   data queued for the Rust client (it picks up via POST /session/<id>/read)
    - from_client: data the Rust client sent us (it pushes via POST /session/<id>/write)
    """

    def __init__(self, session_id: str, target_addr: str = "", target_port: int = 0):
        self.session_id = session_id
        self.target_addr = target_addr
        self.target_port = target_port
        self.lock = threading.Lock()
        self.to_client: deque[bytes] = deque()
        self.from_client: deque[bytes] = deque()
        self.stats = {
            "created_at": datetime.now(timezone.utc).isoformat(),
            "reads": 0,
            "reads_with_data": 0,
            "writes": 0,
            "bytes_to_client": 0,
            "bytes_from_client": 0,
        }

    def enqueue_for_client(self, data: bytes):
        """Queue data that the Rust client will receive on next /read poll."""
        with self.lock:
            self.to_client.append(data)
            self.stats["bytes_to_client"] += len(data)

    def dequeue_for_client(self) -> bytes | None:
        """Dequeue data for the Rust client. Returns None if empty."""
        with self.lock:
            self.stats["reads"] += 1
            if self.to_client:
                chunks = []
                while self.to_client:
                    chunks.append(self.to_client.popleft())
                data = b"".join(chunks)
                self.stats["reads_with_data"] += 1
                return data
            return None

    def store_from_client(self, data: bytes):
        """Store data that the Rust client wrote (read from TCP)."""
        with self.lock:
            self.from_client.append(data)
            self.stats["writes"] += 1
            self.stats["bytes_from_client"] += len(data)

    def drain_from_client(self, clear: bool = False) -> list[bytes]:
        """Get all data the client has written. Optionally clear the buffer."""
        with self.lock:
            result = list(self.from_client)
            if clear:
                self.from_client.clear()
            return result

    def get_status(self) -> dict:
        with self.lock:
            return {
                "session_id": self.session_id,
                "target_addr": self.target_addr,
                "target_port": self.target_port,
                "to_client_queued_chunks": len(self.to_client),
                "from_client_buffered_chunks": len(self.from_client),
                **self.stats,
            }


class SessionStore:
    def __init__(self):
        self.lock = threading.Lock()
        self.sessions: dict[str, Session] = {}

    def create(self, target_addr: str, target_port: int) -> Session:
        with self.lock:
            session_id = uuid.uuid4().hex[:12]
            session = Session(session_id, target_addr, target_port)
            self.sessions[session_id] = session
            log.info(
                f"Created session {session_id} for {target_addr}:{target_port}"
            )
            return session

    def get(self, session_id: str) -> Session | None:
        with self.lock:
            return self.sessions.get(session_id)

    def get_or_create(self, session_id: str) -> Session:
        with self.lock:
            if session_id not in self.sessions:
                log.info(f"Auto-creating session: {session_id}")
                self.sessions[session_id] = Session(session_id)
            return self.sessions[session_id]

    def list_ids(self) -> list[str]:
        with self.lock:
            return list(self.sessions.keys())


store = SessionStore()


class BridgeHandler(BaseHTTPRequestHandler):
    """Handles client, session management, and debug endpoints."""

    def log_message(self, format, *args):
        pass

    def _send_json(self, code: int, obj: dict):
        body = json.dumps(obj, indent=2).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self) -> bytes:
        length = int(self.headers.get("Content-Length", 0))
        return self.rfile.read(length) if length > 0 else b""

    def _parse_path(self) -> tuple[str, dict]:
        parsed = urlparse(self.path)
        return parsed.path.rstrip("/"), parse_qs(parsed.query)

    # ── Session management ────────────────────────────────────────────

    def _handle_session_create(self):
        """POST /session/create — Create a new session.

        Accepts JSON:
          {"target_addr": "example.com", "target_port": 80}
        Returns:
          {"session_id": "abc123def456"} or {"error": "..."}
        """
        body = self._read_body()
        try:
            payload = json.loads(body)
            target_addr = payload.get("target_addr", "unknown")
            target_port = payload.get("target_port", 0)
        except (json.JSONDecodeError, KeyError) as e:
            log.error(f"Bad session create payload: {e}")
            self._send_json(400, {"error": f"Invalid JSON: {e}"})
            return

        session = store.create(target_addr, target_port)
        self._send_json(
            200,
            {
                "session_id": session.session_id,
                "target_addr": target_addr,
                "target_port": target_port,
            },
        )

    # ── Client endpoints ──────────────────────────────────────────────

    def _handle_session_read(self, session_id: str):
        """POST /session/<id>/read — Rust client polls for data."""
        session = store.get(session_id)
        if session is None:
            self._send_json(404, {"error": f"Session {session_id} not found"})
            return

        data = session.dequeue_for_client()

        if data is not None:
            b64 = base64.b64encode(data).decode()
            log.info(
                f"[{session_id}] READ  → sending {len(data)} bytes to client: {_preview(data)}"
            )
            self._send_json(200, {"data": b64})
        else:
            log.debug(f"[{session_id}] READ  → no data")
            self._send_json(200, {"data": None})

    def _handle_session_write(self, session_id: str):
        """POST /session/<id>/write — Rust client pushes data it read from TCP."""
        session = store.get(session_id)
        if session is None:
            self._send_json(404, {"error": f"Session {session_id} not found"})
            return

        body = self._read_body()
        try:
            payload = json.loads(body)
            b64 = payload["data"]
            data = base64.b64decode(b64)
        except (json.JSONDecodeError, KeyError, Exception) as e:
            log.error(f"[{session_id}] WRITE — bad payload: {e}")
            self._send_json(400, {"error": str(e)})
            return

        session.store_from_client(data)
        log.info(
            f"[{session_id}] WRITE ← received {len(data)} bytes from client: {_preview(data)}"
        )
        self._send_json(200, {"ok": True})

    # ── Debug endpoints ───────────────────────────────────────────────

    def _handle_debug_send(self, session_id: str):
        """POST /debug/<id>/send — Inject data for the Rust client to pick up."""
        session = store.get(session_id)
        if session is None:
            session = store.get_or_create(session_id)

        body = self._read_body()
        try:
            payload = json.loads(body)
        except json.JSONDecodeError as e:
            self._send_json(400, {"error": f"Invalid JSON: {e}"})
            return

        if "text" in payload:
            data = payload["text"].encode("utf-8")
        elif "data" in payload:
            data = base64.b64decode(payload["data"])
        else:
            self._send_json(400, {"error": 'Provide "text" or "data" field'})
            return

        session.enqueue_for_client(data)
        log.info(
            f"[{session_id}] DEBUG SEND → queued {len(data)} bytes for client: {_preview(data)}"
        )
        self._send_json(200, {"ok": True, "queued_bytes": len(data)})

    def _handle_debug_recv(self, session_id: str, query: dict):
        """GET /debug/<id>/recv — View data the Rust client has written."""
        session = store.get(session_id)
        if session is None:
            self._send_json(404, {"error": f"Session {session_id} not found"})
            return

        clear = "1" in query.get("clear", [])
        chunks = session.drain_from_client(clear=clear)

        entries = []
        for chunk in chunks:
            entries.append(
                {
                    "bytes": len(chunk),
                    "base64": base64.b64encode(chunk).decode(),
                    "text": chunk.decode("utf-8", errors="replace"),
                }
            )

        log.info(
            f"[{session_id}] DEBUG RECV → returning {len(chunks)} chunks, clear={clear}"
        )
        self._send_json(
            200,
            {
                "session_id": session_id,
                "chunks": entries,
                "total_chunks": len(entries),
                "cleared": clear,
            },
        )

    def _handle_debug_status(self, session_id: str):
        """GET /debug/<id>/status — Session info."""
        session = store.get(session_id)
        if session is None:
            self._send_json(404, {"error": f"Session {session_id} not found"})
            return
        self._send_json(200, session.get_status())

    def _handle_debug_sessions(self):
        """GET /debug/sessions — List all sessions."""
        ids = store.list_ids()
        sessions = [store.get(sid).get_status() for sid in ids if store.get(sid)]
        self._send_json(200, {"sessions": sessions})

    # ── Routing ───────────────────────────────────────────────────────

    def do_POST(self):
        path, query = self._parse_path()
        parts = path.strip("/").split("/")

        # POST /session/create
        if len(parts) == 2 and parts[0] == "session" and parts[1] == "create":
            self._handle_session_create()
            return

        # POST /session/<id>/read
        if len(parts) == 3 and parts[0] == "session" and parts[2] == "read":
            self._handle_session_read(parts[1])
            return

        # POST /session/<id>/write
        if len(parts) == 3 and parts[0] == "session" and parts[2] == "write":
            self._handle_session_write(parts[1])
            return

        # POST /debug/<id>/send
        if len(parts) == 3 and parts[0] == "debug" and parts[2] == "send":
            self._handle_debug_send(parts[1])
            return

        self._send_json(404, {"error": "Not found"})

    def do_GET(self):
        path, query = self._parse_path()
        parts = path.strip("/").split("/")

        # GET /debug/<id>/recv
        if len(parts) == 3 and parts[0] == "debug" and parts[2] == "recv":
            self._handle_debug_recv(parts[1], query)
            return

        # GET /debug/<id>/status
        if len(parts) == 3 and parts[0] == "debug" and parts[2] == "status":
            self._handle_debug_status(parts[1])
            return

        # GET /debug/sessions
        if len(parts) == 2 and parts[0] == "debug" and parts[1] == "sessions":
            self._handle_debug_sessions()
            return

        self._send_json(404, {"error": "Not found"})


def _preview(data: bytes, max_len: int = 80) -> str:
    """Human-readable preview of bytes for logging."""
    try:
        text = data.decode("utf-8")
        if len(text) > max_len:
            text = text[:max_len] + "..."
        return repr(text)
    except UnicodeDecodeError:
        hex_str = data[:max_len].hex()
        return f"<{len(data)} bytes: {hex_str}...>"


def main():
    parser = argparse.ArgumentParser(description="HTTP Bridge Server")
    parser.add_argument("--port", type=int, default=8080, help="Port to listen on")
    parser.add_argument(
        "--bind", type=str, default="0.0.0.0", help="Address to bind to"
    )
    args = parser.parse_args()

    server = HTTPServer((args.bind, args.port), BridgeHandler)
    log.info(f"Bridge server listening on {args.bind}:{args.port}")
    log.info("")
    log.info("Session management:")
    log.info(f"  POST http://localhost:{args.port}/session/create")
    log.info("")
    log.info("Client endpoints:")
    log.info(f"  POST http://localhost:{args.port}/session/<id>/read")
    log.info(f"  POST http://localhost:{args.port}/session/<id>/write")
    log.info("")
    log.info("Debug endpoints:")
    log.info(f"  POST http://localhost:{args.port}/debug/<id>/send")
    log.info(f"  GET  http://localhost:{args.port}/debug/<id>/recv")
    log.info(f"  GET  http://localhost:{args.port}/debug/<id>/status")
    log.info(f"  GET  http://localhost:{args.port}/debug/sessions")
    log.info("")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        log.info("Shutting down")
        server.shutdown()


if __name__ == "__main__":
    main()
