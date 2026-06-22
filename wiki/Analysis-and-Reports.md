# Analysis and Reports

Beyond per-node queries, Synaptic runs a whole-graph structural analysis and
renders it as a human-readable `GRAPH_REPORT.md`. The report surfaces the most
connected abstractions, non-obvious connections, circular imports, low-confidence
edges, and a list of questions the graph is positioned to answer.

## Producing the report

The report is written automatically by `synaptic extract`. Every extract writes
`synaptic-out/GRAPH_REPORT.md` alongside `graph.json` and the other outputs.

You can also regenerate it from an existing `graph.json` without re-extracting:

```
synaptic export report
```

`export report` loads the graph, runs community detection and the analysis bundle
over it (these are not stored in `graph.json`, so they are recomputed), and
writes `GRAPH_REPORT.md`. Use `--graph <path>` to read a different graph,
`--out <path>` to choose the destination, and `--repo <tag>` to scope to one
federated member first. See [Commands] and [Visualizations] for the other export
formats.

The same analysis powers the MCP server's graph tools (see [MCP-Server]).

## The analysis bundle

A single analysis pass computes four results over the clustered graph:

- God nodes: the 10 most-connected real abstractions.
- Surprising connections: the top 5 non-obvious edges.
- Suggested questions: up to 7 questions worth investigating.
- Import cycles: up to 20 circular file-level import chains (cycle length up to 5).

These feed the report sections below. Each is deterministic (no randomness), so
the same graph always produces the same report.

## Report sections

### Overview

Headline counts: nodes, edges, communities, and the edge-confidence breakdown as
percentages of EXTRACTED, INFERRED, and AMBIGUOUS edges. When INFERRED edges
carry confidence scores, the average score is shown. On a federated (multi-repo)
graph a **Cross-repo** line reports how many edges span repositories and how many
of those are cross-language coupling (HTTP/RPC/FFI/WebSocket). The commit the graph
was built at is included when known.

Confidence tiers come from extraction: EXTRACTED edges are read directly from
source, INFERRED edges are deduced, and AMBIGUOUS edges are low-confidence guesses
worth a human look.

### God Nodes

The most-connected core abstractions, ranked by degree (number of distinct
neighbours, ignoring self-loops), highest first with ties broken by id.

Degree is computed undirected, so an incoming and an outgoing edge to the same
neighbour count once. Before ranking, several kinds of noise are filtered out so
the list reflects real abstractions rather than scaffolding:

- File nodes (a node whose label is its own filename, or a low-degree `.foo()`
  callable label).
- Concept nodes (nodes with no source file, or whose source filename has no
  extension).
- JSON config-key nodes (generic keys like `name`, `id`, `type`,
  `dependencies` in a `.json` source).
- Built-in/library noise labels (`str`, `int`, `Optional`, `os`, `json`, `Mock`,
  and similar).

```
## God Nodes

The most-connected core abstractions:

1. `AuthService` - degree 27
2. `Database` - degree 19
```

### Surprising Connections

Non-obvious edges between two entities. The detector picks one of two strategies
based on the corpus:

- Multi-source corpora (more than one distinct source file): every cross-file
  non-structural edge is scored by a composite surprise score, and the top edges
  are reported. Structural relations (`imports`, `imports_from`, `contains`,
  `method`) and edges touching file/concept nodes are excluded; both endpoints
  must be in different source files.

  The surprise score rewards: lower-confidence edges (AMBIGUOUS over INFERRED
  over EXTRACTED), edges crossing file categories (code/doc/paper/image), edges
  spanning different top-level directories or repos, edges bridging two
  communities, `semantically_similar_to` edges (a 1.5x multiplier), and a
  peripheral node unexpectedly reaching a hub. The score is dampened for INFERRED
  cross-language or code-to-doc `calls`/`uses` edges, which are usually
  coincidental name collisions rather than real links. Each reported connection
  includes a human-readable reason string explaining why it scored.

- Single-source corpora (or when no cross-file candidates exist): falls back to
  community-bridge edges. With community info, one representative edge per pair of
  bridged communities is reported, AMBIGUOUS edges first. With no community info,
  edges are ranked by Brandes edge-betweenness and the top structural bridges are
  surfaced.

```
## Surprising Connections

- `Tokenizer` -> `MetricsClient` (references, INFERRED) - inferred connection - not explicitly stated in source; crosses file types (code <-> doc)
```

### Suggested Questions

Up to seven questions the graph can help answer, generated in priority order and
truncated to the limit. The kinds, in order, are:

1. Ambiguous edges: "What is the exact relationship between A and B?" for every
   AMBIGUOUS edge. If these alone fill the budget, later kinds are skipped.
2. Bridge nodes: the top non-file/non-concept nodes by Brandes node-betweenness
   centrality that actually span communities, asking why a node connects one
   community to others.
3. Inferred relationships: high-degree nodes with two or more INFERRED edges,
   asking whether those inferred links are correct.
4. Isolated nodes: weakly-connected nodes (degree at most 1) that may indicate
   documentation gaps.
5. Low-cohesion communities: communities of at least five nodes whose internal
   edge density is below 0.15, asking whether they should be split into more
   focused modules.

