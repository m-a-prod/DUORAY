#!/usr/bin/env python3
"""DUORAY hub: receives error reports (sent only with the user's consent) and
serves signed update files. Standard library only.

Listens on 127.0.0.1 behind haproxy (TLS). Stores reports in SQLite under
$STATE_DIRECTORY; never stores client IPs (they are used in memory for rate
limiting and, hashed with a salt that changes daily, for counting visitors).

Reports are open to anyone who reads the client's source, so they are
limited here: per address and for everyone together, by size, by database
size, and repeated errors of one install within minutes are stored once.

Traffic counters (table `traffic`, `duoray-reports traffic`): page visits,
/download/<kind> downloads, update checks (≈ active installs per day),
self-updates and Arch packages. Daily totals only, no cookies or scripts.

  POST /v1/report                  JSON report -> {"id": "DR-XXXXXX"}
  GET  /v1/update/manifest.json    update manifest (signed, see manifest.sig)
  GET  /v1/update/manifest.sig
  GET  /v1/update/files/<name>     installers / AppImages
  GET  /arch/<arch>/<file>          signed pacman repository (duoray.db, packages)
  GET  /download/<kind>              302 to the current build, e.g. /download/windows
  GET  /, /robots.txt, /og.png, ... download page and its files (site/, see PAGES)
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
SITE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "site")
PAGES = {
    "/": ("index.html", "text/html; charset=utf-8"),
    # Telegram buttons take only http(s): this page opens duoray://add/<url>.
    "/add": ("add.html", "text/html; charset=utf-8"),
    "/favicon.svg": ("favicon.svg", "image/svg+xml"),
    "/apple-touch-icon.png": ("apple-touch-icon.png", "image/png"),
    "/og.png": ("og.png", "image/png"),  # link previews (og:image)
    "/robots.txt": ("robots.txt", "text/plain; charset=utf-8"),
    "/sitemap.xml": ("sitemap.xml", "application/xml"),
}

MAX_BODY = 64 * 1024  # the largest real report so far is ~18 KB
RETENTION_DAYS = 90
MAX_DB_BYTES = 256 * 1024 * 1024
RATE_PER_HOUR = 20  # per client address
GLOBAL_PER_HOUR = 200  # all clients together: a flood from many addresses stops here
DEDUP_SECONDS = 600  # the same error of one install within this: stored once
ID_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"  # no 0/O, 1/I
# The app (since 0.3.0) sends this; anything else is not a DUORAY client.
CLIENT_UA = "Duoray/"
# Page visits from these User-Agents are not counted.
BOTS = ("bot", "crawl", "spider", "slurp", "preview", "facebookexternalhit", "headless")

# Stable links for people (bot, site): /download/<kind> -> the current file.
# Names as packaging/release.sh publishes them; {v} is the manifest version.
DOWNLOADS = {
    "windows": "DUORAY-Setup-{v}-x64.exe",
    "windows-x86": "DUORAY-Setup-{v}-x86.exe",
    "macos": "DUORAY-{v}-macos-aarch64.dmg",
    "macos-intel": "DUORAY-{v}-macos-x86_64.dmg",
    "linux": "DUORAY-{v}-x86_64.AppImage",
    "linux-arm": "DUORAY-{v}-aarch64.AppImage",
    "deb": "duoray_{v}-1_amd64.deb",
    "deb-arm": "duoray_{v}-1_arm64.deb",
    "rpm": "duoray-{v}-1.x86_64.rpm",
    "rpm-arm": "duoray-{v}-1.aarch64.rpm",
}

_db_lock = threading.Lock()
_rate = defaultdict(deque)
_global = deque()
_rate_lock = threading.Lock()
# Unique visitors per day: hashes of (daily salt, address), in memory only.
_seen = {}
_salt = ("", b"")
_traffic_lock = threading.Lock()


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
        c.execute(
            """CREATE TABLE IF NOT EXISTS traffic (
                day TEXT NOT NULL, key TEXT NOT NULL,
                hits INTEGER NOT NULL, uniq INTEGER NOT NULL,
                PRIMARY KEY (day, key))"""
        )


def safe_name(name):
    """A plain file name: no paths, no hidden files."""
    return bool(name) and all(c.isalnum() or c in "._-+" for c in name) and not name.startswith(".")


def clip(value, limit):
    return value[:limit] if isinstance(value, str) else ""


def allowed(ip):
    now = time.time()
    with _rate_lock:
        hits = _rate[ip]
        for q in (hits, _global):
            while q and q[0] < now - 3600:
                q.popleft()
        if len(hits) >= RATE_PER_HOUR or len(_global) >= GLOBAL_PER_HOUR:
            return False
        hits.append(now)
        _global.append(now)
        if len(_rate) > 10000:
            for key in [k for k, q in _rate.items() if not q or q[-1] < now - 3600]:
                del _rate[key]
        return True


def db_full():
    return sum(os.path.getsize(p) for p in (DB_PATH, DB_PATH + "-wal") if os.path.exists(p)) > MAX_DB_BYTES


def count(key, ip):
    """Adds a hit to today's `key`, and a unique one if this address is new today."""
    global _salt
    day = time.strftime("%Y-%m-%d", time.gmtime())
    with _traffic_lock:
        if _salt[0] != day:
            _salt = (day, secrets.token_bytes(16))
            _seen.clear()
        seen = _seen.setdefault(key, set())
        mark = hashlib.sha256(_salt[1] + ip.encode()).digest()[:12]
        new = mark not in seen
        if new and len(seen) < 200000:
            seen.add(mark)
    try:
        with _db_lock, db() as conn:
            conn.execute(
                "INSERT INTO traffic VALUES (?,?,1,?) "
                "ON CONFLICT(day, key) DO UPDATE SET hits = hits + 1, uniq = uniq + excluded.uniq",
                (day, key, int(new)),
            )
    except sqlite3.Error as e:  # counting never breaks a download
        print(f"traffic count failed: {e}", flush=True)


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
    install = clip(report.get("install_id"), 64)
    now = int(time.time())
    with _db_lock, db() as conn:
        # An error that keeps repeating: the report from minutes ago stands for
        # it. Not for reports people send themselves: each one is meant.
        if install and kind != "user_report":
            row = conn.execute(
                "SELECT id FROM reports WHERE signature=? AND install_id=? AND received>? ORDER BY received DESC LIMIT 1",
                (signature, install, now - DEDUP_SECONDS),
            ).fetchone()
            if row:
                return row[0]
        rid = new_id(conn)
        conn.execute(
            "INSERT INTO reports VALUES (?,?,?,?,?,?,?,?,?)",
            (
                rid,
                now,
                install,
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
        # haproxy (option forwardfor) adds its own X-Forwarded-For line after
        # any the client sent: only the last line is the real address. (The
        # first one used to be read: a made-up header got past the limits.)
        lines = self.headers.get_all("X-Forwarded-For") or []
        return (lines[-1].split(",")[-1].strip() if lines else "") or self.client_address[0]

    def user_agent(self):
        return self.headers.get("User-Agent", "")

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
        if not self.user_agent().startswith(CLIENT_UA):
            return self.reply_json(400, {"error": "not a DUORAY client"})
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            length = -1
        if not 0 < length <= MAX_BODY:
            return self.reply_json(413, {"error": "too large"})
        if not allowed(self.client_ip()):
            return self.reply_json(429, {"error": "too many reports"})
        if db_full():
            return self.reply_json(503, {"error": "storage full"})
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
        get = self.command == "GET"
        page = PAGES.get(self.path.split("?", 1)[0])
        if page:
            ua = self.user_agent().lower()
            if get and page[0] == "index.html" and ua and not any(b in ua for b in BOTS):
                count("visit", self.client_ip())
            return self.send_file(os.path.join(SITE, page[0]), page[1], {"Cache-Control": "no-cache"})
        names = {
            "/v1/update/manifest.json": ("manifest.json", "application/json"),
            "/v1/update/manifest.sig": ("manifest.sig", "text/plain"),
        }
        if self.path in names:
            name, ctype = names[self.path]
            # Each running app checks: unique addresses a day ≈ active installs.
            if get and name == "manifest.json" and self.user_agent().startswith(CLIENT_UA):
                count("update-check", self.client_ip())
            return self.send_file(os.path.join(UPDATES, name), ctype, {"Cache-Control": "no-cache"})
        prefix = "/v1/update/files/"
        if self.path.startswith(prefix):
            name = self.path[len(prefix):]
            if safe_name(name):
                # The app updating itself (people come here through /download/).
                if get and self.user_agent().startswith(CLIENT_UA):
                    count("update", self.client_ip())
                return self.send_file(
                    os.path.join(UPDATES, "files", name),
                    "application/octet-stream",
                    {"Content-Disposition": f'attachment; filename="{name}"'},
                )
        if self.path.startswith("/download/"):
            kind = self.path[len("/download/"):].split("?", 1)[0].strip("/")
            template = DOWNLOADS.get(kind)
            try:
                with open(os.path.join(UPDATES, "manifest.json"), encoding="utf-8") as f:
                    version = json.load(f)["version"]
            except (OSError, ValueError, KeyError):
                version = None
            if template and version:
                name = template.format(v=version)
                if os.path.isfile(os.path.join(UPDATES, "files", name)):
                    if get:
                        count(f"download:{kind}", self.client_ip())
                    return self.reply(302, b"", "text/plain", {"Location": f"/v1/update/files/{name}", "Cache-Control": "no-cache"})
            return self.reply_json(404, {"error": "not found"})
        # pacman repository: Server = https://<hub>/arch/$arch
        if self.path.startswith("/arch/"):
            arch, _, name = self.path[len("/arch/"):].partition("/")
            if arch in ("x86_64", "aarch64", "any") and safe_name(name):
                if get and ".pkg.tar." in name and not name.endswith(".sig"):
                    count("arch", self.client_ip())
                return self.send_file(os.path.join(UPDATES, "arch", arch, name), "application/octet-stream", {})
            if arch == "duoray.asc":
                return self.send_file(os.path.join(UPDATES, "arch", "duoray.asc"), "text/plain", {})
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
