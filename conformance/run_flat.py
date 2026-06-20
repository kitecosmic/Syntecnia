"""Gate de flat_syntax full-program (test_flat_full_program): el oráculo hace
translate_flat + run_source; el Rust hace `conform --flat`. Differential end-to-end."""
import sys, os, json, subprocess

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine
from syntecnia.core.flat_syntax import translate_flat

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
OUT = os.path.join(REPO, "conformance", "flat")
os.makedirs(OUT, exist_ok=True)

# aislamiento StatePersistence (defensivo)
_STATE = os.path.join(os.path.expanduser("~"), ".syntecnia", "state")
if os.path.isdir(_STATE):
    for _f in os.listdir(_STATE):
        try:
            os.remove(os.path.join(_STATE, _f))
        except OSError:
            pass

FLAT = """-- A flat syntax program
task double(x):
    Give x * 2.
end

let nums be [1, 2, 3]
For each n in nums, print(text(double(n))).
"""

flat_path = os.path.join(OUT, "full.flat")
with open(flat_path, "w", encoding="utf-8", newline="\n") as f:
    f.write(FLAT)

# oráculo: translate_flat + run_source
std = translate_flat(FLAT)
r = SyntecniaEngine().run_source(std, filename=flat_path)
oracle = {"ok": r.success, "out": list(r.output), "err": list(r.errors)}

# rust: conform --flat
p = subprocess.run([RUST, "conform", "--flat", flat_path], capture_output=True)
try:
    rust = json.loads(p.stdout.decode("utf-8", "replace").strip())
except Exception:
    rust = {"_raw": p.stdout.decode("utf-8", "replace"), "_stderr": p.stderr.decode("utf-8", "replace")}

ok = (oracle == rust)
print(f"=== flat full-program (conform --flat): {'OK' if ok else 'DIFF'} ===")
print(f"    oracle: {json.dumps(oracle, ensure_ascii=False)}")
print(f"    rust  : {json.dumps(rust, ensure_ascii=False)}")
sys.exit(0 if ok else 1)
