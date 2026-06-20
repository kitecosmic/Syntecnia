"""Entry-point oráculo para el harness differential: corre un .syn con un bloque
serve (que arranca el server en hilo background) y bloquea para mantenerlo vivo.
El harness lo mata por PID cuando termina."""
import sys, os, threading

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)
from syntecnia.runtime.engine import SyntecniaEngine

syn = open(sys.argv[1], encoding="utf-8").read()
engine = SyntecniaEngine()
r = engine.run_source(syn, filename=sys.argv[1])
if not r.success:
    sys.stderr.write("ORACLE_FAIL: " + repr(r.errors))
    sys.stderr.flush()
    sys.exit(1)
threading.Event().wait()  # bloquea; el server corre en su hilo daemon
