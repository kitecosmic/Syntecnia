# Plan: migración de Syntecnia a Rust + features pendientes

> Documento de planificación. No toca código. Estado: **decidido el rumbo, pendiente
> de ejecutar.** Fecha de la decisión: 2026-06-19.

---

## 0. Decisión

- **Migrar todo lo existente de Python a Rust.**
- **Congelar el código Python tal cual está ahora.** Es la *referencia dorada*
  (golden master): mientras dure la migración no se le agregan features ni se
  refactoriza. Sigue corriendo en producción (con Caddy
  terminando TLS por delante).
- **Criterio de éxito de la migración:** la implementación Rust pasa **las mismas
  pruebas que hoy tiene Python** (329 tests en 10 archivos). Cuando el corpus
  completo pasa en Rust, sabemos que la paridad es real y recién ahí empezamos a
  testear/usar Rust de verdad.
- **Quedan explícitamente para *después* de la migración** (como features del
  lenguaje, ya en Rust):
  1. **Concurrencia** (async / fan-out / swarm a escala).
  2. **El stack que hoy cubre Caddy/nginx/apache** (TLS, vhosts, ACME, etc.).

El porqué del orden está en las conversaciones previas y se resume así:
**construir esas dos cosas en Python sería reescribir el runtime que estamos por
tirar, y ninguna de las dos alcanza la escala buscada bajo el GIL.** Son la mejor
razón para ir a Rust, no algo a prototipar antes.

---

## 1. Principio rector

Separar siempre dos capas:

- **Semántica / superficie del lenguaje** — lo que el lenguaje *significa y
  expone*. Transfiere a cualquier implementación. Es lo valioso y ya está casi
  todo definido y probado en Python.
- **Runtime / plomería** — sockets, TLS, modelo de concurrencia, ejecución. **No
  transfiere: se reescribe.** Es donde Python es flojo y Rust brilla.

La migración es, en esencia: **preservar la semántica (los 329 tests la fijan) y
reescribir el runtime en Rust, mejor.**

---

## 2. Parte A — Features pendientes (congelados como spec)

Esto queda registrado para retomarse **en Rust, después de la paridad**. No se
implementa en Python.

### A1. Concurrencia

**Cómo es hoy en Python (diagnóstico, para saber qué se reemplaza):**
- Único primitivo: `spawn Agent` → un `threading.Thread` con un `Interpreter`
  nuevo y completo por agente.
- Coordinación: `blackboard` (dict con `RLock`), señales (cola + `Event`, pero
  `wait_for_signal` poll-ea con `sleep` de 100 ms), locks de recursos.
- Servidor: `ThreadingHTTPServer` = un hilo por conexión; cada SSE retiene un hilo.
- `fetch`: `urllib.urlopen` bloqueante.
- **No existe** primitivo de paralelismo de datos (`parallel`/`map`/`gather`).
- Paredes: el **GIL** serializa todo el cómputo (y el intérprete es tree-walking
  en Python puro), y **hilo-por-todo** no llega a C10k+.

**Activo importante:** el modelo ya es de **paso de mensajes / blackboard
compartido** (no memoria mutable compartida). Eso es CSP/actores y **mapea casi
perfecto a Rust** (canales + tareas). Baja mucho el riesgo de la migración del
modelo de agentes.

**Diseño objetivo en Rust (a cerrar como spec antes de implementarlo):**
- Runtime: **tokio** (tareas async M:N — millones, baratas: agentes, conexiones,
  SSE) + **rayon** (paralelismo de datos real para el *merge*) + canales
  (`tokio::sync::mpsc` / `crossbeam`). **Sin GIL → multicore real.**
- Surface del lenguaje a definir:
  - `spawn` / swarm: validar el modelo actual (transfiere) y reescribir el
    runtime (tareas tokio en vez de hilos+poll).
  - **Primitivo de fan-out/merge** para el caso "10k en 10×1000 y los junta".
    Semántica explícita a decidir: **fallo parcial** (¿aborta todo / devuelve los
    que salieron / colecta errores?), **orden de resultados**, **backpressure /
    límite de concurrencia**, e interacción con el modelo de **capabilities/intent**.
- Regla: **no portar el modelo de hilos+poll 1:1** — sería desperdiciar Rust. Se
  porta la *semántica* (blackboard/señales/locks), se reimplementa el *mecanismo*.

### A2. Stack web nativo (reemplazo de Caddy / nginx / apache)

Los 5 puntos acordados, en orden de prioridad (el primero es el único que hoy te
ata a Caddy):

