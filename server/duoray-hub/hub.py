#!/usr/bin/env python3
"""DUORAY hub: receives error reports (sent only with the user's consent) and
serves signed update files. Standard library only.

Listens on 127.0.0.1 behind haproxy (TLS). Stores reports in SQLite under
$STATE_DIRECTORY; never stores client IPs (they are used in memory for rate
limiting only).

  POST /v1/report                  JSON report -> {"id": "DR-XXXXXX"}
  GET  /v1/update/manifest.json    update manifest (signed, see manifest.sig)
  GET  /v1/update/manifest.sig
  GET  /v1/update/files/<name>     installers / AppImages
  GET  /health
"""

import hashlib
import json
import os
import secrets
import shutil
import sqlite3
import threading
import time
from collections import defaultdict, deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STATE = os.environ.get("STATE_DIRECTORY", "/var/lib/duoray-hub")
DB_PATH = os.path.join(STATE, "reports.db")
UPDATES = os.path.join(STATE, "updates")
LISTEN = os.environ.get("HUB_LISTEN", "127.0.0.1:5090")

MAX_BODY = 256 * 1024
RETENTION_DAYS = 90
MAX_DB_BYTES = 1024 * 1024 * 1024
RATE_PER_HOUR = 30
ID_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"  # no 0/O, 1/I

_db_lock = threading.Lock()
_rate = defaultdict(deque)
_rate_lock = threading.Lock()


def db():
    conn = sqlite3.connect(DB_PATH, timeout=10)
    conn.execute("PRAGMA journal_mode=WAL")
    return conn


def init_db():
    os.makedirs(os.path.join(UPDATES, "files"), exist_ok=True)
    with db() as c:
        c.execute(
            """CREATE TABLE IF NOT EXISTS reports (
                id TEXT PRIMARY KEY,
                received INTEGER NOT NULL,
                install_id TEXT, version TEXT, os TEXT, kind TEXT,
                signature TEXT, message TEXT, payload TEXT NOT NULL)"""
        )
        c.execute("CREATE INDEX IF NOT EXISTS reports_received ON reports(received)")
        c.execute("CREATE INDEX IF NOT EXISTS reports_signature ON reports(signature)")


def clip(value, limit):
    return value[:limit] if isinstance(value, str) else ""


def allowed(ip):
    now = time.time()
    with _rate_lock:
        hits = _rate[ip]
        while hits and hits[0] < now - 3600:
            hits.popleft()
        if len(hits) >= RATE_PER_HOUR:
            return False
        hits.append(now)
        return True


def new_id(conn):
    while True:
        rid = "DR-" + "".join(secrets.choice(ID_ALPHABET) for _ in range(6))
        if not conn.execute("SELECT 1 FROM reports WHERE id=?", (rid,)).fetchone():
            return rid


def store(report):
    kind = clip(report.get("kind"), 32) or "unknown"
    message = clip(report.get("message"), 8192)
    first = message.splitlines()[0] if message else ""
    signature = clip(report.get("signature"), 64) or hashlib.sha1(f"{kind}\n{first}".encode()).hexdigest()[:12]
    with _db_lock, db() as conn:
        rid = new_id(conn)
        conn.execute(
            "INSERT INTO reports VALUES (?,?,?,?,?,?,?,?,?)",
            (
                rid,
                int(time.time()),
                clip(report.get("install_id"), 64),
                clip(report.get("version"), 32),
                clip(report.get("os"), 96),
                kind,
                signature,
                message,
                json.dumps(report, ensure_ascii=False),
            ),
        )
    return rid


def purge_forever():
    while True:
        try:
            with _db_lock, db() as conn:
                conn.execute("DELETE FROM reports WHERE received < ?", (int(time.time()) - RETENTION_DAYS * 86400,))
                if os.path.getsize(DB_PATH) > MAX_DB_BYTES:
                    conn.execute(
                        "DELETE FROM reports WHERE id IN "
                        "(SELECT id FROM reports ORDER BY received LIMIT (SELECT COUNT(*) / 10 FROM reports))"
                    )
            with db() as conn:
                conn.execute("VACUUM")
        except Exception as e:  # keep serving even if a purge fails
            print(f"purge failed: {e}", flush=True)
        time.sleep(6 * 3600)


class Handler(BaseHTTPRequestHandler):
    server_version = "duoray-hub"
    sys_version = ""

    def log_message(self, fmt, *args):  # no client addresses in the journal
        pass

    def client_ip(self):
        # haproxy (option forwardfor) appends the real address last.
        fwd = self.headers.get("X-Forwarded-For", "")
        return fwd.split(",")[-1].strip() or self.client_address[0]

    def reply(self, status, body=b"", ctype="application/json", extra=None):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def reply_json(self, status, obj):
        self.reply(status, json.dumps(obj).encode())

    def do_POST(self):
        if self.path != "/v1/report":
            return self.reply_json(404, {"error": "not found"})
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            length = -1
        if not 0 < length <= MAX_BODY:
            return self.reply_json(413, {"error": "too large"})
        if not allowed(self.client_ip()):
            return self.reply_json(429, {"error": "too many reports"})
        try:
            report = json.loads(self.rfile.read(length))
            if not isinstance(report, dict):
                raise ValueError
        except ValueError:
            return self.reply_json(400, {"error": "bad json"})
        self.reply_json(201, {"id": store(report)})

    def do_GET(self):
        if self.path == "/health":
            return self.reply(200, b"ok", "text/plain")
        names = {
            "/v1/update/manifest.json": ("manifest.json", "application/json"),
            "/v1/update/manifest.sig": ("manifest.sig", "text/plain"),
        }
        if self.path in names:
            name, ctype = names[self.path]
            return self.send_file(os.path.join(UPDATES, name), ctype, {"Cache-Control": "no-cache"})
        prefix = "/v1/update/files/"
        if self.path.startswith(prefix):
            name = self.path[len(prefix):]
            if name and all(c.isalnum() or c in "._-" for c in name) and not name.startswith("."):
                return self.send_file(
                    os.path.join(UPDATES, "files", name),
                    "application/octet-stream",
                    {"Content-Disposition": f'attachment; filename="{name}"'},
                )
        self.reply_json(404, {"error": "not found"})

    do_HEAD = do_GET

    def send_file(self, path, ctype, extra):
        try:
            f = open(path, "rb")
        except OSError:
            return self.reply_json(404, {"error": "not found"})
        with f:
            size = os.fstat(f.fileno()).st_size
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(size))
            for k, v in extra.items():
                self.send_header(k, v)
            self.end_headers()
            if self.command != "HEAD":
                shutil.copyfileobj(f, self.wfile, 256 * 1024)


def main():
    init_db()
    threading.Thread(target=purge_forever, daemon=True).start()
    host, port = LISTEN.rsplit(":", 1)
    server = ThreadingHTTPServer((host, int(port)), Handler)
    server.daemon_threads = True
    print(f"duoray-hub listening on {LISTEN}, state in {STATE}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
