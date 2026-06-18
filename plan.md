# CodeGraph MCP Server Improvement Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Evolve the CodeGraph MCP server from a metadata-only graph query surface into a full code-intelligence server: it returns real source code, answers transitive impact questions, speaks the current MCP spec (2025-06-18 with structured output + tool annotations), and adds prompts, pagination, live resources, and diff-aware tooling.

**Architecture:** All work lands in `crates/codegraph-server` (the hand-rolled JSON-RPC dispatcher) and reuses engine code already present in `codegraph-query` (`affected_nodes`, `explain`, `resolve_seed`) and `codegraph-graph` (`graph_diff`, `compute_pr_impact`). New capabilities are added as additional `tools/call` handlers, new JSON-RPC methods (`prompts/*`, `completion/complete`, `logging/setLevel`, `resources/templates/list`, `resources/subscribe`), and one new server-state field (`source_root`) with a path-traversal jail. The stdio dispatcher stays `&self`-pure and unit-testable; HTTP shares the same engine behind the existing `RwLock`.

**Tech Stack:** Rust (edition/workspace pinned to 1.95.0), `serde_json` for the protocol, `axum`/`tokio` for HTTP, `tiktoken-rs` (new) for token budgeting, `assert_cmd` for end-to-end CLI tests. No `rmcp` dependency (the server speaks MCP directly).

---

## Conventions (read before any task)

These are CodeGraph house rules; every task assumes them:

1. **TDD always.** Write the failing test, run it red, implement the minimum, run it green, commit. Never write implementation before its test.
2. **Plain-ASCII comments, no AI tells.** Terse comments that explain *why*, never restate the code. No em-dashes, no unicode arrows, no box-divider banners. Doc comments (`///`, `//!`) are allowed to be fuller.
3. **The tool surface is ASCII-only.** `tools_list()`, prompt text, and `SERVER_INSTRUCTIONS` are checked by `tool_surface_is_plain_ascii` for em-dashes (`U+2014`), smart quotes, and arrows (`U+2192`). Use ASCII `->`, `<=`, `"` only.
4. **Sanitize labels, not code.** Every graph-derived *label* must pass through `codegraph_core::sanitize_label` before it reaches tool text (security boundary on corpus/LLM-derived names). Raw file *content* returned by `get_source` is NOT sanitized (it is verbatim source the agent explicitly asked for); only the surrounding header labels are sanitized.
5. **No Claude co-author trailer** on commits in this repo. Commit messages are `type: summary` (e.g. `feat:`, `test:`, `refactor:`), plain ASCII.
6. **Update the tool-count tests when you add a tool.** `initialize_and_tools_list` asserts `names.len() == 12` and lists the expected names; bump the count and add the name in the SAME task that adds the tool, or the suite goes red.
7. **Check skill drift after changing the tool surface.** Adding/renaming tools can drift the generated skill snapshots. Run `cargo test -p codegraph-skillgen`; if it fails on an intentional change, run `cargo run -p codegraph -- skill bless` and commit the re-blessed `expected/` files.

**Per-task verification baseline** (run before every commit):

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
cargo fmt --all
```

**Full pre-merge gate** (mirrors `.github/workflows/ci.yml`):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `crates/codegraph-server/src/lib.rs` | Server state, tools, dispatch, tools_list | Modify (most tasks) |
| `crates/codegraph-server/src/http.rs` | HTTP transport, REST, SSE | Modify (subscriptions, logging) |
| `crates/codegraph-server/src/session.rs` | Session store | Modify (per-session subscriptions) |
| `crates/codegraph-server/src/source.rs` | NEW: source-root jail + line parsing | Create (Task 1) |
| `crates/codegraph-server/src/prompts.rs` | NEW: prompts/list + prompts/get | Create (Task 7) |
| `crates/codegraph-server/Cargo.toml` | Deps | Modify (Task 9: tiktoken-rs) |
| `bin/codegraph/src/commands/serve.rs` | Wires source-root into the Server | Modify (Task 1) |
| `bin/codegraph/src/cli.rs` | `--source-root`, `--allow-edits` flags | Modify (Task 1, Task 15) |
| `bin/codegraph/tests/mcp_e2e.rs` | NEW: stdio protocol conformance e2e | Create (Task 16) |
| `.github/workflows/ci.yml` | CI matrix | Modify (Task 16) |

Each task below produces a self-contained, committable change.

---

## Implementation Order

```
TIER 1 (do first, highest leverage)
  Task 1  source-root plumbing + path jail        (foundation)
  Task 2  get_source tool                         (depends on 1)
  Task 3  affected tool                           (independent)
  Task 4  find_callers / find_callees tools       (independent)
  Task 5  protocol negotiation + tool annotations (independent)
  Task 6  structured output (outputSchema)        (after 2-5 so new tools get schemas)

TIER 2
  Task 7  prompts capability
  Task 8  result pagination (offset/limit)
  Task 9  real token budgeting (tiktoken-rs)
  Task 10 resource templates
  Task 11 resource subscriptions (live updates over SSE)

TIER 3
  Task 12 working_changes_impact tool (git diff, no gh)
  Task 13 completion/complete capability
  Task 14 logging capability + log notifications
  Task 15 opt-in write tools (behind --allow-edits)   [optional]

CROSS-CUTTING
  Task 16 MCP e2e conformance test + CI job
```

---
---

# TIER 1

## Task 1: Source-root plumbing + path-traversal jail

**Why:** Every "return real code" feature needs a trusted directory to resolve repo-relative `source_file` paths against, plus a jail so a crafted `source_file` (`../../etc/passwd`) cannot escape it. This task adds the state and the safe resolver with zero new tools.

**Files:**
- Create: `crates/codegraph-server/src/source.rs`
- Modify: `crates/codegraph-server/src/lib.rs` (add `source_root` field, `with_source_root`, re-export)
- Modify: `bin/codegraph/src/commands/serve.rs` (set source root)
- Modify: `bin/codegraph/src/cli.rs` (add `--source-root`)

- [ ] **Step 1: Write the failing test** for the line-marker parser and the jail.

Create `crates/codegraph-server/src/source.rs`:

```rust
//! Source-file access for the code-retrieval tools: parse a node's
//! `source_location` line marker and resolve a repo-relative `source_file`
//! against a trusted root, refusing anything that escapes it.

use std::path::{Path, PathBuf};

/// Parse a `source_location` marker into a 1-based start line. The extractor
/// writes `"L<n>"`; tolerate a range (`"L42-L60"` -> 42) and a bare number.
pub fn parse_line_marker(s: &str) -> Option<usize> {
    let head = s.split('-').next().unwrap_or(s).trim();
    let digits = head.trim_start_matches(|c: char| !c.is_ascii_digit());
    digits.parse().ok()
}

/// Resolve `rel` (a repo-relative `source_file`) under `root`, returning the
/// canonical path only if it stays inside `root`. `None` means: no readable
/// file, or the path escaped the jail. Canonicalizing both sides collapses
/// `..` so traversal is caught by the `starts_with` check.
pub fn resolve_in_root(root: &Path, rel: &str) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let canon = root.join(rel).canonicalize().ok()?;
    canon.starts_with(&root).then_some(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_marker_variants() {
        assert_eq!(parse_line_marker("L42"), Some(42));
        assert_eq!(parse_line_marker("L42-L60"), Some(42));
        assert_eq!(parse_line_marker("7"), Some(7));
        assert_eq!(parse_line_marker("Lxyz"), None);
    }

    #[test]
    fn jail_allows_inside_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "x = 1\n").unwrap();

        assert!(resolve_in_root(root, "src/a.py").is_some());
        // Escape attempt: canonicalizes outside root -> rejected.
        assert!(resolve_in_root(root, "../../etc/passwd").is_none());
        // Missing file -> None (not a panic).
        assert!(resolve_in_root(root, "src/missing.py").is_none());
    }
}
```

- [ ] **Step 2: Wire the module in and run the test red.**

In `crates/codegraph-server/src/lib.rs`, under the existing `mod http;` block, add:

```rust
mod source;
```

Run:

```bash
cargo test -p codegraph-server source::
```

Expected: the two `source::tests` compile and PASS (the module is self-contained, so this is green immediately; treat a compile error as the red-to-fix signal).

- [ ] **Step 3: Add `source_root` to the Server and a builder.**

In `crates/codegraph-server/src/lib.rs`, add the field to `struct Server` (after `log_path`):

```rust
    /// Trusted root for resolving repo-relative `source_file` paths to real
    /// files (the code-retrieval tools). `None` disables source reading.
    source_root: Option<PathBuf>,
