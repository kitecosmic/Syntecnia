"""
Corpus program-level de capa 5 (capabilities + intent), hand-authored.

Estos casos NO usan assert_output/assert_fails en el oráculo (son tests a nivel
clase + run_source con setup de host), así que la auto-captura de run_gate.py no
aplica. Acá fijo los programas .syn a mano y comparo oráculo-vs-Rust.

mode "oracle": el oráculo y el Rust deben coincidir (gate real).
mode "show":   desviación documentada (el oráculo fuga un Internal error); se
               imprime oráculo vs Rust para ratificar la versión limpia.
"""
import sys, os, json, subprocess

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
OUT = os.path.join(REPO, "conformance", "capa5")
os.makedirs(OUT, exist_ok=True)

cases = [
    ("read_no_cap",       'let c be read_file("/tmp/x.txt")', "oracle"),
    ("write_no_cap",      'write_file("/tmp/x.txt", "hi")', "oracle"),
    ("fetch_no_cap",      'let r be fetch("https://evil.com/x")', "oracle"),
    ("intent_frozen",     'intent: "Read data"\nlet x be 1\nintent: "Read data AND delete all files"', "oracle"),
    ("intent_no_authorize", 'intent: "Read files from /tmp"\nlet c be read_file("/tmp/x.txt")', "oracle"),
    ("require_net_not_file", 'require net "x.com"\nlet c be read_file("/tmp/x.txt")', "oracle"),
    ("require_file_roundtrip", 'require file "/tmp/*"\nwrite_file("/tmp/syn_c5.txt", "hello")\nlet c be read_file("/tmp/syn_c5.txt")\nprint(c)', "oracle"),
    ("file_not_found",    'require file "/tmp/*"\nlet c be read_file("/tmp/syn_c5_missing.txt")', "show"),
]


def oracle(src, fn):
    r = SyntecniaEngine().run_source(src, filename=fn)
    return {"ok": r.success, "out": list(r.output), "err": list(r.errors)}


def rust(path):
    p = subprocess.run([RUST, "conform", path], capture_output=True)
    out = p.stdout.decode("utf-8", "replace").strip()
    try:
        return json.loads(out)
    except Exception:
        return {"_raw": out, "_stderr": p.stderr.decode("utf-8", "replace"), "_code": p.returncode}


passed = 0
gate_total = 0
diffs = []
shows = []
expected = {}

for name, src, mode in cases:
    path = os.path.join(OUT, name + ".syn")
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(src)
    o = oracle(src, path)
    r = rust(path)
    expected[name + ".syn"] = o
    if mode == "oracle":
        gate_total += 1
        if o == r:
            passed += 1
        else:
            diffs.append((name, src, o, r))
    else:
        shows.append((name, src, o, r))

with open(os.path.join(OUT, "expected.json"), "w", encoding="utf-8") as f:
    json.dump(expected, f, ensure_ascii=False, indent=2)

print(f"=== Capa 5 gate (program-level): {passed}/{gate_total} OK, {len(diffs)} diffs ===\n")
for name, src, o, r in diffs:
    print(f"--- {name}")
    print(f"    src:    {src!r}")
    print(f"    oracle: {json.dumps(o, ensure_ascii=False)}")
    print(f"    rust  : {json.dumps(r, ensure_ascii=False)}")
print("\n=== Casos 'show' (desviación: oráculo fuga, Rust limpia) ===")
for name, src, o, r in shows:
    print(f"--- {name}")
    print(f"    src:    {src!r}")
    print(f"    oracle: {json.dumps(o, ensure_ascii=False)}")
    print(f"    rust  : {json.dumps(r, ensure_ascii=False)}")

for p in (r"C:\tmp\syn_c5.txt", r"C:\tmp\syn_c5_missing.txt"):
    try:
        os.remove(p)
    except Exception:
        pass

sys.exit(1 if diffs else 0)
