"""Subsistema 3 de serve (static): content-types (pinneadas + fallback mimetypes +
registro), index.html (raíz/bare/subdir), anti-traversal, precedencia ruta-vs-static.
Differential con body byte-exacto (incluye binarios)."""
import sys, os
from serve_harness import report, REPO

SDIR = os.path.join(REPO, "conformance", "serve_static_dir")
SUB = os.path.join(SDIR, "sub")
os.makedirs(SUB, exist_ok=True)


def w(path, data):
    if isinstance(data, bytes):
        with open(path, "wb") as f:
            f.write(data)
    else:
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write(data)


w(os.path.join(SDIR, "index.html"), "<html>root</html>")
w(os.path.join(SDIR, "style.css"), "body{color:red}")
w(os.path.join(SDIR, "app.js"), "console.log(1)")
w(os.path.join(SDIR, "data.csv"), "a,b,c")
w(os.path.join(SDIR, "notes.md"), "# notes")
w(os.path.join(SDIR, "weird.xyz"), "xyzdata")
w(os.path.join(SDIR, "pic.png"), b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03binary")
w(os.path.join(SDIR, "favicon.ico"), b"\x00\x00\x01\x00ICONbytes\xff")
w(os.path.join(SDIR, "archive.zip"), b"PK\x03\x04zipbytes\x00")
w(os.path.join(SDIR, "info"), "static info file")
w(os.path.join(SUB, "index.html"), "<html>sub</html>")
w(os.path.join(REPO, "conformance", "serve_secret.txt"), "SECRET")  # fuera del mount

PROG = '''require serve(__PORT__)
serve on __PORT__
    static "/assets" from "./conformance/serve_static_dir"
    route "GET /assets/info"
        give {"route": "wins"}
'''

REQS = [
    {"method": "GET", "path": "/assets/style.css"},                 # pinneada
    {"method": "GET", "path": "/assets/app.js"},                    # pinneada
    {"method": "GET", "path": "/assets/pic.png"},                   # binario
    {"method": "GET", "path": "/assets/favicon.ico"},              # binario
    {"method": "GET", "path": "/assets/archive.zip"},              # registro override
    {"method": "GET", "path": "/assets/data.csv"},                 # fallback
    {"method": "GET", "path": "/assets/notes.md"},                 # sin match → octet-stream
    {"method": "GET", "path": "/assets/weird.xyz"},               # sin match → octet-stream
    {"method": "GET", "path": "/assets/"},                         # → index.html
    {"method": "GET", "path": "/assets"},                          # bare → index.html
    {"method": "GET", "path": "/assets/sub/"},                     # subdir index
    {"method": "GET", "path": "/assets/../serve_secret.txt"},      # traversal → 404
    {"method": "GET", "path": "/assets/nope"},                     # 404
    {"method": "GET", "path": "/assets/info"},                     # ruta gana sobre static
]

sys.exit(report("Serve S3 (static)", PROG, REQS))
