"""
Harness differential HTTP para el gate de serve (capa 8).

Levanta el server ORÁCULO (Python, vía _serve_oracle.py que bloquea) y el server
RUST (`syntecnia-cli serve`) en puertos libres distintos, connect-poll, manda las
MISMAS requests a ambos y compara status + headers (ignorando Server/Date,
version-specific/volátiles) + body.
"""
import socket, subprocess, time, http.client, os, sys, json, contextlib

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
ORACLE = os.path.join(REPO, "conformance", "_serve_oracle.py")
# server/date: volátiles. etag/accept-ranges: additive de estáticos de producción
# (A2, post-paridad) que el oráculo congelado no emite → se ignoran al comparar
# paridad; su CORRECCIÓN se gatea aparte en run_a2.py.
IGNORE_HEADERS = {"server", "date", "etag", "accept-ranges"}


def free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def _wait_ready(port, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            c = socket.create_connection(("127.0.0.1", port), timeout=0.5)
            c.close()
            return True
        except OSError:
            time.sleep(0.05)
    return False


def _send(port, method, path, body=None, headers=None):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    b = body.encode("utf-8") if isinstance(body, str) else body
    conn.request(method, path, body=b, headers=headers or {})
    r = conn.getresponse()
    data = r.read()
    hdrs = {k.lower(): v for k, v in r.getheaders() if k.lower() not in IGNORE_HEADERS}
    conn.close()
    return {"status": r.status, "headers": hdrs, "body": data}  # body = bytes (compara byte-exacto)


def _disp(resp):
    """Versión imprimible de una respuesta (body bytes → texto truncado)."""
    b = resp.get("body", b"")
    s = b.decode("utf-8", "replace") if isinstance(b, (bytes, bytearray)) else str(b)
    if len(s) > 300:
        s = s[:300] + f"...(+{len(b)} bytes)"
    return {"status": resp.get("status"), "headers": resp.get("headers"), "body": s}


def _drain(p):
    try:
        return p.stderr.read().decode("utf-8", "replace")[-800:]
    except Exception:
        return ""


def differential(prog_template, requests):
    """prog_template usa __PORT__. requests = [{method, path, body?, headers?}, ...].
    Devuelve dict con 'results' (cada uno con match True/False) o 'error'."""
    pa, pb = free_port(), free_port()
    odir = os.path.join(REPO, "conformance", "serve_tmp")
    os.makedirs(odir, exist_ok=True)
    op_syn = os.path.join(odir, f"oracle_{pa}.syn")
    rp_syn = os.path.join(odir, f"rust_{pb}.syn")
    with open(op_syn, "w", encoding="utf-8", newline="\n") as f:
        f.write(prog_template.replace("__PORT__", str(pa)))
    with open(rp_syn, "w", encoding="utf-8", newline="\n") as f:
        f.write(prog_template.replace("__PORT__", str(pb)))
    op = subprocess.Popen([sys.executable, ORACLE, op_syn],
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    rp = subprocess.Popen([RUST, "serve", rp_syn],
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        oready, rready = _wait_ready(pa), _wait_ready(pb)
        if not (oready and rready):
            return {"error": f"ready oracle={oready} rust={rready}",
                    "oracle_stderr": _drain(op), "rust_stderr": _drain(rp)}
        results = []
        for req in requests:
            o = _send(pa, req["method"], req["path"], req.get("body"), req.get("headers"))
            r = _send(pb, req["method"], req["path"], req.get("body"), req.get("headers"))
            results.append({"req": req, "oracle": o, "rust": r, "match": o == r})
        return {"results": results}
    finally:
        op.kill()
        rp.kill()


@contextlib.contextmanager
def servers(prog):
    """Levanta oráculo + Rust (puertos libres) y cede (pa, pb) para requests custom
    (SSE, conexiones concurrentes). Los mata al salir."""
    pa, pb = free_port(), free_port()
    odir = os.path.join(REPO, "conformance", "serve_tmp")
    os.makedirs(odir, exist_ok=True)
    op_syn = os.path.join(odir, f"oracle_{pa}.syn")
    rp_syn = os.path.join(odir, f"rust_{pb}.syn")
    with open(op_syn, "w", encoding="utf-8", newline="\n") as f:
        f.write(prog.replace("__PORT__", str(pa)))
    with open(rp_syn, "w", encoding="utf-8", newline="\n") as f:
        f.write(prog.replace("__PORT__", str(pb)))
    op = subprocess.Popen([sys.executable, ORACLE, op_syn],
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    rp = subprocess.Popen([RUST, "serve", rp_syn],
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        if not (_wait_ready(pa) and _wait_ready(pb)):
            raise RuntimeError(f"server no listo — oracle:{_drain(op)} rust:{_drain(rp)}")
        yield pa, pb
    finally:
        op.kill()
        rp.kill()


@contextlib.contextmanager
def rust_server(prog):
    """Levanta SOLO el server Rust (para features A2 que el oráculo congelado no tiene,
    p.ej. estáticos de producción / TLS). Cede el puerto; lo mata al salir."""
    p = free_port()
    odir = os.path.join(REPO, "conformance", "serve_tmp")
    os.makedirs(odir, exist_ok=True)
    syn = os.path.join(odir, f"rustonly_{p}.syn")
    with open(syn, "w", encoding="utf-8", newline="\n") as f:
        f.write(prog.replace("__PORT__", str(p)))
    rp = subprocess.Popen([RUST, "serve", syn],
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        if not _wait_ready(p):
            raise RuntimeError(f"rust server no listo — {_drain(rp)}")
        yield p
    finally:
        rp.kill()


def _parse_sse_block(block):
    ev, data = None, []
    for line in block.split("\n"):
        if line.startswith("event:"):
            ev = line[len("event:"):].strip()
        elif line.startswith("data:"):
            data.append(line[len("data:"):].strip())
    return {"event": ev, "data": "\n".join(data)}


def sse_get(port, path, timeout=10):
    """GET a una ruta SSE; lee el event-stream hasta que el server cierra (stream
    finito → Connection: close). Devuelve status, headers (sin Server/Date) y eventos."""
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    conn.request("GET", path)
    r = conn.getresponse()
    body = r.read().decode("utf-8", "replace")  # lee hasta EOF (cierre del stream)
    hdrs = {k.lower(): v for k, v in r.getheaders() if k.lower() not in IGNORE_HEADERS}
    conn.close()
    events = [_parse_sse_block(b) for b in body.split("\n\n") if b.strip()]
    return {"status": r.status, "headers": hdrs, "events": events}


def report(title, prog, requests):
    out = differential(prog, requests)
    if "error" in out:
        print(f"=== {title}: ERROR — {out['error']} ===")
        print("oracle stderr:", out.get("oracle_stderr", ""))
        print("rust   stderr:", out.get("rust_stderr", ""))
        return 1
    passed = sum(1 for x in out["results"] if x["match"])
    total = len(out["results"])
    print(f"=== {title}: {passed}/{total} requests byte-idénticas ===")
    for x in out["results"]:
        if not x["match"]:
            rq = x["req"]
            print(f"--- DIFF {rq['method']} {rq['path']}")
            print(f"    oracle: {json.dumps(_disp(x['oracle']), ensure_ascii=False)}")
            print(f"    rust  : {json.dumps(_disp(x['rust']), ensure_ascii=False)}")
    return 0 if passed == total else 1
