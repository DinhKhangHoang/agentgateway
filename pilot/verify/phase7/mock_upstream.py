#!/usr/bin/env python3
"""Parameterisable mock LLM upstream for the Phase 7 agentgateway probe suite.

One process per provider tier.  Every request that is not a `/__` control call
is recorded as a *hit* -- this is the primary evidence the probes assert on,
because a clean client-side 200 is consistent with both "retried" and "never
tried tier0" (EVALUATION.md sec.3, probe design traps).

Control plane (never counted as a hit):
  POST /__control  {json}  merge into the response-behaviour state
  POST /__reset            clear hits + restore default behaviour
  GET  /__hits             the recorded hits as JSON
  GET  /__healthz

Behaviour modes:
  ok              200; SSE when the request body has "stream": true, else JSON
  status          respond `status_code` with a JSON error body
  midstream_abort 200 + SSE headers, `abort_after` chunks, then the connection
                  is torn down without the terminating chunk
  silent          200 + SSE, one chunk, silence for `silent_secs`, then resume
  hang            accept, sleep `hang_secs`, then answer normally

`count` (>0) applies the mode to the next N requests only, then reverts to ok.

stdlib only.
"""

import argparse
import hashlib
import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEFAULTS = {
    "mode": "ok",
    "status_code": 503,
    "chunks": 4,
    "abort_after": 2,
    "silent_secs": 0,
    "hang_secs": 0,
    "delay_ms": 0,
    "count": 0,
    "save_body_max": 4_000_000,
    "error_body": {"error": {"message": "mock upstream forced failure",
                             "type": "server_error"}},
}

LOCK = threading.Lock()
STATE = dict(DEFAULTS)
HITS = []
SEQ = [0]
LAST_BODY = [b""]

TIER = os.environ.get("MOCK_TIER", "tier")
BODY_DIR = os.environ.get("MOCK_BODY_DIR", "")