```

Initialize it to `None` in BOTH `from_graph_data` (the struct literal) and anywhere else the struct is built. Then add the builder next to `with_runner`:

```rust
    /// Set the trusted source root for `get_source` (and other code-reading
    /// tools). Stored as-is; resolution canonicalizes per request.
    pub fn with_source_root(mut self, root: PathBuf) -> Server {
        self.source_root = Some(root);
        self
    }

    /// Resolve a node's `source_file` to a real, in-jail path (or `None`).
    fn resolve_source_path(&self, rel: &str) -> Option<PathBuf> {
        let root = self.source_root.as_deref()?;
        source::resolve_in_root(root, rel)
    }
```

- [ ] **Step 4: Verify the workspace still builds and the existing suite is green.**

```bash
cargo test -p codegraph-server
```

Expected: PASS (new field defaults to `None`, no behavior change yet).

- [ ] **Step 5: Set the source root from the CLI.**

In `bin/codegraph/src/cli.rs`, add a field to the `Serve` variant:

```rust
        /// Trusted root for resolving source files in code-reading tools
        /// (default: the directory above codegraph-out/, i.e. the repo root).
        #[arg(long)]
        source_root: Option<PathBuf>,
```

In `bin/codegraph/src/commands/serve.rs`, change `run_serve`'s signature to accept it and set it on the server:

```rust
pub(crate) fn run_serve(
    graph: Option<PathBuf>,
    http: Option<String>,
    api_key: Option<String>,
    source_root: Option<PathBuf>,
) -> Result<()> {
    let path = default_graph_path(graph);
    let mut server = Server::load(path.clone()).with_context(|| {
        format!("loading {} (run `codegraph extract` first?)", path.display())
    })?;
    // Default: graph is at <root>/codegraph-out/graph.json, so the repo root is
    // two levels up. Fall back to the current dir when that is unavailable.
    let root = source_root.unwrap_or_else(|| {
        path.parent()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    });
    server = server.with_source_root(root);
    // ... rest unchanged ...
```

Add `use std::path::Path;` at the top of `serve.rs`. Update the call site in `bin/codegraph/src/main.rs` (or wherever `run_serve` is dispatched) to pass the new `source_root` argument.

- [ ] **Step 6: Run the e2e build and commit.**

```bash
cargo build -p codegraph
cargo test -p codegraph-server
```

Expected: PASS.

```bash
git add crates/codegraph-server/src/source.rs crates/codegraph-server/src/lib.rs bin/codegraph/src/commands/serve.rs bin/codegraph/src/cli.rs bin/codegraph/src/main.rs
git commit -m "feat(server): add jailed source-root resolver for code-reading tools"
```

---

## Task 2: `get_source` tool (return real code)

**Why:** The single biggest gap vs Serena. Today the tools return `NODE login_user [code] src/auth.py` and the agent still has to open the file. This returns the actual lines.

**Files:**
- Modify: `crates/codegraph-server/src/lib.rs` (new tool method, dispatch arm, tools_list entry, count test)

- [ ] **Step 1: Write the failing test.**

Add to the `tests` module in `crates/codegraph-server/src/lib.rs`. This builds a server whose graph points at a real temp file and asserts the lines come back:

```rust
#[test]
fn get_source_returns_lines_under_jail() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/auth.py"),
        "def login_user(u):\n    return check(u)\n\n\ndef check(u):\n    return True\n",
    )
    .unwrap();

    let mut n = node("login", "login_user", Some(0));
    n.source_file = "src/auth.py".into();
    n.source_location = Some("L1".into());
    let gd = GraphData {
        nodes: vec![n],
        ..Default::default()
    };
    let mut s = Server::from_graph_data(gd, None).with_source_root(root.to_path_buf());

    let out = call_tool(&mut s, "get_source", json!({"label": "login_user", "context_lines": 2}));
    assert!(out.contains("def login_user(u):"), "should include the body: {out}");
    assert!(out.contains("src/auth.py:L1"), "header names the file+line: {out}");
}

#[test]
fn get_source_without_root_is_graceful() {
    let mut s = server(); // no source root
    let out = call_tool(&mut s, "get_source", json!({"label": "AuthService"}));
    assert!(out.contains("Source not available"), "{out}");
}
```

(`node(...)` in this test sets `source_file = "{id}.py"`; we override it. `GraphData::default()` is already used elsewhere in this module, so it is in scope.)

- [ ] **Step 2: Run it red.**

```bash
cargo test -p codegraph-server get_source
```

Expected: FAIL with `Unknown tool: get_source` in the returned text (the dispatch has no arm yet).

- [ ] **Step 3: Implement the tool method.**

Add to `impl Server` (near `tool_get_node`):

```rust
    /// `get_source` - the actual source lines for a symbol. Resolves the node,
    /// reads its file under the source-root jail, and returns a window starting
    /// at the node's recorded line (`source_location` = "L<n>"). The graph has
    /// no end line, so it returns `context_lines` lines from the start.
    pub fn tool_get_source(&self, label: &str, context_lines: usize) -> String {
        let Some(id) = resolve_seed(&self.kg, label) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let Some(n) = self.kg.node(&id) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let Some(path) = self.resolve_source_path(&n.source_file) else {
            return format!(
                "Source not available for {} ({}).",
                sanitize_label(&n.label),
                sanitize_label(&n.source_file)
            );
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return format!("Could not read {}.", sanitize_label(&n.source_file));
        };
        let start = n
            .source_location
            .as_deref()
            .and_then(source::parse_line_marker)
            .unwrap_or(1);
        let window = context_lines.clamp(1, 400);
        let lines: Vec<&str> = text.lines().collect();
        let from = start.saturating_sub(1).min(lines.len());
        let to = (from + window).min(lines.len());
        // Header labels are sanitized; the code body is returned verbatim.
        let mut out = format!(
            "{} [{}] {}:L{}-L{}\n",
            sanitize_label(&n.label),
            file_type_str(&n.file_type),
            sanitize_label(&n.source_file),
            from + 1,
            to
        );
        for (i, line) in lines[from..to].iter().enumerate() {
            out.push_str(&format!("{:>5}  {}\n", from + 1 + i, line));
        }
        out
    }
```

- [ ] **Step 4: Add the dispatch arm.**

In `dispatch_tool`, add before the `other =>` arm:

```rust
            "get_source" => {
                self.tool_get_source(&s("label"), u("context_lines", 40) as usize)
            }
```

- [ ] **Step 5: Add the `tools/list` entry and bump the count test.**

In `tools_list()`, add (plain ASCII only):

```rust
        { "name": "get_source", "description": "Return the actual source code for a symbol (the lines at its location), so you do not have to open the file. Use after query_graph or get_node to read a function or class body directly.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." },
              "context_lines": { "type": "integer", "description": "How many lines to return from the symbol start (default 40, max 400)." }
          }, "required": ["label"] } },
```

In `initialize_and_tools_list`, change `assert_eq!(names.len(), 12)` to `13` and add `"get_source"` to the expected-names array.

- [ ] **Step 6: Run green, lint, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
cargo test -p codegraph-skillgen   # skill-drift guard
```

Expected: PASS. If skillgen fails on an intentional surface change, run `cargo run -p codegraph -- skill bless` and stage the result.

```bash
git add crates/codegraph-server/src/lib.rs
git commit -m "feat(server): add get_source tool returning real symbol code"
```

---

## Task 3: `affected` tool (transitive reverse-impact)

**Why:** `codegraph-query` already has `affected_nodes` + `DEFAULT_AFFECTED_RELATIONS` (fully tested), and the CLI exposes it as `affected`, but the MCP server does not. "What breaks if I change X" is the flagship code-intelligence query. This is parity-with-yourself.

