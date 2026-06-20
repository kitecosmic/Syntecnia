"""Gate de A1 Fase 1 (concurrencia). parallel_map/chunk son features NUEVAS (no en
el oráculo), pero parallel_map debe ≡ apply (que sí tiene paridad), así que se gatea
parallel_map-Rust contra apply-oráculo. chunk se compara contra un esperado literal."""
import sys, os, json, subprocess, re

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _norm(e):
    # normaliza el path .syn en los errores (oráculo y rust usan archivos distintos)
    return re.sub(r"[^\s:]*\.syn", "FILE", e)
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
OUT = os.path.join(REPO, "conformance", "a1")
os.makedirs(OUT, exist_ok=True)
_STATE = os.path.join(os.path.expanduser("~"), ".syntecnia", "state")
if os.path.isdir(_STATE):
    for _f in os.listdir(_STATE):
        try:
            os.remove(os.path.join(_STATE, _f))
        except OSError:
            pass


def oracle(src):
    r = SyntecniaEngine().run_source(src, filename=os.path.join(OUT, "_o.syn"))
    return {"ok": r.success, "out": list(r.output), "err": [_norm(e) for e in r.errors]}


def rust(src, name):
    path = os.path.join(OUT, name + ".syn")
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(src)
    p = subprocess.run([RUST, "conform", path], capture_output=True)
    try:
        d = json.loads(p.stdout.decode("utf-8", "replace").strip())
        if isinstance(d.get("err"), list):
            d["err"] = [_norm(e) for e in d["err"]]
        return d
    except Exception:
        return {"_raw": p.stdout.decode("utf-8", "replace"), "_stderr": p.stderr.decode("utf-8", "replace")}


# (nombre, src_rust, src_oracle)  → compara rust(parallel_map) vs oracle(apply)
EQUIV = [
    ("equiv_simple",
     "task sq(x)\n    give x * x\nlet l be [1, 2, 3, 4, 5]\neach n in parallel_map(sq, l)\n    print(text(n))",
     "task sq(x)\n    give x * x\nlet l be [1, 2, 3, 4, 5]\neach n in apply(sq, l)\n    print(text(n))"),
    ("equiv_limit",
     "task sq(x)\n    give x * x\nlet l be [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]\neach n in parallel_map(sq, l, 3)\n    print(text(n))",
     "task sq(x)\n    give x * x\nlet l be [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]\neach n in apply(sq, l)\n    print(text(n))"),
    ("pattern_10x1000",  # chunk → parallel_map → flatten  ≡  apply
     "task double(x)\n    give x * 2\ntask double_batch(b)\n    give apply(double, b)\nlet items be [1, 2, 3, 4, 5, 6, 7]\neach n in flatten(parallel_map(double_batch, chunk(items, 3), 2))\n    print(text(n))",
     "task double(x)\n    give x * 2\nlet items be [1, 2, 3, 4, 5, 6, 7]\neach n in apply(double, items)\n    print(text(n))"),
    ("fail_fast",  # división por cero en un item → falla (igual que apply secuencial)
     "task d(x)\n    give 10 / x\nlet r be parallel_map(d, [1, 2, 0, 4])\nprint(\"done\")",
     "task d(x)\n    give 10 / x\nlet r be apply(d, [1, 2, 0, 4])\nprint(\"done\")"),
]

# (nombre, src_rust, esperado_literal)  → chunk es nuevo, no está en el oráculo
LITERAL = [
    ("chunk_basic", "print(chunk([1, 2, 3, 4, 5], 2))",
     {"ok": True, "out": ["[[1, 2], [3, 4], [5]]"], "err": []}),
    ("chunk_exact", "print(chunk([1, 2, 3, 4], 2))",
     {"ok": True, "out": ["[[1, 2], [3, 4]]"], "err": []}),
]

passed = 0
total = 0
diffs = []
for name, rs, os_ in EQUIV:
    total += 1
    o, r = oracle(os_), rust(rs, name)
    if o == r:
        passed += 1
    else:
        diffs.append((name, o, r))
for name, rs, exp in LITERAL:
    total += 1
    r = rust(rs, name)
    if r == exp:
        passed += 1
    else:
        diffs.append((name, exp, r))

print(f"=== A1 Fase 1 gate: {passed}/{total} OK ===")
for name, exp, got in diffs:
    print(f"--- {name}")
    print(f"    esperado: {json.dumps(exp, ensure_ascii=False)}")
    print(f"    rust    : {json.dumps(got, ensure_ascii=False)}")
sys.exit(0 if not diffs else 1)
