# Distribución de Syntecnia — instalable en todos lados, fácil y liviano

> Documento de diseño/roadmap. La paridad Rust + features (A1/A2) están hechas; esto
> es lo que falta para **shippear**. Estado: **diseño, a ejecutar.**

---

## 1. Visión

Syntecnia debe **instalarse en cualquier dispositivo, ser livianísimo y trivial de
instalar**. El norte a largo plazo: que **cualquier persona cree su propia aplicación
desde el teléfono, escribiendo en lenguaje natural, sin saber programar**. La
distribución tiene que servir a TODO tipo de dispositivo — escritorio, servidor, móvil,
y si se puede, **IoT/embebido**.

La buena noticia: **la decisión de Rust ya habilita casi todo esto.** El resultado de
`cargo build --release` es **un único binario nativo, sin runtime** (sin Python, sin
npm, sin JVM, sin DLLs). Esa es la base de "liviano e instalable en todos lados".

---

## 2. La base: un binario, cero dependencias

- `cargo build --release` → **un ejecutable estático**. Con target **musl** en Linux es
  totalmente self-contained (corre en cualquier distro, en un contenedor `scratch`, etc.).
- **Cross-compilación** desde una sola máquina a todas las plataformas/arquitecturas.
- Tamaño: optimizable con `strip`, `opt-level="z"`, LTO, `panic="abort"` → binario chico
  (objetivo: pocos MB), clave para IoT y para descargar rápido en móvil.
- **Ojo (deuda actual):** hoy `database` usa `rusqlite bundled` (SQLite en C) → necesita
  un compilador C al **construir** y complica el cross-compile a algunos targets. Para la
  distribución multi-plataforma conviene evaluar **feature-flags** (build sin SQLite para
  targets mínimos/IoT) o un backend SQLite puro-Rust opcional. Decisión a tomar por target.

---

## 3. Niveles de dispositivo (de lo fácil a lo difícil, con honestidad)

### Nivel 1 — Escritorio + servidor (x86_64 / ARM64) — **fácil, ya alcanzable**
Linux, macOS, Windows; Intel y ARM (incluido Apple Silicon y servidores ARM). Es
cross-compile directo. **Esto se puede shippear ya.**

### Nivel 2 — Instalación trivial (la experiencia "fácil")
- **Script one-liner:** `curl -fsSL https://syntecnia.org/install | sh` (y equivalente
  PowerShell para Windows). Detecta SO/arquitectura, baja el binario correcto, lo pone en
  el PATH. Es el estándar de facto (rustup, deno, bun lo hacen así).
- **Gestores de paquetes:** Homebrew (mac/linux), Scoop/winget (Windows), `.deb`/`.rpm`,
  AUR, Nix. Cada uno es "envoltura" del mismo binario.
- **GitHub Releases** con binarios por plataforma + checksums firmados.
- **Contenedor:** imagen Docker `FROM scratch` + el binario (imagen de pocos MB).

### Nivel 3 — Raspberry Pi / SBCs ARM — **fácil-medio**
Cross-compile a `armv7`/`aarch64-unknown-linux-musl`. Corre en Raspberry Pi y similares
sin problema. Buen primer paso hacia "IoT".

### Nivel 4 — Móvil (Android / iOS) — **el desafío del norte de la visión**
Aquí está el corazón de "crear una app desde el teléfono". Es lo más difícil y tiene
sub-opciones — hay que elegir arquitectura:

- **(a) Syntecnia como runtime embebido en una app contenedora.** Una app nativa
  (o el propio Syntecnia) que **interpreta programas `.syn`** en el dispositivo. Rust
  compila a Android (NDK, `aarch64-linux-android`) y iOS (`aarch64-apple-ios`) — el
  intérprete corre on-device. La "app del usuario" es un programa `.syn` que el runtime
  ejecuta. **Esta es la vía más alineada con la visión** (el usuario no compila nada;
  escribe y el runtime corre).
- **(b) Syntecnia en la nube, el teléfono es cliente.** El programa `.syn` corre en un
  servidor Syntecnia y el teléfono accede vía web/PWA. Más simple de shippear, pero no es
  "en el dispositivo".
- **(c) Generar una app real.** El `.syn` se empaqueta como app instalable. Mucho más
  complejo (stores, firmas, build per-plataforma).

Recomendación: **(a) como objetivo, (b) como puente** mientras (a) madura. El generar-NL
→ `.syn` (la parte de "sin saber programar") es una **capa de producto encima**, no de la
distribución; pero la distribución debe dejar el runtime listo on-device para habilitarla.

### Nivel 5 — IoT / embebido de verdad — **difícil, evaluable**
- **Linux embebido** (OpenWRT, Yocto): es como el Nivel 3 — cross-compile musl. **Viable.**
- **Microcontroladores sin SO** (ESP32, etc., `no_std`): el intérprete actual usa `std`
  (hilos, archivos, red). Correr en `no_std` requeriría una **edición mínima del runtime**
  (sin threads/FS, intérprete core solo). Es un proyecto aparte; **evaluable a futuro**,
  no de entrada. El núcleo del lenguaje (lexer/parser/intérprete) es portable; la stdlib
  (serve/db/cron) no aplica en ese nivel.

---

## 4. Lo que la distribución debe garantizar (requisitos)

- **Un comando para instalar**, sin prerequisitos (ni Python, ni Node, ni compilador).
- **Binario chico** (optimizado para tamaño) — importa en móvil e IoT.
- **Multi-arquitectura** desde el día uno (x86_64 + ARM64 al menos).
- **Auto-update** simple (el binario puede actualizarse a sí mismo, estilo rustup).
- **Verificable**: releases firmadas + checksums (cadena de suministro confiable).
- **Sin telemetría oculta** (lenguaje libre, instalable donde quieras).

---

## 5. Roadmap sugerido (orden por valor/esfuerzo)

1. **Binario release optimizado** (tamaño) + resolver el tema SQLite/C para cross-compile
   (feature-flags por target).
2. **GitHub Releases multi-plataforma** (Linux/mac/Win × x86_64/ARM64) con CI que
   cross-compila y firma.
3. **Script de instalación** `curl|sh` + PowerShell → el "súper fácil".
4. **Sitio `syntecnia.org`** con la descarga + docs + el one-liner.
5. **Gestores de paquetes** (brew/scoop/deb/...) y **Docker** `scratch`.
6. **Raspberry Pi / ARM SBC** (cross-compile musl ARM) — primer "todos los dispositivos".
7. **Móvil (a): runtime on-device** (Android NDK / iOS) — la apuesta grande de la visión.
8. **IoT/embebido `no_std`** — evaluación + edición mínima del runtime (proyecto aparte).

---

## 6. Conexión con la visión "app desde el teléfono sin programar"

La distribución es el **cimiento**, no el producto final. Dos capas encima, fuera de este
documento pero habilitadas por él:

- **Runtime on-device** (Nivel 4a) → el teléfono puede *ejecutar* programas Syntecnia.
- **Capa NL→`.syn`** (lenguaje natural a programa) → la persona describe lo que quiere y
  se genera el `.syn` que el runtime corre. (Syntecnia ya es agent/LLM-oriented, así que
  esta capa encaja naturalmente — pero es producto, no distribución.)

El binario único + el runtime on-device hacen **posible** que "cada persona cree su app
desde el teléfono". La distribución bien hecha es lo que vuelve esa visión instalable en
las manos de cualquiera.