1. **TLS** (terminación + SNI + redirect HTTP→HTTPS + HSTS). Defaults seguros
   impuestos por el lenguaje. *Es lo que hoy hace Caddy por vos.*
2. **Virtual hosting** por header `Host` + wildcard de subdominios. (Las rutas y
   prefijos ya están resueltos en el `serve` actual.)
3. **Estáticos de producción**: ETag/304, Range/206, gzip/brotli.
4. **ACME / HTTPS automático** (Let's Encrypt) + auto-renovación. El diferenciador
   real de Caddy.
5. **Reverse proxy** + **HTTP/2** (y eventualmente HTTP/3).

**¿Rust tiene algo nativo para reemplazar Caddy/nginx/apache? Sí — y de primer
nivel. No estaríamos "envolviendo" Caddy, sino construyendo sobre los mismos
bloques que usan Caddy y Cloudflare:**

| Necesidad | Crate(s) Rust nativos |
|---|---|
| TLS (sin OpenSSL, puro Rust) | `rustls` (+ `tokio-rustls`) |
| **HTTPS automático / ACME** | `rustls-acme`, `instant-acme` — *esto es la "Automatic HTTPS" de Caddy, como librería* |
| Servidor HTTP/1 + HTTP/2 | `hyper`; framework `axum` (sobre hyper+tokio) |
| HTTP/3 / QUIC | `quinn` + `h3` |
| **Reverse proxy a escala nginx** | `pingora` — el framework de **Cloudflare**, open-source, con el que **reemplazaron su flota de nginx**; `river` (proxy sobre pingora) |
| Servir estáticos (ETag/Range) | `tower-http` (`ServeDir`), `hyper-staticfile` |
| Compresión gzip/br | `tower-http` (compression layer), `async-compression` |

Conclusión: el plan de "incluir el stack web en el lenguaje" es viable y de bajo
riesgo en Rust — la pieza difícil (ACME automático) ya existe como crate maduro.

---

## 3. Parte B — Migración a Rust

### B1. Estrategia: conformance suite como contrato (golden master)

El problema: los tests Python de hoy llaman al engine Python directo
(`engine.run_source(...)`, `engine.swarm.blackboard.read(...)`) — no se pueden
correr contra Rust tal cual.

**Solución: extraer un *corpus de conformidad* independiente del lenguaje.** Cada
caso = un programa `.syn` + su resultado esperado (stdout / errores / estado final
observable). **Las dos implementaciones (Python y Rust) corren el mismo `.syn` y
deben dar salida idéntica.**

- Los tests que hoy hurgan en internos (`engine.swarm.blackboard.read("x")`) se
  re-expresan como **comportamiento observable** (que el programa imprima el valor),
  para que el oráculo no dependa de la implementación.
- Un runner único corre el corpus contra cualquier binario/engine.
- Mientras se porta, **el engine Python es el oráculo**: genera/valida las salidas
  esperadas. Es la red de seguridad de toda la migración.

> Primer trabajo concreto de la migración (mío, en rol de testing/diseño):
> inventariar los 329 tests y convertirlos al corpus `.syn` + salidas esperadas.

### B2. Orden de port por capas (sigue dependencias y testabilidad)

```
1. core/lexer  → core/tokens
2. core/parser → core/ast_nodes (+ flat_syntax, addressable, intentional_ops)
3. core/types  (SynValue, números, text, list, map, nothing)
4. core/interpreter   ← acá ya corren programas puros del lenguaje
5. capabilities/{model, intent, enforcer, builtins}
6. stdlib/{http, database, templates, cron}
7. agents/{blackboard, resource_lock, swarm, memory, progress, builtins}
8. stdlib/server + runtime/engine   (el `serve`)
9. runtime/{persistence, recovery, speculative, daemon, error_reporter}
10. llm/{provider, context, validator}, human/interaction
11. cli
```

Cada capa se cierra cuando pasa **su porción del corpus de conformidad**.
Subconjuntos de pruebas por capa: `test_core` (57) → 1-4; `test_capabilities`
(14)/`test_intent` (10) → 5; `test_stdlib` (20) → 6; `test_agents`
(17)/`test_agent_systems` (25)/`test_concurrency` (10) → 7;
`test_serve` (112) → 8; `test_recovery` (24)/`test_advanced` (40) → 9+.

### B3. Arquitectura Rust

- **Cargo workspace** con crates que espejan los paquetes Python
  (`syntecnia-core`, `syntecnia-capabilities`, `syntecnia-stdlib`,
  `syntecnia-runtime`, `syntecnia-agents`, `syntecnia-cli`, …). Permite portar y
  testear incremental, y trabajar en paralelo.
- **Núcleo del intérprete: síncrono.** Se porta la semántica síncrona actual tal
  cual (necesario para que el corpus pase sin re-arquitecturar). La concurrencia
  async (A1) se **layerea después**, no se mete en el intérprete ahora.
- **Cáscara de I/O: async (tokio).** El único componente que justifica tokio desde
  el día uno es `serve` (`hyper`/`axum`), porque es donde aterrizan TLS/vhost/etc.
  Patrón limpio: **shell async + intérprete sync** (cada request corre el intérprete
  sync, vía `spawn_blocking` si hace falta). Ese borde ya es el correcto a futuro.
- **`spawn` de agentes (paridad):** `std::thread` para replicar exactamente la
  semántica actual y pasar `test_concurrency`. El reemplazo por tareas tokio es la
  feature A1, posterior.

### B4. Elección de crates (para el port, no para A1/A2)

| Pieza | Crate | Nota |
|---|---|---|
| Lexer/Parser | **hand-port** (opcional `logos` p/ lexer) | Preservar **mensajes de error exactos** — son identidad del lenguaje |
| Valores | `enum SynValue` | Las enums de Rust calzan con el union actual |
| JSON | `serde_json` | |
| SQLite | `rusqlite` | reemplaza el `database.py` |
| HTTP cliente (`fetch`, LLM) | `reqwest` (async) o `ureq` (sync) | |
| Servidor `serve` | `hyper` + `tokio` | base para A2 |
| Cron | timers de `tokio` o `std::thread`+sleep | |
| CLI | `clap` | |

### B5. Riesgos de fidelidad (vigilar contra el oráculo)

- **Mensajes de error exactos.** El lenguaje vale por errores claros, nunca
  adivinar (decisión de diseño "predecible > flexible"). El corpus debe assertar
  texto/forma de error, no solo el happy path.
- **Modelo numérico.** Python usa int de precisión arbitraria + float. En Rust hay
  que decidir (`enum Number { Int(i64), Float(f64) }` o bigint). Revisar si algún
  test depende de enteros grandes.
- **Intent en español.** El enforcer entiende intención en español ("require SÍ
  concede", deniega fuera de scope, sin intent = permisivo). Portar ese matching
  con cuidado — hay tests dedicados (`test_intent`).
- **Rutas `/tmp` en Windows.** Python ve `/tmp` como `C:\tmp`; verificar el manejo
  de paths en Rust contra el mismo comportamiento.
- **Concurrencia no-determinista.** Los tests de swarm dependen de timing
  (`time.sleep`, `wait_all`). Al portar, hacerlos deterministas (esperar estado
  observable, no dormir) para que no sean flaky en Rust.

### B6. Gates ("¿está bien?")

- **Gate por capa:** su subconjunto del corpus pasa al 100%.
- **Gate de paridad total:** los 329 casos pasan idénticos en Rust y Python.
- **Recién tras el gate total** se considera Rust la implementación viva y se
  empieza a testear/usar en serio (y a construir A1 y A2 encima).

---

## 4. Parte C — Distribución (post-migración)

Una vez Rust funcione, Rust **habilita la visión del lenguaje libre que instalás
donde quieras**:

- `cargo build --release` → **un solo binario estático**, sin runtime (sin Python,
  sin npm, sin nada). Con target musl en Linux, totalmente self-contained.
- Cross-compilación directa (Linux/macOS/Windows, x86_64/arm64).
- Sitio `.org` + **instalador**: GitHub Releases + script de instalación
  (`curl | sh` / PowerShell), `cargo install`, y/o paquetes por plataforma.
- Resultado: lenguaje completamente libre, un binario, "lo instalás donde quieras".

---

## 5. Próximos pasos concretos (en orden)

1. **Cerrar este plan** (revisión del usuario).
2. **Spec de concurrencia (A1)** como documento aparte: surface `spawn`/swarm +
   primitivo fan-out/merge + semántica de fallo/orden/backpressure. (Es lo único
   "de diseño" que conviene madurar ya, porque define el lenguaje y apunta al
   modelo de Rust.)
3. **Inventariar y convertir los 329 tests** al corpus de conformidad `.syn` +
   salidas esperadas, con el engine Python como oráculo.
4. **Scaffold del workspace Cargo** y empezar el port por la capa 1 (lexer).
5. Avanzar capa por capa con su gate de conformidad.

> Nota de rol: el documento de plan, la spec de A1 y el corpus de conformidad son
> trabajo de diseño/testing. El port del core a Rust lo ejecuta el agente de
> desarrollo; yo diagnostico y valido contra el oráculo.
