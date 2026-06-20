"""
Gate program-level de Stage 3/4 (progress + memory/rules) — los 5 casos
`test_engine_*` de test_agent_systems.py. Modos según asierta el test:
  exact    : output completo idéntico al oráculo.
  progress : display tiene timing (0ms) → se chequea ok + 'OK' en el display + percent exacto.
  summary  : se chequea ok + contiene 'Agent Memory'.
"""
import sys, os, json, subprocess

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
OUT = os.path.join(REPO, "conformance", "capa7_pm")
os.makedirs(OUT, exist_ok=True)

# Aislamiento: el engine persiste memory/progress entre corridas en
# ~/.syntecnia/state/<programa>.db (StatePersistence). Sin limpiar, re-correr
# acumula en el oráculo y diverge del Rust. Limpiamos para comparar estado limpio.
_STATE = os.path.join(os.path.expanduser("~"), ".syntecnia", "state")


def clean_state():
    # AMBOS (oráculo y Rust) persisten memory/progress al mismo state db (mismo
    # stem). Hay que limpiar ANTES de cada corrida para comparar estado limpio.
    if os.path.isdir(_STATE):
        for _f in os.listdir(_STATE):
            try:
                os.remove(os.path.join(_STATE, _f))
            except OSError:
                pass


clean_state()

SRC = {
 "progress": 'let job be create_progress("sync", ["fetch", "validate", "save"])\nstart_step("sync", "fetch")\ncomplete_step("sync", "fetch", "got 50 items")\nstart_step("sync", "validate")\ncomplete_step("sync", "validate")\nprint(progress_display("sync"))\nprint(text(progress_percent("sync")))',
 "resume_point": 'create_progress("job", ["a", "b", "c"])\nstart_step("job", "a")\ncomplete_step("job", "a")\nlet next be resume_point("job")\nprint(next)',
 "memory": 'remember("preference", "Always use formal tone", ["communication"])\nremember("learning", "API is slow on Mondays", ["api", "performance"])\nlet prefs be recall("preference")\nprint(text(length(prefs)))\nlet api_stuff be recall("learning", ["api"])\nprint(text(length(api_stuff)))',
 "rules": 'add_rule("max_discount", "must", "discount <= 0.20", "pricing")\nlet violations be check_rules("pricing", {"discount": 0.25})\nprint(text(length(violations)))\nlet ok be check_rules("pricing", {"discount": 0.10})\nprint(text(length(ok)))',
 "memory_summary": 'remember("preference", "Dark mode")\nadd_rule("formal", "should", "Use formal tone", "communication")\nprint(memory_summary())',
}
MODE = {"progress": "progress", "resume_point": "exact", "memory": "exact",
        "rules": "exact", "memory_summary": "summary"}


def oracle(src, fn):
    r = SyntecniaEngine().run_source(src, filename=fn)
    return {"ok": r.success, "out": list(r.output), "err": list(r.errors)}


def rust(path):
    p = subprocess.run([RUST, "conform", path], capture_output=True)
    out = p.stdout.decode("utf-8", "replace").strip()
    try:
        return json.loads(out)
    except Exception:
        return {"_raw": out, "_stderr": p.stderr.decode("utf-8", "replace")}


passed = 0
fails = []
for name, src in SRC.items():
    path = os.path.join(OUT, name + ".syn")
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(src)
    # oráculo y rust con stems DISTINTOS → state dbs distintos. Evita el lock de la
    # conexión SQLite del oráculo (que impide limpiar el db entre corridas) y la
    # contaminación cruzada. Ambos arrancan limpios (clean_state al inicio).
    opath = os.path.join(OUT, name + "_oracle.syn")
    with open(opath, "w", encoding="utf-8", newline="\n") as f:
        f.write(src)
    o = oracle(src, opath)
    r = rust(path)
    mode = MODE[name]
    ok = False
    if mode == "exact":
        ok = (o == r)
    elif mode == "progress":
        ro = r.get("out", [])
        ok = (r.get("ok") is True and len(ro) == 2
              and any("OK" in ro[0] for _ in [0]) and "OK" in ro[0]
              and ro[1] == "66.66666666666666")
        # sanity: el oráculo cumple lo mismo
        ok = ok and ("OK" in o["out"][0] and o["out"][1] == "66.66666666666666")
    elif mode == "summary":
        ro = r.get("out", [])
        ok = (r.get("ok") is True and any("Agent Memory" in l for l in ro)
              and any("Agent Memory" in l for l in o["out"]))
    if ok:
        passed += 1
    else:
        fails.append((name, mode, o, r))

print(f"=== Capa 7 Stage 3/4 (progress+memory) program-level: {passed}/{len(SRC)} OK ===")
for name, mode, o, r in fails:
    print(f"--- {name} ({mode})")
    print(f"    oracle: {json.dumps(o, ensure_ascii=False)}")
    print(f"    rust  : {json.dumps(r, ensure_ascii=False)}")
sys.exit(0 if not fails else 1)
