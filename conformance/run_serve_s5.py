"""Subsistema 5 de serve (rate-limit): token bucket, override por-ruta, herencia
del default de bloque, `none`. Comparador custom: strip solo RateLimit-Reset y
Retry-After (dependientes de wall-clock); ventana grande → Remaining determinista."""
import sys, json
from serve_harness import differential

VOLATILE = {"ratelimit-reset", "retry-after"}

PROG = '''require serve(__PORT__)
serve on __PORT__
    rate_limit 4 per hour
    route "GET /limited"
        rate_limit 3 per hour
        give {"ok": true}
    route "GET /default"
        give {"ok": true}
    route "GET /health"
        rate_limit none
        give {"ok": true}
'''

REQS = (
    [{"method": "GET", "path": "/limited"}] * 5    # 3 per hour → 200,200,200,429,429
    + [{"method": "GET", "path": "/default"}] * 6  # 4 per hour → 200×4,429,429
    + [{"method": "GET", "path": "/health"}] * 4   # none → 4×200, sin headers RateLimit
)


def strip(resp):
    h = {k: v for k, v in resp["headers"].items() if k not in VOLATILE}
    return {"status": resp["status"], "headers": h, "body": resp["body"]}


out = differential(PROG, REQS)
if "error" in out:
    print("ERROR:", out["error"])
    print("oracle stderr:", out.get("oracle_stderr", ""))
    print("rust   stderr:", out.get("rust_stderr", ""))
    sys.exit(2)

passed = 0
diffs = []
seq = []
for x in out["results"]:
    o, r = strip(x["oracle"]), strip(x["rust"])
    rem = r["headers"].get("ratelimit-remaining", "-")
    seq.append(f"{x['req']['path'].split('/')[-1]}:{r['status']}(rem={rem})")
    if o == r:
        passed += 1
    else:
        diffs.append((x["req"], o, r))

print(f"=== Serve S5 (rate-limit): {passed}/{len(out['results'])} byte-idénticas (sin Reset/Retry-After) ===")
print("secuencia (rust):", " ".join(seq))
for req, o, r in diffs:
    print(f"--- DIFF {req['method']} {req['path']}")
    print(f"    oracle: {json.dumps({**o, 'body': o['body'].decode('utf-8','replace')}, ensure_ascii=False)}")
    print(f"    rust  : {json.dumps({**r, 'body': r['body'].decode('utf-8','replace')}, ensure_ascii=False)}")
sys.exit(0 if not diffs else 1)
