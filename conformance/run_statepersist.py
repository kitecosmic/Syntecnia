"""Gate de StatePersistence cross-run (memory/progress sobreviven reinicio). Está en
el oráculo (engine load_into/save_from por run_source, keyed por stem) → differential:
corro el MISMO programa dos veces (mismo stem → acumula) en oráculo y en Rust, comparo.
Oráculo y Rust usan stems DISTINTOS entre sí (evita el lock SQLite + contaminación)."""
import sys, os, json, subprocess

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
OUT = os.path.join(REPO, "conformance", "statepersist")
os.makedirs(OUT, exist_ok=True)
STATE = os.path.join(os.path.expanduser("~"), ".syntecnia", "state")

PROG = 'remember("preference", "v", ["t"])\nprint(text(length(recall("preference"))))'
path_o = os.path.join(OUT, "persist_o.syn")
path_r = os.path.join(OUT, "persist_r.syn")
for p in (path_o, path_r):
    with open(p, "w", encoding="utf-8", newline="\n") as f:
        f.write(PROG)


def clean():
    if os.path.isdir(STATE):
        for f in os.listdir(STATE):
            try:
                os.remove(os.path.join(STATE, f))
            except OSError:
                pass


def oracle_run():
    return list(SyntecniaEngine().run_source(PROG, filename=path_o).output)


def rust_run():
    p = subprocess.run([RUST, "conform", path_r], capture_output=True)
    return json.loads(p.stdout.decode("utf-8", "replace").strip()).get("out")


clean()
o1, o2 = oracle_run(), oracle_run()   # mismo stem → acumula entre "reinicios"
r1, r2 = rust_run(), rust_run()

# No basta oracle==rust: hay que verificar que efectivamente persiste (1 luego 2).
ok = (o1 == r1 == ["1"] and o2 == r2 == ["2"])
print(f"=== StatePersistence cross-run: {'OK' if ok else 'DIFF'} ===")
print(f"  run1: oracle={o1} rust={r1}  (esperado ['1'])")
print(f"  run2: oracle={o2} rust={r2}  (esperado ['2'] — persiste entre reinicios)")
clean()
sys.exit(0 if ok else 1)
