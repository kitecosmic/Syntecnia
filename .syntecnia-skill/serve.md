# Syntecnia HTTP Server — `serve`

A native HTTP server with **zero dependencies** (built on Python's `http.server`).
Define routes in Syntecnia, the runtime enforces a consistent response contract,
pagination, auth and input validation for you.

## Capability

Serving on a port requires the `serve` capability, scoped to that exact port:

```
require serve(8080)
```

Without it, `serve on 8080` fails with a clear error:
`serve on 8080 is not permitted: missing capability serve(8080). Add `require serve(8080)``.
The scope is the port — `require serve(8080)` does **not** allow `serve on 9090`.

## Basic shape

```
require serve(8080)

serve on 8080
    auth with check_token            -- optional
    route "GET /products"
        give all_products()
    route "POST /products" requires auth
        expect body {name: text, price: number}
        let b be json of request
        give created(b)
```

- `serve on PORT` opens a server block. It starts a background, threaded server
  and returns immediately. The CLI keeps the process alive while servers run.
- `route "METHOD /path"` defines a handler. The body is ordinary Syntecnia.
- Named path params use `:name` → `route "GET /products/:id"`.
- A trailing **catch-all** `*name` captures the rest of the path (variable depth)
  → `route "GET /files/*path"` matches `/files/a/b/c` with `params.path == "a/b/c"`.
  It must be the last segment and needs at least one segment to capture.

### Route precedence (by specificity, not order)

When several routes could match the same path, the **most specific** wins —
regardless of declaration order, so a `:param` or catch-all can never accidentally
swallow a more specific route:

```
exact segment  >  :param  >  *catchall
```

`route "GET /files/special"` beats `route "GET /files/:id"` beats
`route "GET /files/*path"` for `/files/special`, even if the catch-all is declared
first.

### Soft keywords

`serve`, `on`, `route`, `auth`, `requires`, `expect`, `max_body`, `max_streams`,
`stream`, `send`, `rate_limit`, `per`, `static`, `from` and `cors` are **soft
keywords**: they are special *only* at the start of their construction
(`serve on N`, `route "..."`, `requires auth`, `expect body {...}`,
`max_body "10mb"`, `max_streams N`, a `stream` block, `send` inside one,
`rate_limit N per window`, `static "./dir"`, `static "/p" from "./dir"`,
`cors "*"`). Everywhere else they are ordinary names — `let route be "/x"`,
`let static be 1`, `let from be 3` and `task auth(x)` are valid. The parser
decides with fixed lookahead, never heuristics.

## The request

Inside a handler you have:

```
request          -- map with .method .path .body .json .headers .user .body_file
json of request  -- parsed JSON body (a map), or nothing
body of request  -- raw body text (in-memory bodies; "" when spilled to disk)
headers of request
user of request  -- set after auth (see below)
ip of request    -- the client's real peer IP (used for rate limiting)
body_file of request  -- temp file path when a large body spilled to disk, else nothing
read_body()      -- read the full body text (from memory or the temp file)
query            -- query string as a map: /x?page=2 → query.page == "2"
params           -- path params as a map: /products/:id → params.id
```

All `query` and `params` values are text. Use `read_body()` to get the whole
body regardless of where it lives (memory or disk) — see "Request body limits".

## Response contract (enforced by the runtime, on the BODY you `give`)

| You `give`            | Response body                                                        |
|-----------------------|----------------------------------------------------------------------|
| a **map**             | the object as-is                                                     |
| a **list**            | `{"items": [...], "count": <page>, "total": <real>, "cursor": <next or null>}` |
| a **scalar** (text/number/bool) | the value as JSON, as-is                                    |
| nothing / no `give`   | `null`                                                              |

Helpers set the HTTP status:

```
ok(x)             -- 200, body shaped per the table above
created(x)        -- 201
not_found(text)   -- 404 → {"error": text, "status": 404}
not_found(map)    -- 404 → the map as-is (custom 404 body)
fail(code, msg)   -- {"error": msg, "status": code}
fail(msg)         -- {"error": msg, "status": 400}
fail(code)        -- {"error": "error", "status": code}
```

Errors never crash the server:

```
expect failure         → 400  {"error": "...", "status": 400, "field": "..."}
malformed JSON body    → 400  {"error": "malformed JSON body", "status": 400}
uncaught error / 1/0   → 500  {"error": "...", "status": 500}
unauthorized           → 401
method not allowed     → 405  (with an `Allow` header listing valid methods)
unknown route          → 404
body larger than 1 MB  → 413  {"error": "payload too large", "status": 413}
```

`OPTIONS` returns `204` with an `Allow` header; `HEAD` behaves like `GET` with no
body. A malformed body is only an error when `Content-Type` says JSON; otherwise
`json of request` is `nothing` and `body of request` keeps the raw text.

## Serving web pages (HTML, static files, CORS)

`serve` is not only a JSON API — it can serve a real web app: HTML responses,
static assets (CSS/JS/images), and the CORS headers a browser needs.

### HTML and other content-types — `html()`, `respond()`

`give <value>` always produces JSON (`give "<h1>Hi</h1>"` returns the JSON string
`"<h1>Hi</h1>"`, **not** a page). To return non-JSON, use these helpers:

```
route "GET /"
    give html("<h1>Hello</h1>")           -- 200, text/html; charset=utf-8, raw body

route "GET /report.csv"
    give respond("a,b,c\n1,2,3", "text/csv")   -- any content-type

route "GET /legacy"
    give respond("<x/>", "application/xml", 404)   -- content-type + status
```

- `html(content)` → status `200`, `Content-Type: text/html; charset=utf-8`, body
  written **verbatim** (no `json.dumps`, no quotes).
- `respond(content, content_type, status?)` → arbitrary content-type, optional
  status (default `200`).
- `give <map>` / `give <list>` are unchanged — still JSON. For text plain use
  `respond(x, "text/plain")` (the `text(...)` builtin is value conversion, not a
  response helper).

### Static files — `static "./dir"` (and mounts)

Declare a directory and any GET/HEAD that doesn't match a declared route is served
from it. You can mount several dirs, each at its own URL prefix:

```
serve on 8090
    static "./public"                 -- root mount: "/" → ./public/index.html
    static "/assets" from "./assets"  -- mount ./assets under the "/assets" prefix
    route "POST /api/signup"          -- declared routes ALWAYS win over static
        ...
```

- A `GET`/`HEAD` with no matching route falls through to the static handler:
  the file is served with a content-type from its extension (`.html`, `.css`,
  `.js`, `.png`, `.svg`, `.json`, … — these common web types are **pinned** so the
  result doesn't depend on the host's mime registry); a missing file → `404` JSON.
- **Directory index.** `/` serves `index.html`; a subfolder serves its
  `index.html` too — `/docs/` (and `/docs`) → `<dir>/docs/index.html`.
- **Multiple mounts.** `static "./dir"` mounts at the root; `static "/p" from
  "./dir"` mounts under `/p`. Longer prefixes are matched first, so `/assets/...`
  is served from the `/assets` mount before the root mount is tried. Declaring two
  mounts at the **same** prefix is an **error** (no silent shadowing).
- **Declared routes always win.** If a path is declared (for any method) it is
  never shadowed by a file — a different method on it gets `405`, not a file.
- **Only `GET`/`HEAD`.** A `POST` to a static path is not served (→ `404`/`405`).
- **The declaration is the permission.** `static "./public"` grants reading from
  that directory; you do **not** also need a `file()` capability for it. Relative
  paths are resolved against the program's working directory.
- **Path traversal is blocked, per mount.** `../`, encoded `..%2f`, absolute paths
  and symlinks escaping the directory are rejected — the resolved real path must
  stay inside that mount's root.

> **Same-origin tip:** if the landing page is served by `static` from the same
> server as the API, the browser's `fetch` is **same-origin** and needs no CORS.

### CORS — `cors "*"` / `cors "https://app.com"`

For APIs called from a browser on a **different** origin, declare CORS:

```
serve on 8090
    cors "*"                     -- or cors "https://app.example.com"
    route "GET /api/data"
        give [...]
```

- With `cors` declared, every response carries `Access-Control-Allow-Origin:
  <origin>`. A preflight `OPTIONS` additionally returns
  `Access-Control-Allow-Methods` (the path's methods),
  `Access-Control-Allow-Headers: Content-Type, Authorization` and
  `Access-Control-Max-Age`.
- Without `cors`, no CORS headers are sent (unchanged behavior).
- **Credentials caveat:** the CORS spec forbids `*` for requests with
  credentials (`Authorization`/cookies). If you send credentials cross-origin, set
  a **specific** origin (`cors "https://app.example.com"`), not `*`.

## Pagination

Collections are **never** returned unbounded.

- Default `limit` is 100 (max 1000).
- `?limit=N` sets the page size.
- `?cursor=N` (or `?offset=N`) sets where the page starts.
- `total` is always the real total; `cursor` is the next offset, or `null` on the last page.

```
GET /products?limit=2          → {"items":[...2...], "count":2, "total":57, "cursor":2}
GET /products?limit=2&cursor=2 → {"items":[...2...], "count":2, "total":57, "cursor":4}
```

**Rule:** with `give <list>`, the handler must return the **whole** collection —
the runtime is the sole owner of `LIMIT`/`OFFSET`/`total`. Never put `LIMIT` in
your own query when you `give <list>`, or `total` would be wrong. Note that
`give <list>` also loads the full collection into memory.

### `paged()` — for large tables (SQL pushdown, exact total)

For big result sets, `give paged("SELECT ...", [params])` fetches **only the
requested page** (the runtime appends `LIMIT`/`OFFSET`) and computes `total` with
a `COUNT(*)`, so nothing is fully materialized:

```
route "GET /products"
    give paged("SELECT id, name, price FROM products ORDER BY id")
```

- Same envelope and `?limit`/`?cursor` semantics as `give <list>`, but `total` is
  exact and only one page is read from the DB.
- Always pass values as `params` (parameterized) — never string-concatenate.
- Do **not** add your own `LIMIT`/`;` to the query.
- Outside a route handler, `paged()` degrades to the full result set.

## Streaming responses (SSE)

A route can emit many messages over time on one connection — LLM tokens, a data
feed, MCP events — using **Server-Sent Events**. Open a `stream` block and emit
with `send`:

```
serve on 8080
    max_streams 200                  -- optional; default 100

    route "GET /events"
        stream
            each tick in range(10)
                send {"count": tick}         -- → data: {"count":0}\n\n

    route "GET /llm"
        stream
            let answer be generate "reply" given prompt
            each token in answer
                send token as "token"        -- → event: token\n data: "..."\n\n
```

- `send <value>` emits `data: <json(value)>` (the value as-is — no pagination
  envelope; that is only for `give`). `send <value> as "name"` adds `event: name`.
- The stream ends when the `stream` block ends; the server then closes the
  connection. **`stream` and `give` are mutually exclusive** in a route: a route
  with a `stream` block responds in SSE mode, otherwise it follows the `give`
  contract.
- Response headers: `Content-Type: text/event-stream`, `Cache-Control: no-cache`,
  `X-Accel-Buffering: no` (disables proxy buffering), and no `Content-Length`.
  Each event is **flushed immediately**, so clients receive messages as they are
  produced.
- **Client disconnect:** if the client goes away mid-stream, the next `send`
  unwinds the handler cleanly (the `each`/loop stops), frees the thread, and
  never crashes the server.
- **Errors mid-stream:** the status was already sent, so the runtime emits a
  final `event: error` event and closes — never a crash.
- **Isolation:** each stream runs in its own interpreter/scope, like any request.
- **Concurrency cap:** in the current one-thread-per-connection model each open
  stream holds a thread, so `max_streams N` (default 100) bounds concurrent
  streams. Over the cap a new stream gets `503 {"error":"too many concurrent
  streams","status":503}` with a `Retry-After` header.
- **Pacing / heartbeat:** `sleep(seconds)` (requires `require time`) paces a
  stream; send a periodic event to keep proxies from timing out.

`stream`, `send` and `max_streams` are soft keywords — only special in this
construction; `let send be 1` is still valid.

## Rate limiting

Protect against brute-force, scraping and spam. Declare a limit on the serve
block (default for all routes) and/or override it per route:

```
serve on 8080
    rate_limit 100 per minute        -- default for every route, per client IP
    auth with check_token

    route "POST /login"
        rate_limit 5 per minute      -- stricter override
        ...

    route "GET /public"              -- inherits the 100/min default
        ...

    route "GET /webhook"
        rate_limit none              -- disable the inherited default
        ...
```

- `rate_limit <N> per <window>` — window is `second`, `minute` or `hour`.
- **Opt-in:** with no `rate_limit` there is no limit. `rate_limit none` (or
  `unlimited`) disables an inherited default on one route.
- **Algorithm:** token bucket — up to `N` per window sustained, with bursts up
  to `N`. Tokens refill continuously.
- **Keyed by the real peer IP.** `X-Forwarded-For` is **not** trusted (a client
  could forge it to evade the limit or flood the table). Per-user keying and
  trusted-proxy `X-Forwarded-For` are future work.
- **Order:** the limit is checked after route matching but **before** auth and
  the handler — so it also throttles the auth task (e.g. 5 login attempts/min
  even with invalid tokens).
- **Over the limit → 429** `{"error":"rate limit exceeded","status":429}` with a
  `Retry-After` header; responses also carry `RateLimit-Limit`,
  `RateLimit-Remaining` and `RateLimit-Reset`. The handler does not run.
- **Memory:** routes sharing the default share one bucket per IP; an overridden
  route gets its own. Stale buckets are purged automatically, so a flood of
  unique IPs can't grow the table without bound.
- The client IP is also available to handlers as `ip of request`.

`rate_limit` and `per` are soft keywords — `let per be 1` is still valid.

## Auth (incoming)

```
serve on 8080
    auth with check_token
    route "GET /me" requires auth
        give {"name": name of (user of request)}
```

For a route marked `requires auth`, the runtime:
1. extracts the bearer token from `Authorization: Bearer <token>`,
2. calls the `auth with` task with that token,
3. if it returns `nothing` → responds **401**,
4. otherwise the returned value is placed in `request.user`.

```
task check_token(token)
    when token == "secret"
        give {"name": "alice"}
    give nothing
```

A route that uses `requires auth` must have an `auth with <task>` on the `serve`
block — otherwise it's a parse error (`syntecnia check` catches it).

## Input validation

```
route "POST /users"
    expect body {name: text, age: number}
    ...
```

`expect body {field: type, ...}` validates the request's JSON body. A missing
field or a type mismatch → **400** with the offending `field` named. Types:
`text`, `number`, `bool`, `list`, `map`.

For finer checks than a type (email, phone, slug…), use `matches(value, pattern)`
— it is a **full match** (the whole value must match), so an unanchored pattern is
already safe for validation; no `^...$` needed:

```
route "POST /signup"
    expect body {email: text}
    let email be email of (json of request)
    when not matches(email, "[^@ ]+@[^@ ]+\.[^@ ]+")
        give fail(422, "that doesn't look like an email")
    ...
```

(For "does the pattern appear somewhere" use `find_all`/`capture` — see builtins.md.)

## Request body limits

The request body is bounded so a single oversized request can't exhaust memory.

```
serve on 8080
    max_body "10mb"        -- optional; default 1mb
    route "POST /upload"
        give {"bytes": length(read_body())}
```

- **Default:** 1 MB when `max_body` is not declared.
- **`max_body`** accepts a size string — `"512kb"`, `"10mb"`, `"1gb"`
  (case-insensitive, 1024-based) — or a raw byte count, or `"unlimited"` /
  `"none"` to disable the cap (only for trusted, internal use). The `"10mb"`
  form is recommended for readability.
- **Real bytes are counted**, never the declared `Content-Length`, so a lying
  length or a `Transfer-Encoding: chunked` body cannot evade the limit.
- **Over the limit → 413** `{"error":"payload too large","status":413}` and the
  connection is closed cleanly (`Connection: close`) — it never leaves an unread
  body to corrupt the next request on a keep-alive connection.
- **Memory vs disk:** small bodies stay in memory (`body of request`,
  `json of request`). Bodies larger than ~1 MB stream to a temp file;
  `body_file of request` is its path and `read_body()` reads it. The temp file
  is removed when the request finishes (even if the handler errors).
- **Chunked** request bodies (no `Content-Length`) are supported and counted.

This is why raising the limit is safe: the cap is on the in-memory buffer, not
on what can be served — large uploads stream to disk rather than being buffered.

## Isolation

Every request runs in its own isolated interpreter and scope (its own variables,
logs and trace) — just like a spawned agent. There is **no shared mutable state**
between requests except the blackboard (`share`/`observe`) and the database
(`sql`/`sql_exec`). Always use parameterized `sql(..., [params])` — never string
concatenation — so path/query/body values can't inject SQL.

## Full example

```
require serve(8080)
require db("./store.db")

db_open("./store.db")
sql_exec("CREATE TABLE IF NOT EXISTS products (id INTEGER PRIMARY KEY, name TEXT, price NUMBER)")

task check_token(token)
    when token == "admin-key"
        give {"role": "admin"}
    give nothing

serve on 8080
    auth with check_token

    route "GET /products"
        give sql("SELECT id, name, price FROM products")

    route "GET /products/:id"
        let rows be sql("SELECT id, name, price FROM products WHERE id = ?", [params.id])
        when length(rows) == 0
            give not_found("product not found")
        give rows[0]

    route "POST /products" requires auth
        expect body {name: text, price: number}
        let b be json of request
        sql_exec("INSERT INTO products (name, price) VALUES (?, ?)", [name of b, price of b])
        give created({"created": true})
```

Run it:

```bash
syntecnia run store.syn      # stays alive while the server runs; Ctrl+C to stop
```