def common_prefix_len(a: bytes, b: bytes) -> int:
    n = min(len(a), len(b))
    i = 0
    while i < n and a[i] == b[i]:
        i += 1
    return i


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "mock-upstream/1"

    def log_message(self, fmt, *args):  # keep stderr clean
        pass

    # ---------------- plumbing ----------------

    def _read_body(self) -> bytes:
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            buf = bytearray()
            while True:
                line = self.rfile.readline().strip()
                if not line:
                    continue
                size = int(line.split(b";")[0], 16)
                if size == 0:
                    self.rfile.readline()
                    break
                buf += self.rfile.read(size)
                self.rfile.readline()
            return bytes(buf)
        n = int(self.headers.get("Content-Length") or 0)
        if n <= 0:
            return b""
        buf = bytearray()
        while len(buf) < n:
            part = self.rfile.read(min(1 << 20, n - len(buf)))
            if not part:
                break
            buf += part
        return bytes(buf)

    def _json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _chunk(self, data: bytes):
        self.wfile.write(b"%x\r\n" % len(data) + data + b"\r\n")
        self.wfile.flush()

    # ---------------- control ----------------

    def do_GET(self):
        if self.path.startswith("/__hits"):
            with LOCK:
                self._json(200, {"tier": TIER, "hits": HITS})
            return
        if self.path.startswith("/__state"):
            with LOCK:
                self._json(200, STATE)
            return
        if self.path.startswith("/__healthz"):
            self._json(200, {"ok": True, "tier": TIER})
            return
        self._json(404, {"error": "not found"})

    def do_POST(self):
        if self.path.startswith("/__"):
            body = self._read_body()
            if self.path.startswith("/__reset"):
                with LOCK:
                    HITS.clear()
                    SEQ[0] = 0
                    LAST_BODY[0] = b""
                    STATE.clear()
                    STATE.update(DEFAULTS)
                self._json(200, {"reset": True, "tier": TIER})
                return
            if self.path.startswith("/__control"):
                patch = json.loads(body or b"{}")
                with LOCK:
                    STATE.update(patch)
                    snap = dict(STATE)
                self._json(200, snap)
                return
            self._json(404, {"error": "not found"})
            return
        self._serve_upstream()

    # ---------------- upstream ----------------

    def _serve_upstream(self):
        body = self._read_body()
        auth = self.headers.get("Authorization") or ""
        try:
            parsed = json.loads(body) if body else {}
        except Exception:
            parsed = {}
        stream = bool(parsed.get("stream")) if isinstance(parsed, dict) else False

        with LOCK:
            SEQ[0] += 1
            seq = SEQ[0]
            prefix = common_prefix_len(LAST_BODY[0], body)
            saved = None
            if BODY_DIR and len(body) <= STATE["save_body_max"]:
                saved = os.path.join(BODY_DIR, "%s-%05d.bin" % (TIER, seq))
                with open(saved, "wb") as fh:
                    fh.write(body)
            if len(body) <= STATE["save_body_max"]:
                LAST_BODY[0] = body
            hit = {
                "seq": seq,
                "tier": TIER,
                "ts": time.time(),
                "method": self.command,
                "path": self.path,
                "body_len": len(body),
                "body_sha256": hashlib.sha256(body).hexdigest(),
                # never record the credential itself, only proof of injection
                "auth_present": bool(auth),
                "auth_sha256": hashlib.sha256(auth.encode()).hexdigest() if auth else None,
                "x_retry_attempt": self.headers.get("x-retry-attempt"),
                "model": parsed.get("model") if isinstance(parsed, dict) else None,
                "stream": stream,
                "prefix_match_bytes": prefix,
                "body_file": saved,
                "headers": {k.lower(): v for k, v in self.headers.items()
                            if k.lower() not in ("authorization",)},
            }
            HITS.append(hit)
            st = dict(STATE)
            if st["count"] and st["mode"] != "ok":
                STATE["count"] = st["count"] - 1
                if STATE["count"] <= 0:
                    STATE["mode"] = "ok"

        if st["delay_ms"]:
            time.sleep(st["delay_ms"] / 1000.0)

        mode = st["mode"]
        if mode == "hang":
            time.sleep(st["hang_secs"])
            mode = "ok"
        if mode == "status":
            self._json(st["status_code"], st["error_body"])
            self.close_connection = True
            return
        if stream:
            self._serve_sse(st, mode, parsed)
        else:
            self._serve_json(parsed)

    def _completion_id(self):
        return "chatcmpl-mock-%s" % TIER

    def _serve_json(self, parsed):
        model = (parsed.get("model") if isinstance(parsed, dict) else None) or "mock"
        self._json(200, {
            "id": self._completion_id(),
            "object": "chat.completion",
            "created": 1700000000,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant",
                            "content": "served-by-%s" % TIER},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7,
                      "total_tokens": 18},
        })

    def _sse_frames(self, st, parsed):
        model = (parsed.get("model") if isinstance(parsed, dict) else None) or "mock"
        cid = self._completion_id()
        base = {"id": cid, "object": "chat.completion.chunk",
                "created": 1700000000, "model": model}
        frames = []
        f = dict(base)
        f["choices"] = [{"index": 0, "delta": {"role": "assistant", "content": ""},
                         "finish_reason": None}]
        frames.append(f)
        for i in range(st["chunks"]):
            f = dict(base)
            f["choices"] = [{"index": 0,
                             "delta": {"content": "served-by-%s-%d " % (TIER, i)},
                             "finish_reason": None}]
            frames.append(f)
        f = dict(base)
        f["choices"] = [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        f["usage"] = {"prompt_tokens": 11, "completion_tokens": st["chunks"],
                      "total_tokens": 11 + st["chunks"]}
        frames.append(f)
        return frames

    def _serve_sse(self, st, mode, parsed):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        frames = self._sse_frames(st, parsed)
        for idx, fr in enumerate(frames):
            if mode == "midstream_abort" and idx >= st["abort_after"]:
                # tear down without the terminating 0-length chunk
                self.close_connection = True
                try:
                    self.wfile.flush()
                    self.connection.close()
                except Exception:
                    pass
                return
            if mode == "silent" and idx == 1 and st["silent_secs"]:
                time.sleep(st["silent_secs"])
            self._chunk(("data: %s\n\n" % json.dumps(fr)).encode())
        self._chunk(b"data: [DONE]\n\n")
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--tier", default=os.environ.get("MOCK_TIER", "tier"))
    ap.add_argument("--body-dir", default=os.environ.get("MOCK_BODY_DIR", ""))
    a = ap.parse_args()
    global TIER, BODY_DIR
    TIER = a.tier
    BODY_DIR = a.body_dir
    if BODY_DIR:
        os.makedirs(BODY_DIR, exist_ok=True)
    srv = ThreadingHTTPServer(("0.0.0.0", a.port), Handler)
    srv.daemon_threads = True
    srv.serve_forever()


if __name__ == "__main__":
    main()