**Files:**
- Modify: `crates/codegraph-server/src/lib.rs`

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn affected_lists_transitive_dependents() {
    // login_user calls check; AuthService calls login_user.
    // Changing `check` affects login_user (1 hop) and AuthService (2 hops).
    let gd = GraphData {
        nodes: vec![
            node("check", "check", Some(0)),
            node("login", "login_user", Some(0)),
            node("auth", "AuthService", Some(0)),
        ],
        links: vec![
            edge("login", "check", "calls"),
            edge("auth", "login", "calls"),
        ],
        ..Default::default()
    };
    let mut s = Server::from_graph_data(gd, None);
    let out = call_tool(&mut s, "affected", json!({"label": "check", "depth": 5}));
    assert!(out.contains("login_user"), "{out}");
    assert!(out.contains("AuthService"), "{out}");
    assert!(out.contains("via calls"), "{out}");
}
```

- [ ] **Step 2: Run it red.**

```bash
cargo test -p codegraph-server affected_lists
```

Expected: FAIL with `Unknown tool: affected`.

- [ ] **Step 3: Implement.**

At the top of `lib.rs`, extend the `codegraph_query` import:

```rust
use codegraph_query::{
    affected_nodes, explain, resolve_seed, shortest_path, QueryIndex, TraversalMode,
    DEFAULT_AFFECTED_RELATIONS,
};
```

Add the method:

```rust
    /// `affected` - the nodes that transitively depend on `label`, found by
    /// walking impact edges backward up to `depth` hops. Empty `relations`
    /// uses the default structural-impact set.
    pub fn tool_affected(&self, label: &str, depth: usize, relations: &[String]) -> String {
        let Some(id) = resolve_seed(&self.kg, label) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let rels: Vec<&str> = if relations.is_empty() {
            DEFAULT_AFFECTED_RELATIONS.to_vec()
        } else {
            relations.iter().map(String::as_str).collect()
        };
        let depth = depth.clamp(1, 16);
        let hits = affected_nodes(&self.kg, &id, &rels, depth);
        let seed = sanitize_label(&self.label_of(&id));
        if hits.is_empty() {
            return format!("Nothing depends on {seed} within {depth} hops.");
        }
        let mut out = format!("{} nodes depend on {seed} (<= {depth} hops):", hits.len());
        for h in &hits {
            out.push_str(&format!(
                "\n  [{}h via {}] {}",
                h.depth,
                sanitize_label(&h.via_relation),
                sanitize_label(&self.label_of(&h.node_id))
            ));
        }
        out
    }
```

- [ ] **Step 4: Add the dispatch arm.**

```rust
            "affected" => {
                let rels: Vec<String> = args
                    .get("relations")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                self.tool_affected(&s("label"), u("depth", 3) as usize, &rels)
            }
```

- [ ] **Step 5: Add the `tools/list` entry, bump the count, update instructions.**

In `tools_list()`:

```rust
        { "name": "affected", "description": "Reverse-impact: the nodes that transitively depend on a symbol, i.e. what could break if you change it. Walks calls/imports/inherits/uses edges backward. Answers 'what is the blast radius of changing X'.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." },
              "depth": { "type": "integer", "description": "Max hops to walk backward (default 3, max 16)." },
              "relations": { "type": "array", "items": { "type": "string" }, "description": "Optional edge relations to follow; defaults to the structural-impact set (calls, imports, inherits, implements, uses, references, depends_on, reads_from)." }
          }, "required": ["label"] } },
```

In `initialize_and_tools_list`: bump `13` to `14`, add `"affected"`.

Optionally add one line to `SERVER_INSTRUCTIONS` mentioning `affected` for impact analysis (keep ASCII).

- [ ] **Step 6: Green, lint, skill-drift, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
cargo test -p codegraph-skillgen
git add crates/codegraph-server/src/lib.rs
git commit -m "feat(server): add affected tool for transitive reverse-impact"
```

---

## Task 4: `find_callers` / `find_callees` directional tools

**Why:** `get_neighbors` shows direction with arrows but cannot filter by direction. "Who calls X" and "what does X call" are the two most common navigation queries and deserve first-class, unambiguous tools.

**Files:**
- Modify: `crates/codegraph-server/src/lib.rs`

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn find_callers_and_callees_split_by_direction() {
    let gd = GraphData {
        nodes: vec![
            node("auth", "AuthService", Some(0)),
            node("login", "login_user", Some(0)),
            node("db", "Database", Some(0)),
        ],
        // AuthService calls login_user; login_user calls Database.
        links: vec![edge("auth", "login", "calls"), edge("login", "db", "calls")],
        ..Default::default()
    };
    let mut s = Server::from_graph_data(gd, None);

    let callers = call_tool(&mut s, "find_callers", json!({"label": "login_user"}));
    assert!(callers.contains("AuthService"), "{callers}");
    assert!(!callers.contains("Database"), "callees must not appear: {callers}");

    let callees = call_tool(&mut s, "find_callees", json!({"label": "login_user"}));
    assert!(callees.contains("Database"), "{callees}");
    assert!(!callees.contains("AuthService"), "callers must not appear: {callees}");
}
```

- [ ] **Step 2: Run it red.**

```bash
cargo test -p codegraph-server find_callers_and_callees
```

Expected: FAIL with `Unknown tool: find_callers`.

- [ ] **Step 3: Implement.**

```rust
    /// `find_callers` - who calls/uses this node (incoming call-like edges).
    pub fn tool_find_callers(&self, label: &str) -> String {
        self.directional("Callers", label, "in")
    }

    /// `find_callees` - what this node calls/uses (outgoing call-like edges).
    pub fn tool_find_callees(&self, label: &str) -> String {
        self.directional("Callees", label, "out")
    }

    fn directional(&self, title: &str, label: &str, dir: &str) -> String {
        let Some(id) = resolve_seed(&self.kg, label) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let Some(ex) = explain(&self.kg, &id) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let mut out = format!("{title} of {}:", sanitize_label(&ex.label));
        let mut any = false;
        for nb in &ex.neighbors {
            // Call-like relations only; direction filtered.
            let rel = nb.relation.to_lowercase();
            let call_like = rel.contains("call") || rel.contains("use") || rel.contains("reference");
            if nb.direction == dir && call_like {
                any = true;
                out.push_str(&format!(
                    "\n  {} [{}]",
                    sanitize_label(&nb.label),
                    sanitize_label(&nb.relation)
                ));
            }
        }
        if !any {
            out.push_str("\n  (none)");
        }
        out
    }
```

- [ ] **Step 4: Dispatch arms.**

```rust
            "find_callers" => self.tool_find_callers(&s("label")),
            "find_callees" => self.tool_find_callees(&s("label")),
```

- [ ] **Step 5: tools_list entries + count.**

```rust
        { "name": "find_callers", "description": "List the nodes that call, use, or reference this symbol (incoming edges only). Answers 'who calls X'.",
          "inputSchema": { "type": "object", "properties": { "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." } }, "required": ["label"] } },
        { "name": "find_callees", "description": "List the nodes this symbol calls, uses, or references (outgoing edges only). Answers 'what does X call'.",
          "inputSchema": { "type": "object", "properties": { "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." } }, "required": ["label"] } },
```

In `initialize_and_tools_list`: bump `14` to `16`, add `"find_callers"`, `"find_callees"`.

- [ ] **Step 6: Green, lint, skill-drift, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
cargo test -p codegraph-skillgen
git add crates/codegraph-server/src/lib.rs
git commit -m "feat(server): add find_callers and find_callees directional tools"
```

---

## Task 5: Protocol negotiation + tool annotations

**Why:** `PROTOCOL_VERSION` is hardcoded to `"2024-11-05"` (two revisions stale) and `initialize` ignores the client's requested version. Modern clients send `"2025-06-18"`. Tool annotations (`readOnlyHint`, `openWorldHint`) let clients decide what is safe to auto-run.

**Files:**
- Modify: `crates/codegraph-server/src/lib.rs`

- [ ] **Step 1: Write the failing tests.**

