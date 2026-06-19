# Visualizations

CodeGraph renders the knowledge graph into several browser-viewable artifacts. All of them are self-contained HTML or SVG files that embed the graph data directly and load any libraries from a CDN, so you can open them by double-clicking (the interactive HTML viewers need a network connection for the CDN libraries).

Each is written during `codegraph extract` and can be regenerated individually with `codegraph export <format>`. See [Output-Formats] for the full artifact list and the `export` flags (`--graph`, `--out`, `--repo`).

| Artifact | Format | What it shows |
| --- | --- | --- |
| `graph.html` | `export html` | Interactive 2D explorer (vis-network) |
| `graph-3d.html` | `export 3d` (alias `force3d`) | Interactive 3D force graph |
| `graph.svg` | `export svg` | Static, deterministic layout image |
| `callflow.html` | `export callflow` | Mermaid call-flow diagram |
| `tree.html` | `export tree` | Collapsible file/class/method tree (D3) |

All five are written by default during `extract`.

## graph.html (2D explorer)

The primary interactive view, rendered with vis-network (loaded from `unpkg.com`). It embeds the graph as node and edge arrays in a `<script>` block.

What it shows:

- Node **shape** reflects its kind (table = diamond, column = dot, view = triangle, index/class = square, procedure = hexagon, trigger = star, code = dot); **color** is its community; size scales with degree (incident-edge count, self-loops ignored).
- Each edge **color reflects its relation**, so cross-language code → SQL bridges (`queries`/`writes_to`) and SQL structure (`has_column`/`references`/`protected_by`) stand apart from generic `calls`/`imports`.
- A top bar shows the node and edge counts.

Interactive features:

- Search box: type to select and fly to nodes whose label contains the query.
- Min-degree slider: hide nodes below a degree threshold.
- Community dropdown: filter to a single community.
- Relation toggles: a checkbox per relation type to show/hide those edges. Edges are hidden unless both endpoints are visible and the relation is enabled.
- "Hide SQL columns" toggle (shown when the graph has column nodes): drops the column layer — the dominant node class in a SQL graph — without a re-extract.
- Hover tooltips showing the node label, kind, and SQL facts (dialect, data type, PK/FK, RLS) where present.

Large-graph behavior: above 5000 nodes the page renders a community-aggregated view instead of every node, collapsing each community into one super-node sized by member count, with inter-community edges between them, plus a notice telling you to open `graph-3d.html` for the full node-level view. This keeps the browser responsive.

Security: node labels and relations are embedded JSON-escaped, with `</` rewritten to `<\/`, so a label cannot break out of the `<script>` block.

## graph-3d.html (3D force graph)

A full node-level interactive 3D view, rendered with `3d-force-graph` (Three.js + d3-force-3d) from a version-pinned CDN. The browser runs the force simulation live. The graph is embedded via `JSON.parse('...')` for fast parsing.

What it shows:

- Nodes colored by community (or by kind/repo via the "Color by" selector); node size scales with degree. The real node kind (table / column / function / class / …) drives the shape and the kind filters.
- Edge color reflects the **relation**; cross-language code → SQL bridges are drawn brighter and slightly thicker.
- A control panel (left) with node/edge counts and the find/filter tools; a details panel (right) appears on focus.

Interactive features:

- Search: matches by label, honouring the current visibility filters; flies the camera to the first match. Pressing Enter steps through matches.
- Click a node to focus it: the camera flies in, the node and its neighbors are highlighted, others dim, and a details panel shows its kind, **SQL facts (dialect, type, PK/FK, RLS)**, link count, community, source file, and a clickable list of connected nodes.
- "Color by" selector: community (default), kind, or repo (federated).
- **Kind filters**: a checkbox per node kind present, with a color swatch. Unchecking everything but `table`/`column` gives a schema-only view; everything but the code kinds gives a code-only view (the "layers").
- Relation toggles: a checkbox per relation present.
- "Show SQL columns" toggle (when columns exist) and "Code↔SQL bridges only" toggle (when cross-language edges exist).
- Min-connections (degree) slider to declutter.
- **Spread slider**: scales the force simulation's repulsion (and link distance) and reheats the layout live, so a dense central clump can be expanded outward for a clearer view. It only changes spacing — no node is hidden (that is the degree slider's job). 1x is the default (untouched) layout.
- Reset view button restoring all filters.

Node shapes: SQL schema objects and assets render as distinct 3D meshes (table = octahedron, view = cone, index = box, procedure = hex-prism, trigger = torus-knot, policy = tri-cone, role/other = dodecahedron; stylesheet = box, data = octahedron, image = tetrahedron, font = cylinder, media = torus). Columns and code stay spheres (there can be tens of thousands; the structural objects are few). These meshes come from a Three.js module loaded lazily via a dynamic `import()` as a progressive enhancement; if it fails, nodes simply stay spheres and the graph still renders.

