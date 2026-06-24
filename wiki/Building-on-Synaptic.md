# Building on Synaptic

Synaptic's output *is* an API. `synaptic extract` writes `graph.json`, a single,
self-contained, fully-documented file describing every symbol and relationship it
found. You do not need to link any Synaptic code, run a server, or learn a Rust
API to build on it: load that one file in whatever language you already use and
you have the whole knowledge graph in memory.

This page is about that path -- treating `graph.json` as the contract and building
your own tools, dashboards, CI checks, or **your own MCP server** on top of it.
The exhaustive field-by-field spec lives in [Output Formats](Output-Formats#graphjson-the-canonical-artifact);
this page is the consumer's guide to using it.

> **License note.** `graph.json` is a data artifact your copy of Synaptic
> produces. Reading it imposes no licensing obligation on your code. (Linking the
> Synaptic crates as a library is a different path and does carry Synaptic's
> AGPL-3.0 copyleft -- not covered here.) Building on the graph file keeps your own
> code entirely your own.

## Why the graph file is a good API surface

- **Language-agnostic.** It is plain JSON in the widely-understood NetworkX
  node-link shape. Any language with a JSON parser can read it; Python even loads
  it directly with `networkx.node_link_graph(data, edges="links")`.
- **Self-contained.** One file holds every node and edge. No database, no daemon,
  no network. Reading it is offline and instant.
- **Deterministic.** The same source tree produces the same graph: node ids and
  community numbers are stable across rebuilds (see [Architecture](Architecture)).
  A reference your tool stored last week still resolves after a re-extract.
- **Lossless.** Unknown keys round-trip, so re-importing a graph you have read and
  re-emitted does not lose information.

## The contract at a glance

The full schema is in [Output Formats](Output-Formats#graphjson-the-canonical-artifact).
The parts a consumer almost always touches:

```json
{
  "directed": false,
  "multigraph": false,
  "nodes": [ ... ],
  "links": [ ... ],
  "hyperedges": []
}
```

- Edges are under **`links`** (NetworkX's `edges="links"` convention), not
  `edges`. `hyperedges` is always present (often empty).

A **node** -- the keys you can rely on:

```json
{
  "id": "auth_service",
  "label": "AuthService",
  "file_type": "code",
  "source_file": "src/auth/service.py",
  "source_location": "L42",
  "kind": "class",
  "visibility": "public",
  "community": 3,
  "repo": "app"
}
```

`id` is the stable key you join on. `label` is the display name. `kind`
(`function`/`class`/`method`/`struct`/`table`/`column`/...), `visibility`, and
`span` appear for code nodes in supported languages and are omitted otherwise.
`community` is the cluster id. `repo` is present only in a federated graph (see
below).

A **link** (edge):

```json
{
  "source": "login_controller",
  "target": "auth_service",
  "relation": "calls",
  "confidence": "EXTRACTED",
  "source_file": "src/web/login.py",
  "source_location": "L42"
}
```

`source`/`target` are node ids. `relation` is the relationship name -- `calls`,
`imports`, `inherits`, `implements`, `uses`, `contains`, `method`, the
cross-language relations (`invokes`, `calls_service`, `binds_native`, ...), and
more. `confidence` is `EXTRACTED` / `INFERRED` / `AMBIGUOUS`. A federated
cross-repo edge additionally carries `"cross_repo": true`.

> **Stable vs. internal keys.** Treat the keys documented in
> [Output Formats](Output-Formats) as the contract. Keys prefixed with an
> underscore (`_origin`, `_node_type`, ...) are internal extraction details that
> round-trip through the file but are **not** part of the stable surface -- do not
> build on them.

## Reading it (any language)

The whole pattern is: index nodes by `id`, then build adjacency from `links`. In
Python:

```python
import json, collections

g = json.load(open("synaptic-out/graph.json"))
by_id = {n["id"]: n for n in g["nodes"]}

outgoing = collections.defaultdict(list)   # what each node depends on
incoming = collections.defaultdict(list)   # what depends on each node
for e in g["links"]:
    outgoing[e["source"]].append(e)
    incoming[e["target"]].append(e)

def label_to_id(label):
    return next((n["id"] for n in g["nodes"] if n["label"] == label), None)

# "Who calls AuthService?" -- direct incoming call/use edges.
nid = label_to_id("AuthService")
callers = [by_id[e["source"]]["label"]
           for e in incoming[nid]
           if "call" in e["relation"] or "use" in e["relation"]]
```

The same in JavaScript is a few lines:

```js
const g = JSON.parse(fs.readFileSync("synaptic-out/graph.json", "utf8"));
const byId = new Map(g.nodes.map(n => [n.id, n]));
const incoming = new Map();
for (const e of g.links) {
  if (!incoming.has(e.target)) incoming.set(e.target, []);
  incoming.get(e.target).push(e);
}
```

From this base you can answer most structural questions yourself: dependents
(walk `incoming` transitively for reverse impact), dependencies (walk `outgoing`),
symbols in a file (filter `nodes` by `source_file`), the public API of a module
(filter by `visibility == "public"`), or the biggest hubs (sort by degree).

## Building your own MCP server

To expose the graph to a coding assistant with tool semantics you control, wrap
the same in-memory adjacency in an MCP server. A minimal one with the Python MCP
SDK:

```python
import json, collections
from mcp.server.fastmcp import FastMCP

g = json.load(open("synaptic-out/graph.json"))
by_id = {n["id"]: n for n in g["nodes"]}
incoming = collections.defaultdict(list)
for e in g["links"]:
    incoming[e["target"]].append(e)

mcp = FastMCP("my-graph-tools")

@mcp.tool()
def find_callers(symbol: str) -> list[dict]:
    """Direct callers/users of a symbol."""
    node = next((n for n in g["nodes"] if n["label"] == symbol), None)
    if not node:
        return []
    return [
        {"caller": by_id[e["source"]]["label"],
         "relation": e["relation"],
         "file": e.get("source_file")}
        for e in incoming[node["id"]]
        if "call" in e["relation"] or "use" in e["relation"]
    ]

if __name__ == "__main__":
    mcp.run()
```

You are reusing Synaptic's extraction and the graph it produces, and adding only
the tool surface that fits your workflow. Re-run `synaptic extract` (or an
incremental update) to refresh the file your server reads.

> If you would rather not build your own, `synaptic serve` already exposes a
> read-only MCP server with the full official tool set over the same graph -- see
> [MCP Server](MCP-Server) and [Assistant Integration](Assistant-Integration).

## Keeping the graph fresh

Your tool reads a snapshot, so refresh it when the code changes:

- `synaptic extract <path>` rebuilds the whole graph.
- Incremental updates re-extract only what changed and are much faster on large
  trees -- see [Incremental Updates](Incremental-Updates).

Because ids and community numbers are deterministic, references your tool stored
against an earlier build stay valid across a refresh as long as the underlying
symbol still exists.

## Federated graphs

A multi-repo (federated) graph is the same shape with a few additions, so a
consumer that ignores them still works on a single-repo graph unchanged:

- Every node carries a **`repo`** tag, and its `source_file` is prefixed with that
  tag (`billing-service/src/...`). Scope to one member by filtering on `repo`.
- Node **ids** are namespaced as `tag::id` (`billing-service::auth_service`), so
  ids stay unique across members; the un-prefixed original is kept in the node's
  `local_id`. Edge `source`/`target` use the namespaced ids, so joins still work.
- Cross-repo edges carry **`"cross_repo": true`**, so you can find (or exclude)
  the links that span members.

See [Workspaces and Federation](Workspaces-and-Federation) for how federated
graphs are built.

## What you can depend on

| Stable (build on it) | Internal (do not) |
| --- | --- |
| The keys documented in [Output Formats](Output-Formats) | `_`-prefixed node/edge extras |
| `links` as the edge array; `hyperedges` always present | Exact community **numbers** across schema changes |
| `relation` names and the `confidence` levels | Crate/internal Rust APIs |
| Deterministic ids and clustering for a given build | Field ordering within an object |

## See also

- [Output Formats](Output-Formats) -- the exhaustive `graph.json` field spec and
  the other serializations (GraphML, Cypher, Neo4j/FalkorDB) you can target
  instead.
- [MCP Server](MCP-Server) -- the official read-only server if you would rather
  consume tools than build them.
- [Querying](Querying) -- the questions Synaptic's own commands answer, useful as
  a model for tools you build.
- [Architecture](Architecture) -- how the graph is built and why it is
  deterministic.
- [Workspaces and Federation](Workspaces-and-Federation) -- multi-repo graphs.
