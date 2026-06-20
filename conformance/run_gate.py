"""
Runner del corpus de conformidad (contrato de la migración a Rust).

Captura los casos a nivel-programa de un archivo de tests del oráculo
(p.ej. tests/test_core.py: assert_output / assert_fails), genera el corpus
(.syn + expected.json) con el ENGINE PYTHON como oráculo, y corre el binario
Rust `syntecnia-cli conform` sobre cada caso comparando {ok, out, err}.

Mismo path de fuente para ambos → el prefijo <archivo>:line:col de los errores
es comparable. Uso: python conformance/run_gate.py [test_core]
"""
import sys, os, json, subprocess, importlib

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
sys.path.insert(0, os.path.join(REPO, "tests"))

from syntecnia.runtime.engine import SyntecniaEngine

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")


def capture_cases(module_name):
    """Monkeypatchea assert_output/assert_fails y corre cada test_* para
    capturar (nombre, fuente, tipo)."""
    mod = importlib.import_module(module_name)
    cases = []
    state = {"cur": "?"}

    def cap_output(source, expected):
        cases.append((state["cur"], source, "output"))

    def cap_fails(source):
        cases.append((state["cur"], source, "fails"))

    mod.assert_output = cap_output
    mod.assert_fails = cap_fails
    # otros helpers (en capas posteriores) se agregan acá
    for name in sorted(n for n in dir(mod) if n.startswith("test_")):
        state["cur"] = name
        try:
            getattr(mod, name)()
        except Exception:
            # tests unit-level (p.ej. lexer por tokens) no usan los helpers:
            # ejecutan sus propios asserts y no aportan casos .syn → se ignoran.
            pass
    return cases


def oracle(source, filename):
    r = SyntecniaEngine().run_source(source, filename=filename)
    return {"ok": r.success, "out": list(r.output), "err": list(r.errors)}


def run_rust(syn_path):
    p = subprocess.run([RUST, "conform", syn_path], capture_output=True)
    out = p.stdout.decode("utf-8", "replace").strip()
    try:
        return json.loads(out)
    except Exception:
        return {"_parse_error": out, "_stderr": p.stderr.decode("utf-8", "replace"),
                "_code": p.returncode}


def main():
    test_mod = sys.argv[1] if len(sys.argv) > 1 else "test_core"
    area = test_mod.replace("test_", "")
    out_dir = os.path.join(REPO, "conformance", area)
    os.makedirs(out_dir, exist_ok=True)

    if not os.path.exists(RUST):
        print(f"FALTA el binario Rust: {RUST}\n(compilá release primero)")
        sys.exit(2)

    cases = capture_cases(test_mod)
    expected = {}
    diffs = []
    passed = 0

    for i, (name, source, kind) in enumerate(cases):
        fname = f"{i:03d}_{name}.syn"
        syn_path = os.path.join(out_dir, fname)
        with open(syn_path, "w", encoding="utf-8", newline="\n") as f:
            f.write(source)
        # el nombre de fuente que ve el oráculo = el path que recibe el Rust
        exp = oracle(source, syn_path)
        expected[fname] = exp
        got = run_rust(syn_path)

        if got == exp:
            passed += 1
        else:
            diffs.append({"case": fname, "source": source,
                          "oracle": exp, "rust": got})

    with open(os.path.join(out_dir, "expected.json"), "w", encoding="utf-8") as f:
        json.dump(expected, f, ensure_ascii=False, indent=2)

    total = len(cases)
    print(f"=== Gate {test_mod}: {passed}/{total} OK, {len(diffs)} diffs ===\n")
    # clasificar los diffs por campo
    by_field = {"ok": 0, "out": 0, "err": 0, "parse": 0}
    for d in diffs:
        o, g = d["oracle"], d["rust"]
        if "_parse_error" in g:
            by_field["parse"] += 1
        else:
            if o["ok"] != g.get("ok"): by_field["ok"] += 1
            if o["out"] != g.get("out"): by_field["out"] += 1
            if o["err"] != g.get("err"): by_field["err"] += 1
    print(f"diffs por campo: {by_field}\n")
    for d in diffs:
        print(f"--- {d['case']}")
        print(f"    src: {d['source']!r}")
        print(f"    oracle: {json.dumps(d['oracle'], ensure_ascii=False)}")
        print(f"    rust  : {json.dumps(d['rust'], ensure_ascii=False)}")
    sys.exit(1 if diffs else 0)


if __name__ == "__main__":
    main()
