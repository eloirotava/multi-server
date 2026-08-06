import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

started = time.monotonic()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/ready" and time.monotonic() - started >= 1:
            self.send_response(204)
            self.end_headers()
            return
        if self.path == "/ready":
            self.send_response(503)
            self.end_headers()
            return
        body = b"health check concluido\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


ThreadingHTTPServer(("127.0.0.1", int(os.environ["PORT"])), Handler).serve_forever()
