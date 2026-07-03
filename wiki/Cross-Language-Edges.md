# Cross-Language Edges

Most graph edges come from parsing one file in one language. **Cross-language
edges** capture coupling that no single-language parse can see: a Python script
that shells out to a Rust binary, a Node addon bound to a native library, a
JavaScript client calling a Python HTTP route, a Rust function exported to
Python through PyO3. Synaptic detects these as a post-extraction pass and adds
edges so impact analysis traverses the boundary.

These edges are always `INFERRED` confidence. Detection is regex-driven over
source (after a masking pass that blanks comments, docstrings, and string
contents that would otherwise produce false matches), so it is best-effort by
design, not a proof. They are never `EXTRACTED` facts.

## The relations

| Relation | Tier | Meaning | Direction |
|---|---|---|---|
| `invokes` | Subprocess / CLI | A function runs an external command or in-repo binary | caller -> command/binary |
| `binds_native` | FFI | Code binds to a native library or exported native symbol | binding site -> native sink |
| `calls_service` | HTTP / RPC / queue / WS / IPC / event | A client, producer, or publisher targets a boundary | caller -> boundary |
| `handled_by` | HTTP / RPC / queue / WS / IPC / event | A boundary is served by a handler/consumer | boundary -> handler |

(`dynamic_ref` and the code->SQL relations `queries`/`writes_to`/`calls_proc`
also count as cross-language couplings for impact traversal, stats, and
cross-repo flagging.)

A client and a server meet at a shared **boundary node**, keyed so both sides
land on the same node:

- a **route** node, keyed by normalized path (`/api/users`), for HTTP;
- a **`grpc_service`** node, keyed by lowercased service name (`grpc:greeter`);
- a **`pyo3_module`** node, keyed by module name (`pyo3:mymod`);
- a **`ws_endpoint`** node, keyed by socket path (`ws:/feed`), and a
  **`ws_message`** node, keyed by message type / event (`ws #connect`), for
  WebSockets;
- a **`queue_topic`** node, keyed by lowercased topic / task name
  (`queue #orders`), for message queues and pub/sub;
- a **command** stub for an unresolved subprocess target, and
  `native_library` / `native_addon` / `jni_symbol` / `c_symbol` stubs for FFI
  sinks.

Because a server's `handled_by` edge and a client's `calls_service` edge point at
the *same* boundary node, reverse-impact from the handler reaches every client
through that node, even across languages and repos.

## Tier 1: subprocess / CLI (`invokes`)

A process invocation links the enclosing function to the command it runs.
Detected forms, by language:

| Language | Detected calls |
|---|---|
| Python | `subprocess.run/call/check_call/check_output/Popen`, `os.system/popen`, `os.exec*` |
| JavaScript / TypeScript | `child_process.exec/execSync/spawn/spawnSync/execFile/execFileSync` (and the bare distinctive names) |
| Go | `exec.Command`, `exec.CommandContext` |
| Rust | `Command::new` — only in files that use `std::process`/`tokio::process`, so a clap `Command::new("myapp")` CLI builder is not misread as a spawn |
| Ruby | `system`, `exec`, `Open3.*`, `IO.popen`, and backtick command strings |
| PHP | `exec`, `shell_exec`, `system`, `passthru`, `proc_open` |
| Java / Kotlin | `new ProcessBuilder(...)`, `Runtime.getRuntime().exec(...)` |
| C# | `Process.Start(...)`, `new ProcessStartInfo(...)` |
| C / C++ | `system(...)`, `popen(...)` |
| Shell | `python/node/ruby/sh <script>` runner lines and `./script` executions |