If none of these signals are present, a single placeholder explains that there
was not enough signal to generate questions.

```
## Suggested Questions

1. What is the exact relationship between `parseConfig` and `Settings`?
   - _Edge tagged AMBIGUOUS (relation: references) - confidence is low._
2. Why does `EventBus` connect `Community 0` to `Community 4`?
   - _High betweenness centrality (0.214) - a cross-community bridge._
```

### Import Cycles

File-level circular import dependencies. The detector builds a directed
file-to-file graph from `imports_from` and `re_exports` edges (oriented by each
edge's importing source file), then runs a bounded depth-first search for simple
cycles. Cycles of length 2 up to the maximum (5 in the report) are returned,
shortest first, deduplicated by rotation. Each cycle is printed as a loop with its
length.

```
## Import Cycles

- src/a.ts -> src/b.ts -> src/a.ts (length 2)
```

### Communities

The number of communities, plus a line per non-thin community: its name (a
semantic name when available, otherwise `Community <id>`), node count, cohesion
score, and a sample of up to five member labels. Communities of fewer than three
nodes are omitted here and counted toward Knowledge Gaps.

Cohesion is the ratio of actual intra-community edges to the maximum possible
(`actual / (n*(n-1)/2)`); 1.0 means every member is connected to every other.

```
## Communities

5 total (2 thin <3 nodes omitted).

- **Authentication** (community 0) - 18 nodes, cohesion 0.34: AuthService, login_user, TokenStore, ...
```

#### How communities are computed

Community detection is in-house, weighted, undirected, and deterministic (no
randomness; nodes are processed in sorted-id order). Edges are treated as
undirected with weights summed.

- The default algorithm is Leiden: a multi-level Louvain modularity optimization,
  followed by a refinement phase that guarantees every community's induced
  subgraph is internally connected and can split a community that is connected but
  poorly knit (two dense groups joined by one weak edge) into well-connected
  pieces. The refined partition is kept only when its modularity matches or beats
  the Louvain baseline, so the result never regresses. Plain Louvain is also
  available.
- Resolution controls granularity: above 1.0 yields more, smaller communities;
  below 1.0 yields fewer, larger ones (default 1.0).
- Post-processing reattaches isolated nodes as singletons, optionally excludes
  very high-degree hubs from partitioning and reattaches them by majority-vote
  neighbour community, splits oversized communities (those exceeding 25% of the
  graph), and re-splits large low-cohesion communities. Communities are then
  renumbered deterministically so that id 0 is always the largest.

On incremental rebuilds, community ids are remapped to overlap the previous
assignment so a community keeps a stable id across runs. See
[Incremental-Updates].

### Ambiguous Edges

Up to 20 AMBIGUOUS-confidence edges, listed as `source -> target (relation)
[AMBIGUOUS]`, with a count of any beyond the first 20. These are the
relationships the extractor was least sure about and that most reward a human
review.

### Knowledge Gaps

Quality flags that suggest where the graph may be incomplete:

- Isolated nodes: nodes with at most one connection (possible missing edges or
  undocumented components), up to five named.
- Thin communities: the count of communities smaller than three nodes that were
  omitted from the Communities list.
- High ambiguity: flagged when AMBIGUOUS edges are 20% or more of all edges.

If there are no isolated nodes, no thin communities, and ambiguity is under 20%,
this section reports `None`.

## Centrality (betweenness)

Bridge detection in Suggested Questions and the no-community surprise fallback use
Brandes betweenness centrality, computed unweighted and undirected:

- Node betweenness matches the standard normalized betweenness centrality. On
  graphs over 1000 nodes it is approximated from a deterministic sample of source
  nodes (the first 100 in sorted-id order) and rescaled, so it stays fast and
  reproducible without randomness.
- Edge betweenness is the normalized edge variant and uses every source node;
  callers bound the graph size before invoking it (the edge-betweenness surprise
  fallback is skipped above 5000 nodes).

## Entity deduplication

Before analysis, near-duplicate non-code entities can be merged so the same
concept does not appear as several nodes. The pipeline normalizes labels, gates
on Shannon entropy, blocks candidate pairs with MinHash/LSH, verifies with
Jaro-Winkler similarity, boosts same-community pairs, and merges via union-find,
rewiring edges onto the surviving node.

Code symbols are never label-merged: a code node's identity is its
fully-qualified id (already collapsed by id), and two same-named symbols in
different files are deliberately kept distinct. Deduplication only acts on
non-code document/concept nodes.

Pairs whose similarity lands in an ambiguous band (too similar to ignore, too
different to auto-merge) are surfaced for resolution. The default offline
tiebreaker confirms only safe merges (labels that are the same multiset of
words, such as a reordering); genuinely ambiguous pairs are left for an LLM
(`--semantic`) or human. See [Semantic-Analysis].

## See also

- [Commands] for `extract` and `export report`.
- [Visualizations] for the graph rendering formats.
- [Output-Formats] for the underlying graph shape.
- [Querying] for per-node lookups.
- [Incremental-Updates] for stable community ids across rebuilds.
