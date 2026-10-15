#!/usr/bin/env python3
"""Preview the landing page directly from source, without stale copied assets.

Usage: python3 scripts/landing-shots/serve-learn.py [--port 8766]
"""
import argparse
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote, urlsplit

REPO = Path(__file__).resolve().parents[2]
SITE = REPO / "site"
WEB = REPO / "crates/muxa/src/dashboard/web"


class PreviewHandler(SimpleHTTPRequestHandler):
    extensions_map = {**SimpleHTTPRequestHandler.extensions_map, ".mjs": "text/javascript"}

    def translate_path(self, path):
        relative = unquote(urlsplit(path).path).lstrip("/")
        if ".." in Path(relative).parts:
            return str(SITE / ".preview-not-found")
        if relative in ("", "index.html"):
            return str(SITE / "index.html")
        if relative in ("learn", "learn/", "learn/index.html"):
            return str(SITE / "learn/index.html")
        if relative == "demo.gif":
            return str(REPO / "docs/demo.gif")
        site_file = SITE / relative
        return str(site_file if site_file.is_file() else WEB / relative)

    def end_headers(self):
        self.send_header("Cache-Control", "no-store, no-cache, must-revalidate")
        super().end_headers()

    def list_directory(self, path):
        self.send_error(404)
        return None


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8766)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), PreviewHandler)
    print(f"muxa browser practice: http://127.0.0.1:{args.port}/learn/", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
