"""Subsistema 6 de serve (SSE): event-stream (data:/event:), coexistencia con
rutas normales, y max_streams/503. Differential con lectura incremental."""
import sys, json, http.client
from serve_harness import servers, sse_get, _send, IGNORE_HEADERS

PROG_SSE = '''require serve(__PORT__)
serve on __PORT__
    route "GET /count"
        stream
            each tick in range(3)
                send {"count": tick}
    route "GET /tokens"
        stream
            each word in ["Hello", "World", "!"]
                send word as "token"
    route "GET /health"
        give {"ok": true}
'''

PROG_503 = '''require serve(__PORT__)
serve on __PORT__
    max_streams 1
    route "GET /slow"
        stream
            each tick in range(3)
                send {"n": tick}
                sleep(1)
'''


def check_503(port):
    """Mantiene un stream abierto (retiene el slot) y abre una 2da conexión → 503."""
    c1 = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    c1.request("GET", "/slow")
    r1 = c1.getresponse()  # headers recibidos → stream corriendo → slot retenido
    s1 = r1.status
    c2 = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    c2.request("GET", "/slow")
    r2 = c2.getresponse()
    body = r2.read().decode("utf-8", "replace")
    hdrs = {k.lower(): v for k, v in r2.getheaders() if k.lower() not in IGNORE_HEADERS}
    c2.close()
    c1.close()
    return {"first_status": s1, "status": r2.status, "headers": hdrs, "body": body}


results = []
with servers(PROG_SSE) as (pa, pb):
    for path in ("/count", "/tokens"):
        o, r = sse_get(pa, path), sse_get(pb, path)
        results.append((f"SSE {path}", o == r, o, r))
    oh, rh = _send(pa, "GET", "/health"), _send(pb, "GET", "/health")
    oh["body"] = oh["body"].decode("utf-8", "replace")
    rh["body"] = rh["body"].decode("utf-8", "replace")
    results.append(("GET /health (coexiste)", oh == rh, oh, rh))

with servers(PROG_503) as (pa, pb):
    o, r = check_503(pa), check_503(pb)
    results.append(("max_streams/503", o == r, o, r))

passed = sum(1 for _, ok, _, _ in results if ok)
print(f"=== Serve S6 (SSE): {passed}/{len(results)} OK ===")
for name, ok, o, r in results:
    flag = "OK " if ok else "DIFF"
    print(f"[{flag}] {name}")
    if not ok:
        print(f"    oracle: {json.dumps(o, ensure_ascii=False)[:400]}")
        print(f"    rust  : {json.dumps(r, ensure_ascii=False)[:400]}")
sys.exit(0 if passed == len(results) else 1)
