# Querying

CodeGraph reads a built `graph.json` and answers four kinds of questions about it:
`query` (relevant subgraph), `path` (shortest route between two nodes), `explain`
(one node and its neighbours), and `affected` (reverse-impact: what depends on a
node). All four are read-only and operate on the graph produced by `codegraph
extract` (see [Commands] and [Output-Formats]).

By default each command loads `codegraph-out/graph.json`. Pass `--graph <path>`
to point at a different file.

For *structural* queries (match on kind, visibility, lines of code, fan-in/out,
relationships, variable-length paths, and aggregation), use the `search` command
and its CGQL query language instead. CGQL is documented in full under
[Commands](Commands#search); `search` matches on structure rather than on the
free-text relevance that `query` scores.

The SQL layer is queryable the same way: `MATCH (t:table) WHERE t.rls_enabled =
"false" RETURN t` finds tables without row-level security, and `(c:column)` /
`(i:index)` / `(p:policy)` match the SQL objects extraction now models (tables
also expose `dialect`). See [SQL Auditing](SQL-Auditing).

## query

```
codegraph query "user authentication" --max-nodes 30
```

`query` retrieves a subgraph relevant to free text. It scores every node by how
well its label tokens overlap the query, picks the best-scoring nodes as seeds,
then expands outward from those seeds until it has collected `--max-nodes` nodes.

How scoring works:

- Labels and the query are tokenized into lowercased word tokens, splitting on
  both `snake_case` and `camelCase` boundaries and dropping tokens shorter than
  two characters. `run_analysis()` becomes `run`, `analysis`; `AuthService`
  becomes `auth`, `service`.
- Each node's score is the sum of IDF weights of the query tokens it contains,
  where IDF is `ln((N + 1) / (1 + df)) + 1` with `N` the node count and `df` the
  number of nodes whose label contains that token. Rarer tokens count for more.
- Nodes with a score above zero are ranked highest-first (ties broken by node id
  for determinism). The top 8 become the seeds.

Expansion from the seeds uses the undirected adjacency of the graph (edge
direction and self-loops are ignored). New neighbours are pushed onto a queue;
nodes are collected until the count reaches `--max-nodes`.

### bfs vs --dfs

The expansion order is the only difference between the two traversal modes:

- Default (breadth-first): collects nodes in order of distance from the seeds, so
  the result is a broad neighbourhood around the matches.
- `--dfs`: follows a chain as far as it goes before fanning out, favoring deep
  call chains over wide neighbourhoods.

```
codegraph query "request handler" --dfs --max-nodes 50
```

### --max-nodes

`--max-nodes` (default 30) bounds the number of nodes in the returned subgraph.
It is a node count, not a token budget. Expansion stops as soon as the limit is
reached; edges are then included only when both their endpoints are in the
collected set.

### Output

The command prints the matched seeds, then the subgraph as a list of edges:

```
Seeds:
  - AuthService
  - login_user

Subgraph (12 nodes, 9 edges):
  AuthService --calls--> login_user
  AuthService --uses--> Database
  ...
```

If no node scores above zero, it prints `No matches for "...".`

### --repo

In a federated graph, `--repo <tag>` scopes the query to a single member before
running. Scoping drops nodes tagged with other repos and the cross-repo edges
that span them, so seeds and the subgraph come only from that member. See
[Workspaces-and-Federation].

```
codegraph query "payment" --repo billing-service
```

## path

```
codegraph path AuthService Database
```

`path` finds the shortest undirected path between two nodes and prints it as a
chain of labels:

```
AuthService → SessionStore → Database
```

Both endpoints are resolved from your arguments: an exact node id is used
directly, otherwise the first node whose label equals the argument exactly. If
either endpoint cannot be resolved it prints `Could not resolve one or both
endpoints.` If both resolve but no route connects them it prints `No path
between <from> and <to>.`

The search is a breadth-first walk over undirected adjacency (edge direction is
ignored), so the path returned has the fewest hops. A node has a one-element path
to itself.

`path` also accepts `--graph` and `--repo`.

## explain

```
codegraph explain AuthService
```

`explain` shows one node plus every node it is directly connected to. It prints
the label and source file, the community id (if the node has one), and each
neighbour grouped by direction:

```
AuthService [src/auth/service.py]
community: 3
neighbours (5):
  --> login_user (calls)
  --> Database (uses)
  <-- LoginController (calls)
  ...
```

`-->` is an outgoing edge (this node is the source); `<--` is incoming (this node
is the target). Neighbours are sorted by direction, then relation, then id. The
node argument is resolved the same way as `path` (exact id, else exact label). If
nothing resolves it prints `Node not found: <node>`.

`explain` also accepts `--graph` and `--repo`.

## affected

```
codegraph affected login_user --depth 2
```

`affected` is reverse-impact analysis: it reports the nodes that (transitively)
depend on a node, so you can see the blast radius of changing it. It walks edges
*backward* (from target to source) so that "X calls Y" means changing Y affects
X.

### Seed resolution (the fallback cascade)

`affected` resolves its node argument through a conservative cascade, stopping at
the first match and never guessing on a tie:

1. Exact node id.
2. Unique case-insensitive exact label.
3. Unique bare name: the label with a trailing `()` removed, matched
   case-insensitively (so `transform` matches a node labeled `transform()`).
4. Unique case-insensitive source file path.
5. Unique case-insensitive substring of a label.

If any step would match more than one node, or nothing matches at all, it prints
`No unique node match for <node>` and stops. (The `query`/`path`/`explain`
commands use a simpler resolver: exact id, then exact label.)

### --relation and --depth

- `--depth <n>` (default 2) bounds how many hops backward the walk follows. Each
  reported node records the relation it was first reached through and the hop
  count.
- `--relation <name>` restricts which edge relations propagate impact. It is
  repeatable. When omitted, a default set of structural relations is used:

  `calls`, `references`, `imports`, `imports_from`, `re_exports`, `inherits`,
  `extends`, `implements`, `uses`, `mixes_in`, `embeds`, `depends_on`,
  `reads_from`, and the cross-language relations `invokes`, `binds_native`,
  `calls_service`, and `handled_by`.

  The four cross-language relations mean reverse-impact crosses language
  boundaries: changing an HTTP/gRPC handler reaches the clients that call it, a
  Rust function exported through PyO3 reaches the Python that imports it, and a
  binary reaches the scripts that invoke it. See
  [Cross-Language-Edges](Cross-Language-Edges).

  Containment relations such as `contains` and `method` are intentionally not in
  the default set: containing something is not the same as depending on it, so
  they do not propagate impact.

```
codegraph affected Database --relation reads_from --relation depends_on --depth 3
```

### Output

```
Affected nodes for login_user
Relations: calls, references, imports, ...
Depth: 2
- LoginController [calls] src/web/login.py:L42
- AuthRouter [imports] src/web/router.py:L10
```

Each line is the affected node, the relation it was reached through, and its
source location. If nothing depends on the seed within the depth bound it prints
`No affected nodes found.`

`affected` accepts `--graph`. It does not take a `--repo` flag.

## See also

- [Commands] for the full command reference.
- [Output-Formats] for the JSON shape these queries operate on.
- [Analysis-and-Reports] for whole-graph structural analysis.
- [Workspaces-and-Federation] for `--repo` scoping.
