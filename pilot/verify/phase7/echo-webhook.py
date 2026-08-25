#!/usr/bin/env python3
"""Guardrail webhook test double.

Records every request it receives to stdout as one JSON line, then always
returns a pass action. Its only job is to answer: which headers arrive?
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = self.rfile.read(length)
        print(
            json.dumps(
                {
                    "path": self.path,
                    "headers": {k.lower(): v for k, v in self.headers.items()},
                    "body_len": len(body),
                }
            ),
            flush=True,
        )
        payload = json.dumps({"action": {"pass": {}}}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    HTTPServer(("0.0.0.0", int(sys.argv[1]) if len(sys.argv) > 1 else 8099), Handler).serve_forever()
