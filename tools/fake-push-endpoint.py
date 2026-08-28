#!/usr/bin/env python3
"""Fake Web Push service endpoint for boundary testing.

Records full requests (headers + body, base64) on stdout and replies 201.
Run: python3 tools/fake-push-endpoint.py PORT
"""
import base64
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def _record(self):
        print(f"\n=== PUSH {self.command} {self.path} ===", flush=True)
        for k, v in self.headers.items():
            print(f"{k}: {v}", flush=True)
        length = int(self.headers.get("content-length", 0))
        body = self.rfile.read(length) if length else b""
        if body:
            print(f"body({len(body)}B) b64: {base64.b64encode(body).decode()}", flush=True)

    def do_POST(self):
        self._record()
        self.send_response(201)
        self.end_headers()
        self.wfile.write(b"{}")

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9123
    print(f"fake push endpoint listening on 127.0.0.1:{port}", flush=True)
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()