```rust
#[test]
fn initialize_echoes_supported_protocol_else_latest() {
    let mut s = server();
    // Client asks for a supported version -> echoed back.
    let r = s.handle_request(&json!({
        "jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2025-06-18"}
    })).unwrap();
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");

    // Unknown version -> server returns its latest supported.
    let r = s.handle_request(&json!({
        "jsonrpc":"2.0","id":2,"method":"initialize",
        "params":{"protocolVersion":"1999-01-01"}
    })).unwrap();
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
}

#[test]
fn every_tool_is_annotated_read_only() {
    let tools = tools_list();
    for t in tools.as_array().unwrap() {
        let name = t["name"].as_str().unwrap();
        let ann = &t["annotations"];
        assert_eq!(ann["readOnlyHint"], json!(true), "tool {name} must be read-only");
        // PR + git tools reach the network/working tree -> openWorldHint true.
        let open = matches!(name, "list_prs" | "get_pr_impact" | "triage_prs");
        assert_eq!(ann["openWorldHint"], json!(open), "tool {name} openWorldHint");
    }
}
```

- [ ] **Step 2: Run red.**

```bash
cargo test -p codegraph-server initialize_echoes_supported_protocol_else_latest every_tool_is_annotated_read_only
```

Expected: FAIL (protocolVersion is the const; tools have no `annotations`).

- [ ] **Step 3: Implement negotiation.**

Replace the `const PROTOCOL_VERSION` line with:

```rust
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const LATEST_PROTOCOL: &str = "2025-06-18";

/// Echo the client's requested protocol when we support it, else our latest.
fn negotiate_protocol(requested: Option<&str>) -> &'static str {
    match requested {
        Some(v) => SUPPORTED_PROTOCOLS
            .iter()
            .copied()
            .find(|s| *s == v)
            .unwrap_or(LATEST_PROTOCOL),
        None => LATEST_PROTOCOL,
    }
}
```

In `dispatch_request`, change the `"initialize"` arm:

```rust
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                Ok(json!({
                    "protocolVersion": negotiate_protocol(requested),
                    "capabilities": { "tools": {}, "resources": {} },
                    "serverInfo": { "name": "codegraph", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": SERVER_INSTRUCTIONS,
                }))
            }
```

Fix the existing `initialize_and_tools_list` test, which asserts `init["result"]["protocolVersion"] == PROTOCOL_VERSION`: change it to `== "2025-06-18"`.

- [ ] **Step 4: Add annotations to every tool.**

In `tools_list()`, add an `"annotations"` object to each entry. For the read-only graph tools:

```json
"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
```

For `list_prs`, `get_pr_impact`, `triage_prs`, set `"openWorldHint": true` (they shell out to `gh`).

To avoid hand-editing 16 objects inconsistently, refactor `tools_list()` to merge a shared annotation into each entry after construction:

```rust
fn tools_list() -> Value {
    let mut tools = json!([ /* ... existing entries WITHOUT annotations ... */ ]);
    let open_world = ["list_prs", "get_pr_impact", "triage_prs"];
    for t in tools.as_array_mut().unwrap() {
        let name = t["name"].as_str().unwrap_or("").to_string();
        t["annotations"] = json!({
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": open_world.contains(&name.as_str()),
        });
    }
    tools
}
```

- [ ] **Step 5: Green, lint, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
git add crates/codegraph-server/src/lib.rs
git commit -m "feat(server): negotiate MCP protocol version and annotate tools read-only"
```

---

## Task 6: Structured tool output (outputSchema + structuredContent)

**Why:** Every tool returns plain text today, so agents string-scrape. The 2025-06-18 spec lets a tool declare `outputSchema` and return a validated `structuredContent` object alongside the text. Do this for the highest-value tools: `graph_stats`, `query_graph`, `affected`, `god_nodes`.

**Files:**
- Modify: `crates/codegraph-server/src/lib.rs`

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn graph_stats_returns_structured_content() {
    let mut s = server();
    let resp = s.handle_request(&json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"graph_stats","arguments":{}}
    })).unwrap();
    let sc = &resp["result"]["structuredContent"];
    assert_eq!(sc["nodes"], json!(3));
    assert_eq!(sc["edges"], json!(2));
    // Text content is still present for display.
    assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("3 nodes"));
}

#[test]
fn structured_tools_declare_output_schema() {
    let tools = tools_list();
    for name in ["graph_stats", "query_graph", "affected", "god_nodes"] {
        let t = tools.as_array().unwrap().iter().find(|t| t["name"] == name).unwrap();
        assert!(t.get("outputSchema").is_some(), "{name} needs an outputSchema");
    }
}
```

- [ ] **Step 2: Run red.**

```bash
cargo test -p codegraph-server graph_stats_returns_structured_content structured_tools_declare_output_schema
```

Expected: FAIL (no `structuredContent` key, no `outputSchema`).

- [ ] **Step 3: Add structured builders.**

Add JSON producers next to the text tools:

```rust
    fn stats_json(&self) -> Value {
        let s = &self.stats;
        json!({ "nodes": s.nodes, "edges": s.edges, "communities": s.communities,
                "extracted": s.extracted, "inferred": s.inferred, "ambiguous": s.ambiguous })
    }

    fn god_nodes_json(&self, top_n: usize) -> Value {
        let take = self.god_nodes_all.len().min(top_n.max(1));
        let arr: Vec<Value> = self.god_nodes_all[..take].iter().map(|g| json!({
            "label": sanitize_label(&g.label), "degree": g.degree, "id": sanitize_label(&g.id.0)
        })).collect();
        json!({ "god_nodes": arr })
    }

    fn affected_json(&self, label: &str, depth: usize, relations: &[String]) -> Value {
        let Some(id) = resolve_seed(&self.kg, label) else { return json!({ "affected": [] }); };
        let rels: Vec<&str> = if relations.is_empty() {
            DEFAULT_AFFECTED_RELATIONS.to_vec()
        } else { relations.iter().map(String::as_str).collect() };
        let hits = affected_nodes(&self.kg, &id, &rels, depth.clamp(1, 16));
        let arr: Vec<Value> = hits.iter().map(|h| json!({
            "label": sanitize_label(&self.label_of(&h.node_id)),
            "depth": h.depth, "via_relation": sanitize_label(&h.via_relation)
        })).collect();
        json!({ "seed": sanitize_label(&self.label_of(&id)), "affected": arr })
    }

    fn query_graph_json(&self, question: &str, mode: TraversalMode, token_budget: usize, ctx: &[String]) -> Value {
        // Reuse the existing retrieval; emit nodes + edges as arrays.
        let max_nodes = (token_budget / 40).clamp(10, 400);
        let r = self.query_index.query(&self.kg, question, max_nodes, mode);
        let _ = ctx; // context_filter applied identically to the text path if set
        let nodes: Vec<Value> = r.nodes.iter().filter_map(|id| self.kg.node(id)).map(|n| json!({
            "label": sanitize_label(&n.label),
            "file_type": file_type_str(&n.file_type),
            "source_file": sanitize_label(&n.source_file)
        })).collect();
        let edges: Vec<Value> = r.edges.iter().map(|e| json!({
            "source": sanitize_label(&self.label_of(&e.source)),
            "relation": sanitize_label(&e.relation),
            "target": sanitize_label(&self.label_of(&e.target))
        })).collect();
        json!({ "nodes": nodes, "edges": edges })
    }
```

- [ ] **Step 4: Thread structured content through `dispatch_tool`.**

Change the tail of `dispatch_tool` to attach `structuredContent` for the four tools. Compute it after `text`:

```rust
        let structured: Option<Value> = match name {
            "graph_stats" => Some(self.stats_json()),
            "god_nodes" => Some(self.god_nodes_json(u("top_n", 10) as usize)),
            "affected" => {
                let rels: Vec<String> = args.get("relations").and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                Some(self.affected_json(&s("label"), u("depth", 3) as usize, &rels))
            }
            "query_graph" => {
                let mode = match args.get("mode").and_then(Value::as_str) {
                    Some("dfs") => TraversalMode::Dfs, _ => TraversalMode::Bfs,
                };
                Some(self.query_graph_json(&s("question"), mode, u("token_budget", 2000) as usize, &[]))
            }
            _ => None,
        };

        let mut result = json!({ "content": [{ "type": "text", "text": text }], "isError": false });
        if let Some(sc) = structured {
            result["structuredContent"] = sc;
        }
        Ok(result)
```

