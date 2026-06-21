<!-- synaptic:start -->
## Synaptic

This repo has a Synaptic knowledge graph (`synaptic-out/graph.json`) -- a
code-intelligence layer for navigating code and analyzing change impact, not just a
faster search. Query it before broad file exploration: `synaptic query
"<question>"` / `synaptic affected <node>`, or the **synaptic** MCP tools if your
assistant has them connected.
Before editing a symbol other code depends on, forecast the blast radius with
`synaptic predict <files>` (or the `predict_impact` MCP tool).
Rebuild with `synaptic extract .` / `synaptic update <files>`.
<!-- synaptic:end -->