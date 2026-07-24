# Synaptic vs other tools

Synaptic sits in a crowded space: code search platforms, static-analysis query engines,
and the codebase-context features built into AI coding tools all overlap with it in
different ways. This page is an honest map of where Synaptic is genuinely different and
where another tool is the better choice. It is not a scoreboard.

**How this page is sourced.** Every claim about another tool is backed by that tool's own
documentation or repository, linked inline and listed under [Sources](#sources). Claims
about Synaptic link to its own wiki pages and source. Facts in this space change often
(licensing, pricing, and indexing internals especially), so each competitor fact reflects
its source as verified in **June 2026**; check the linked page for the current state. If a
claim could not be confirmed from a primary source, it is not made here.

## At a glance

| Tool | What it is for | How it models code | Languages | Runs where | AI / LLM role | License |
|---|---|---|---|---|---|---|
| **Synaptic** | Persistent code knowledge graph you query, diff across git history, forecast changes against, and refactor against, instead of re-reading source | Symbols + typed edges, clustered into communities; every edge tagged `Extracted` / `Inferred` / `Ambiguous`; nodes carry kind/visibility/line-spans; plus boundary edges across language / process / repo (HTTP, FFI, WebSocket, IPC, event bus) and flagged dynamic-dispatch sites | tree-sitter, 30+ languages | Single static binary, local, offline by default | MCP server (30 read-only tools) over the graph, incl. content search, find-all-references, dynamic-dispatch hazards, change forecasting, predictive test selection, edit-impact prediction, readiness audit, structural search, describe-node, time-travel diff, and plan-only rename | FSL-1.1-ALv2, source available; Apache-2.0 after two years |
| **Sourcegraph / Cody** | Org-scale code search and navigation; Cody is its AI assistant | Search index, plus precise navigation from uploaded SCIP indexes (opt-in) [2][3] | Search works broadly; precise nav has SCIP indexers for ~8 languages [11] | Self-hosted (Kubernetes / Docker) or Sourcegraph Cloud [4] | Cody answers and edits using search + code-graph context [7] | Main product not open source; enterprise pricing [5][9] |
| **CodeQL** | Semantic analysis to find security vulnerabilities and their variants [12] | Relational "CodeQL database" queried with the QL language [13] | C/C++, C#, Go, Java, Kotlin, JS, TS, Python, Ruby, Rust, Swift, GitHub Actions [14] | CLI; compiled languages need a build observed during extraction [13] | None; it is a query engine (powers GitHub code scanning) [12] | Free on open-source/public code; paid for private code [15][16] |
| **Joern** | Static analysis for vulnerability discovery via code property graphs [17] | Code Property Graph (AST + control-flow + data-flow in one graph), Scala-based query language [18] | C/C++, Java, JS, Python, Kotlin, PHP, Go, Ruby, Swift, C#, JVM bytecode, x86/x64 [17] | Local shell/CLI; imports code even without a working build [17] | None | Apache-2.0, open source [19] |
| **Aider (repo map)** | AI pair-programming CLI; its repo map feeds the LLM whole-repo context [20] | tree-sitter symbol map ranked by a graph-ranking algorithm, sized to a token budget [21] | tree-sitter (many) | Local CLI that calls an LLM API | The repo map *is* the LLM-context mechanism, rebuilt per request [20] | Apache-2.0, open source [22] |
| **Cursor (indexing)** | AI code editor; indexes the codebase for semantic search by the agent [23] | Vector embeddings of code chunks in a vector database (not a graph) [23] | Language-agnostic chunking | Desktop editor; indexing computes embeddings via Cursor's cloud [24][25] | Core to the editor's AI features | Proprietary (closed source) |
| **graphify** | Queryable knowledge graph over code and mixed media, shipped as an installable AI-assistant skill [26] | Symbols + typed edges, Leiden/Louvain communities; edges tagged `EXTRACTED`/`INFERRED`/`AMBIGUOUS` (same scheme as Synaptic) [26] | tree-sitter, 36 grammars (+ regex for Apex) [26] | Python >=3.10 package (PyPI `graphifyy`); needs an interpreter and dependencies [26][27] | MCP server (10 tools + 6 resources); installs a `/graphify` skill into ~18 assistants [26] | MIT, open source [26] |

Every competitor claim in this table is expanded and sourced in the sections below.

## What's distinctive about Synaptic

Beyond building and querying a structural graph, Synaptic layers a set of capabilities that,
together on one offline, confidence-tagged graph, none of the tools below combine. Each is
exposed at the CLI, and most over the MCP server as well:

- **Change forecasting and speculative verification** ([Commands: predict](Commands#predict),
  [Commands: speculate](Commands#speculate)). Given the files (or `git diff`) a change touches,
  Synaptic forecasts its consequences *before* the edit is made: the reverse-impact blast
  radius, the tests that exercise the changed code (`affected_tests`, predictive test selection),
  which edited symbols are public API, new import cycles or removed APIs (from a time-travel
  diff), git-history co-change coupling that no static edge captures, and a heuristic risk score
  with a verify checklist. `synaptic predict --edit <symbol>` does the same analytically for a
  single symbol edit, classifying each dependent as "will break" vs "to review". With
  `serve --allow-exec`, the `speculate` tool then *proves* the forecast by applying the change in
  a throwaway git worktree and running the build plus the at-risk tests. Forecasting a change
  before you make it, then proving it by running it in a sandbox, is a loop none of the other
  tools here offer.
- **Impact that crosses language, process, and repository boundaries**
  ([Cross-Language Edges](Cross-Language-Edges)). A post-extraction pass mints boundary nodes so
  reverse-impact and shortest-path traverse coupling no single-language parse can see: HTTP/RPC
  routes (Flask/FastAPI, Express, axum/actix, Go net/http, gRPC), FFI bindings (PyO3, ctypes,
  JNI, cgo, N-API), subprocess invocations, WebSocket / socket.io message channels, Electron
  `ipcMain` / `ipcRenderer` IPC, and event buses (Node `EventEmitter`, DOM `CustomEvent`, C#
  events). These resolve across files and across repositories in a federation, so `affected` on a
  backend handler reaches the front-end code that invokes it over HTTP or a socket, even though
  no import connects them.
- **Honest handling of dynamic dispatch — "0 dependents" is not "safe"**
  ([Commands: hazards](Commands#hazards)). Static analysis cannot see reflection, dispatch
  tables, or fully-dynamic calls, so a symbol reached only that way looks like a safe leaf.
  Synaptic detects what it can — a dynamic call whose target is a string literal resolving to one
  symbol becomes a low-confidence `dynamic_ref` edge — and is explicit about the rest: `affected`,
  `get_node`, and `describe_node` attach a `dynamic_caveat` when a 0-dependent symbol's scope uses
  dynamic dispatch, and the `dynamic_hazards` MCP tool (and `synaptic hazards` CLI) catalogs every
  reflection / `eval` / `Reflect.*` / `getattr` / `Class.forName` site. The graph tells you when
  it might be under-reporting instead of asserting a false "safe to change".
- **Time-travel architectural diff** ([Commands: diff](Commands#diff)). `synaptic diff
  <rev1> [rev2]` (or `--since <date>`) builds the graph at each git revision in a throwaway
  worktree and reports what changed *architecturally*, added/removed module dependencies,
  removed APIs, coupling drift, newly-introduced dependency cycles, and change hotspots, as
  terminal output, Markdown, or a self-contained HTML report. This is a diff of the structure,
  not of the text; the search and security tools below diff or index a single snapshot.
- **Architectural search with a query language (SYNQL)** ([Commands: search](Commands#search)).
  A small Cypher-inspired language matches on *structure*, kind, visibility, lines-of-code,
  fan-in/out, degree, community, and relationship patterns including variable-length paths
  (`-[:calls*1..3]->`), with `count(...)` aggregation, `--explain`, and saved queries, plus a
  library of named patterns (singleton, factory, observer, service-locator, god-class). It is
  structural, not textual or embedding-based, and every matched edge keeps its confidence tag.
- **Safe refactor as plan + verify for an AI agent** ([Commands: refactor](Commands#refactor)).
  `synaptic refactor rename` / `move` / `extract` resolve the symbol, compute the blast
  radius, score each edit site by confidence, and emit a `plan.json` + `plan.md` for an agent
  to apply; Synaptic never edits source. `synaptic refactor verify` then rebuilds and checks
  the graph held its shape (the symbol was renamed/relocated, no references were lost, no new
  cycles appeared). The graph is used to make an agent's edit auditable, not to perform it.
- **Past pure structure: graph-attributed content search and a SQL auditor.** `search_text`
  ([MCP Server](MCP-Server#search_text)) runs a ripgrep-backed content/regex search over the real
  source — through the same per-repo containment jail and federation routing as the graph — and
  attributes every hit to the symbol whose body encloses it, so a matched string literal is a
  pivot straight to `affected` / `find_callers`. And a built-in SQL performance and security
  auditor ([SQL Auditing](SQL-Auditing)) runs a rule engine over a SQL-aware graph (row-level
  security gaps, string-concatenation injection, over-broad grants, unindexed foreign keys,
  `SELECT *`, `UPDATE`/`DELETE` with no `WHERE`, and more), with an optional live `EXPLAIN`.

The rest of this page maps Synaptic against each neighbouring tool, including where those
tools are the better choice.

## Sourcegraph and Cody

**What it is.** Sourcegraph describes itself as "a Code Intelligence platform that deeply
understands your code, no matter how large or where it's hosted." [1] Its core is code
search and code navigation across an organization's repositories, deployed either as
Sourcegraph Cloud or self-hosted on Kubernetes or Docker. [4] Cody is its AI coding
assistant, which gathers context with `@`-mentions backed by keyword search, the
Sourcegraph search API, and code-graph analysis of how components connect. [7]

**Where it is stronger than Synaptic.** Sourcegraph is built for a scale Synaptic does not
target: searching and navigating across thousands of repositories from a hosted web UI with
team and admin features. Its *precise* code navigation uses the open-source SCIP Code
Intelligence Protocol, with generally-available indexers for Go, TypeScript/JavaScript,
C/C++, Java/Scala/Kotlin, Rust, Python, Ruby, and C#. [2][11] Those indexers use
compile-time information, [3] so cross-repository "go to definition" is more accurate than
Synaptic's tree-sitter heuristics for the languages they cover.

**Where Synaptic differs.** Synaptic is a single local binary that runs offline with no
server to operate, and its engine is source available under FSL-1.1-ALv2; the main
Sourcegraph repository moved to a private monorepo and its public snapshot was archived in
September 2024 as "primarily non-OSS-licensed," [5] with pricing now centered on an
Enterprise plan. [9]
(Cody's client was open-sourced under Apache-2.0, but its public repository is likewise an
archived snapshot as of August 2025. [6] The free and pro Cody tiers were discontinued in
2025. [10]) Synaptic also emits artifacts Sourcegraph does not: a portable `graph.json`,
2D/3D/SVG visualizations, GraphML/Cypher/DOT/Obsidian exports
([Output Formats](Output-Formats)), and reverse-impact ("what would changing this break")
queries ([Querying](Querying)). It also goes past search-and-navigate: it diffs the graph
across git revisions, runs a structural query language (SYNQL) over it, and turns a
rename/move into a verifiable plan for an AI agent (see
[What's distinctive about Synaptic](#whats-distinctive-about-synaptic)). And where Cody
fetches context to feed an LLM, Synaptic exposes the graph itself to any MCP client
([MCP Server](MCP-Server)).

## CodeQL and Joern (code property graphs)

These are static-analysis engines that turn code into a graph or database and let you run
queries over it. They overlap with Synaptic in shape but aim at a different goal: finding
bugs and security vulnerabilities, not summarizing structure for an LLM.

**CodeQL** is GitHub's "industry-leading semantic code analysis engine" that lets you "query
code as though it were data" to find every variant of a vulnerability. [12] Extraction
produces a "CodeQL database" holding a relational representation of the code plus a
language-specific schema, which queries written in the object-oriented **QL** language run
against. [13] For compiled languages, "extraction works by monitoring the normal build
process," while interpreted languages are extracted directly from source. [13] It supports
C/C++, C#, Go, Java, Kotlin, JavaScript, TypeScript, Python, Ruby, Rust, Swift, and GitHub
Actions. [14] The CodeQL CLI is free to use on public repositories, but its terms restrict
use "in connection with any codebase that is not an Open Source Codebase (e.g., code in a
private repo in GitHub)" unless you hold a paid GitHub Code Security / Advanced Security
license. [15][16]

**Joern** is an open-source (Apache-2.0) "platform for robust analysis of source code,
bytecode, and binary code." [17][19] Its data structure is the **Code Property Graph**, in
which "different classic program representations are merged into a property graph into a
single data structure that holds information about the program's syntax, control- and
intra-procedural data-flow." [18] Code is queried through a strongly-typed Scala-based query
language, [17] and Joern "allows importing code even if a working build environment cannot be
supplied or parts of the code are missing." [17] The Code Property Graph was first introduced
in the paper *Modeling and Discovering Vulnerabilities with Code Property Graphs*. [18]

**Where they are stronger than Synaptic.** Both perform deep semantic analysis Synaptic
does not attempt: data-flow and taint tracking, control-flow reasoning, and security queries
that find exploitable bugs. CodeQL's build-observing extraction also gives it type-resolved
precision on compiled languages.

**Where Synaptic differs.** Synaptic's edges are structural relationships (calls, imports,
references, inheritance) lifted from tree-sitter across 30+ languages
([Extraction](Extraction), [Languages](Languages)), not data-flow facts, and every edge is
tagged with a confidence level (`Extracted`, `Inferred`, `Ambiguous`) so inferred links are
auditable rather than asserted. Its purpose is to produce a compact, navigable map of a
codebase, surface god nodes, cycles, and communities
([Analysis and Reports](Analysis-and-Reports)), and answer structural questions cheaply for
a human or an LLM, rather than to prove the presence of a vulnerability. Synaptic does offer
its own query language, SYNQL, but it matches on graph *structure*, kind, visibility,
lines-of-code, fan-in/out, communities, and relationship paths, not the data-flow and taint
facts QL and Joern's Scala queries are built for. It needs no build and no query language to
get a useful first result, and it federates across repositories
([Workspaces and Federation](Workspaces-and-Federation)). Synaptic does ship one focused
security-and-performance layer of its own — a SQL auditor ([SQL Auditing](SQL-Auditing)) whose
rule engine flags row-level-security gaps, string-concatenation injection, over-broad grants,
and unindexed-key performance traps from the SQL it extracts (optionally confirmed with a live
`EXPLAIN`) — but that is rule matching over a SQL-aware graph, not the general taint and
data-flow analysis CodeQL and Joern are built for. If your goal is general security analysis
across a codebase, reach for CodeQL or Joern; if it is understanding and querying structure
(with SQL-specific auditing alongside), Synaptic is a closer fit.

## AI repo maps (Aider and Cursor)

These are the closest tools to Synaptic's token-economy goal: give an AI assistant just
enough of a large codebase to reason about it without pasting in every file. They take two
different approaches, and one of them is nearly the same idea as Synaptic.

**Aider** is an open-source (Apache-2.0) AI pair-programming tool that runs in the terminal.
[22] To give the model whole-repo context it builds a **repository map**: "a concise map of
your whole git repository that includes the most important classes and functions along with
their types and call signatures." [20] The mechanism is strikingly close to Synaptic's:
tree-sitter parses each file's definitions and references, and a "graph ranking algorithm,
computed on a graph where each source file is a node and edges connect files which have
dependencies," selects the most important identifiers to fit a token budget set by
`--map-tokens` (default 1k). [21] This is the same family of idea Synaptic uses to rank
structurally important nodes, and Aider's write-up is a good explanation of why graph
ranking beats naively dumping files.

The difference is scope and lifetime. Aider's map is rebuilt inside its own chat loop and
sent with each request; [20] it is a context feature of one assistant. Synaptic builds a
*persistent* graph you can query repeatedly, visualize, export, diff incrementally
([Incremental Updates](Incremental-Updates)), federate across repos, and expose to any MCP
client rather than a single tool ([MCP Server](MCP-Server)). The same graph also powers
analyses a per-request context map does not attempt: time-travel architectural diff, change
forecasting with predictive test selection (and optional speculative verification), impact that
crosses language and process boundaries, and agent-executable refactor plans
([Commands](Commands)).

**Cursor** is an AI code editor whose codebase indexing takes the other approach: semantic
similarity rather than structure. Cursor "breaks your code into meaningful chunks (functions,
classes, logical blocks), converts each chunk into a vector embedding that captures its
semantic meaning, and stores the results in a vector database," [23] using a Merkle tree to
detect which files changed and re-index only those. [24] Embeddings are computed via Cursor's
cloud; the indexing is designed so raw code is not persisted and file paths are obfuscated.
[24][25]

The contrast with Synaptic is paradigm, not detail. Embeddings find code that is
*semantically similar* to a query, which is powerful for "where is something like this," but
they do not encode explicit relationships. Synaptic models who-calls-whom and
who-depends-on-what as concrete edges — including across HTTP, FFI, WebSocket, and IPC
boundaries an embedding cannot represent — which is what makes reverse-impact and shortest-path
queries (and change forecasting) possible ([Querying](Querying)), and it runs fully offline with
the index staying on your machine. Cursor's indexing is also tied to the Cursor editor; Synaptic's graph is a
standalone artifact usable from any client.

## graphify

Of every tool on this page, graphify is the closest to Synaptic. It shares the core idea
(turn a folder into a queryable knowledge graph you consult instead of grepping) and a number
of identical design choices. graphify is an open-source (MIT) Python tool by Safi Shamsi,
published on PyPI as `graphifyy` and developed at github.com/safishamsi/graphify. [26][27]

**Where they line up.** Both extract structure with tree-sitter, tag every edge with the same
three confidence levels (`EXTRACTED` / `INFERRED` / `AMBIGUOUS`), detect communities with
Leiden/Louvain, expose the graph over an MCP server, answer reverse-impact and shortest-path
queries, support incremental rebuilds with file watching and git hooks, drive a PR dashboard
with blast radius, and export to an overlapping set of formats (a node-link `graph.json`,
GraphML, Cypher, an Obsidian vault, a Markdown highlights report, and Wikipedia-style wiki
pages). [26] If you know one, the other will feel familiar.

**Where graphify is distinctive.** Its headline framing is as an installable assistant skill:
a single `graphify install` wires a `/graphify` command into roughly eighteen AI coding
assistants (Claude Code, Codex, Cursor, Copilot, Gemini CLI, Aider, and others). [26] It also
leans harder into graph-database and note-taking workflows, with direct Neo4j and FalkorDB
push and first-class Obsidian Canvas and Mermaid call-flow outputs. [26]

**Where Synaptic differs.** Synaptic ships as a single static Rust binary with no
interpreter or dependency tree to install; graphify is a Python package that needs Python
3.10+ and its libraries. [26] Synaptic's MCP server is larger (30 read-only tools versus
graphify's 10 tools plus 6 resources) and implements the 2025-11-25 protocol revision with
prompts, completions, resource subscriptions, and structured tool output
([MCP Server](MCP-Server)). [26] Its cross-repo federation resolves references across
repositories through export surfaces and import / tsconfig / module-federation aliases
([Workspaces and Federation](Workspaces-and-Federation)), where graphify's global graph merges
per-project graphs and deduplicates external-library nodes by label, a lighter form of
linking. [26] On the analysis side, Synaptic adds layers aimed at editing safely: change
forecasting with predictive test selection and speculative worktree verification, impact edges
across process boundaries (WebSocket / IPC / event bus), explicit dynamic-dispatch hazard
flagging, and a SQL performance and security auditor — the capabilities detailed in
[What's distinctive about Synaptic](#whats-distinctive-about-synaptic). The two are also licensed
differently: current Synaptic versions use FSL-1.1-ALv2 (non-competing use,
including a patent grant, then Apache-2.0 after two years), while graphify is
MIT (permissive). [26]

Both tools ingest non-code material as well, and both run code extraction fully offline with
tree-sitter, so neither has a clear edge on corpus breadth or offline operation. The practical
choice is implementation and emphasis: a dependency-free Rust binary with deeper cross-repo
resolution, or a Python tool with ready-made skill integration across many assistants and
graph-database / Obsidian / Mermaid outputs.

## When to use which

- **Org-wide code search and navigation as a hosted service — thousands of repos, a team web
  UI, admin controls, and precise SCIP "go to definition":** Sourcegraph. (Synaptic also
  resolves references across repositories, but as a local federation you run yourself, not a
  hosted org-scale search engine.)
- **Finding security vulnerabilities, taint, and data-flow bugs:** CodeQL (especially with a
  build and GitHub integration) or Joern (open source, no build required). (For SQL-specific
  security and performance auditing, Synaptic's auditor overlaps; for general taint and
  data-flow, these are the right tools.)
- **Zero-setup context baked into one specific tool's own edit loop:** Aider's repo map
  (graph-based) or Cursor's indexing (embedding-based), inside those tools.
- **A near-identical knowledge graph you install as a `/graphify` skill across many
  assistants, with mixed-media ingest and Neo4j / FalkorDB / Obsidian / Mermaid outputs, in
  Python:** graphify.
- **A persistent, structural map any AI assistant can query over MCP** — explicit
  who-calls-whom / who-depends-on-what edges (across language, process, and repository
  boundaries) rather than a per-request repo map or an embedding index, available to Claude
  Code, Cursor, Copilot, and any other MCP client at once, fully offline: Synaptic.
- **A persistent, offline, auditable structural graph you can query, visualize, export, and
  federate across repos, as a single dependency-free Rust binary:** Synaptic.
- **A diff of a codebase's *architecture* across git history, a structural query language
  (SYNQL) for "find every class over 500 LOC with 20+ dependencies", or a confidence-scored,
  verifiable rename/move plan for an AI agent to apply:** Synaptic.
- **Forecasting what a change will break (with predictive test selection) and optionally
  proving it in a throwaway worktree, tracing impact across HTTP / FFI / WebSocket / IPC /
  event-bus boundaries, or auditing SQL for security and performance:** Synaptic.

These categories overlap, and several of these tools are complementary to Synaptic rather
than alternatives to it.

## Sources

1. Sourcegraph docs - <https://sourcegraph.com/docs>
2. Sourcegraph, Precise code navigation (SCIP) - <https://sourcegraph.com/docs/code-search/code-navigation/precise_code_navigation>
3. Sourcegraph, Code navigation (search-based vs precise) - <https://sourcegraph.com/docs/code-search/code-navigation>
4. Sourcegraph, Deployment overview - <https://sourcegraph.com/docs/admin/deploy>
5. Sourcegraph public snapshot (archived; licensing) - <https://github.com/sourcegraph/sourcegraph-public-snapshot>
6. Cody public snapshot (Apache-2.0; archived) - <https://github.com/sourcegraph/cody-public-snapshot>
7. Cody, Context core concept - <https://sourcegraph.com/docs/cody/core-concepts/context>
8. Cody, Clients - <https://sourcegraph.com/docs/cody/clients>
9. Sourcegraph, Pricing - <https://sourcegraph.com/pricing>
10. Sourcegraph, Changes to Cody Free, Pro, and Enterprise Starter plans - <https://sourcegraph.com/blog/changes-to-cody-free-pro-and-enterprise-starter-plans>
11. Sourcegraph, SCIP indexers - <https://sourcegraph.com/docs/code-search/code-navigation/writing_an_indexer>
12. CodeQL home - <https://codeql.github.com/>
13. CodeQL, About CodeQL - <https://codeql.github.com/docs/codeql-overview/about-codeql/>
14. CodeQL, Supported languages and frameworks - <https://codeql.github.com/docs/codeql-overview/supported-languages-and-frameworks/>
15. GitHub Docs, About the CodeQL CLI - <https://docs.github.com/en/code-security/codeql-cli/getting-started-with-the-codeql-cli/about-the-codeql-cli>
16. GitHub CodeQL Terms and Conditions (LICENSE.md) - <https://github.com/github/codeql-cli-binaries/blob/main/LICENSE.md>
17. Joern documentation - <https://docs.joern.io/>
18. Joern, Code Property Graph - <https://docs.joern.io/code-property-graph/>
19. Joern repository (Apache-2.0) - <https://github.com/joernio/joern>
20. Aider, Repository map - <https://aider.chat/docs/repomap.html>
21. Aider, Building a better repository map with tree-sitter - <https://aider.chat/2023/10/22/repomap.html>
22. Aider repository (Apache-2.0) - <https://github.com/Aider-AI/aider>
23. Cursor, Codebase indexing - <https://cursor.com/docs/context/codebase-indexing>
24. Cursor, Secure codebase indexing - <https://cursor.com/blog/secure-codebase-indexing>
25. Cursor, Privacy and data governance - <https://cursor.com/docs/enterprise/privacy-and-data-governance>
26. graphify repository (Safi Shamsi) - <https://github.com/safishamsi/graphify>
27. graphify on PyPI (`graphifyy`) - <https://pypi.org/project/graphifyy/>
