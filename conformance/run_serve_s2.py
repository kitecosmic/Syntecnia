"""Subsistema 2 de serve (contrato de respuesta): ok/created/not_found/fail,
html/respond, content() negotiation (Accept + .md/.json), sql/paged. Differential."""
import sys
from serve_harness import report

PROG_RESP = '''require serve(__PORT__)
serve on __PORT__
    route "GET /ok"
        give ok({"a": 1})
    route "GET /created"
        give created({"ok": true})
    route "GET /notfound"
        give not_found("nope")
    route "GET /notfoundmap"
        give not_found({"reason": "deleted", "id": 7})
    route "GET /fail"
        give fail(422, "bad input")
    route "GET /failtext"
        give fail("not allowed")
    route "GET /failcode"
        give fail(503)
    route "GET /html"
        give html("<h1>Hi</h1>")
    route "GET /csv"
        give respond("a,b,c", "text/csv")
    route "GET /xml"
        give respond("<x/>", "application/xml", 201)
    route "GET /blog/:slug"
        let p be {"title": "Hi <there>", "body": "hello & welcome", "tag": "news"}
        give content(page([heading(1, title of p), prose(body of p), list(["one", "two"]), link("Back", "/blog"), code("print(1)", "python"), raw("<hr class='x'>")], {"title": title of p, "description": "An <intro>"}))
'''

REQS_RESP = [
    {"method": "GET", "path": "/ok"},
    {"method": "GET", "path": "/created"},
    {"method": "GET", "path": "/notfound"},
    {"method": "GET", "path": "/notfoundmap"},
    {"method": "GET", "path": "/fail"},
    {"method": "GET", "path": "/failtext"},
    {"method": "GET", "path": "/failcode"},
    {"method": "GET", "path": "/html"},
    {"method": "GET", "path": "/csv"},
    {"method": "GET", "path": "/xml"},
    {"method": "GET", "path": "/blog/hello"},                                    # content → HTML (default)
    {"method": "GET", "path": "/blog/hello", "headers": {"Accept": "text/markdown"}},
    {"method": "GET", "path": "/blog/hello", "headers": {"Accept": "application/json"}},
    {"method": "GET", "path": "/blog/hello.md"},                                 # suffix → MD
    {"method": "GET", "path": "/blog/hello.json"},                              # suffix → JSON
]

PROG_DB = '''require serve(__PORT__)
db_open(":memory:", "memory")
sql_exec("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)")
sql_exec("INSERT INTO items (name) VALUES (?)", ["a"])
sql_exec("INSERT INTO items (name) VALUES (?)", ["b"])
sql_exec("INSERT INTO items (name) VALUES (?)", ["c"])
sql_exec("INSERT INTO items (name) VALUES (?)", ["d"])
sql_exec("INSERT INTO items (name) VALUES (?)", ["e"])
serve on __PORT__
    route "GET /items"
        give paged("SELECT id, name FROM items ORDER BY id")
    route "GET /rows"
        give sql("SELECT id, name FROM items ORDER BY id")
'''

REQS_DB = [
    {"method": "GET", "path": "/items?limit=2"},
    {"method": "GET", "path": "/items?limit=2&cursor=2"},
    {"method": "GET", "path": "/rows"},
]

rc = 0
rc |= report("Serve S2a (helpers + content)", PROG_RESP, REQS_RESP)
rc |= report("Serve S2b (sql + paged)", PROG_DB, REQS_DB)
sys.exit(rc)
