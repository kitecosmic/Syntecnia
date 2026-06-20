"""Último grupo de serve (discovery + CORS + auth + max_body/spill). Differential."""
import sys
from serve_harness import report

AUTH = '''require serve(__PORT__)

task check_token(token)
    when token == "secret"
        give {"name": "alice"}
    give nothing

serve on __PORT__
    auth with check_token
    route "GET /me" requires auth
        give {"user": name of (user of request)}
'''
REQS_AUTH = [
    {"method": "GET", "path": "/me"},                                                 # 401 sin token
    {"method": "GET", "path": "/me", "headers": {"Authorization": "Bearer wrong"}},   # 401 token malo
    {"method": "GET", "path": "/me", "headers": {"Authorization": "Bearer secret"}},  # 200 {"user":"alice"}
]

DISCOVERY = '''intent: "Internal purpose text"
require serve(__PORT__)
serve on __PORT__
    describe
        about: "Public Blog API"
        api: ["GET /blog/:slug -- an article", "POST /api/signup -- join"]
    route "GET /blog/:slug"
        give {"x": 1}
    route "POST /api/signup"
        give {"ok": true}
'''
REQS_DISC = [
    {"method": "GET", "path": "/llms.txt"},
    {"method": "GET", "path": "/robots.txt"},
]

PRIVATE = '''intent: "Internal dashboard"
require serve(__PORT__)
serve on __PORT__
    private
    route "GET /admin"
        give {"secret": true}
'''
REQS_PRIV = [
    {"method": "GET", "path": "/llms.txt"},     # 404 (private)
    {"method": "GET", "path": "/robots.txt"},   # Disallow: /
]

CORS = '''require serve(__PORT__)
serve on __PORT__
    cors "https://app.example.com"
    route "GET /x"
        give {"ok": true}
    route "POST /x"
        give {"ok": true}
'''
REQS_CORS = [
    {"method": "GET", "path": "/x", "headers": {"Origin": "https://app.example.com"}},
    {"method": "OPTIONS", "path": "/x"},   # preflight: ACAO+ACAM+ACAH+ACMA
]

MAXBODY_SPILL = '''require serve(__PORT__)
serve on __PORT__
    max_body "2mb"
    route "POST /readbody"
        give {"len": length(read_body())}
    route "POST /body"
        give {"len": length(body of request)}
'''
BIG = "x" * 1572864  # 1.5 MB > 1 MB MEM_SPILL → spill a disco
REQS_SPILL = [
    {"method": "POST", "path": "/body", "body": "hello"},                  # memoria → len 5
    {"method": "POST", "path": "/readbody", "body": BIG},                  # spill → read_body lee el file → 1572864
    {"method": "POST", "path": "/body", "body": BIG},                      # spill → body of request = "" → len 0
]

MAXBODY_413 = '''require serve(__PORT__)
serve on __PORT__
    max_body "2kb"
    route "POST /e"
        give {"ok": true}
'''
REQS_413 = [
    {"method": "POST", "path": "/e", "body": "x" * 5000},   # > 2kb → 413
]

rc = 0
rc |= report("S7-9 auth", AUTH, REQS_AUTH)
rc |= report("S7-9 discovery", DISCOVERY, REQS_DISC)
rc |= report("S7-9 private", PRIVATE, REQS_PRIV)
rc |= report("S7-9 CORS", CORS, REQS_CORS)
rc |= report("S7-9 max_body/spill", MAXBODY_SPILL, REQS_SPILL)
rc |= report("S7-9 max_body/413", MAXBODY_413, REQS_413)
sys.exit(rc)
