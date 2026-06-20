"""Gate de vhost (multi-dominio). Feature NUEVA (no en el oráculo) → behavioral:
dispatch por header Host (exacto → wildcard → default) + aislamiento entre hosts."""
import sys, http.client
from serve_harness import rust_server

PROG = '''require serve(__PORT__)
serve on __PORT__
    host "a.example.com"
        route "GET /"
            give {"host": "a"}
        route "GET /only-a"
            give {"x": 1}
    host "b.example.com"
        route "GET /"
            give {"host": "b"}
    host "*.wild.com"
        route "GET /"
            give {"host": "wild"}
    route "GET /"
        give {"host": "default"}
'''

# (Host header, path, status esperado, body esperado o None=solo status)
CASES = [
    ("a.example.com", "/", 200, '{"host": "a"}'),
    ("b.example.com", "/", 200, '{"host": "b"}'),
    ("x.wild.com", "/", 200, '{"host": "wild"}'),
    ("unknown.com", "/", 200, '{"host": "default"}'),
    ("a.example.com", "/only-a", 200, '{"x": 1}'),
    ("b.example.com", "/only-a", 404, None),   # aislamiento: /only-a no existe en host-b
]


def get(port, host, path):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    c.request("GET", path, headers={"Host": host})
    r = c.getresponse()
    body = r.read().decode("utf-8", "replace")
    st = r.status
    c.close()
    return st, body


checks = []
with rust_server(PROG) as port:
    for host, path, exp_st, exp_body in CASES:
        st, body = get(port, host, path)
        ok = (st == exp_st) and (exp_body is None or body == exp_body)
        checks.append((f"Host={host} {path}", ok, st, body))

passed = sum(1 for _, ok, _, _ in checks if ok)
print(f"=== vhost (multi-dominio): {passed}/{len(checks)} OK ===")
for name, ok, st, body in checks:
    print(f"  [{'OK ' if ok else 'FAIL'}] {name} -> {st} {body if not ok else ''}")
sys.exit(0 if passed == len(checks) else 1)