Federation: when the graph carries `repo` tags, the "Color by" selector gains a `repo` option and extra controls appear — a "cross-repo edges only" filter and a repo legend. Cross-repo edges use a distinct accent color, and external-package stubs render as translucent spheres (the 3D analog of the 2D dashed ring).

Large-graph performance: when nodes + edges exceed 6000, the viewer transparently switches to a faster render path: edges drop from cylinder meshes to GL lines, regular code nodes collapse into a single GPU-instanced mesh (one draw call, with a custom raycast picker so click-to-focus and hover tooltips still work), node spheres get a coarser resolution, the layout warms up off-screen, the link haze dims, the simulation is bounded so it settles and stops, and the device-pixel-ratio is capped. None of this caps the scan itself.

## graph.svg (static layout)

A standalone, dependency-free SVG image of the graph. The layout is a deterministic Fruchterman-Reingold relaxation (positions seeded on a circle, no RNG), so the output is reproducible run to run. Repulsion uses an exact all-pairs computation for small graphs and a Barnes-Hut quadtree (O(n log n)) above ~1000 nodes.

What it shows:

- Nodes colored by community, **shaped by kind** (table = diamond, column = small dot, view = triangle, index/class = square, procedure = hexagon, trigger = star, policy = inverted triangle, code = circle), sized by degree, on a dark background. A legend in the lower-left lists the kinds present.
- The graph is laid out per connected component and shelf-packed into separate cells, so disconnected fragments do not stretch the viewport or crush the main cluster.
- Edges drawn under nodes, **colored by relation** (so SQL structure and code → SQL bridges read apart from generic calls). `EXTRACTED` edges are solid; lower-confidence edges are dashed.
- Each node carries a `<title>` so hovering shows its kind and SQL facts (the SVG previously had no tooltip).

Federation: for graphs with `repo` tags, each node gets a colored ring by repo, external-package stubs render dimmed with a dashed ring, cross-repo edges get an accent color and extra width, and a repo legend appears in the lower-right. Single-repo SVG omits all of this and is identical to before.

Scale limits: at most 5000 nodes are laid out. When a graph is larger, **structural nodes (tables, code, policies) are kept and columns are dropped first** — columns dominate a SQL graph and are the least informative individually — instead of an arbitrary first-5000 cut. Per-node text labels are drawn only at or below 500 nodes; above that, only shapes and edges render to keep the file readable. This is a static image with no interactivity.

## callflow.html (Mermaid call flow)

A call-flow diagram rendered by mermaid.js (from a CDN) as a left-to-right `graph LR` of the graph's edges. Each node is declared with its label and each edge is drawn with its relation as the edge label.

What it shows:

- A directed flow diagram of relationships, dark-themed, with a header showing node and edge counts.

Scale limit: capped at 250 edges (mermaid degrades on huge diagrams). When the graph has more, a "Showing N of M edges" note appears. For the full interactive view use `graph.html`.

Labels and ids are sanitized for Mermaid's parser: ids are folded to a valid `[A-Za-z0-9_]` identifier (deduped on collision), and labels neutralize backticks, quotes, smart quotes, pipes, and angle brackets so arbitrary code text cannot produce a syntax error. This view is non-interactive (it is a rendered static diagram).

## tree.html (file tree)

A collapsible tree rendered with D3 (from a CDN), built over the containment hierarchy (file -> class -> method/function) from the graph's `contains` and `method` edges.

What it shows:

- A horizontal tree. Roots are file-like nodes (labels ending in a source extension such as `.py`, `.js`, `.ts`, `.go`, `.rs`, `.java`) with no parent; if none qualify, any parentless node with children is used.
- Each node label shows its subtree size in parentheses when greater than one. Cycles are guarded (rendered as a leaf).

Interactive features:

- Drag to pan, scroll or the +/- buttons to zoom (scale extent 0.01 to 4).
- A "Fit" button (also run on load) fits the whole tree into the viewport, so even a very large tree is visible and you can then zoom in to explore.

Labels are embedded with `</` rewritten to `<\/` so a label cannot break out of the `<script>` block.

## Color reference

- Community palette, per-kind palette, and per-repo (federation) palette are fixed categorical palettes; community/repo colors are assigned by index, kind colors are fixed per kind (SQL objects warm, code symbols cool).
- Node color is the community by default; the interactive viewers can switch to color-by-kind (and color-by-repo when federated).
- Edge color reflects the **relation**: cross-language code → SQL bridges (`queries`/`writes_to`/`calls_proc`) share one bright accent; SQL structural edges (`references`/`has_column`/`indexes`/`protected_by`/`grants`) and generic code edges each get their own. Edge confidence is conveyed by solid vs dashed strokes in `graph.svg`.
- Cross-repo edges use a distinct cyan accent in the SVG and 3D views.

## See also

- [Output-Formats] for non-visual artifacts (JSON, GraphML, Cypher, DOT) and live database push.
- [Analysis-and-Reports] for GRAPH_REPORT.md.
- [Workspaces-and-Federation] for repo tags and cross-repo styling.
- [Commands] for the `extract` and `export` CLI reference.
