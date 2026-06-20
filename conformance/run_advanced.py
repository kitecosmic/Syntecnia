"""Program-level de test_advanced: intentional ops (apply/where/reduce/collect/
transform/sort_by/every/some/flatten). Differential vía conform. (El resto de
test_advanced —ast_api/testgen/speculative/resource_lock/addressable/translate_flat—
es API interna, gateado por cargo test.)"""
import sys, os, json, subprocess

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
OUT = os.path.join(REPO, "conformance", "advanced")
os.makedirs(OUT, exist_ok=True)

# aislamiento de StatePersistence (defensivo)
_STATE = os.path.join(os.path.expanduser("~"), ".syntecnia", "state")
if os.path.isdir(_STATE):
    for _f in os.listdir(_STATE):
        try:
            os.remove(os.path.join(_STATE, _f))
        except OSError:
            pass

CASES = {
    "apply": 'task double(x)\n    give x * 2\nlet nums be [1, 2, 3, 4, 5]\nlet doubled be apply(double, nums)\neach n in doubled\n    print(text(n))',
    "where": 'task is_big(x)\n    give x > 3\nlet nums be [1, 2, 3, 4, 5]\nlet big be where(nums, is_big)\nprint(text(length(big)))',
    "reduce": 'task add(acc, x)\n    give acc + x\nlet total be reduce([1, 2, 3, 4, 5], add, 0)\nprint(text(total))',
    "collect": 'let users be [{"name": "Alice"}, {"name": "Bob"}]\nlet names be collect(users, "name")\neach n in names\n    print(n)',
    "transform": 'task double(x)\n    give x * 2\ntask is_even(x)\n    give x % 2 == 0\nlet nums be [1, 2, 3, 4]\nlet result be transform(nums, double, is_even)\neach n in result\n    print(text(n))',
    "sort_by": 'task neg(x)\n    give 0 - x\nlet nums be [3, 1, 4, 1, 5]\nlet sorted be sort_by(nums, neg)\neach n in sorted\n    print(text(n))',
    "every_some": 'task positive(x)\n    give x > 0\nprint(text(every([1, 2, 3], positive)))\nprint(text(every([-1, 2, 3], positive)))\nprint(text(some([-1, -2, 3], positive)))\nprint(text(some([-1, -2, -3], positive)))',
    "flatten": 'let nested be [[1, 2], [3, 4], [5]]\nlet flat be flatten(nested)\nprint(text(length(flat)))',
}


def oracle(src, fn):
    r = SyntecniaEngine().run_source(src, filename=fn)
    return {"ok": r.success, "out": list(r.output), "err": list(r.errors)}


def rust(path):
    p = subprocess.run([RUST, "conform", path], capture_output=True)
    try:
        return json.loads(p.stdout.decode("utf-8", "replace").strip())
    except Exception:
        return {"_raw": p.stdout.decode("utf-8", "replace"), "_stderr": p.stderr.decode("utf-8", "replace")}


passed = 0
diffs = []
for name, src in CASES.items():
    path = os.path.join(OUT, name + ".syn")
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(src)
    o, r = oracle(src, path), rust(path)
    if o == r:
        passed += 1
    else:
        diffs.append((name, o, r))

print(f"=== test_advanced intentional ops: {passed}/{len(CASES)} OK ===")
for name, o, r in diffs:
    print(f"--- {name}")
    print(f"    oracle: {json.dumps(o, ensure_ascii=False)}")
    print(f"    rust  : {json.dumps(r, ensure_ascii=False)}")
sys.exit(0 if not diffs else 1)
