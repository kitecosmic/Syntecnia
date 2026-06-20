"""
Gate de Stage 2 (swarm) — espeja tests/test_concurrency.py comparando el ESTADO
INTERNO del swarm (blackboard + estados de agentes) oráculo-vs-Rust vía
`syntecnia-cli conform --swarm`.

Modos:
  exact    : dump completo {ok,out,err,blackboard,agents} idéntico (casos deterministas).
  error    : ambos ok=false (mensaje tolerante; se imprime para inspección).
  property : casos no-deterministas (carrera por una clave) → se chequean propiedades.
"""
import sys, os, json, subprocess

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
OUT = os.path.join(REPO, "conformance", "capa7_swarm")
os.makedirs(OUT, exist_ok=True)

C = {
 "define_no_exec": 'agent Worker\n    print("I am running!")\n\nprint("After definition")',
 "spawn_runs":     'agent Greeter\n    share "hello from agent" as "greeting"\n\nspawn Greeter',
 "spawn_args":     'agent Calculator\n    let result be x * 2\n    share result as "calc_result"\n\nspawn Calculator with x = 21',
 "two_agents":     'agent Producer\n    share "data_from_producer" as "shared_data"\n    signal "data_ready"\n\nagent Consumer\n    wait_for "data_ready"\n    observe "shared_data" as data\n    share data as "consumed"\n\nspawn Producer\nspawn Consumer',
 "signal_wakes":   'agent Sender\n    share "preparing" as "status"\n    signal "ready"\n\nagent Receiver\n    wait_for "ready"\n    share "received" as "status"\n\nspawn Receiver\nspawn Sender',
 "main_shares":    'share "hello from main" as "main_data"\n\nagent Reader\n    observe "main_data" as data\n    share data as "agent_read"\n\nspawn Reader',
 "undefined":      'spawn NonExistent',
 "dashboard":      'agent Worker\n    share "done" as "status"\n\nspawn Worker',
 "multi_spawn":    'agent Adder\n    let result be n + 100\n    share result as "sum"\n\nspawn Adder with n = 1\nspawn Adder with n = 2\nspawn Adder with n = 3',
 "error_captured": 'agent Crasher\n    let x be 1 / 0\n\nspawn Crasher',
}
MODE = {
 "define_no_exec":"exact","spawn_runs":"exact","spawn_args":"exact","two_agents":"exact",
 "signal_wakes":"exact","main_shares":"exact","dashboard":"exact","error_captured":"exact",
 "undefined":"error","multi_spawn":"property",
}


def oracle_swarm(src, fn):
    e = SyntecniaEngine()
    r = e.run_source(src, filename=fn)
    e.swarm.wait_all(timeout=5)
    bb = {k: str(v.value) for k, v in e.swarm.blackboard._data.items()}
    ag = {i: info.state.name for i, info in e.swarm.agents.items()}
    return {"ok": r.success, "out": list(r.output), "err": list(r.errors), "blackboard": bb, "agents": ag}


def rust(path):
    p = subprocess.run([RUST, "conform", "--swarm", path], capture_output=True)
    out = p.stdout.decode("utf-8", "replace").strip()
    try:
        return json.loads(out)
    except Exception:
        return {"_raw": out, "_stderr": p.stderr.decode("utf-8", "replace"), "_code": p.returncode}


passed = 0
fails = []
for name, src in C.items():
    path = os.path.join(OUT, name + ".syn")
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(src)
    o = oracle_swarm(src, path)
    r = rust(path)
    mode = MODE[name]
    ok = False
    note = ""
    if mode == "exact":
        ok = (o == r)
    elif mode == "error":
        ok = (o.get("ok") is False and r.get("ok") is False)
        note = f"oracle.err={o.get('err')} | rust.err={r.get('err')}"
    elif mode == "property":
        rb = r.get("blackboard", {})
        ra = r.get("agents", {})
        ok = (rb.get("sum") in ("101", "102", "103")
              and len(ra) == 3
              and all(s in ("DONE",) for s in ra.values()))
        note = f"rust sum={rb.get('sum')} agents={ra}"
    if ok:
        passed += 1
        if note:
            print(f"[OK] {name} ({mode}) — {note}")
    else:
        fails.append((name, mode, o, r))

print(f"\n=== Capa 7 Stage 2 (swarm): {passed}/{len(C)} OK ===")
for name, mode, o, r in fails:
    print(f"--- {name} ({mode})")
    print(f"    oracle: {json.dumps(o, ensure_ascii=False)}")
    print(f"    rust  : {json.dumps(r, ensure_ascii=False)}")
sys.exit(0 if not fails else 1)
