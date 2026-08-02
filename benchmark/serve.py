#!/usr/bin/env python3
"""Serve the benchmark viewer, regenerating bench-data.json on demand so the
page's poll picks up freshly-rendered runs without a restart. Static files
otherwise. Bind 127.0.0.1 only."""
import http.server
import os
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
PORT = int(os.environ.get("PORT", "8124"))


class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        # Rebuild the data file from results/v*.md on every fetch of it, so a new
        # run.sh / run-phase-*.sh report shows up on the next viewer poll.
        if self.path.split("?", 1)[0].lstrip("/") == "bench-data.json":
            subprocess.run(["python3", "gen-viewer.py"], cwd=HERE, capture_output=True)
        super().do_GET()

    def log_message(self, *_):  # quiet — the poll every 10s would spam the log
        pass


os.chdir(HERE)
with http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler) as httpd:
    print(f"→ http://127.0.0.1:{PORT}/  (bench-data.json regenerates on each request)")
    httpd.serve_forever()