The command's basename becomes a `command` stub target. A later graph pass
([resolve_command_invocations](#resolution-passes)) retargets the stub to a
matching in-repo file when exactly one file shares its name or stem (e.g. a
Python `subprocess.run("mytool")` linking to the Rust `src/bin/mytool.rs`).
Commands that do not resolve stay as external stubs and are surfaced as
[suggested questions](Analysis-and-Reports) (`cross_language_sink` kind) rather
than dropped.

## Tier 2: FFI (`binds_native`)

Native bindings link the binding site to a shared native sink, so the two sides
of a binding connect once a graph holds both files.

| Convention | Languages | Detected |
|---|---|---|
| PyO3 | Rust + Python | `#[pymodule]` / `#[pyfunction]` / `#[pyclass]`, `wrap_pyfunction!`, `add_class`, `#[pymodule_export]`, `#[pyo3(name=...)]` |
| ctypes / cffi | Python | `CDLL`/`WinDLL`/`OleDLL`/`PyDLL("libfoo")`, `cdll/windll/oledll.LoadLibrary(...)` |
| JNI | Java + C/C++ | a Java `native` method and the matching C `Java_*` export both link to a shared `jni:<method>` sink; the C name is demangled (`_1` → `_`, overload signature suffixes dropped), so `native do_work()` meets `Java_pkg_Cls_do_1work` |
| node-gyp / N-API | JavaScript / TypeScript | `require('bindings')('addon')`, `require('node-gyp-build')(...)`, a direct `.node` / `build/Release/...` require |
| cgo | Go | `C.fn()` in a file that `import "C"` (detected by the Go extractor, not the post-pass) |
| P/Invoke | C# | `[DllImport("lib")]` / `[LibraryImport("lib")]` -> `native_library` sink |
| cffi | Python | `ffi.dlopen("lib")` -> `native_library` sink |
| extern "C" | Rust + any caller | `#[no_mangle]` exports and ctypes call-sites (`lib.add(...)`) meet at a shared `c_symbol:<name>` sink |

### Two-sided PyO3

PyO3 is split across the file boundary. A `#[pymodule]` emits a `pyo3:<module>`
boundary node carrying the symbol names it registers; each `#[pyfunction]` /
`#[pyclass]` definition is tagged as an export. A graph pass then links the
boundary to those definitions **by name across files** (the module and the
function it exports usually live in different files), and joins any Python
`import <module>` to the boundary. The result: reverse-impact from a Rust
function reaches the Python code that imports the module, even when the
`#[pymodule]` and the `#[pyfunction]` are in separate files.

## Tier 3: HTTP / RPC service boundaries (`calls_service` / `handled_by`)

A route is keyed by its normalized path, so a server handler and a client call
to the same path land on the same node and connect with no resolution pass for
the same-repo case. The HTTP method rides along as edge context.

| Side | Python | JS / TS (incl. Vue/Svelte/Astro `<script>` blocks) | Go | Rust | Java / Kotlin | C# | PHP | Ruby | Shell |
|---|---|---|---|---|---|---|---|---|---|
| Server route (`handled_by`) | Flask / FastAPI decorators (+ Blueprint/APIRouter prefixes), Django `urlpatterns`, aiohttp `add_get` | Express-style on any receiver (+ `app.use` mounts), NestJS `@Controller`/`@Get` | `net/http` `HandleFunc` (incl. Go 1.22 method patterns), gin/echo/chi/fiber verb methods, gorilla `.Methods` | axum `.route` (+ `.nest` prefixes, chained `get(h).post(h2)`), actix `#[get]` | Spring `@GetMapping`/`@RequestMapping` (+ class prefix), JAX-RS `@GET`+`@Path` | minimal-API `MapGet/...`, `[HttpVerb]` + `[Route("api/[controller]")]` | Laravel `Route::verb` | Sinatra / Rails-routes verbs | — |
| Client call (`calls_service`) | `requests`/`httpx` (+f-strings, Session/Client instances), aiohttp, `urlopen` | `axios` (+ `axios.create` baseURL), `fetch` (+ options method, template literals, consts) | `http.Get/Post/Head/PostForm` | `reqwest::get`, builder `.verb("https://...")` | RestTemplate, `HttpRequest`+`URI.create`, OkHttp, Retrofit `@GET` | `HttpClient` verb `...Async`, `HttpRequestMessage` | Guzzle `->verb(absolute URL)`, `Http::` facade | `Net::HTTP`, Faraday, HTTParty | `curl` (`-X` honored), `wget` |

### gRPC

A gRPC service is keyed by its lowercased name, so every server and client
meets at one `grpc:<service>` node. Servers attach via `handled_by`: tonic
impls, Python `<Svc>Servicer` subclasses / `add_<Svc>Servicer_to_server`, Go
`Register<Svc>Server`, Java `extends <Svc>Grpc.<Svc>ImplBase`, C#
`: Svc.SvcBase`. Clients via `calls_service`: tonic `<Svc>Client`, Python
`<Svc>Stub`, Go `New<Svc>Client`, Java `<Svc>Grpc.new*Stub`, C#
`new Svc.SvcClient`, JS `new <Svc>Client` (gated on `@grpc/grpc-js`).
Detection is gated on a `tonic`/`grpc` mention in the file so the common
`<Name>Client` shape is not mistaken for gRPC, a denylist excludes well-known
non-gRPC `<Name>Client` types (`reqwest`, `redis`, `postgres`, ...), and
generated sources (`*_pb2_grpc.py`, `*_grpc.pb.go`, `*_grpc_pb.js`) are
skipped entirely. When one file holds two service impls that share a method
name, each method resolves within its own `impl` block.

### Message queues / pub-sub (`queue #<topic>`)

A producer and a consumer never reference each other -- only a topic name -- so
each detected site attaches to a **`queue_topic`** boundary node (`queue
#orders`), producers via `calls_service` and consumers via `handled_by`
(context `queue`). Covered, each gated on its library's token so a generic
`.publish(`/`.subscribe(` never fires alone: Kafka (kafka-python, kafkajs,
Spring `@KafkaListener` + `KafkaTemplate.send`), RabbitMQ (pika
`basic_publish`/`basic_consume`, amqplib `sendToQueue`/`consume`), NATS and
Redis pub/sub (Python + JS), and Celery (`@app.task` workers meet
`send_task`/`.delay()` producers at `queue #task:<name>`).

### WebSocket

A WebSocket couples a client and a server that exchange JSON command messages
(or socket.io events) over a long-lived socket — coupling the AST walk does not
see and that no HTTP/RPC detector covers. Two boundary-node kinds are minted,
both reusing `calls_service` (client) / `handled_by` (server) so the cross-repo
flagging applies unchanged:

- a **`ws_endpoint`** node, keyed by the socket URL path (`ws:/feed`); named
  paths only — a bare `/` is too generic to key on;
- a **`ws_message`** node, keyed by the lowercased application message type /
  event name (`ws #connect`). It is intentionally endpoint-independent, because
  the connection URL and the message sites routinely live in different files
  (a connector module vs. the domain modules that send commands).

Covered stacks: JS/TS raw `ws` (`socket.send({ cmd: 'connect' })` /
`.request({...})` plus a `case "connect":` dispatch) and socket.io
(`emit`/`on`); C# WebSocketSharp / `System.Net.WebSockets` (`AddWebSocketService`
+ `case` arms); Python `websockets` + python-socketio (`@sio.on` / `emit`); Rust
tungstenite (endpoint only — per-frame dispatch is not regex-tractable).
socket.io lifecycle events (`connection`, `connect`, `disconnect`, ...) are
excluded. So a JS client that sends `{ cmd: 'subscribe' }` and a C# service whose
`case "subscribe":` handles it meet at one `ws #subscribe` node, and editing the
handler surfaces the client as an affected dependent across the repo boundary.

### Route identity

A route node's identity is the **canonical path**: every parameter segment
(`:id`, `{id}`, `<int:id>`, `{id:int}`) normalizes to one placeholder, literal
segments are lowercased, and a hash of the canon keeps folded-away distinctions
apart. So an Express `/users/:id`, an axum `/users/{id}`, and a Flask
`/users/<int:id>` are ONE endpoint node, while `/users/id` (literal), `/a-b`
vs `/a/b`, and `{*rest}` catch-alls stay distinct. A bare `/` is too generic
to key on and is skipped (like WebSocket endpoints).

### Composed prefixes & URL forms

Route keys include same-file mount/constructor prefixes, and clients are read
in their modern spellings:

- **Server prefixes**: FastAPI `APIRouter(prefix=...)` + `include_router(...,
  prefix=...)`, Flask `Blueprint(url_prefix=...)` + `register_blueprint`,
  Express `app.use('/api', router)`, axum `.nest("/api", sub)` (same-file,
  single level; cross-file mounts are out of scope).
- **Template URLs**: `` fetch(`/api/users/${id}`) `` and Python f-strings key
  `/api/users/{param}`; a leading `${BASE}` hole is dropped; an all-hole
  template is skipped.
- **Constants and instances**: `const U = '/api/users'; fetch(U)` resolves one
  hop (same file, single binding); `axios.create({baseURL})`,
  `httpx.Client(base_url=...)`, and `requests.Session()` instance calls
  compose their base URL.
- **Authority as context**: matching stays path-keyed, but an absolute URL's
  host rides on the edge as context (`GET api.github.com`), so consumers can
  tell same-path couplings on different hosts apart.

### Parameterized routes

A concrete client path resolves to a parameterized server template when exactly
one template matches: a `reqwest::get("/users/7")` connects to the
`/users/{id}` handler. Express (`:id`), Flask (`<int:id>`), and axum
(`{id}` / `{*rest}` catch-all) parameter styles are matched. An ambiguous or
unmatched concrete path is left untouched.

### Precision guards

Detection runs over **masked** source: comments, docstrings, and string-literal
*contents* are blanked first (raw strings and char-vs-lifetime handled), so a
commented-out route or a command named inside a doc comment is not detected.
Beyond masking:

- the bare-builder reqwest client form (`.post("https://...")`) is only trusted
  in a file that actually uses `reqwest`, and absolute-URL-only matching keeps a
  local `.get("/x")` from being read as a service call;
- the WebSocket `.send({ type: ... })` message scan runs only in files that use
  a WebSocket API, so an HTTP response body (`res.send({ type: 'success' })`)
  cannot mint a message boundary;
- an Express route registration requires a handler argument, so the 1-arg
  settings getter `app.get('port')` is not a route;
- Rust `Command::new` requires a `std::process`/`tokio::process` mention (clap
  builders excluded), and generated gRPC sources (`*_pb2_grpc.py` etc.) are
  skipped entirely;
- a URL built by concatenation (`'/users/' + id`) keys a `/users/{param}`
  template rather than truncating to the wrong `/users` route, and `fetch`'s
  `{ method: ... }` option, Flask's full `methods=[...]` list, every chained
  axum `get(h).post(h2)` pair, and Go's `PostForm` all record their real
  HTTP methods.

## Dynamic dispatch

Runtime dispatch -- event buses, Electron IPC, and reflection -- is coupling the
AST walk does not see, and a symbol reached only that way looks like a 0-dependent
leaf. Synaptic resolves what it can and is honest about the rest.

### Event buses and IPC (boundary nodes)

Like WebSockets, these mint a channel boundary node that reuses `calls_service`
(publisher) / `handled_by` (subscriber), so a handler reached only across the bus
is not a phantom 0-caller and a cross-file/cross-repo publisher meets its
subscriber on one node:

- **Event buses** -> an `event #<name>` node. Node `EventEmitter`
  (`.emit` / `.on` / `.once` / `.addListener`, gated on an `EventEmitter` token so
  ordinary `.on` from jQuery/sockets does not fire), DOM `CustomEvent`
  (`dispatchEvent(new CustomEvent('e'))` + `addEventListener('e')`, with standard
  DOM events like `click`/`load` excluded), and C# events (`Foo?.Invoke(...)` +
  `Foo += handler`, gated on a real `event` declaration so an arithmetic `total +=
  x` does not mint a channel).
