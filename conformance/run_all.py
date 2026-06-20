"""
Certificación de paridad en UN comando: limpia estado, recompila el CLI release,
corre la suite Rust (cargo test --workspace) y TODOS los differential conform/serve
contra el oráculo Python. Exit 0 sólo si todo pasa.

Uso:  python conformance/run_all.py
(Resuelve solo el PATH de cargo + gcc; no requiere setup previo.)
"""
import sys, os, glob, subprocess

try:
    sys.stdout.reconfigure(encoding="utf-8")
except Exception:
    pass

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CONF = os.path.join(REPO, "conformance")
CARGO = os.path.join(os.path.expanduser("~"), ".cargo", "bin", "cargo.exe")


def build_env():
    env = os.environ.copy()
    parts = [os.path.join(os.path.expanduser("~"), ".cargo", "bin")]
    g = glob.glob(os.path.join(env.get("LOCALAPPDATA", ""), "Microsoft", "WinGet",
                               "Packages", "BrechtSanders.WinLibs.POSIX.MSVCRT_*",
                               "mingw64", "bin"))
    if g:
        parts.append(g[0])
    env["PATH"] = os.pathsep.join(parts + [env.get("PATH", "")])
    return env


def clean_state():
    state = os.path.join(os.path.expanduser("~"), ".syntecnia", "state")
    if os.path.isdir(state):
        for f in os.listdir(state):
            try:
                os.remove(os.path.join(state, f))
            except OSError:
                pass


RUNNERS = [
    ("core (test_core 71)", ["run_gate.py", "test_core"]),
    ("capa5 capabilities", ["run_capa5.py"]),
    ("capa6 time/cron/http", ["run_capa6.py"]),
    ("capa6 database", ["run_capa6_db.py"]),
    ("capa7 swarm", ["run_capa7_swarm.py"]),
    ("capa7 progress/memory", ["run_capa7_pm.py"]),
    ("serve s1 routing", ["run_serve_s1.py"]),
    ("serve s2 response", ["run_serve_s2.py"]),
    ("serve s3 static", ["run_serve_s3.py"]),
    ("serve s4 negotiation", ["run_serve_s4.py"]),
    ("serve s5 rate-limit", ["run_serve_s5.py"]),
    ("serve s6 SSE", ["run_serve_s6.py"]),
    ("serve s7-9 disc/cors/auth/body", ["run_serve_s789.py"]),
    ("serve templates", ["run_serve_templates.py"]),
    ("advanced intentional ops", ["run_advanced.py"]),
    ("flat full-program", ["run_flat.py"]),
    ("A1 concurrencia (parallel_map/chunk)", ["run_a1.py"]),
    ("A2 estáticos producción (etag/304/range/gzip)", ["run_a2.py"]),
    ("StatePersistence cross-run", ["run_statepersist.py"]),
    ("vhost (multi-dominio)", ["run_vhost.py"]),
]


def main():
    env = build_env()
    clean_state()
    failures = []

    print("### rebuild release CLI ###")
    rb = subprocess.run([CARGO, "build", "--release", "--manifest-path",
                         os.path.join(REPO, "rust", "Cargo.toml"), "-p", "syntecnia-cli"],
                        env=env, cwd=REPO, capture_output=True)
    if rb.returncode != 0:
        print("BUILD FALLÓ:\n", rb.stderr.decode("utf-8", "replace")[-1500:])
        sys.exit(2)
    print("  build OK")

    print("\n### cargo test --workspace ###")
    ct = subprocess.run([CARGO, "test", "--manifest-path",
                         os.path.join(REPO, "rust", "Cargo.toml"), "--workspace"],
                        env=env, cwd=REPO, capture_output=True)
    out = ct.stdout.decode("utf-8", "replace")
    npass = out.count(" passed;")
    print(f"  cargo test: {'OK' if ct.returncode == 0 else 'FAILED'} ({npass} binarios de test)")
    if ct.returncode != 0:
        failures.append("cargo test --workspace")

    print("\n### differential conform/serve ###")
    renv = os.environ.copy()
    renv["PYTHONIOENCODING"] = "utf-8"  # los runners emiten utf-8 (no cp1252)
    for name, argv in RUNNERS:
        clean_state()  # cada runner desde estado limpio
        p = subprocess.run([sys.executable] + [os.path.join(CONF, argv[0])] + argv[1:],
                           cwd=REPO, capture_output=True, env=renv)
        sout = p.stdout.decode("utf-8", "replace")
        summary = next((ln for ln in reversed(sout.splitlines()) if "===" in ln), "(sin resumen)")
        flag = "OK " if p.returncode == 0 else "FAIL"
        print(f"  [{flag}] {name:34} {summary.strip()}")
        if p.returncode != 0:
            failures.append(name)

    print("\n" + "=" * 60)
    if not failures:
        print("PARIDAD CERTIFICADA: cargo test + todos los differential verdes.")
        sys.exit(0)
    print(f"FALLOS ({len(failures)}): " + ", ".join(failures))
    sys.exit(1)


if __name__ == "__main__":
    main()