- [ ] **Step 5: Declare `outputSchema` on the four tools.**

In `tools_list()`, add an `"outputSchema"` to each of `graph_stats`, `query_graph`, `affected`, `god_nodes`. Example for `graph_stats`:

```rust
"outputSchema": { "type": "object", "properties": {
    "nodes": {"type":"integer"}, "edges": {"type":"integer"}, "communities": {"type":"integer"},
    "extracted": {"type":"integer"}, "inferred": {"type":"integer"}, "ambiguous": {"type":"integer"}
}, "required": ["nodes","edges","communities"] },
```

(Write the analogous schemas for the array-returning tools: `god_nodes` -> `{god_nodes: [{label,degree,id}]}`, `affected` -> `{seed, affected: [{label,depth,via_relation}]}`, `query_graph` -> `{nodes: [...], edges: [...]}`.)

- [ ] **Step 6: Green, lint, skill-drift, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
cargo test -p codegraph-skillgen
git add crates/codegraph-server/src/lib.rs
git commit -m "feat(server): emit structuredContent + outputSchema for key tools"
```

---
---

# TIER 2

## Task 7: MCP Prompts capability

**Why:** The server advertises `tools` and `resources` but not `prompts`. Prompts are user-selectable, slash-command-style entry points. Wrap the common workflows so a host surfaces them directly.

**Files:**
- Create: `crates/codegraph-server/src/prompts.rs`
- Modify: `crates/codegraph-server/src/lib.rs` (capability, `prompts/list`, `prompts/get`)

- [ ] **Step 1: Write the failing test** (in `lib.rs` tests):

```rust
#[test]
fn prompts_list_and_get() {
    let mut s = server();
    let list = s.handle_request(&json!({"jsonrpc":"2.0","id":1,"method":"prompts/list"})).unwrap();
    let names: Vec<&str> = list["result"]["prompts"].as_array().unwrap()
        .iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"onboard"));
    assert!(names.contains(&"explain_subsystem"));

    let got = s.handle_request(&json!({
        "jsonrpc":"2.0","id":2,"method":"prompts/get",
        "params":{"name":"explain_subsystem","arguments":{"topic":"authentication"}}
    })).unwrap();
    let text = got["result"]["messages"][0]["content"]["text"].as_str().unwrap();
    assert!(text.contains("authentication"), "arg interpolated: {text}");
}
```

- [ ] **Step 2: Run red.**

```bash
cargo test -p codegraph-server prompts_list_and_get
```

Expected: FAIL with method-not-found (`-32601`).

- [ ] **Step 3: Implement `prompts.rs`.**

```rust
//! MCP prompts: user-selectable, parameterized workflows over the graph tools.

use codegraph_core::sanitize_label;
use serde_json::{json, Value};

/// The `prompts/list` payload.
pub fn prompts_list() -> Value {
    json!([
        { "name": "onboard", "description": "Get oriented in this codebase fast.", "arguments": [] },
        { "name": "explain_subsystem", "description": "Explain how a subsystem works.",
          "arguments": [{ "name": "topic", "description": "Subsystem or feature, e.g. 'authentication'.", "required": true }] },
        { "name": "assess_pr", "description": "Assess a pull request's risk via graph blast radius.",
          "arguments": [{ "name": "pr_number", "description": "PR number.", "required": true }] },
        { "name": "trace_flow", "description": "Trace the path between two symbols.",
          "arguments": [
            { "name": "from", "description": "Start symbol.", "required": true },
            { "name": "to", "description": "End symbol.", "required": true }] }
    ])
}

/// Build a `prompts/get` response, or `None` for an unknown name.
pub fn prompts_get(name: &str, args: &Value) -> Option<Value> {
    let arg = |k: &str| args.get(k).and_then(Value::as_str).map(sanitize_label).unwrap_or_default();
    let text = match name {
        "onboard" => "Orient me in this codebase. Call graph_stats, then god_nodes, then read \
            codegraph://questions, and summarize the main subsystems and entry points.".to_string(),
        "explain_subsystem" => format!(
            "Explain how the '{}' subsystem works. Use query_graph for it, then get_source on the \
             key symbols, and find_callers/find_callees to map the flow.", arg("topic")),
        "assess_pr" => format!(
            "Assess the risk of PR #{}. Call get_pr_impact, then affected on the changed symbols, \
             and summarize the blast radius and what to review.", arg("pr_number")),
        "trace_flow" => format!(
            "Trace how '{}' reaches '{}'. Call shortest_path, then get_source on each hop.",
            arg("from"), arg("to")),
        _ => return None,
    };
    Some(json!({ "messages": [{ "role": "user", "content": { "type": "text", "text": text } }] }))
}
```

- [ ] **Step 4: Wire into `lib.rs`.**

Add `mod prompts;` near the other module decls. In `dispatch_request`:
- add `"prompts"` to the `capabilities` object in `initialize`: `"capabilities": { "tools": {}, "resources": {}, "prompts": {} }`.
- add arms:

```rust
            "prompts/list" => Ok(json!({ "prompts": prompts::prompts_list() })),
            "prompts/get" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let pargs = params.get("arguments").cloned().unwrap_or(Value::Null);
                match prompts::prompts_get(name, &pargs) {
                    Some(v) => Ok(v),
                    None => Err((-32602, format!("Unknown prompt: {name}"))),
                }
            }
```

- [ ] **Step 5: Green, lint, skill-drift, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
cargo test -p codegraph-skillgen
git add crates/codegraph-server/src/prompts.rs crates/codegraph-server/src/lib.rs
git commit -m "feat(server): add MCP prompts capability with onboarding workflows"
```

---

## Task 8: Result pagination (offset/limit)

**Why:** `get_community` and `god_nodes` can dump unbounded lists that blow the context window. Add `offset`/`limit` with a "showing N of M" footer so an agent can page.

**Files:**
- Modify: `crates/codegraph-server/src/lib.rs`

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn get_community_paginates() {
    // 6 members in community 0.
    let nodes: Vec<_> = (0..6).map(|i| {
        let id: &'static str = Box::leak(format!("n{i}").into_boxed_str());
        let lbl: &'static str = Box::leak(format!("N{i}").into_boxed_str());
        node(id, lbl, Some(0))
    }).collect();
    let gd = GraphData { nodes, ..Default::default() };
    let mut s = Server::from_graph_data(gd, None);
    let out = call_tool(&mut s, "get_community", json!({"community_id":0,"offset":2,"limit":2}));
    assert!(out.contains("showing 2 of 6"), "footer: {out}");
}
```

- [ ] **Step 2: Run red.**

```bash
cargo test -p codegraph-server get_community_paginates
```

Expected: FAIL (no footer; offset/limit ignored).

- [ ] **Step 3: Implement.**

Change `tool_get_community` to take optional `offset`/`limit`:

```rust
    pub fn tool_get_community(&self, community_id: u32, offset: usize, limit: usize) -> String {
        let Some(ids) = self.communities.get(&community_id).filter(|v| !v.is_empty()) else {
            return format!("No community {community_id}.");
        };
        let total = ids.len();
        let end = offset.saturating_add(limit).min(total);
        let page = if offset >= total { &[][..] } else { &ids[offset..end] };
        let mut out = format!("Community {community_id} (showing {} of {total}):", page.len());
        for id in page {
            if let Some(n) = self.kg.node(id) {
                out.push_str(&format!("\n  {} [{}]", sanitize_label(&n.label), sanitize_label(&n.source_file)));
            }
        }
        out
    }
```

Update the dispatch arm:

```rust
            "get_community" => self.tool_get_community(
                u("community_id", 0) as u32,
                u("offset", 0) as usize,
                u("limit", 100) as usize,
            ),
