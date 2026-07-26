# Querying

Synaptic reads a built `graph.json` and answers five kinds of questions about it:
`query` (relevant subgraph), `path` (shortest route between two nodes), `explain`
(one node and its neighbours), `affected` (reverse-impact: what *transitively*
depends on a node), and `references` (find-all-references: every *direct* use of a
symbol). All are read-only and operate on the graph produced by `synaptic extract`
(see [Commands] and [Output-Formats]).

By default each command loads `synaptic-out/graph.json`. Pass `--graph <path>`
to point at a different file.

For *structural* queries (match on kind, visibility, lines of code, fan-in/out,
relationships, variable-length paths, and aggregation), use the `search` command
and its SYNQL query language instead. SYNQL is documented in full under
[Commands](Commands#search); `search` matches on structure rather than on the
free-text relevance that `query` scores.

The SQL layer is queryable the same way: `MATCH (t:table) WHERE t.rls_enabled =
"false" RETURN t` finds tables without row-level security, and `(c:column)` /
`(i:index)` / `(p:policy)` match the SQL objects extraction now models (tables
also expose `dialect`). See [SQL Auditing](SQL-Auditing).

## query

```
synaptic query "user authentication" --max-nodes 30
```

`query` retrieves a subgraph relevant to free text. It scores every node by how
well its label, architectural source-path tokens, and bounded extractor search
aliases overlap the query, picks the best-scoring nodes as seeds, then expands
outward from those seeds — best-first, by relevance — until it has collected
`--max-nodes` nodes. Results come back ranked, each with a relevance score.

How scoring works:

- Labels and the query are tokenized into lowercased word tokens, splitting on
  both `snake_case` and `camelCase` boundaries and dropping tokens shorter than
  two characters. `run_analysis()` becomes `run`, `analysis`; `AuthService`
  becomes `auth`, `service`. Sentence-shaped natural-language questions discard
  interrogative scaffolding such as `how`, `where`, and `is` when substantive
  terms remain. Symbol-shaped queries (camelCase, snake_case, qualified names,
  and punctuation-bearing identifiers) retain every token.
- Natural-language inflections add conservative alternatives such as
  `traveling` → `travel`, `teleported` → `teleport`, and `planets` → `planet`
  only when the alternative already exists in the graph token index. The
  original term remains present, and the alternative carries less weight than
  an exact match.
- Natural-language decision terms use one deliberately bounded implementation
  alias: `choose`, `decide`, and `determine` (including common inflections) may
  recall `resolve`. The alias is admitted only when `resolve` exists in the
  graph, is weighted below exact evidence, and is not applied to direct
  single-symbol queries.
- A node's seed score is the sum of IDF weights of the query tokens it contains
  — IDF is `ln((N + 1) / (1 + df)) + 1`, with `N` the node count and `df` the
  number of nodes containing that token, so rarer tokens count for more —
  divided by the square root of each channel's token count, so a long label,
  path, or extractor alias catalog cannot out-score a tight match by accumulation.
  Direct label evidence has full weight; source-path evidence is lower weight and
  retains the repository's full path vocabulary (including languages, targets,
  generated/test directories, and file extensions); extractor aliases are also
  lower weight.
- For a multi-concept question, the score also rewards coverage: a node matching
  `rocket` and `travel` receives more confidence than a similarly scored node
  matching only `planet`. This is a bounded multiplier and is disabled for
  one-concept queries, so exact symbol ranking does not change.
- Nodes scoring above zero are ranked highest-first (ties broken by node id for
  determinism). Up to 8 become seeds. In a multi-concept question, candidates
  matching only one discriminative concept contribute at most two seeds, so a
  generic token such as `handle` cannot hide every `rocket` or `planet` match;
  repeated multi-concept evidence signatures contribute at most three. A direct
  one-concept symbol query retains the full seed budget, and full exact
  symbol-label matches never consume a diversity allowance.
- A lower-weight intent candidate is promoted only when it is directly adjacent
  to selected or otherwise scored evidence covering at least two substantive
  query concepts. Alias-only symbols cannot enter through the normal seed path.
  With a full seed budget the intent candidate may replace only a seed whose
  concepts are represented elsewhere, and it inherits 75% of its strongest
  supporting match's confidence. High-degree supporting hubs are penalized when
  choosing among intent candidates. This makes a relationship such as
  `load_graph_data()` → `resolve_backend()` visible without promoting unrelated
  resolver helpers or weakening direct matches.

Expansion uses the undirected adjacency of the graph (edge direction and
self-loops are ignored), but it is **best-first**, not a plain breadth-first
wave: the frontier is a priority queue keyed by relevance, so the `--max-nodes`
budget is spent on the most relevant neighbourhood rather than on whatever a
breadth-first sweep happened to reach first. Two refinements keep the result
clean:

