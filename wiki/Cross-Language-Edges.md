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

## The four relations

| Relation | Tier | Meaning | Direction |
|---|---|---|---|
| `invokes` | Subprocess / CLI | A function runs an external command or in-repo binary | caller -> command/binary |
| `binds_native` | FFI | Code binds to a native library or exported native symbol | binding site -> native sink |
| `calls_service` | HTTP / RPC | A client call targets a route or service | caller -> route/service |
| `handled_by` | HTTP / RPC | A route or service is served by a handler | route/service -> handler |

A client and a server meet at a shared **boundary node**, keyed so both sides
land on the same node:

- a **route** node, keyed by normalized path (`/api/users`), for HTTP;
- a **`grpc_service`** node, keyed by lowercased service name (`grpc:greeter`);
- a **`pyo3_module`** node, keyed by module name (`pyo3:mymod`);
- a **`ws_endpoint`** node, keyed by socket path (`ws:/feed`), and a
  **`ws_message`** node, keyed by message type / event (`ws #connect`), for
  WebSockets;
- a **command** stub for an unresolved subprocess target, and
  `native_library` / `native_addon` / `jni_symbol` stubs for FFI sinks.

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
| Rust | `Command::new` |
| Ruby | `system`, `exec`, `Open3.*`, `IO.popen`, and backtick command strings |
| PHP | `exec`, `shell_exec`, `system`, `passthru`, `proc_open` |

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
| JNI | Java + C/C++ | a Java `native` method and the matching C `Java_*` export both link to a shared `jni:<method>` sink |
| node-gyp / N-API | JavaScript / TypeScript | `require('bindings')('addon')`, `require('node-gyp-build')(...)`, a direct `.node` / `build/Release/...` require |
| cgo | Go | `C.fn()` in a file that `import "C"` (detected by the Go extractor, not the post-pass) |

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

| Side | Python | JS / TS | Go | Rust |
|---|---|---|---|---|
| Server route (`handled_by`) | Flask / FastAPI decorators (`@app.get(...)`, `@router.route(...)`) | Express (`app.get`, `router.post`, ...) | `net/http` `HandleFunc` (incl. Go 1.22 `"GET /path"` method patterns) | axum `.route("/p", get(handler))`, actix `#[get("/p")]` |
| Client call (`calls_service`) | `requests` / `httpx` | `axios`, `fetch` | `http.Get/Post/Head/PostForm` | `reqwest::get`, builder `.get/.post("https://...")` |

### gRPC

A gRPC service is keyed by its lowercased name, so a tonic server impl, a tonic
client, and a cross-language Python client all meet at one `grpc:<service>`
node. tonic server method impls attach via `handled_by`; clients via
`calls_service`. Detection is gated on a `tonic`/`grpc` mention in the file so
the common `<Name>Client` shape is not mistaken for gRPC, and a denylist
excludes well-known non-gRPC `<Name>Client` types (`reqwest`, `redis`,
`postgres`, ...). When one file holds two service impls that share a method
name, each method resolves within its own `impl` block.

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

### Parameterized routes

A concrete client path resolves to a parameterized server template when exactly
one template matches: a `reqwest::get("/users/7")` connects to the
`/users/{id}` handler. Express (`:id`), Flask (`<int:id>`), and axum
(`{id}` / `{*rest}` catch-all) parameter styles are matched. An ambiguous or
unmatched concrete path is left untouched.

### Precision guards

Detection runs over **masked** source: comments, docstrings, and string-literal
*contents* are blanked first (raw strings and char-vs-lifetime handled), so a
commented-out route or a command named inside a doc comment is not detected. In
addition, the bare-builder reqwest client form (`.post("https://...")`) is only
trusted in a file that actually uses `reqwest`, and absolute-URL-only matching
keeps a local `.get("/x")` from being read as a service call.

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
by label during composition, so a client in repo A and a handler in repo B meet
at one route/service node. `mark_cross_repo_edges` then flags the edges that span
repos as `cross_repo` -- but only when the target is genuinely in-repo-backed (a
real definition or a service with an in-repo handler), so a shared external
command or third-party API URL that two repos happen to use is not mislabeled a
cross-repo dependency.

## How impact analysis uses these edges

The four relations are part of the default reverse-impact set, so they are
traversed automatically by:

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

- **Service connectivity** -- of the service-boundary nodes (HTTP route, gRPC
  service, PyO3 module), the fraction that are *two-sided* (have both a consumer
  `calls_service` in and a producer `handled_by` out). A two-sided boundary is
  almost certainly a real coupling; a half-open one is a client to an
  out-of-repo service, a server with no in-repo client, or detector noise.
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
- **Dynamic targets are missed.** A subprocess command or URL built at runtime
  (a template-literal URL `` fetch(`https://${host}/x`) ``, a variable command)
  is not detected -- only literal string arguments are read.
- **Per-binding heuristics.** FFI matching is convention-by-convention; partial
  coverage degrades gracefully rather than guessing.

## See also

- [Querying](Querying) -- the impact relation set and traversal.
- [MCP-Server](MCP-Server) -- the `affected` and `describe_node` tools.
- [Workspaces-and-Federation](Workspaces-and-Federation) -- cross-repo matching.
- [Languages](Languages) -- per-language extraction.
