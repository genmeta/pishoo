#!/usr/bin/env python3
"""Local HTTP/1.1 upstream for the Pishoo cross-process smoke test."""

import functools
import sys
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer


class Handler(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        if self.path != "/duplex":
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
            while True:
                line = self.rfile.readline()
                if not line:
                    return
                size = int(line.split(b";", 1)[0], 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    break
                chunk = self.rfile.read(size)
                self.rfile.read(2)
                self._echo(chunk)
        else:
            remaining = int(self.headers.get("Content-Length", "0"))
            while remaining:
                chunk = self.rfile.read(min(remaining, 16 * 1024))
                if not chunk:
                    return
                remaining -= len(chunk)
                self._echo(chunk)

        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def _echo(self, chunk):
        self.wfile.write(f"{len(chunk):X}\r\n".encode() + chunk + b"\r\n")
        self.wfile.flush()


if __name__ == "__main__":
    port = int(sys.argv[1])
    directory = sys.argv[2]
    handler = functools.partial(Handler, directory=directory)
    ThreadingHTTPServer(("127.0.0.1", port), handler).serve_forever()
