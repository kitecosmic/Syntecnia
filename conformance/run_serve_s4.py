"""Subsistema 4 de serve (content negotiation, edges): literal/catchall conservan el
punto, :param re-interpreta sufijo, Accept substring (q ignorado), static-real gana
sobre re-interpretación, bare node degrada a JSON, sin content() Accept no afecta."""
import sys, os
from serve_harness import report, REPO

# dir estático para el caso static-real-gana
SDIR2 = os.path.join(REPO, "conformance", "serve_static_dir2")
os.makedirs(SDIR2, exist_ok=True)
with open(os.path.join(SDIR2, "data.json"), "w", encoding="utf-8", newline="") as f:
    f.write('{"real": "file"}')

PROG_A = '''require serve(__PORT__)
serve on __PORT__
    route "GET /report.json"
        give {"literal": true}
    route "GET /x"
        give {"ok": true}
    route "GET /n"
        give heading(2, "Title")
    route "GET /:slug"
        give content(page([heading(1, params.slug)], {"title": params.slug}))
'''

REQS_A = [
    {"method": "GET", "path": "/report.json"},                                              # literal gana sobre :slug
    {"method": "GET", "path": "/x", "headers": {"Accept": "text/markdown"}},                # sin content() Accept no afecta
    {"method": "GET", "path": "/n"},                                                        # bare node → JSON
    {"method": "GET", "path": "/hello"},                                                    # content default HTML
    {"method": "GET", "path": "/hello.json"},                                               # :param tragó sufijo → JSON
    {"method": "GET", "path": "/hello.md"},                                                 # → MD
    {"method": "GET", "path": "/hello", "headers": {"Accept": "application/json"}},         # → JSON
    {"method": "GET", "path": "/hello", "headers": {"Accept": "application/json;q=0.9, text/html;q=0.1"}},  # → HTML (q ignorado)
    {"method": "GET", "path": "/hello", "headers": {"Accept": "text/markdown, text/html"}}, # → HTML (md suprimido)
    {"method": "GET", "path": "/hello", "headers": {"Accept": "application/json, image/png"}},  # → JSON
]

PROG_B = '''require serve(__PORT__)
serve on __PORT__
    static "./conformance/serve_static_dir2"
    route "GET /:slug"
        give content(page([heading(1, params.slug)], {"title": params.slug}))
'''

REQS_B = [
    {"method": "GET", "path": "/data.json"},    # archivo estático real gana sobre re-interpretar
    {"method": "GET", "path": "/other.json"},   # sin archivo → :param re-interpreta → JSON
]

rc = 0
rc |= report("Serve S4a (negotiation edges)", PROG_A, REQS_A)
rc |= report("Serve S4b (static-real vs negotiation)", PROG_B, REQS_B)
sys.exit(rc)