- **Seed preservation.** Every selected seed settles before expanded neighbours,
  so a tight hosted token budget cannot evict a lower-scored intent branch.
- **Hub penalty.** A high-fan-out node (a registry, a `Builder`, a documentation
  index) is down-weighted in proportion to how far its degree exceeds the graph
  average, so it is expanded last and its many incidental neighbours rarely reach
  the budget. This stops one hub from flooding the result with noise.
- **Decay.** A neighbour inherits a fraction of the relevance of the node that
  reached it, so far-flung nodes fade while a genuinely relevant chain survives.
- **Repeated-label penalty.** A label repeated more than twice is down-weighted
  only when reached as a non-matching neighbor. Generic duplicates such as
  `render()` therefore sink below specific neighbors, while an exact query for
  that label still makes it a full-strength seed.
- **Evidence-signature diversity.** In multi-concept questions, the best two
  single-concept or three joint-concept results keep full strength; later
  duplicates with the same query evidence receive a deterministic score
  discount. The terse prefix therefore presents distinct intent branches instead
  of many equivalent overloads. A full exact symbol-label match is exempt from
  this discount even when the identifier contains multiple tokens.

Every returned node keeps a final relevance score; nodes and edges are returned
sorted by it (edges by the relevance of their weaker endpoint), so you can read
the top of the list and ignore the low-scored tail.

### bfs vs --dfs

Both modes expand best-first by relevance; the traversal mode only breaks score
ties:

- Default (breadth-first): among equally-relevant frontier nodes, the
  earlier-discovered (shallower) one is taken first, giving a broad neighbourhood
  around the matches.
- `--dfs`: among equal scores, the later-discovered (deeper) one is taken first,
  favoring deep call chains over wide neighbourhoods.

```
synaptic query "request handler" --dfs --max-nodes 50
```

### --max-nodes

`--max-nodes` (default 30) bounds the number of nodes in the returned subgraph.
It is a node count, not a token budget. Expansion stops as soon as the limit is
reached; edges are then included only when both their endpoints are in the
collected set.

### --since and --seed-changed (recency)

`--since <baseline>` boosts nodes whose file changed on the current branch, so
in-progress code surfaces first. The baseline is a git ref (`main`, `HEAD~10`), a
date (`"2 weeks ago"`), or `auto` to detect the default branch. The changed set is
scoped to `merge-base(<baseline>, HEAD)..working-tree`, so it includes uncommitted
edits — what you are working on right now — and the boost is weighted by each
file's churn (lines changed).

```
synaptic query "collider mesh" --since main
```

Changed nodes are marked `(changed)` in the ranked list and float toward the top,
while a strong query match still holds its rank — recency re-ranks *within* the
relevant set rather than replacing it. Add `--seed-changed` to also inject the
changed-file nodes as seeds, so the branch's changed surface appears even when the
query matches little ("what did this branch change"):

```
synaptic query "anything" --since main --seed-changed
```

Resolution runs `git`; if the directory is not a git repo, the ref does not
resolve, or nothing changed, the command prints a short note and falls back to a
plain query. The MCP `query_graph` tool exposes the same via its `since` and
`recency_mode` arguments — see [MCP-Server].

### Output

The command prints the matched seeds, the ranked nodes with their scores, then the
subgraph as a list of edges (a `Recency:` header and `(changed)` markers appear
when `--since` is used):

```
Seeds:
  - AuthService
  - login_user

Ranked nodes (12):
  [6.10] AuthService
  [4.80] login_user
  ...

Subgraph (12 nodes, 9 edges):
  AuthService --calls--> login_user
  AuthService --uses--> Database
  ...
```

If no node scores above zero (and no changed nodes are seeded), it prints
`No matches for "...".`

### --repo

In a federated graph, `--repo <tag>` scopes the query to a single member before
running. Scoping drops nodes tagged with other repos and the cross-repo edges
that span them, so seeds and the subgraph come only from that member. See
[Workspaces-and-Federation].

```
synaptic query "payment" --repo billing-service
```

## path

```
synaptic path AuthService Database
```

`path` finds the shortest undirected path between two nodes and prints it as a
chain of labels:

```
AuthService -[calls]-> SessionStore -[queries]-> Database
```

