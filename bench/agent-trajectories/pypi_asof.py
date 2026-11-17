"""A local PyPI index frozen at a point in time.

Agents run `pip install ...` during their trajectories, and pip resolves
against PyPI as it is *now*: a replay months later gets newer packages than
the recording did (e.g. a pandas that needs numpy >= 2), and the trajectory
diverges for reasons that have nothing to do with the agent.

This serves PyPI's simple index (HTML, PEP 503, which every pip version
reads) filtered to the files uploaded before `cutoff`, using PyPI's own
`upload-time` (PEP 700). Downloads go straight to files.pythonhosted.org.
Pinning the index to the recording time reproduces what pip could have seen
then, except for files deleted from PyPI since (rare; they show up as
replay divergences, not as silent differences).

    with AsOf(cutoff_epoch) as url:   # url = http://127.0.0.1:<port>/simple/
        ... PIP_INDEX_URL=url ...
"""

import html
import json
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM = "https://pypi.org/simple/"
JSON = "application/vnd.pypi.simple.v1+json"


def _epoch(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()


class AsOf:
    def __init__(self, cutoff):
        self.cutoff, self.cache, self.lock = cutoff, {}, threading.Lock()

    def page(self, name):
        with self.lock:
            if name in self.cache:
                return self.cache[name]
        req = urllib.request.Request(UPSTREAM + name + "/", headers={"Accept": JSON})
        for attempt in range(5):
            try:
                with urllib.request.urlopen(req, timeout=60) as r:
                    d = json.load(r)
                break
            except urllib.error.HTTPError as e:
                if e.code == 404:
                    return None
                if attempt == 4:
                    raise
            except (urllib.error.URLError, ConnectionError, TimeoutError):
                if attempt == 4:
                    raise
            time.sleep(5 * (attempt + 1))
        rows = []
        for f in d["files"]:
            # Files without an upload time are from before 2005ish: keep them.
            if f.get("upload-time") and _epoch(f["upload-time"]) > self.cutoff:
                continue
            h = f.get("hashes", {})
            url = f["url"] + (f"#sha256={h['sha256']}" if "sha256" in h else "")
            attrs = ""
            if f.get("requires-python"):
                attrs += f' data-requires-python="{html.escape(f["requires-python"])}"'
            if f.get("yanked"):
                y = f["yanked"] if isinstance(f["yanked"], str) else ""
                attrs += f' data-yanked="{html.escape(y)}"'
            rows.append(f'<a href="{html.escape(url)}"{attrs}>{html.escape(f["filename"])}</a><br/>')
        body = ("<!DOCTYPE html><html><body>\n" + "\n".join(rows) + "\n</body></html>\n").encode()
        with self.lock:
            self.cache[name] = body
        return body

    def __enter__(self):
        idx = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                parts = [p for p in self.path.split("?")[0].split("/") if p]
                body = None
                if len(parts) == 2 and parts[0] == "simple":
                    try:
                        body = idx.page(parts[1].lower())
                    except Exception:
                        self.send_error(502)
                        return
                if body is None:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/html")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.srv.daemon_threads = True
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        return f"http://127.0.0.1:{self.srv.server_address[1]}/simple/"

    def __exit__(self, *a):
        self.srv.shutdown()
        self.srv.server_close()
