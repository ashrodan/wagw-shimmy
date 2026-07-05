#!/usr/bin/env python3
"""wagw-hermes-adapter — bridges wagw-shimmy (AGENT-WHATSAPP-CONTRACT) to Hermes profiles.

Inbound:  POST /whatsapp/inbound  (Bearer WHATSAPP_WEBHOOK_TOKEN) — ack-fast 200,
          dedup on message id, then a detached Hermes turn.
Reply:    POST {WHATSAPP_GATEWAY_URL}/send (Bearer WHATSAPP_GATEWAY_TOKEN),
          echoing chat_id verbatim (the one correctness invariant).
Routing:  the shim stamps `channel`; CHANNEL_PROFILES maps channel label ->
          Hermes profile wrapper (e.g. "hermes:wagw"). Session continuity is
          per conversation: `<wrapper> chat -Q --continue wa_<chat_id>`.
"""

import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WEBHOOK_TOKEN = os.environ["WHATSAPP_WEBHOOK_TOKEN"]
GATEWAY_TOKEN = os.environ["WHATSAPP_GATEWAY_TOKEN"]
GATEWAY_URL = os.environ["WHATSAPP_GATEWAY_URL"].rstrip("/")
SEND_PATH = os.environ.get("WHATSAPP_GATEWAY_SEND_PATH", "/send")
BIND_HOST = os.environ.get("BIND_HOST", "0.0.0.0")
BIND_PORT = int(os.environ.get("BIND_PORT", "8000"))
TURN_TIMEOUT = int(os.environ.get("TURN_TIMEOUT_SECS", "300"))
# "label:wrapper,label2:wrapper2" — wrapper is the profile alias on PATH.
CHANNEL_PROFILES = dict(
    pair.split(":", 1)
    for pair in os.environ.get("CHANNEL_PROFILES", "hermes:wagw,default:wagw").split(",")
    if ":" in pair
)

DEDUP_TTL = 900.0
_dedup: dict[str, float] = {}
_dedup_lock = threading.Lock()
_session_locks: dict[str, threading.Lock] = {}
_session_locks_guard = threading.Lock()

# chat_id -> hermes session_id, persisted so conversations survive restarts.
SESSIONS_FILE = os.environ.get(
    "SESSIONS_FILE", os.path.expanduser("~/wagw-hermes-adapter/sessions.json")
)
_sessions_lock = threading.Lock()


def _load_sessions() -> dict:
    try:
        with open(SESSIONS_FILE) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def get_session(key: str) -> str | None:
    with _sessions_lock:
        return _load_sessions().get(key)


def set_session(key: str, session_id: str) -> None:
    with _sessions_lock:
        sessions = _load_sessions()
        sessions[key] = session_id
        tmp = SESSIONS_FILE + ".tmp"
        with open(tmp, "w") as f:
            json.dump(sessions, f, indent=1)
        os.replace(tmp, SESSIONS_FILE)


def log(msg: str) -> None:
    print(msg, flush=True)


def seen(msg_id: str) -> bool:
    now = time.monotonic()
    with _dedup_lock:
        for k in [k for k, ts in _dedup.items() if now - ts > DEDUP_TTL]:
            del _dedup[k]
        if msg_id in _dedup:
            return True
        _dedup[msg_id] = now
        return False


def session_lock(name: str) -> threading.Lock:
    with _session_locks_guard:
        return _session_locks.setdefault(name, threading.Lock())


