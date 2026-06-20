# Spec A1 — Concurrencia (diseño para Rust, post-paridad)

> Documento de diseño. La paridad Python→Rust ya está lograda ([[syntecnia-roadmap-pending]]);
> esto es la primera feature *nueva* sobre la base Rust. Es la razón #1 por la que se
> migró a Rust (sin GIL, tokio/rayon). Estado: **diseño propuesto, a revisar.**

---

## 1. Objetivo (lo que el usuario quiere)

- Un agente que hace **muchas cosas a la vez** / swarms de subagentes.
- Plataformas/APIs que **no procesan de a una operación** — escala tipo telco / marketplace / mensajería.
- **Datalakes** y cómputo pesado, multi-core real.
- Ejemplo guía del usuario: **"una request de 10k, hecha como 10 grupos de 1000 en paralelo, y después se juntan"** (fan-out acotado + merge).

Python no daba esto (GIL serializa el cómputo; hilo-por-todo no escala). Rust sí: sin GIL, `tokio` (tareas async baratas), `rayon` (paralelismo de datos).

---

## 2. Modelo

- **Paso de mensajes / CSP, NO memoria mutable compartida.** Ya es el modelo del port: `SendValue` (snapshot owned `Send+Sync`); `share`/`observe` copian. Las tareas paralelas reciben copias y devuelven valores; el merge lo hace el llamador. (Esto es lo que mapea bien a Rust y evita data races.)
- **Determinismo de resultados:** aunque la ejecución sea concurrente, los **resultados se devuelven en orden de entrada**. (La concurrencia es de rendimiento, no de semántica observable.)
- **Aislamiento:** cada tarea corre su propio intérprete (como `spawn` hoy), hereda las capabilities del scope llamador, dentro del **intent congelado**. Sin grants nuevos.

---

## 3. Superficie del lenguaje (propuesta)

Dos primitivos nuevos + el `spawn`/swarm que ya existe:

### `parallel_map(task, list, limit?)`  — fan-out acotado (el primitivo central)
Aplica `task` a cada item de `list` **concurrentemente**, devuelve los resultados **en orden de entrada**. `limit` = máximo de tareas simultáneas (backpressure); si se omite, un default sensato.
```
let results be parallel_map(fetch_user, ids, 50)   -- 50 fetches a la vez, resultados en orden
```
Es el `apply(task, list)` que ya existe, pero concurrente y acotado.

### `chunk(list, size)` — batching (para el patrón 10×1000)
Parte una lista en sublistas de `size`.
```
let batches be chunk(items, 1000)     -- 10 batches de 1000
```

### El ejemplo del usuario (10k en 10×1000), expresado:
```
let batches be chunk(items, 1000)                    -- 10 batches
let partial be parallel_map(process_batch, batches, 10)   -- 10 batches en paralelo
let merged be flatten(partial)                       -- se juntan (flatten ya existe)
```

### Concurrencia heterogénea → `spawn`/swarm (ya existe)
Correr *cosas distintas* a la vez (no un fan-out homogéneo) ya se hace con `spawn Agent` + blackboard/`signal`/`wait_for`. No se agrega primitivo nuevo para eso; se reescribe su *runtime* (ver §5).

---

## 4. Semántica (a fijar)

- **Orden:** resultados de `parallel_map` en orden de entrada, siempre.
- **Fallo parcial:** **fail-fast por defecto** — si una tarea falla, se cancela el resto y el error se propaga (predecible, consistente con el modelo de errores). Para *colectar parcial* (seguir aunque algunas fallen), la tarea se envuelve en `try/recover` y devuelve un valor-o-error; así no hace falta una variante aparte del primitivo. (Decisión §6.)
- **Backpressure / `limit`:** nunca se lanzan las 10k a la vez; `limit` acota la concurrencia simultánea. Default propuesto: para I/O un valor fijo conservador (p.ej. 64); para cómputo, `num_cpus`. (Decisión §6.)
- **Capabilities/intent:** cada tarea hereda las capabilities del llamador y corre bajo el intent congelado. Un `fetch`/`read_file` dentro sigue requiriendo su `require`.
- **Sin estado mutable compartido:** la coordinación entre tareas, si hace falta, va por blackboard/db (igual que el aislamiento por-request de `serve`).

---

## 5. Runtime en Rust (cómo se implementa)

El intérprete sigue **síncrono** (como en la paridad). La concurrencia se agrega *alrededor*, en dos fases:

- **Fase 1 (cubre el ejemplo del usuario, 10×1000):** `parallel_map` sobre un **pool de hilos acotado** (`limit` = tamaño efectivo), cada hilo corre un intérprete sync sobre un item (igual que `spawn` con `std::thread` hoy, pero en pool con límite). Para fan-outs de **cómputo puro** (datalake), `rayon`. Sin GIL → multicore real. Esto entrega el 10×1000 y el cómputo pesado sin re-arquitecturar el intérprete a async.
- **Fase 2 (escala C100k+, si hace falta):** I/O async real con `tokio` — un camino async para `fetch` que permita millones de requests concurrentes baratas (no un hilo por request). Necesita un `fetch` async; el resto del intérprete sigue sync. **No es necesario para el ejemplo del usuario**; se hace cuando se busque escala de conexiones masiva.
- **Swarm a escala:** reescribir el runtime de `spawn` de `std::thread` a tareas `tokio` (M:N) cuando se quieran miles de agentes; la *semántica* (blackboard/señales/locks) no cambia.

Crates: `rayon` (data-parallel), `tokio` (async, ya está para serve), canales (`crossbeam`/`tokio::sync::mpsc`).

---

## 6. Decisiones (RESUELTAS 2026-06-20)

1. **Nombres:** `parallel_map(task, list, limit?)` y `chunk(list, size)`. ✔
2. **Default de `limit`:** 64 para fan-out de I/O; `num_cpus` para cómputo puro (rayon). ✔
3. **Fallo:** **fail-fast por defecto** (primer error cancela el resto y propaga). Colectar-parcial se logra envolviendo la `task` en `try/recover` (devuelve valor-o-error); no hay variante aparte. ✔
4. **Alcance:** **Fase 1 ahora** (pool de hilos acotado para I/O + `rayon` para cómputo; cubre el 10×1000 y datalakes). Fase 2 (tokio async masivo, C100k+) después. ✔

---

## 7. Cómo se gatea (testing)

La concurrencia es no-determinista en *timing* pero **determinista en resultados** (orden de entrada). Así que se gatea como el resto:
- **Resultados:** `parallel_map(task, list)` debe dar **idéntico** a `apply(task, list)` (mismo resultado, en orden) — comparación directa, sin depender de timing.
- **`chunk`/`flatten`:** deterministas → corpus `.syn` vía conform.
- **Concurrencia real (que efectivamente corre en paralelo):** un test con tareas que duermen y miden wall-clock total < suma secuencial (tolerante), como integración Rust.
- **No-regresión:** `conformance/run_all.py` sigue verde (A1 no debe romper la paridad ya lograda).
