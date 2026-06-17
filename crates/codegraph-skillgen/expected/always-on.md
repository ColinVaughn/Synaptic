<!-- codegraph:start -->
## CodeGraph

This repo has a CodeGraph knowledge graph (`codegraph-out/graph.json`). Query it
before broad file exploration: `codegraph query "<question>"` / `codegraph affected
<node>`, or the **codegraph** MCP tools if your assistant has them connected.
Before editing a symbol other code depends on, forecast the blast radius with
`codegraph predict <files>` (or the `predict_impact` MCP tool).
Rebuild with `codegraph extract .` / `codegraph update <files>`.
<!-- codegraph:end -->