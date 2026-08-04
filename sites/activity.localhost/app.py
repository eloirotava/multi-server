import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/hls/live.m3u8":
            body = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:2.0,\nsegment-1.ts\n"
            content_type = "application/vnd.apple.mpegurl"
        elif self.path == "/hls/segment-1.ts":
            body = b"example-segment"
            content_type = "video/mp2t"
        else:
            body = b"use /hls/live.m3u8 to renew activity\n"
            content_type = "text/plain"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


ThreadingHTTPServer(("127.0.0.1", int(os.environ["PORT"])), Handler).serve_forever()
