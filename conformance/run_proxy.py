"""Gate de reverse proxy (Lote 2). Feature nueva → behavioral: levanto un UPSTREAM
(rust server) y un PROXY (rust server con `proxy to`), pido al proxy y verifico que
la respuesta sea la del upstream (forward verbatim)."""
import sys, http.client
from serve_harness import rust_server

UPSTREAM = '''require serve(__PORT__)
serve on __PORT__
    route "GET /api/data"
        give {"from": "upstream", "n": 42}
'''

# __UP__ se reemplaza con el puerto del upstream antes de pasar a rust_server.
# El target es la BASE: el proxy le anexa el path de la request (/api/data),
# como nginx proxy_pass. (Poner el path en el target lo duplicaría.)
PROXY_TMPL = '''require serve(__PORT__)
require net "127.0.0.1"
serve on __PORT__
    route "GET /api/data"
        proxy to "http://127.0.0.1:__UP__"
'''


def get(port, path):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    c.request("GET", path)
    r = c.getresponse()
    body = r.read().decode("utf-8", "replace")
    st = r.status
    ct = r.getheader("Content-Type")
    c.close()
    return st, ct, body


with rust_server(UPSTREAM) as up:
    proxy_prog = PROXY_TMPL.replace("__UP__", str(up))
    with rust_server(proxy_prog) as px:
        up_st, up_ct, up_body = get(up, "/api/data")      # upstream directo
        px_st, px_ct, px_body = get(px, "/api/data")      # vía proxy

ok = (px_st == up_st == 200 and px_body == up_body and "upstream" in px_body)
print(f"=== reverse proxy: {'OK' if ok else 'DIFF'} ===")
print(f"  upstream directo: {up_st} {up_body}")
print(f"  vía proxy       : {px_st} {px_body}")
sys.exit(0 if ok else 1)
