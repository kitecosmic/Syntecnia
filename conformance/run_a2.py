"""Gate de A2 — estáticos de producción (ETag/304/Range/gzip). Feature NUEVA (no en
el oráculo) → se gatea por comportamiento correcto sobre el server Rust (HTTP).
TLS handshake/HSTS lo cubren los tests de integración Rust (cliente rustls real)."""
import sys, os, http.client, gzip
from serve_harness import rust_server, REPO

SDIR = os.path.join(REPO, "conformance", "a2_static")
os.makedirs(SDIR, exist_ok=True)
TEXT = "compressible test line for gzip and range checks\n" * 200  # ~10 KB, ASCII
with open(os.path.join(SDIR, "big.txt"), "w", encoding="utf-8", newline="") as f:
    f.write(TEXT)
RAW = TEXT.encode("utf-8")

PROG = '''require serve(__PORT__)
serve on __PORT__
    static "./conformance/a2_static"
'''


def get(port, path, headers=None):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    c.request("GET", path, headers=headers or {})
    r = c.getresponse()
    body = r.read()
    h = {k.lower(): v for k, v in r.getheaders()}
    c.close()
    return r.status, h, body


checks = []
with rust_server(PROG) as port:
    # 1. 200 con ETag + Accept-Ranges + body íntegro
    st, h, body = get(port, "/big.txt")
    etag = h.get("etag")
    checks.append(("200 + etag + accept-ranges + body",
                   st == 200 and bool(etag) and h.get("accept-ranges") == "bytes" and body == RAW))

    # 2. 304 ante If-None-Match con el etag devuelto
    st2, h2, b2 = get(port, "/big.txt", {"If-None-Match": etag or '"x"'})
    checks.append(("304 If-None-Match", st2 == 304 and b2 == b""))

    # 3. 206 Range con Content-Range y body parcial
    st3, h3, b3 = get(port, "/big.txt", {"Range": "bytes=0-9"})
    checks.append(("206 Range bytes=0-9",
                   st3 == 206 and h3.get("content-range", "").startswith("bytes 0-9/") and b3 == RAW[0:10]))

    # 4. gzip cuando el cliente lo acepta (texto comprimible)
    st4, h4, b4 = get(port, "/big.txt", {"Accept-Encoding": "gzip"})
    gz = st4 == 200 and h4.get("content-encoding") == "gzip"
    try:
        gz = gz and gzip.decompress(b4) == RAW
    except Exception:
        gz = False
    checks.append(("gzip Accept-Encoding (round-trip)", gz))

    # 5. 416 ante Range fuera de rango
    st5, h5, b5 = get(port, "/big.txt", {"Range": f"bytes={len(RAW)+100}-{len(RAW)+200}"})
    checks.append(("416 Range inválido", st5 == 416))

passed = sum(1 for _, ok in checks if ok)
print(f"=== A2 estáticos de producción: {passed}/{len(checks)} OK ===")
for name, ok in checks:
    print(f"  [{'OK ' if ok else 'FAIL'}] {name}")
sys.exit(0 if passed == len(checks) else 1)
