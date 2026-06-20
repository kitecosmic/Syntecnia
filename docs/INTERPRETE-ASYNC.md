# Intérprete async — qué es, cuándo hace falta, qué cuesta

> Mejora futura **diferida**. No es un hueco del lenguaje: es un techo muy alto que
> probablemente no se toque. Este documento explica la decisión para no re-discutirla
> cada vez. Relacionado con `SPEC-CONCURRENCIA.md` (A1).

---

## 1. Qué tenemos hoy (y alcanza para casi todo)

`parallel_map(task, list, limit?)` ya corre tareas **concurrentes y multi-core real**
(sin GIL), sobre un **pool de hilos** (tokio + `spawn_blocking` + semáforo de `limit`).

- Cada tarea concurrente usa **un hilo del sistema operativo**.
- Cubre de sobra: el ejemplo guía (10k en 10×1000), datalakes, ETL, fan-out de
  **cientos a miles** de operaciones en paralelo, APIs que procesan lotes.
- Techo práctico: **miles** de tareas simultáneas. Un hilo cuesta ~MB de stack +
  scheduling, así que decenas de miles de hilos vivos a la vez sí pesan.

Para el 99% de los casos —incluido telco / marketplace / mensajería en lotes— **esto
es suficiente**.

## 2. Qué es "el intérprete async" (lo diferido)

Permitir **decenas o cientos de miles** de operaciones de **I/O bloqueante**
(típicamente `fetch` esperando red) corriendo **concurrentes a la vez**, sin gastar un
hilo por cada una. Un puñado de hilos multiplexa todas las esperas vía `async/await`
(modelo C100k+: lo que hace nginx/Go/tokio puro).

Ejemplo donde importaría: un **crawler masivo** que dispara 100.000 requests de red
simultáneas, o un fan-out de 50k llamadas a APIs externas en paralelo dentro de **una
sola** operación.

## 3. Por qué cuesta tanto (la razón de diferirlo)

Hoy el **intérprete es síncrono** — fue una decisión deliberada del port (paridad con
Python, y simplicidad). Para async masivo, una función del lenguaje que hace `fetch`
tendría que poder **suspenderse** (ceder el hilo mientras espera la red) y reanudarse
después. Eso significa que **todo el árbol de evaluación** del intérprete
(`eval`, llamadas a tasks, bucles, el handler de cada request) pasa a ser `async`:

- Reescribir el corazón del intérprete a `async fn` / futures (efecto "async coloring":
  contamina toda la cadena de llamadas).
- Resolver el manejo de estado/ownership a través de los `await` (lo que en Rust es
  exactamente lo más delicado).
- Re-verificar la **paridad** completa (los 329 tests + el harness) sobre el nuevo motor.

Es un cambio de **fundación**, comparable en esfuerzo a una parte importante del port
original. Alto riesgo, mucho trabajo, para un beneficio que solo aparece en un caso de
escala extrema.

## 4. Decisión

**Diferido, no descartado.** Disparadores para retomarlo (si alguno se vuelve real):

1. Se mide la necesidad de **>10.000 operaciones I/O concurrentes en un solo fan-out**.
2. Un caso de uso concreto tipo crawler / scraper / fan-out masivo de APIs.
3. Servir **>10.000 conexiones simultáneas** sostenidas (hoy serve es thread-per-conn
   sobre el pool de hyper/tokio; el límite también es de hilos).

Mientras tanto, el pool de hilos actual **no es un parche** — es la opción correcta para
el rango de escala real esperado. El async masivo es un techo, no una carencia.

## 5. Si se hace algún día — esbozo

- Mantener el intérprete sync como camino por defecto; introducir un **modo async** solo
  para el cuerpo de tasks que hacen I/O (no para todo el lenguaje).
- `fetch`/`read`/`sql` async sobre tokio; `parallel_map` mapea a `futures::join_all` con
  límite de concurrencia en vez de pool de hilos.
- Preservar **`parallel_map ≡ apply`** (mismo resultado y orden) — la semántica no cambia,
  solo el mecanismo de espera.
- Gate: el harness de conformidad (`conformance/run_all.py`) debe seguir verde — el async
  no puede cambiar ningún resultado observable.
