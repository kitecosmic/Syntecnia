"""
Corpus de database (capa 6, SQLite). Casos in-memory + sin-conexión via conform,
y persistencia cross-run (dos engines / dos procesos compartiendo un archivo .db).
"""
import sys, os, json, subprocess

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine

RUST = os.path.join(REPO, "rust", "target", "release", "syntecnia-cli.exe")
OUT = os.path.join(REPO, "conformance", "capa6_db")
os.makedirs(OUT, exist_ok=True)

DB_BUILTINS = (
    'db_open(":memory:", "memory")\n'
    'sql_exec("CREATE TABLE products (name TEXT, price REAL)")\n'
    'sql_exec("INSERT INTO products VALUES (?, ?)", ["Laptop", 999])\n'
    'sql_exec("INSERT INTO products VALUES (?, ?)", ["Mouse", 29])\n'
    'let products be sql("SELECT * FROM products ORDER BY price")\n'
    'each p in products\n'
    '    print(name of p + ": $" + text(price of p))\n'
    'let tables be sql_tables()\n'
    'print("Tables: " + text(length(tables)))\n'
    'db_close()'
)
DB_PARAM = (
    'db_open(":memory:", "memory")\n'
    'sql_exec("CREATE TABLE users (name TEXT)")\n'
    'sql_exec("INSERT INTO users VALUES (?)", ["Alice"])\n'
    'sql_exec("INSERT INTO users VALUES (?)", ["Bob"])\n'
    'let name be "Alice"\n'
    'let found be sql("SELECT * FROM users WHERE name = ?", [name])\n'
    'print(text(length(found)))\n'
    'db_close()'
)
DB_NO_CONN = 'let r be sql("SELECT 1")'

cases = [
    ("db_builtins", DB_BUILTINS),
    ("db_param", DB_PARAM),
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


def rm(path):
    try:
        os.remove(path)
    except Exception:
        pass


passed = 0
diffs = []
for name, src in cases:
    path = os.path.join(OUT, name + ".syn")
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(src)
    o = oracle(src, path)
    r = rust(path)
    if o == r:
        passed += 1
    else:
        diffs.append((name, src, o, r))

# --- Persistencia cross-run: write en un engine/proceso, read en otro ---
WRITE = ('db_open("/tmp/syn_capa6.db")\n'
         'sql_exec("CREATE TABLE IF NOT EXISTS t (val TEXT)")\n'
         'sql_exec("INSERT INTO t VALUES (?)", ["hello"])\n'
         'db_close()')
READ = ('db_open("/tmp/syn_capa6.db")\n'
        'let rows be sql("SELECT * FROM t")\n'
        'print(text(length(rows)))\n'
        'db_close()')
DBF = r"C:\tmp\syn_capa6.db"
wpath = os.path.join(OUT, "persist_write.syn")
rpath = os.path.join(OUT, "persist_read.syn")
for p, s in ((wpath, WRITE), (rpath, READ)):
    with open(p, "w", encoding="utf-8", newline="\n") as f:
        f.write(s)

rm(DBF)
SyntecniaEngine().run_source(WRITE, filename=wpath)
o_persist = oracle(READ, rpath)
rm(DBF)
rust(wpath)
r_persist = rust(rpath)
rm(DBF)

persist_ok = (o_persist == r_persist)

# db_no_connection: desviación ratificada (oráculo fuga Internal error → Rust limpia).
# Pasa si el Rust da un error limpio "No database connection" sin fugar Internal error.
dn_path = os.path.join(OUT, "db_no_connection.syn")
with open(dn_path, "w", encoding="utf-8", newline="\n") as f:
    f.write(DB_NO_CONN)
dn_r = rust(dn_path)
dn_err = dn_r.get("err", [])
dn_ok = (dn_r.get("ok") is False
         and any("No database connection" in e for e in dn_err)
         and not any("Internal error" in e for e in dn_err))

print(f"=== Capa 6 DB gate: {passed}/{len(cases)} in-memory OK, {len(diffs)} diffs; "
      f"persistencia cross-run: {'OK' if persist_ok else 'DIFF'}; "
      f"db_no_connection (desviación ratificada): {'OK' if dn_ok else 'DIFF'} ===\n")
for name, src, o, r in diffs:
    print(f"--- {name}")
    print(f"    oracle: {json.dumps(o, ensure_ascii=False)}")
    print(f"    rust  : {json.dumps(r, ensure_ascii=False)}")
if not persist_ok:
    print("--- persistencia cross-run")
    print(f"    oracle: {json.dumps(o_persist, ensure_ascii=False)}")
    print(f"    rust  : {json.dumps(r_persist, ensure_ascii=False)}")
else:
    print(f"persistencia: ambos -> {json.dumps(o_persist, ensure_ascii=False)}")

sys.exit(0 if (not diffs and persist_ok and dn_ok) else 1)
