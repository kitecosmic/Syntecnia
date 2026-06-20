"""Subsistema 1 de serve (routing/match): especificidad, :param, *catchall,
colección, 404, 405+Allow, OPTIONS. Differential oráculo-vs-Rust."""
import sys
from serve_harness import report

PROG = '''require serve(__PORT__)
serve on __PORT__
    route "GET /health"
        give {"status": "ok"}
    route "GET /products/:id"
        give {"id": params.id}
    route "GET /products"
        give [{"id": 1}, {"id": 2}, {"id": 3}]
    route "POST /products"
        give {"created": true}
    route "GET /files/*path"
        give {"path": params.path}
    route "GET /files/:id"
        give {"single": params.id}
'''

REQS = [
    {"method": "GET", "path": "/health"},
    {"method": "GET", "path": "/products/42"},
    {"method": "GET", "path": "/products"},
    {"method": "POST", "path": "/products", "body": "{}", "headers": {"Content-Type": "application/json"}},
    {"method": "GET", "path": "/files/a/b.txt"},   # *catchall
    {"method": "GET", "path": "/files/solo"},       # :id gana sobre *path (especificidad)
    {"method": "GET", "path": "/nope"},             # 404
    {"method": "DELETE", "path": "/products"},       # 405 + Allow
    {"method": "OPTIONS", "path": "/products"},      # 204 + Allow
]

sys.exit(report("Serve S1 (routing/match)", PROG, REQS))