def send_reply(chat_id: str, text: str, reply_to: str | None) -> None:
    body = {"chat_id": chat_id, "to": chat_id, "text": text, "message": text}
    if reply_to:
        body["reply_to"] = reply_to
    data = json.dumps(body).encode()
    for attempt in range(3):
        req = urllib.request.Request(
            GATEWAY_URL + SEND_PATH,
            data=data,
            headers={
                "Authorization": f"Bearer {GATEWAY_TOKEN}",
                "Content-Type": "application/json",
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                log(f"reply sent chat={chat_id} status={resp.status}")
                return
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503, 504) and attempt < 2:
                time.sleep(2 ** (attempt + 1))
                continue
            log(f"reply FAILED chat={chat_id} http={e.code}")
            return
        except Exception as e:  # connection errors
            if attempt < 2:
                time.sleep(2 ** (attempt + 1))
                continue
            log(f"reply FAILED chat={chat_id} err={e}")
            return


def run_turn(inbound: dict) -> None:
    chat_id = inbound["chat_id"]
    channel = inbound.get("channel") or "default"
    wrapper = CHANNEL_PROFILES.get(channel) or CHANNEL_PROFILES.get("default")
    if not wrapper:
        log(f"no profile for channel={channel}; dropping")
        return
    prompt = inbound.get("body") or ""
    media = inbound.get("media") or []
    if media:
        kinds = ", ".join(m.get("type", "file") for m in media)
        prompt = (prompt + f"\n[the user attached media ({kinds}) — media is not supported on this channel yet; say so briefly if it matters to the request]").strip()
    if not prompt:
        return
    session_key = f"{wrapper}:{chat_id}"
    with session_lock(session_key):
        t0 = time.monotonic()
        stored = get_session(session_key)
        proc = None
        for resume in ([stored, None] if stored else [None]):
            cmd = [wrapper, "chat", "-Q", "-q", prompt]
            if resume:
                cmd[2:2] = ["--resume", resume]
            try:
                proc = subprocess.run(
                    cmd, capture_output=True, text=True, timeout=TURN_TIMEOUT
                )
            except subprocess.TimeoutExpired:
                log(f"turn TIMEOUT chat={chat_id} after {TURN_TIMEOUT}s")
                return
            combined = proc.stdout + proc.stderr
            if resume and (proc.returncode != 0 or "No session found" in combined):
                log(f"stale session {resume} for chat={chat_id}; retrying fresh")
                continue
            break
        m = re.search(r"^session_id:\s*(\S+)", proc.stdout + "\n" + proc.stderr, re.MULTILINE)
        if m:
            set_session(session_key, m.group(1))
        out_lines = [
            ln
            for ln in proc.stdout.splitlines()
            if ln.strip() and not ln.startswith("session_id:") and not ln.lstrip().startswith("⚠")
        ]
        text = "\n".join(out_lines).strip()
        log(
            f"turn done chat={chat_id} channel={channel} profile={wrapper} "
            f"rc={proc.returncode} secs={time.monotonic() - t0:.1f} reply_chars={len(text)}"
        )
        if proc.returncode != 0 or not text:
            err = proc.stderr.strip().splitlines()
            log(f"turn stderr tail: {err[-3:] if err else '(empty)'}")
            return
    send_reply(chat_id, text, inbound.get("id"))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _respond(self, code: int, payload: dict) -> None:
        data = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):  # route to stdout/journal, drop noise
        pass

    def do_GET(self):
        if self.path in ("/health", "/healthz", "/livez"):
            self._respond(200, {"ok": True, "service": "wagw-hermes-adapter"})
        else:
            self._respond(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/whatsapp/inbound":
            self._respond(404, {"error": "not found"})
            return
        auth = self.headers.get("Authorization", "")
        xtok = self.headers.get("X-Webhook-Token", "")
        if auth != f"Bearer {WEBHOOK_TOKEN}" and xtok != WEBHOOK_TOKEN:
            self._respond(401, {"error": "unauthorized"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            inbound = json.loads(self.rfile.read(length) or b"{}")
        except (ValueError, json.JSONDecodeError):
            self._respond(400, {"error": "bad json"})
            return
        chat_id = inbound.get("chat_id")
        if not chat_id or not isinstance(chat_id, str):
            self._respond(400, {"error": "missing chat_id"})
            return
        # Ack everything from here down — the shim retries non-2xx (at-least-once).
        if inbound.get("from_me") or inbound.get("type") == "reaction":
            self._respond(200, {"ok": True, "dropped": True})
            return
        msg_id = str(inbound.get("id") or "")
        if msg_id and seen(msg_id):
            self._respond(200, {"ok": True, "duplicate": True})
            return
        log(f"inbound chat={chat_id} channel={inbound.get('channel')} id={msg_id} chars={len(inbound.get('body') or '')}")
        threading.Thread(target=run_turn, args=(inbound,), daemon=True).start()
        self._respond(200, {"ok": True})


def main() -> None:
    server = ThreadingHTTPServer((BIND_HOST, BIND_PORT), Handler)
    log(f"wagw-hermes-adapter listening on {BIND_HOST}:{BIND_PORT} channels={CHANNEL_PROFILES}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
