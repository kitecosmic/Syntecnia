"""
Corpus program-level de capa 6 (stdlib: http / time / cron — SIN SQLite todavía).

Casos hand-authored (espejan los program-level de tests/test_stdlib.py). El gate de
database (SQLite) y la persistencia cross-run se corren aparte cuando rusqlite compile.
"""
import sys, os, json, subprocess

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
OUT = os.path.join(REPO, "conformance", "capa6")
os.makedirs(OUT, exist_ok=True)

cases = [
    ("http_get_unreachable",
     'let response be http_get("http://localhost:99999")\nprint(text(ok of response))\nprint(text(status of response))'),
    ("http_full_syntax",
     'let r be http("GET", "http://localhost:99999", {"Accept": "application/json"}, {"page": "1"})\nprint(text(ok of r))'),
    ("format_time_iso",
     'require time\nprint(format_time(0))'),
    ("format_time_pattern",
     'require time\nprint(format_time(1781836890, "%Y-%m-%d %H:%M"))'),
    ("parse_time_roundtrip",
     'require time\nlet t be 1781836890\nlet back be parse_time(format_time(t))\nprint(text(back == t))'),
    ("parse_time_iso_zero",
     'require time\nprint(text(parse_time("1970-01-01T00:00:00Z") == 0))'),
    ("date_parts",
     'require time\nlet p be date_parts(0)\nprint(text(year of p))\nprint(text(month of p))\nprint(text(day of p))'),
    ("cron_register_list_status",
     'task ticker()\n    log "tick"\ncron_every(60, ticker)\nlet jobs be cron_list()\nprint(text(length(jobs)))\nprint(cron_status())\ncron_cancel("ticker")'),
    ("time_without_capability",
     'print(format_time(0))'),
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
diffs = []
expected = {}
for name, src in cases:
    path = os.path.join(OUT, name + ".syn")
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(src)
    o = oracle(src, path)
    r = rust(path)
    expected[name + ".syn"] = o
    if o == r:
        passed += 1
    else:
        diffs.append((name, src, o, r))

with open(os.path.join(OUT, "expected.json"), "w", encoding="utf-8") as f:
    json.dump(expected, f, ensure_ascii=False, indent=2)

print(f"=== Capa 6 gate (time/cron/http): {passed}/{len(cases)} OK, {len(diffs)} diffs ===\n")
for name, src, o, r in diffs:
    print(f"--- {name}")
    print(f"    src:    {src!r}")
    print(f"    oracle: {json.dumps(o, ensure_ascii=False)}")
    print(f"    rust  : {json.dumps(r, ensure_ascii=False)}")
sys.exit(1 if diffs else 0)