- **Electron IPC** -> an `ipc #<channel>` node (`ipcMain.handle`/`ipcRenderer.on`
  handlers; `ipcRenderer.invoke`/`webContents.send` senders).

### Reflection / dynamic-dispatch sites (the honest residual)

By-name member calls (`obj[expr]()`), `Reflect.*`, dispatch tables, `eval` /
`new Function`, dynamic `import()`, .NET `GetMethod` / `Activator.CreateInstance`,
Python `getattr` / `importlib`, and JVM `Class.forName` / `getMethod` are recorded
as **`dynamic_sites`** metadata on the enclosing node (no new node kind). When such
a site dispatches on a **string literal** that resolves to exactly one symbol
(same-repo first), a low-confidence **`dynamic_ref`** edge links it to that target,
so the target shows up as a caveated dependent. A computed / opaque name cannot be
linked and stays catalog-only.

Either way the residual risk is surfaced, never hidden: list the sites with
[`synaptic hazards`](Commands#hazards) or the
[`dynamic_hazards`](MCP-Server#dynamic_hazards) MCP tool, and a 0-dependent
[`affected`](Querying#affected) result for a symbol in such a scope carries a
`dynamic_caveat`. `graph_stats` reports the totals (`dynamic_sites`,
`dynamic_sites_opaque`, `dynamic_refs_linked`).

## Resolution passes

After the per-file scan, graph-level passes stitch the boundary nodes together
over the full node set:

| Pass | What it does |
|---|---|
| `resolve_command_invocations` | retarget a `command` stub to a unique in-repo file (subprocess -> binary/script) |
| `resolve_route_handlers` | link an axum route to a handler function defined in another file |
| `resolve_parameterized_routes` | merge a concrete client path into the matching server template |
| `resolve_pyo3_modules` | link a `#[pymodule]` boundary to the definitions it registers, across files |
| `resolve_pyo3_imports` | join a Python importer of a native module to its PyO3 boundary |
| `mark_cross_repo_edges` | flag cross-language edges whose endpoints live in different federated repos |

### Cross-repo

In a federated [workspace](Workspaces-and-Federation), boundary nodes are merged
during composition by **identity** -- node type plus a case-folded label (the
canonical path for routes, so `/users/:id` meets `/users/{id}`) -- so a client
in repo A and a handler in repo B meet at one route/service/queue node, and a
`command` stub can never merge with a same-named SQL table. The PyO3,
subprocess-command, and SQL resolution passes run again over the composed
graph, so an importer, invoker, or query in one member joins the extension,
binary, or schema that lives in another.

`mark_cross_repo_edges` then flags the edges that genuinely span repositories.
A merged boundary node is repo-neutral: an edge through it is judged by the
repos of the real endpoints on its two sides (consumers vs providers), never by
the boundary's own first-seen tag -- so the flags do not depend on member
composition order. Direct real-to-real couplings (a resolved command, a
`dynamic_ref`, code -> table) flag only when the target is in-repo-backed, so a
shared external command or third-party API URL two repos happen to use is not
mislabeled a cross-repo dependency. The flaggable set is `invokes`,
`binds_native`, `calls_service`, `handled_by`, `dynamic_ref`, `queries`,
`writes_to`, `calls_proc`.

## How impact analysis uses these edges

These relations (`calls_service`, `handled_by`, `invokes`, `binds_native`, and the
evidence-linked `dynamic_ref`) are part of the default reverse-impact set, so they
are traversed automatically by:

- `synaptic affected` and the MCP `affected` tool -- the blast radius now spans
  language boundaries.
- `synaptic predict` / `predict_impact`, `affected_tests`, `predict_edit` --
  forecasts and test selection follow the same edges.
- the MCP `describe_node` tool -- its "calls Z" clause includes outgoing
  `invokes` and `calls_service` targets.

See [Querying](Querying) for the relation set and [MCP-Server](MCP-Server) for
the tools.

## Calibration: `synaptic eval cross-language`

Because these edges are inferred, the value is knowing how grounded they are.
The calibration command measures one built graph (no git history):

```
synaptic eval cross-language [--graph <path>] [--json]
```

It reports, per relation, the edge counts plus two precision proxies:

- **Service connectivity** -- of the boundary nodes (HTTP route, gRPC service,
  PyO3 module, queue topic, WebSocket endpoint/message, IPC/event channel), the
  fraction that are *two-sided* (have both a consumer `calls_service` in and a
  producer `handled_by` out), plus a per-type breakdown
  (`two_sided_by_type`). A two-sided boundary is almost certainly a real
  coupling; a half-open one is a client to an out-of-repo service, a server
  with no in-repo client, or detector noise -- list them with the
  `dangling-endpoints` structural pattern.
- **Invocation resolution** -- of the `invokes` (subprocess) edges, the fraction
  whose target resolved to an in-repo file rather than an external command stub.

```
Cross-language calibration: cross-language: 14 edge(s); service boundaries 4/6
two-sided (66%); invocations 0/0 resolved (0%); 0 FFI binding(s)
```

`--json` emits the full `CrossLanguageReport` (relation counts, totals,
boundary/two-sided counts, invocation totals, FFI count). Calibration is
advisory: it measures, it does not retune.

## Limitations

- **Inferred, not proven.** Command strings and route paths are rarely
  statically provable; treat these edges as leads, not facts.
- **Opaque dynamic targets are missed.** Template-literal / f-string URLs,
  same-file constants, and instance base URLs ARE read (holes become `{param}`
  segments), but a URL or command assembled across files or from config still
  is not -- an all-hole template (`` fetch(`${url}`) ``) is skipped rather than
  guessed.
- **Per-binding heuristics.** FFI matching is convention-by-convention; partial
  coverage degrades gracefully rather than guessing.

## See also

- [Querying](Querying) -- the impact relation set and traversal.
- [MCP-Server](MCP-Server) -- the `affected` and `describe_node` tools.
- [Workspaces-and-Federation](Workspaces-and-Federation) -- cross-repo matching.
- [Languages](Languages) -- per-language extraction.