```

Fix the existing `tools_return_expected_text` call (`get_community` now takes 3 args via the tool, but `call_tool` passes JSON, so only the arm changed; the test still calls with `json!({"community_id":0})` and gets defaults). Verify that assertion still passes.

Add `offset`/`limit` to the `get_community` inputSchema, and apply the same offset/limit to `god_nodes` (`top_n` already caps; add `offset`).

- [ ] **Step 4: Green, lint, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
git add crates/codegraph-server/src/lib.rs
git commit -m "feat(server): paginate get_community and god_nodes with offset/limit"
```

---

## Task 9: Real token budgeting (tiktoken-rs)

**Why:** `truncate_to_tokens` uses `chars/4`, which over- or under-counts badly for code. Use a real BPE tokenizer so `token_budget` actually means tokens.

**Files:**
- Modify: `crates/codegraph-server/Cargo.toml`, `crates/codegraph-server/src/lib.rs`
- Modify: root `Cargo.toml` (workspace dep) if deps are centralized there

- [ ] **Step 1: Add the dependency.**

In `crates/codegraph-server/Cargo.toml` under `[dependencies]`:

```toml
tiktoken-rs = "0.6"
```

(`cl100k_base` ranks are embedded in the crate; no network at runtime. Confirm it builds on Linux, macOS, and Windows in CI, Task 16.)

- [ ] **Step 2: Write the failing test.**

```rust
#[test]
fn truncate_uses_real_token_count() {
    // A long ASCII body; budget of 5 tokens must cut it well under chars/4.
    let body = "alpha beta gamma delta epsilon zeta eta theta iota kappa ".repeat(20);
    let out = truncate_to_tokens(body.clone(), 5);
    assert!(out.contains("truncated"), "should truncate: {out}");
    // The kept prefix must be at most ~5 tokens worth, far shorter than chars/4.
    assert!(out.len() < body.len() / 2, "real tokenizer should cut hard: {}", out.len());
}
```

- [ ] **Step 3: Run red.**

```bash
cargo test -p codegraph-server truncate_uses_real_token_count
```

Expected: with the old `chars*4` cap, 5 tokens -> 20 chars cap, which already truncates; this test may pass trivially. Strengthen it to assert the kept token count:

```rust
    let kept = out.split("\n").next().unwrap();
    let bpe = tiktoken_rs::cl100k_base().unwrap();
    assert!(bpe.encode_with_special_tokens(kept).len() <= 6, "kept ~5 tokens");
```

Now it FAILs against the char heuristic.

- [ ] **Step 4: Implement.**

Replace `truncate_to_tokens`:

```rust
/// Truncate `text` to at most `token_budget` real tokens (cl100k_base),
/// appending a note when cut. Falls back to a char heuristic if the tokenizer
/// is somehow unavailable.
fn truncate_to_tokens(text: String, token_budget: usize) -> String {
    let Ok(bpe) = tiktoken_rs::cl100k_base() else {
        let cap = token_budget.saturating_mul(4).min(text.len());
        let mut end = cap;
        while end > 0 && !text.is_char_boundary(end) { end -= 1; }
        return if end < text.len() {
            format!("{}\n... (truncated to ~{token_budget} tokens)", &text[..end])
        } else { text };
    };
    let toks = bpe.encode_with_special_tokens(&text);
    if toks.len() <= token_budget {
        return text;
    }
    let kept = bpe.decode(toks[..token_budget].to_vec()).unwrap_or_default();
    format!("{kept}\n... (truncated to ~{token_budget} tokens)")
}
```

- [ ] **Step 5: Green, lint, full-workspace build (new dep), commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
cargo build --workspace --all-features
git add crates/codegraph-server/Cargo.toml crates/codegraph-server/src/lib.rs Cargo.lock
git commit -m "feat(server): budget query_graph output with a real tokenizer"
```

---

## Task 10: Resource templates

**Why:** All six resources are static URIs. Templates let a client address any node or community as a resource (`codegraph://node/{label}`), which hosts surface as browsable resources.

**Files:**
- Modify: `crates/codegraph-server/src/lib.rs`

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn resource_templates_listed_and_readable() {
    let mut s = server();
    let tl = s.handle_request(&json!({"jsonrpc":"2.0","id":1,"method":"resources/templates/list"})).unwrap();
    let templates = tl["result"]["resourceTemplates"].as_array().unwrap();
    assert!(templates.iter().any(|t| t["uriTemplate"] == "codegraph://node/{label}"));

    let read = s.handle_request(&json!({
        "jsonrpc":"2.0","id":2,"method":"resources/read",
        "params":{"uri":"codegraph://node/AuthService"}
    })).unwrap();
    assert!(read["result"]["contents"][0]["text"].as_str().unwrap().contains("AuthService"));
}
```

- [ ] **Step 2: Run red.**

```bash
cargo test -p codegraph-server resource_templates_listed_and_readable
```

Expected: FAIL (method-not-found for templates/list; unknown-resource error for the templated URI).

- [ ] **Step 3: Implement.**

Add the method arm in `dispatch_request`:

```rust
            "resources/templates/list" => Ok(json!({ "resourceTemplates": resource_templates() })),
```

Add the templates list:

```rust
fn resource_templates() -> Value {
    json!([
        { "uriTemplate": "codegraph://node/{label}", "name": "Node", "mimeType": "text/plain",
          "description": "Metadata for one node by label/id/bare name." },
        { "uriTemplate": "codegraph://community/{id}", "name": "Community", "mimeType": "text/plain",
          "description": "Members of one community by id." }
    ])
}
```

In `dispatch_resource`, handle the templated URIs before the `other =>` arm:

```rust
        if let Some(label) = uri.strip_prefix("codegraph://node/") {
            return Ok(json!({ "contents": [{ "uri": uri, "mimeType": "text/plain",
                "text": self.tool_get_node(label) }] }));
        }
        if let Some(id) = uri.strip_prefix("codegraph://community/") {
            let cid: u32 = id.parse().unwrap_or(u32::MAX);
            return Ok(json!({ "contents": [{ "uri": uri, "mimeType": "text/plain",
                "text": self.tool_get_community(cid, 0, 1000) }] }));
        }
```

- [ ] **Step 4: Green, lint, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
git add crates/codegraph-server/src/lib.rs
git commit -m "feat(server): add resource templates for node and community URIs"
```

---

## Task 11: Resource subscriptions (live updates over SSE)

**Why:** The SSE channel (`GET /mcp`) is a heartbeat with nothing to push, yet the server already detects when `graph.json` changes (`is_stale`). Wire a subscription so a hot-reload pushes `notifications/resources/updated` to subscribed HTTP sessions.

**Files:**
- Modify: `crates/codegraph-server/src/session.rs` (per-session broadcast channel)
- Modify: `crates/codegraph-server/src/http.rs` (subscribe arm, SSE pushes notifications, reload broadcasts)
- Modify: `crates/codegraph-server/src/lib.rs` (`resources/subscribe`/`unsubscribe` accepted; advertise `resources.subscribe` capability)

- [ ] **Step 1: Write the failing test** (in `session.rs` tests) for a notify channel:

```rust
#[test]
fn session_broadcast_delivers_to_subscriber() {
    let s = SessionStore::new();
    let id = s.create();
    let mut rx = s.subscribe(&id).expect("a live session subscribes");
    s.notify_all_resources_changed();
    let got = rx.try_recv();
    assert!(got.is_ok(), "subscriber receives the change notification");
}
```

- [ ] **Step 2: Run red.**

```bash
cargo test -p codegraph-server session_broadcast_delivers_to_subscriber
```

Expected: FAIL (`subscribe`/`notify_all_resources_changed` do not exist).

- [ ] **Step 3: Implement the channel in `session.rs`.**

Hold an optional `tokio::sync::broadcast::Sender<()>` per session. Add:

```rust
// In SessionStore, change the inner map value to carry a broadcast sender:
//   Mutex<HashMap<String, (Instant, broadcast::Sender<()>)>>
// (Adjust create_at/touch_at/reap accordingly; the Instant stays the activity key.)

use tokio::sync::broadcast;

pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<()>> {
    let map = self.guard();
    map.get(id).map(|(_, tx)| tx.subscribe())
}

pub fn notify_all_resources_changed(&self) {
    for (_, tx) in self.guard().values() {
        let _ = tx.send(()); // ignore "no receivers"
    }
}
```

