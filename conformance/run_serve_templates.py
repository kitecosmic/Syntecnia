"""Templates (SSR) de capa 8 vía conform: interpolación auto-escapada, each, when/
otherwise, raw (opt-out de escape). `body of render(path, data)` es observable."""
import sys, os, json, subprocess

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
OUT = os.path.join(REPO, "conformance", "tpl")

SRC = (
    'let data be {"title": "Hi <b>", "items": ["a", "b"], "featured": true, '
    '"feature": "star", "trusted": "<i>x</i>", "amp": "a & b"}\n'
    'print(body of render("conformance/tpl/page.html", data))\n'
    'let empty be {"title": "T", "items": [], "featured": false, "feature": "", '
    '"trusted": "", "amp": ""}\n'
    'print(body of render("conformance/tpl/page.html", empty))'
)


def oracle(src, fn):
    r = SyntecniaEngine().run_source(src, filename=fn)
    return {"ok": r.success, "out": list(r.output), "err": list(r.errors)}


def rust(path):
    p = subprocess.run([RUST, "conform", path], capture_output=True)
    try:
        return json.loads(p.stdout.decode("utf-8", "replace").strip())
    except Exception:
        return {"_raw": p.stdout.decode("utf-8", "replace"), "_stderr": p.stderr.decode("utf-8", "replace")}


syn_path = os.path.join(OUT, "render.syn")
with open(syn_path, "w", encoding="utf-8", newline="\n") as f:
    f.write(SRC)

o = oracle(SRC, syn_path)
r = rust(syn_path)
ok = (o == r)
print(f"=== Templates (SSR): {'OK' if ok else 'DIFF'} ===")
if not ok:
    print(f"    oracle: {json.dumps(o, ensure_ascii=False)}")
    print(f"    rust  : {json.dumps(r, ensure_ascii=False)}")
else:
    print("    " + json.dumps(o['out'], ensure_ascii=False))
sys.exit(0 if ok else 1)
