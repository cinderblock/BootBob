#!/usr/bin/env python3
"""A stand-in for an upstream web server, for e2e/run.sh.

    slow-upstream.py <address> <port> <directory>

Serves files from <directory>, but holds every response until a file named "release"
appears there, so the test can watch a boot waiting for its download. Logs each request to
<directory>/requests.log, which is how the test proves a second boot never came here.
No ranges: BootBob has to manage with a server that doesn't do them.
"""

import http.server
import os
import sys
import time

address, port, directory = sys.argv[1], int(sys.argv[2]), sys.argv[3]


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        with open(os.path.join(directory, "requests.log"), "a") as log:
            log.write(f"{self.path}\n")
        while not os.path.exists(os.path.join(directory, "release")):
            time.sleep(0.2)
        with open(os.path.join(directory, os.path.basename(self.path)), "rb") as f:
            body = f.read()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


http.server.ThreadingHTTPServer((address, port), Handler).serve_forever()