When creating a session: `let (tx, _) = broadcast::channel(8);` and store `(now, tx)`.

- [ ] **Step 4: Push notifications on the SSE stream (`http.rs`).**

In `handle_sse`, when a tracked session exists, select between the keep-alive tick and `sessions.subscribe(id)`; on a broadcast signal, emit an SSE `Event` whose data is the JSON-RPC notification:

```rust
// notification frame:
let note = json!({ "jsonrpc":"2.0","method":"notifications/resources/updated",
                   "params": { "uri": "codegraph://stats" } }).to_string();
// yield Event::default().data(note) when the broadcast fires.
```

In `handle_post`, after a successful `maybe_reload` that actually changed the graph, call `st.sessions.notify_all_resources_changed()`. (Have `maybe_reload` or the stale-check return whether it reloaded so you only notify on real change.)

- [ ] **Step 5: Accept subscribe/unsubscribe + advertise capability (`lib.rs`).**

Add dispatch arms that acknowledge subscriptions (the actual push is the transport's job):

```rust
            "resources/subscribe" | "resources/unsubscribe" => Ok(json!({})),
```

Advertise: `"resources": { "subscribe": true }` in the `initialize` capabilities.

- [ ] **Step 6: Green, lint, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
git add crates/codegraph-server/src/session.rs crates/codegraph-server/src/http.rs crates/codegraph-server/src/lib.rs
git commit -m "feat(server): push resource-updated notifications on graph reload"
```

---
---

# TIER 3

## Task 12: `working_changes_impact` tool (git diff blast radius, no gh)

**Why:** The PR tools require `gh`. Generalize them: diff the working tree against a base ref with `git` and run the same blast-radius computation locally, covering the "before I commit" moment.

**Files:**
- Modify: `crates/codegraph-server/src/lib.rs`

- [ ] **Step 1: Write the failing test** using the injectable `CommandRunner`:

```rust
#[test]
fn working_changes_impact_uses_git_diff() {
    struct GitRunner;
    impl CommandRunner for GitRunner {
        fn run(&self, program: &str, args: &[&str]) -> Option<String> {
            if program == "git" && args.first() == Some(&"diff") {
                return Some("auth.py\n".to_string()); // changed file
            }
            None
        }
    }
    // node "auth" lives in auth.py, community 0.
    let mut a = node("auth", "AuthService", Some(0));
    a.source_file = "auth.py".into();
    let gd = GraphData { nodes: vec![a], ..Default::default() };
    let s = Server::from_graph_data(gd, None).with_runner(Box::new(GitRunner));
    let out = s.tool_working_changes_impact(Some("main"));
    assert!(out.contains("auth.py"), "names the changed file: {out}");
    assert!(out.contains("communities") || out.contains("nodes"), "reports impact: {out}");
}
```

- [ ] **Step 2: Run red.**

```bash
cargo test -p codegraph-server working_changes_impact_uses_git_diff
```

Expected: FAIL (`tool_working_changes_impact` undefined).

- [ ] **Step 3: Implement** reusing `compute_pr_impact` (already used by `graph_impact`):

```rust
    /// `working_changes_impact` - graph blast radius of the working-tree diff
    /// against `base` (default: the detected default branch). Uses `git`, not
    /// `gh`, so it works offline.
    pub fn tool_working_changes_impact(&self, base: Option<&str>) -> String {
        let base = base.map(str::to_string)
            .unwrap_or_else(|| detect_default_branch(&*self.runner, None));
        let diff = self.runner.run("git", &["diff", "--name-only", &base]);
        let files: Vec<String> = diff.unwrap_or_default()
            .lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
        if files.is_empty() {
            return format!("No changes vs {base} (or git unavailable).");
        }
        let (comms, nodes) = self.graph_impact(&files);
        let mut out = format!(
            "Working changes vs {base}: {} files, {} graph nodes, {} communities touched",
            files.len(), nodes, comms.len()
        );
        for f in &files {
            out.push_str(&format!("\n  {}", sanitize_label(f)));
        }
        out
    }
```

- [ ] **Step 4: Dispatch arm + tools_list + count.**

```rust
            "working_changes_impact" => self.tool_working_changes_impact(opt("base")),
```

tools_list entry (set `openWorldHint: true` via the `open_world` list in Task 5's refactor by adding `"working_changes_impact"` to it):

```rust
        { "name": "working_changes_impact", "description": "Graph blast radius of your uncommitted working-tree changes against a base branch (uses git, no gh needed). Shows which graph nodes and communities your edits touch before you commit.",
          "inputSchema": { "type": "object", "properties": { "base": { "type": "string", "description": "Base branch to diff against (default: the repo default branch)." } } } },
```

Bump the count test to `17` and add `"working_changes_impact"`.

- [ ] **Step 5: Green, lint, skill-drift, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
cargo test -p codegraph-skillgen
git add crates/codegraph-server/src/lib.rs
git commit -m "feat(server): add working_changes_impact tool (git diff blast radius)"
```

---

## Task 13: `completion/complete` capability

**Why:** Argument autocomplete (repo tags, community ids, node labels) improves agent and human UX in MCP hosts that surface completions.

**Files:**
- Modify: `crates/codegraph-server/src/lib.rs`

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn completion_completes_node_labels() {
    let mut s = server(); // has AuthService, login_user, Database
    let r = s.handle_request(&json!({
        "jsonrpc":"2.0","id":1,"method":"completion/complete",
        "params":{ "ref": {"type":"ref/resource","uri":"codegraph://node/{label}"},
                   "argument": {"name":"label","value":"Auth"} }
    })).unwrap();
    let values: Vec<&str> = r["result"]["completion"]["values"].as_array().unwrap()
        .iter().map(|v| v.as_str().unwrap()).collect();
    assert!(values.contains(&"AuthService"), "{values:?}");
}
```

- [ ] **Step 2: Run red.**

```bash
cargo test -p codegraph-server completion_completes_node_labels
```

Expected: FAIL (method-not-found).

- [ ] **Step 3: Implement.**

Add the dispatch arm:

```rust
            "completion/complete" => self.dispatch_completion(&params),
```

```rust
    fn dispatch_completion(&self, params: &Value) -> Result<Value, (i64, String)> {
        let arg_name = params.get("argument").and_then(|a| a.get("name")).and_then(Value::as_str).unwrap_or("");
        let prefix = params.get("argument").and_then(|a| a.get("value")).and_then(Value::as_str).unwrap_or("");
        let plow = prefix.to_lowercase();
        let mut values: Vec<String> = match arg_name {
            "label" | "source" | "target" => self.kg.nodes()
                .filter(|n| n.label.to_lowercase().starts_with(&plow))
                .map(|n| sanitize_label(&n.label)).collect(),
            "repo" => self.kg.nodes().filter_map(|n| n.repo.clone())
                .filter(|r| r.to_lowercase().starts_with(&plow)).map(|r| sanitize_label(&r)).collect(),
            "community_id" => self.communities.keys().map(|c| c.to_string())
                .filter(|c| c.starts_with(prefix)).collect(),
            _ => Vec::new(),
        };
        values.sort();
        values.dedup();
        let total = values.len();
        values.truncate(100); // protocol caps at 100
        Ok(json!({ "completion": { "values": values, "total": total, "hasMore": total > 100 } }))
    }
```

Advertise `"completions": {}` in the `initialize` capabilities.

- [ ] **Step 4: Green, lint, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
git add crates/codegraph-server/src/lib.rs
git commit -m "feat(server): add completion/complete for labels, repos, communities"
```

---

## Task 14: `logging` capability + log notifications

**Why:** Query telemetry is file-only (`CODEGRAPH_QUERY_LOG`). Advertise `logging`, accept `logging/setLevel`, and emit `notifications/message` so hosts can show server activity.

**Files:**
- Modify: `crates/codegraph-server/src/lib.rs` (capability + `logging/setLevel`)
- Modify: `crates/codegraph-server/src/http.rs` (carry log notifications on SSE, reusing the Task 11 broadcast)

- [ ] **Step 1: Write the failing test.**

```rust
#[test]
fn logging_set_level_acknowledged() {
    let mut s = server();
    let r = s.handle_request(&json!({
        "jsonrpc":"2.0","id":1,"method":"logging/setLevel","params":{"level":"info"}
    })).unwrap();
    assert!(r.get("error").is_none(), "setLevel should succeed: {r}");
    assert_eq!(r["result"], json!({}));
}
```

- [ ] **Step 2: Run red, then implement.**

```bash
cargo test -p codegraph-server logging_set_level_acknowledged
```

Add the arm and capability:

```rust
            "logging/setLevel" => Ok(json!({})),
```

Advertise `"logging": {}` in `initialize` capabilities. (The minimum bar is accept-and-ack; pushing `notifications/message` over SSE reuses the Task 11 broadcast plumbing and can be a follow-on once subscriptions land.)

- [ ] **Step 3: Green, lint, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
git add crates/codegraph-server/src/lib.rs
git commit -m "feat(server): advertise logging capability and accept setLevel"
```

---

## Task 15 (optional): Opt-in write tools behind `--allow-edits`

**Why:** Serena and code-graph-rag edit code. CodeGraph's read-only default is a good security posture; this adds opt-in symbol-level editing, OFF by default, annotated as non-read-only and destructive, path-jailed.

**Files:**
- Modify: `bin/codegraph/src/cli.rs` (`--allow-edits` on `Serve`), `bin/codegraph/src/commands/serve.rs`
- Modify: `crates/codegraph-server/src/lib.rs` (gated `replace_symbol_body`, `insert_after_symbol`)

- [ ] **Step 1: Write the failing test** (edits only appear when enabled):

```rust
#[test]
fn write_tools_absent_unless_enabled() {
    let s_ro = server();
    assert!(!tools_list_for(&s_ro).iter().any(|n| n == "replace_symbol_body"));
    let s_rw = server().with_edits_enabled(true);
    assert!(tools_list_for(&s_rw).iter().any(|n| n == "replace_symbol_body"));
}
```

(Add a small `tools_list_for(&Server) -> Vec<String>` test helper that calls the server's tool listing; make `tools_list` take the edit flag, see Step 3.)

- [ ] **Step 2: Run red.**

```bash
cargo test -p codegraph-server write_tools_absent_unless_enabled
```

Expected: FAIL (`with_edits_enabled`/gated tools do not exist).

- [ ] **Step 3: Implement.**

Add `allow_edits: bool` to `Server` (default `false`) and `with_edits_enabled(bool)`. Make `tools_list` a method `self.tools_list()` (or pass the flag) so it appends the write tools only when `allow_edits`. Implement:

```rust
    /// `replace_symbol_body` - overwrite the file lines for a symbol. Jailed to
    /// the source root; rejects when edits are disabled.
    pub fn tool_replace_symbol_body(&self, label: &str, new_lines: usize, body: &str) -> String {
        if !self.allow_edits { return "Edits are disabled (start the server with --allow-edits).".into(); }
        // resolve node -> jailed path -> read -> splice [start .. start+new_lines) -> write.
        // Use resolve_source_path; reject when source_location is None.
        // (Full splice implementation here; write atomically via a temp file + rename.)
        // Return a unified summary of the change.
        todo_replace_impl(self, label, new_lines, body)
    }
```

Annotate the write tools `{ "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }`. Wire `--allow-edits` through `run_serve`.

> NOTE: this task is intentionally last and optional. Do NOT enable it by default. If the team prefers to keep the server strictly read-only, skip Task 15 entirely; nothing else depends on it.

- [ ] **Step 4: Green, lint, e2e, commit.**

```bash
cargo test -p codegraph-server
cargo clippy -p codegraph-server --all-targets --all-features -- -D warnings
git add crates/codegraph-server/src/lib.rs bin/codegraph/src/cli.rs bin/codegraph/src/commands/serve.rs
git commit -m "feat(server): optional opt-in symbol-edit tools behind --allow-edits"
```

---
---

# CROSS-CUTTING

## Task 16: MCP e2e conformance test + CI job

**Why:** Unit tests cover the dispatcher in-process. This proves the real `codegraph serve` binary speaks the protocol over stdio end to end (initialize -> tools/list -> tools/call with structuredContent), and a dedicated CI job keeps it honest on all three OSes.

**Files:**
- Create: `bin/codegraph/tests/mcp_e2e.rs`
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Write the e2e test** driving the binary over stdio.

```rust
//! End-to-end: extract a tiny corpus, start `codegraph serve` on stdio, and run
//! an MCP handshake + tool call, asserting structured output comes back.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn mcp_stdio_handshake_and_structured_tool_call() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.py"),
        "def run_analysis(d):\n    return compute(d)\n\n\ndef compute(d):\n    return sum(d)\n",
    ).unwrap();

    // Build the graph.
    assert_cmd::Command::cargo_bin("codegraph").unwrap()
        .arg("extract").arg(root).assert().success();

    // Launch the server on stdio.
    let mut child = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .current_dir(root).arg("serve")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());

    let send = |stdin: &mut std::process::ChildStdin, v: serde_json::Value| {
        writeln!(stdin, "{v}").unwrap(); stdin.flush().unwrap();
    };
    let mut recv = |out: &mut BufReader<std::process::ChildStdout>| -> serde_json::Value {
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    send(&mut stdin, serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2025-06-18"}}));
    let init = recv(&mut out);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

    send(&mut stdin, serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"graph_stats","arguments":{}}}));
    let call = recv(&mut out);
    assert!(call["result"]["structuredContent"]["nodes"].as_u64().unwrap() >= 1,
        "structured graph_stats: {call}");

    drop(stdin); // EOF ends the stdio loop
    let _ = child.wait();
}
```

- [ ] **Step 2: Run it.**

```bash
cargo test -p codegraph --test mcp_e2e
```

Expected: PASS (depends on Tasks 5 and 6 being merged for protocol + structuredContent).

- [ ] **Step 3: Add a CI job.**

In `.github/workflows/ci.yml`, add after the `test` job:

```yaml
  mcp-conformance:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@1.95.0
      - run: cargo test -p codegraph --test mcp_e2e --all-features
```

(The existing `test` job already runs every new unit test via `cargo test --workspace --all-features`; this job isolates the protocol e2e so a regression is unambiguous, and proves the new `tiktoken-rs` dependency builds and runs on macOS and Windows.)

- [ ] **Step 4: Commit.**

```bash
git add bin/codegraph/tests/mcp_e2e.rs .github/workflows/ci.yml
git commit -m "test(server): add MCP stdio conformance e2e and CI job"
```

---

## Final verification (run before opening the PR)

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo run -p codegraph -- skill check   # tool-surface drift guard
```

All four must be clean. The CI matrix (`lint`, `test` x3 OSes, `extract-langs` x33, `mcp-conformance` x3 OSes) reproduces this on every push.

---

## Self-Review (completed against the Tier 1/2/3 spec)

- **Tier 1** — get_source (Task 2), affected (Task 3), find_callers/find_callees (Task 4), protocol 2025-06-18 + annotations (Task 5), structured output (Task 6): covered. Foundation (Task 1) added for source reading.
- **Tier 2** — prompts (7), pagination (8), token budgeting (9), resource templates (10), subscriptions (11): covered.
- **Tier 3** — working_changes_impact (12), completions (13), logging (14), opt-in edits (15): covered.
- **CI/CD** — per-task gate, full pre-merge gate, dedicated e2e job across 3 OSes (16): covered.
- **Type/name consistency** — `with_source_root`/`resolve_source_path`/`source::resolve_in_root`/`source::parse_line_marker`, `tool_get_source`, `tool_affected`, `tool_find_callers`/`tool_find_callees`, `negotiate_protocol`, `stats_json`/`god_nodes_json`/`affected_json`/`query_graph_json`, `tool_working_changes_impact`, `with_edits_enabled`/`allow_edits`: used consistently across tasks.
- **Tool-count assertion** — bumped explicitly in Tasks 2 (13), 3 (14), 4 (16), 12 (17); Task 15's write tools are conditional and excluded from the base count.
```