Each hop is annotated with its connecting relation (and arrow direction), so a
path that crosses an inferred network boundary (`calls_service`, `handled_by`)
is distinguishable from a static call chain.

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
synaptic explain AuthService
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
synaptic affected login_user --depth 2
```

`affected` is reverse-impact analysis: it reports the nodes that (transitively)
depend on a node, so you can see the blast radius of changing it. It walks edges
*backward* (from target to source) so that "X calls Y" means changing Y affects
X.

### Seed resolution (the fallback cascade)

The commands that take a node argument (`explain`, `path`, `affected`) resolve it
through one shared conservative cascade, stopping at the first match and never
guessing on a tie:

1. Exact node id.
2. Unique case-insensitive exact label.
3. Unique bare name: the label with a trailing `()` removed, matched
   case-insensitively (so `transform` matches a node labeled `transform()`).
4. Unique case-insensitive source file path.
5. Unique case-insensitive substring of a label.

When a name is shared by several files, pin it to one by appending a file
qualifier: `name@file-substring` (e.g. `announce@core/foo.ts`). The whole query is
tried as-is first, so a name that itself contains `@` still resolves literally.

If a step matches more than one node, the command lists the candidates with each
one's id, file, and degree (so you can pick the right one without a second lookup)
instead of guessing; if nothing matches at all it prints `No node matches '<node>'`.
The same cascade and messaging back the equivalent MCP tools.

### --relation and --depth

- `--depth <n>` (default 2) bounds how many hops backward the walk follows. Each
  reported node records the relation it was first reached through and the hop
  count.
- `--relation <name>` restricts which edge relations propagate impact. It is
  repeatable. When omitted, a default set of structural relations is used:

  `calls`, `references`, `imports`, `imports_from`, `re_exports`, `inherits`,
  `extends`, `implements`, `uses`, `mixes_in`, `embeds`, `depends_on`,
  `reads_from`, the cross-language relations `invokes`, `binds_native`,
  `calls_service`, `handled_by`, `dynamic_ref`, and the code->SQL relations
  `queries`, `writes_to`, `calls_proc` (a schema change reaches the code that
  reads or writes the table).

  These cross-language relations mean reverse-impact crosses language
  boundaries: changing an HTTP/gRPC handler reaches the clients that call it, a
  Rust function exported through PyO3 reaches the Python that imports it, an
  event-bus publisher reaches the subscribers on its channel, and a
  binary reaches the scripts that invoke it. `dynamic_ref` carries an
  evidence-linked reflection call (a site that dispatches on a string-literal name
  matching exactly one symbol). See
  [Cross-Language-Edges](Cross-Language-Edges).

  Containment relations such as `contains` and `method` are intentionally not in
  the default set: containing something is not the same as depending on it, so
  they do not propagate impact.

```
synaptic affected Database --relation reads_from --relation depends_on --depth 3
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

### "0 dependents" is not always "safe"

A symbol reached only via reflection or fully-dynamic dispatch has no static
dependents, so a bare `No affected nodes found.` could read as "safe to delete"
when it is not. When the empty result's symbol sits in a scope that uses dynamic
dispatch -- it was evidence-linked, or its own file holds unresolved reflection
sites -- `affected` appends a caveat line ("0 static dependents, but N
dynamic-dispatch site(s) ... not provably unused"). List the underlying sites with
[`synaptic hazards`](Commands#hazards) (or the `dynamic_hazards` MCP tool), and see
[Cross-Language-Edges](Cross-Language-Edges#dynamic-dispatch).

`affected` accepts `--graph`. It does not take a `--repo` flag.

## references

```sh
synaptic references User
```

`references` is the find-all-references view: the symbol's **direct** incoming uses
of every kind -- calls plus imports, `implements`/`inherits`, type uses,
cross-language coupling, and reflection refs (every incoming edge except structural
ownership like `contains`). Two distinctions matter:

- vs `affected`: `affected` walks the *transitive* reverse-impact closure (depth
  hops); `references` reports only *direct* uses, but of every relation kind.
- vs a calls-only view: a caller list reports calls/uses/references and so misses a
  type's `imports` and `implements`/`inherits`. For "where is this type/interface
  used", reach for `references`. References are to the symbol itself -- a type's
  members are not folded in.

The header carries the total and a per-relation breakdown. It accepts `--graph`,
`--limit`/`--verbose`, and -- unlike `affected` -- a `--repo` flag to scope to one
federated member; on a federated graph a cross-repo use surfaces the same as a
local one. Mirrors the `find_references` MCP tool. The related file outline
`synaptic search --file <path>` lists every symbol defined in a file, ordered by
line (see [Commands](Commands#search)).

## See also

- [Commands] for the full command reference.
- [Output-Formats] for the JSON shape these queries operate on.
- [Analysis-and-Reports] for whole-graph structural analysis.
- [Workspaces-and-Federation] for `--repo` scoping.
