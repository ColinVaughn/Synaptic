//! Cross-language resolution: retarget subprocess `invokes` edges from a command
//! stub to a matching in-repo file node -- e.g. a Python `subprocess.run("tool")`
//! linking to the Rust binary source `src/bin/tool.rs`. Runs after the graph is
//! built, over the full node set, so it can see targets from other files.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Map;
use synaptic_core::{Confidence, Edge, Node, NodeId, NodeKind};

/// Retarget command-stub `invokes` edges to a unique matching in-repo file node,
/// dropping the now-orphan stub. A command resolves when exactly one file node
/// shares its basename or stem; an ambiguous or absent match stays unresolved
/// (the stub and edge are left as-is).
pub fn resolve_command_invocations(nodes: Vec<Node>, edges: Vec<Edge>) -> (Vec<Node>, Vec<Edge>) {
    // Command stubs: external nodes tagged `_node_type == "command"`.
    let mut stubs: HashMap<NodeId, String> = HashMap::new();
    for n in &nodes {
        if n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("command") {
            stubs.insert(n.id.clone(), n.label.clone());
        }
    }
    if stubs.is_empty() {
        return (nodes, edges);
    }

    // Index file nodes (label == basename of their source_file) by basename and
    // stem; `None` marks an ambiguous key (more than one distinct owner).
    let mut by_key: HashMap<String, Option<NodeId>> = HashMap::new();
    for n in &nodes {
        if stubs.contains_key(&n.id) || n.source_file.is_empty() {
            continue;
        }
        let base = Path::new(&n.source_file)
            .file_name()
            .and_then(|s| s.to_str());
        if base != Some(n.label.as_str()) {
            continue; // not a file node
        }
        index_key(&mut by_key, n.label.clone(), &n.id);
        if let Some(stem) = Path::new(&n.label).file_stem().and_then(|s| s.to_str()) {
            index_key(&mut by_key, stem.to_string(), &n.id);
        }
    }

    // Resolve each stub to a unique file node by basename, else stem.
    let mut resolved: HashMap<NodeId, NodeId> = HashMap::new();
    for (sid, cmd) in &stubs {
        let cmd_stem = Path::new(cmd)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(cmd);
        let hit = by_key
            .get(cmd.as_str())
            .and_then(|o| o.clone())
            .or_else(|| by_key.get(cmd_stem).and_then(|o| o.clone()));
        if let Some(real) = hit {
            resolved.insert(sid.clone(), real);
        }
    }
    if resolved.is_empty() {
        return (nodes, edges);
    }

    let edges = edges
        .into_iter()
        .map(|mut e| {
            if let Some(real) = resolved.get(&e.target) {
                e.target = real.clone();
            }
            e
        })
        .collect();
    let nodes = nodes
        .into_iter()
        .filter(|n| !resolved.contains_key(&n.id))
        .collect();
    (nodes, edges)
}

/// Record `id` under `key`, marking the key ambiguous (`None`) if a different
/// node already claims it.
fn index_key(map: &mut HashMap<String, Option<NodeId>>, key: String, id: &NodeId) {
    map.entry(key)
        .and_modify(|e| {
            if e.as_ref() != Some(id) {
                *e = None;
            }
        })
        .or_insert_with(|| Some(id.clone()));
}

/// Merge concrete client route nodes into the parameterized (template) server
/// route they match, so a client call to `/users/7` connects to the `/users/{id}`
/// handler. Route nodes are tagged `_node_type == "route"`; a path is a template
/// when any segment is a parameter (`{id}`, `:id`, `<int:id>`, `*rest`). A concrete
/// path resolves only when exactly one template matches -- ambiguous or unmatched
/// concretes are left untouched. Edges pointing at a merged concrete node are
/// retargeted to its template; the concrete node is dropped.
pub fn resolve_parameterized_routes(nodes: Vec<Node>, edges: Vec<Edge>) -> (Vec<Node>, Vec<Edge>) {
    let routes: Vec<(NodeId, String)> = nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| (n.id.clone(), n.label.clone()))
        .collect();
    // Only templates with a literal anchor segment are matchable -- a bare `*` or
    // `/{id}` (no literal) would otherwise swallow every concrete route.
    let templates: Vec<&(NodeId, String)> = routes
        .iter()
        .filter(|(_, p)| is_matchable_template(p))
        .collect();
    if templates.is_empty() {
        return (nodes, edges);
    }

    // Route nodes that are the source of a `handled_by` edge are SERVER routes
    // with their own handler; never merge those away (a literal `/users/me` must
    // not collapse into the `/users/{id}` template).
    let server_routes: HashSet<NodeId> = edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| e.source.clone())
        .collect();

    // Each concrete client path resolves to its unique matching template, if any.
    let mut resolved: HashMap<NodeId, NodeId> = HashMap::new();
    for (cid, cpath) in &routes {
        if is_template(cpath) || server_routes.contains(cid) {
            continue;
        }
        let mut hits = templates
            .iter()
            .filter(|(tid, tpath)| tid != cid && path_matches(cpath, tpath));
        if let Some((tid, _)) = hits.next() {
            if hits.next().is_none() {
                resolved.insert(cid.clone(), tid.clone());
            }
        }
    }
    if resolved.is_empty() {
        return (nodes, edges);
    }

    let edges = edges
        .into_iter()
        .filter_map(|mut e| {
            let mut remapped = false;
            if let Some(t) = resolved.get(&e.source) {
                e.source = t.clone();
                remapped = true;
            }
            if let Some(t) = resolved.get(&e.target) {
                e.target = t.clone();
                remapped = true;
            }
            // Drop a self-loop the merge produced (e.g. a stray route -> route edge).
            if remapped && e.source == e.target {
                return None;
            }
            Some(e)
        })
        .collect();
    let nodes = nodes
        .into_iter()
        .filter(|n| !resolved.contains_key(&n.id))
        .collect();
    (nodes, edges)
}

/// A path is a template if any of its `/`-segments is a parameter placeholder.
fn is_template(path: &str) -> bool {
    path.split('/').any(is_param_segment)
}

/// A template is *matchable* only if it has at least one non-empty literal
/// segment to anchor on, alongside a parameter. A bare `*`, `/{id}`, or `/<x>`
/// (all-parameter) would otherwise match arbitrary concrete paths.
fn is_matchable_template(path: &str) -> bool {
    let mut has_param = false;
    let mut has_literal = false;
    for seg in path.split('/') {
        if seg.is_empty() {
            continue;
        }
        if is_param_segment(seg) {
            has_param = true;
        } else {
            has_literal = true;
        }
    }
    has_param && has_literal
}

/// `{id}` / `{id:int}` / `:id` / `<int:id>` / `*rest` style parameter segments.
fn is_param_segment(seg: &str) -> bool {
    (seg.starts_with('{') && seg.ends_with('}'))
        || seg.starts_with(':')
        || (seg.starts_with('<') && seg.ends_with('>'))
        || seg.starts_with('*')
}

/// A trailing catch-all segment (`*rest`, `{*rest}`) matches any number of
/// remaining path segments.
fn is_catch_all_segment(seg: &str) -> bool {
    seg.starts_with('*') || seg.starts_with("{*")
}

/// Whether concrete path `c` matches template `t`: equal segment count with each
/// template segment either a parameter or a literal equal to the concrete one --
/// or, if `t` ends in a catch-all, the leading segments match and `c` is at least
/// as deep.
fn path_matches(c: &str, t: &str) -> bool {
    let cs: Vec<&str> = c.split('/').collect();
    let ts: Vec<&str> = t.split('/').collect();
    if ts.last().is_some_and(|s| is_catch_all_segment(s)) {
        if cs.len() < ts.len() {
            return false;
        }
        return ts[..ts.len() - 1]
            .iter()
            .zip(cs.iter())
            .all(|(tseg, cseg)| seg_match(cseg, tseg));
    }
    cs.len() == ts.len()
        && cs
            .iter()
            .zip(ts.iter())
            .all(|(cseg, tseg)| seg_match(cseg, tseg))
}

/// A concrete segment matches a template segment if the template segment is a
/// parameter, or the two are identical literals.
fn seg_match(c: &str, t: &str) -> bool {
    is_param_segment(t) || c == t
}

/// Connect Python importers of a native module to its PyO3 boundary: a Rust
/// `#[pymodule] fn mymod` emits a `pyo3_module` boundary node `pyo3:mymod`; a
/// Python `import mymod` / `from mymod import ..` emits an `imports`/`imports_from`
/// edge to a module stub labeled `mymod`. When the names match, add a
/// `calls_service` edge from the importer to the boundary, so reverse impact
/// reaches from the Rust impl (boundary `handled_by` impl) to the Python file.
/// Matching is by exact module name, so an unrelated pure-Python import does not
/// connect.
pub fn resolve_pyo3_imports(nodes: Vec<Node>, edges: Vec<Edge>) -> (Vec<Node>, Vec<Edge>) {
    // module name -> boundary id, from `pyo3_module` nodes labeled `pyo3:<module>`.
    let mut boundaries: HashMap<&str, &NodeId> = HashMap::new();
    for n in &nodes {
        if n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("pyo3_module") {
            if let Some(m) = n.label.strip_prefix("pyo3:") {
                if !m.is_empty() {
                    boundaries.insert(m, &n.id);
                }
            }
        }
    }
    if boundaries.is_empty() {
        return (nodes, edges);
    }

    let label_of: HashMap<&NodeId, &str> =
        nodes.iter().map(|n| (&n.id, n.label.as_str())).collect();
    let mut new_edges: Vec<Edge> = Vec::new();
    let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();
    for e in &edges {
        if e.relation != "imports" && e.relation != "imports_from" {
            continue;
        }
        let Some(target_label) = label_of.get(&e.target) else {
            continue;
        };
        let Some(&boundary) = boundaries.get(target_label) else {
            continue;
        };
        if boundary == &e.source || !seen.insert((e.source.clone(), boundary.clone())) {
            continue;
        }
        new_edges.push(Edge {
            source: e.source.clone(),
            target: boundary.clone(),
            relation: "calls_service".to_string(),
            confidence: Confidence::Inferred,
            source_file: e.source_file.clone(),
            source_location: None,
            confidence_score: Some(Confidence::Inferred.default_score()),
            weight: 1.0,
            context: Some("pyo3".to_string()),
            cross_repo: false,
            extra: Map::new(),
        });
    }
    if new_edges.is_empty() {
        return (nodes, edges);
    }
    let mut edges = edges;
    edges.extend(new_edges);
    (nodes, edges)
}

/// Stitch each PyO3 module boundary to the definitions it registers, matched by
/// name across files. A `#[pymodule]` boundary node (`_node_type == "pyo3_module"`)
/// carries `_pyo3_registers` (the Rust symbol names it registers); a
/// `#[pyfunction]`/`#[pyclass]` definition node is tagged `_pyo3_export`. This adds
/// a `handled_by` edge boundary -> definition even when the module and the
/// definition live in different files (the case a per-file scan cannot resolve).
/// Matching is by exact name; an ambiguous name (defined in two files) is skipped.
pub fn resolve_pyo3_modules(nodes: Vec<Node>, edges: Vec<Edge>) -> (Vec<Node>, Vec<Edge>) {
    // Exportable definitions by name (`None` marks an ambiguous name).
    let mut exports: HashMap<String, Option<NodeId>> = HashMap::new();
    for n in &nodes {
        if n.extra.contains_key("_pyo3_export") {
            let name = pyo3_node_name(&n.label).to_string();
            index_key(&mut exports, name, &n.id);
        }
    }
    if exports.is_empty() {
        return (nodes, edges);
    }

    let mut existing: HashSet<(NodeId, NodeId)> = edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| (e.source.clone(), e.target.clone()))
        .collect();
    let mut new_edges: Vec<Edge> = Vec::new();
    for n in &nodes {
        if n.extra.get("_node_type").and_then(|v| v.as_str()) != Some("pyo3_module") {
            continue;
        }
        let Some(regs) = n.extra.get("_pyo3_registers").and_then(|v| v.as_array()) else {
            continue;
        };
        for reg in regs {
            let Some(name) = reg.as_str() else { continue };
            let Some(Some(target)) = exports.get(name) else {
                continue;
            };
            if existing.insert((n.id.clone(), target.clone())) {
                new_edges.push(Edge {
                    source: n.id.clone(),
                    target: target.clone(),
                    relation: "handled_by".to_string(),
                    confidence: Confidence::Inferred,
                    source_file: String::new(),
                    source_location: None,
                    confidence_score: Some(Confidence::Inferred.default_score()),
                    weight: 1.0,
                    context: Some("pyo3".to_string()),
                    cross_repo: false,
                    extra: Map::new(),
                });
            }
        }
    }
    if new_edges.is_empty() {
        return (nodes, edges);
    }
    let mut edges = edges;
    edges.extend(new_edges);
    (nodes, edges)
}

/// The bare identifier of a definition node label: drop a leading `.`, take the
/// part before `(`, then the last whitespace-delimited word, then drop any
/// `Type::` qualifier and `<..>` generics. `add()` -> `add`, `Widget` -> `Widget`,
/// `pub fn serve()` -> `serve`.
fn pyo3_node_name(label: &str) -> &str {
    let l = label.trim_start_matches('.');
    let before_paren = l.split('(').next().unwrap_or(l);
    let last_word = before_paren
        .split_whitespace()
        .last()
        .unwrap_or(before_paren);
    let after_colon = last_word.rsplit("::").next().unwrap_or(last_word);
    after_colon.split('<').next().unwrap_or(after_colon)
}

/// Resolve route handlers referenced by name to the handler function across files
/// -- the axum `.route("/p", get(handler))` case where `handler` is defined in
/// another module. A route node carries `_route_handler` (the handler name) and
/// `_route_method`; if it has no `handled_by` edge yet, link it to the uniquely
/// named function via `handled_by`. An ambiguous handler name is skipped.
pub fn resolve_route_handlers(nodes: Vec<Node>, edges: Vec<Edge>) -> (Vec<Node>, Vec<Edge>) {
    // function name -> node (`None` marks an ambiguous name).
    let mut fns: HashMap<String, Option<NodeId>> = HashMap::new();
    for n in &nodes {
        if matches!(
            n.kind(),
            Some(NodeKind::Function | NodeKind::Method | NodeKind::Constructor)
        ) {
            index_key(&mut fns, pyo3_node_name(&n.label).to_string(), &n.id);
        }
    }

    // Routes that already have a handler (resolved same-file, or a Python/Express
    // decorator handler).
    let handled: HashSet<NodeId> = edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| e.source.clone())
        .collect();

    let mut new_edges: Vec<Edge> = Vec::new();
    for n in &nodes {
        if n.extra.get("_node_type").and_then(|v| v.as_str()) != Some("route")
            || handled.contains(&n.id)
        {
            continue;
        }
        let Some(name) = n.extra.get("_route_handler").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(Some(target)) = fns.get(name) else {
            continue;
        };
        let method = n
            .extra
            .get("_route_method")
            .and_then(|v| v.as_str())
            .unwrap_or("ANY");
        new_edges.push(Edge {
            source: n.id.clone(),
            target: target.clone(),
            relation: "handled_by".to_string(),
            confidence: Confidence::Inferred,
            source_file: String::new(),
            source_location: None,
            confidence_score: Some(Confidence::Inferred.default_score()),
            weight: 1.0,
            context: Some(method.to_string()),
            cross_repo: false,
            extra: Map::new(),
        });
    }
    if new_edges.is_empty() {
        return (nodes, edges);
    }
    let mut edges = edges;
    edges.extend(new_edges);
    (nodes, edges)
}

/// Collapse SQL table stubs (emitted by the code-side `scan_sql` detector) into
/// the real table node when a `.sql` file defines it. Stubs have an empty
/// `source_file`; the real node has the file. Both share the same id, so this
/// keeps the node that has a source file and drops the duplicate. Code->SQL
/// edges are unchanged (their target id already matches). Nodes are sorted by id
/// so graph.json stays byte-stable.
pub fn resolve_sql_queries(nodes: Vec<Node>, edges: Vec<Edge>) -> (Vec<Node>, Vec<Edge>) {
    use std::collections::HashMap;
    let mut best: HashMap<NodeId, Node> = HashMap::new();
    for n in nodes {
        match best.get(&n.id) {
            Some(existing) if !existing.source_file.is_empty() => {
                // Keep the existing real node; ignore this stub.
            }
            _ => {
                best.insert(n.id.clone(), n);
            }
        }
    }
    let mut nodes: Vec<Node> = best.into_values().collect();
    nodes.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    (nodes, edges)
}

/// Cross-language relations whose edges represent a real coupling boundary that
/// can span repositories in a federated graph.
const CROSS_LANGUAGE_RELATIONS: &[&str] =
    &["invokes", "binds_native", "calls_service", "handled_by"];

/// Flag cross-language edges (subprocess/FFI/HTTP/gRPC) whose endpoints live in
/// different repos as `cross_repo`, so a federated graph shows which coupling
/// boundaries actually span repositories. Runs after federation has merged
/// shared boundary nodes (route/service/pyo3) by label, so e.g. a client in repo
/// A and a handler in repo B meet at one node and the client edge is flagged.
/// Deps and resolved symbols are left to the export-surface resolver.
pub fn mark_cross_repo_edges(nodes: &[Node], mut edges: Vec<Edge>) -> Vec<Edge> {
    let repo_of: HashMap<&NodeId, &str> = nodes
        .iter()
        .filter_map(|n| n.repo.as_deref().map(|r| (&n.id, r)))
        .collect();
    // A real in-repo definition carries a source file (a resolved command -> file,
    // a handler, ...). External stubs (commands, external service URLs, FFI sinks)
    // do not.
    let in_repo: HashSet<&NodeId> = nodes
        .iter()
        .filter(|n| !n.source_file.is_empty())
        .map(|n| &n.id)
        .collect();
    // A service boundary with an in-repo provider is the source of a `handled_by`
    // edge -- a route/grpc/pyo3 that is actually served in the graph.
    let provided: HashSet<NodeId> = edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| e.source.clone())
        .collect();
    for e in &mut edges {
        if !CROSS_LANGUAGE_RELATIONS.contains(&e.relation.as_str()) {
            continue;
        }
        let (Some(s), Some(t)) = (repo_of.get(&e.source), repo_of.get(&e.target)) else {
            continue;
        };
        if s == t {
            continue;
        }
        // Only a coupling to an in-repo-backed target is a genuine cross-repo
        // dependency. A deduped external stub (npm, cmd, an external API URL) that
        // two repos happen to share is not -- its repo tag is just whichever repo
        // was seen first.
        if in_repo.contains(&e.target) || provided.contains(&e.target) {
            e.cross_repo = true;
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};
    use synaptic_core::{Confidence, FileType};

    fn command_stub(name: &str) -> Node {
        let mut extra = Map::new();
        extra.insert("_node_type".into(), json!("command"));
        Node {
            id: NodeId(format!("cmd::{name}")),
            label: name.into(),
            file_type: FileType::Code,
            source_file: String::new(),
            source_location: None,
            community: None,
            repo: None,
            extra,
        }
    }

    fn file_node(id: &str, label: &str, path: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: path.into(),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    fn invokes(src: &str, tgt: &str) -> Edge {
        Edge {
            source: NodeId(src.into()),
            target: NodeId(tgt.into()),
            relation: "invokes".into(),
            confidence: Confidence::Inferred,
            source_file: "a.py".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: Some("subprocess".into()),
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn retargets_command_to_matching_file_node() {
        let nodes = vec![
            command_stub("mybinary"),
            file_node("src_bin_mybinary_rs", "mybinary.rs", "src/bin/mybinary.rs"),
            file_node("caller_py", "caller.py", "caller.py"),
        ];
        let edges = vec![invokes("caller_py", "cmd::mybinary")];
        let (nodes, edges) = resolve_command_invocations(nodes, edges);

        assert!(
            !nodes.iter().any(|n| n.id.0 == "cmd::mybinary"),
            "resolved stub is dropped"
        );
        let e = edges.iter().find(|e| e.relation == "invokes").unwrap();
        assert_eq!(
            e.target.0, "src_bin_mybinary_rs",
            "invokes retargeted to the binary source file"
        );
    }

    #[test]
    fn ambiguous_command_stays_unresolved() {
        let nodes = vec![
            command_stub("run"),
            file_node("a_run_rs", "run.rs", "a/run.rs"),
            file_node("b_run_py", "run.py", "b/run.py"),
        ];
        let edges = vec![invokes("x", "cmd::run")];
        let (nodes, edges) = resolve_command_invocations(nodes, edges);

        assert!(
            nodes.iter().any(|n| n.id.0 == "cmd::run"),
            "ambiguous stub kept"
        );
        assert_eq!(edges[0].target.0, "cmd::run", "edge unchanged");
    }

    // --- parameterized route resolution ---

    fn route_node(id: &str, path: &str) -> Node {
        let mut extra = Map::new();
        extra.insert("_node_type".into(), json!("route"));
        Node {
            id: NodeId(id.into()),
            label: path.into(),
            file_type: FileType::Code,
            source_file: String::new(),
            source_location: None,
            community: None,
            repo: None,
            extra,
        }
    }

    fn edge(src: &str, tgt: &str, rel: &str) -> Edge {
        Edge {
            source: NodeId(src.into()),
            target: NodeId(tgt.into()),
            relation: rel.into(),
            confidence: Confidence::Inferred,
            source_file: "a".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    /// A concrete client path merges into the unique template it matches, and the
    /// client's `calls_service` edge is retargeted to the template route.
    #[test]
    fn concrete_path_merges_into_template() {
        let nodes = vec![
            route_node("tmpl", "/users/{id}"),
            route_node("concrete", "/users/7"),
            file_node("client_fn", "fetch()", "c.py"),
        ];
        let edges = vec![
            edge("tmpl", "handler", "handled_by"),
            edge("client_fn", "concrete", "calls_service"),
        ];
        let (nodes, edges) = resolve_parameterized_routes(nodes, edges);

        assert!(
            !nodes.iter().any(|n| n.id.0 == "concrete"),
            "merged concrete route is dropped"
        );
        let cs = edges
            .iter()
            .find(|e| e.relation == "calls_service")
            .unwrap();
        assert_eq!(cs.target.0, "tmpl", "client now calls the template route");
    }

    #[test]
    fn express_and_flask_param_styles_match() {
        for (tmpl, concrete) in [
            ("/users/:id", "/users/7"),         // Express
            ("/items/<int:id>", "/items/3"),    // Flask typed converter
            ("/static/{*path}", "/static/a/b"), // axum catch-all (deeper path)
        ] {
            let nodes = vec![route_node("t", tmpl), route_node("c", concrete)];
            let edges = vec![edge("x", "c", "calls_service")];
            let (nodes, edges) = resolve_parameterized_routes(nodes, edges);
            assert!(
                !nodes.iter().any(|n| n.id.0 == "c"),
                "{concrete} should merge into {tmpl}"
            );
            assert_eq!(edges[0].target.0, "t", "{concrete} retargeted to {tmpl}");
        }
    }

    #[test]
    fn ambiguous_templates_leave_concrete_unresolved() {
        // Two distinct templates both match /users/7 -> ambiguous, no merge.
        let nodes = vec![
            route_node("t1", "/users/{id}"),
            route_node("t2", "/users/{uid}"),
            route_node("c", "/users/7"),
        ];
        let edges = vec![edge("x", "c", "calls_service")];
        let (nodes, edges) = resolve_parameterized_routes(nodes, edges);
        assert!(
            nodes.iter().any(|n| n.id.0 == "c"),
            "ambiguous concrete is kept"
        );
        assert_eq!(edges[0].target.0, "c", "edge unchanged");
    }

    #[test]
    fn different_segment_count_does_not_match() {
        let nodes = vec![
            route_node("t", "/users/{id}"),
            route_node("c", "/users/7/posts"),
        ];
        let edges = vec![edge("x", "c", "calls_service")];
        let (nodes, edges) = resolve_parameterized_routes(nodes, edges);
        assert!(
            nodes.iter().any(|n| n.id.0 == "c"),
            "deeper concrete must not match a non-catch-all template"
        );
        assert_eq!(edges[0].target.0, "c");
    }

    #[test]
    fn concrete_server_route_with_handler_is_not_merged() {
        // /users/me is a literal SERVER route (it has its own handler); it must not
        // be merged into the /users/{id} template even though the path matches,
        // else its distinct handler is lost.
        let nodes = vec![
            route_node("tmpl", "/users/{id}"),
            route_node("me", "/users/me"),
        ];
        let edges = vec![
            edge("tmpl", "h_generic", "handled_by"),
            edge("me", "h_me", "handled_by"),
        ];
        let (nodes, edges) = resolve_parameterized_routes(nodes, edges);
        assert!(
            nodes.iter().any(|n| n.id.0 == "me"),
            "literal server route is kept"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.source.0 == "me" && e.target.0 == "h_me"),
            "its distinct handler edge is preserved"
        );
    }

    #[test]
    fn bare_catch_all_does_not_match_everything() {
        // A route labeled `*` (Express catch-all / 404) has no literal anchor, so
        // it must not become a template that swallows every concrete route.
        let nodes = vec![route_node("star", "*"), route_node("c", "/files")];
        let edges = vec![edge("client", "c", "calls_service")];
        let (nodes, edges) = resolve_parameterized_routes(nodes, edges);
        assert!(
            nodes.iter().any(|n| n.id.0 == "c"),
            "concrete not merged into a bare * catch-all"
        );
        assert_eq!(edges[0].target.0, "c");
    }

    // --- pyo3 import join ---

    fn pyo3_module(id: &str, module: &str) -> Node {
        let mut extra = Map::new();
        extra.insert("_node_type".into(), json!("pyo3_module"));
        Node {
            id: NodeId(id.into()),
            label: format!("pyo3:{module}"),
            file_type: FileType::Code,
            source_file: String::new(),
            source_location: None,
            community: None,
            repo: None,
            extra,
        }
    }

    fn module_stub(name: &str) -> Node {
        // The import-target stub a Python `import <name>` produces (no source file).
        file_node(name, name, "")
    }

    #[test]
    fn python_importer_joins_matching_pyo3_module() {
        let nodes = vec![
            pyo3_module("b_mathmod", "mathmod"),
            module_stub("mathmod"),
            file_node("client_py", "client.py", "client.py"),
        ];
        let edges = vec![
            // Rust impl side already attached by the extractor.
            edge("b_mathmod", "add_fn", "handled_by"),
            // Python `import mathmod`.
            edge("client_py", "mathmod", "imports"),
        ];
        let (_nodes, edges) = resolve_pyo3_imports(nodes, edges);
        let joined = edges.iter().any(|e| {
            e.relation == "calls_service"
                && e.source.0 == "client_py"
                && e.target.0 == "b_mathmod"
                && e.context.as_deref() == Some("pyo3")
        });
        assert!(joined, "importer of mathmod connects to the pyo3 boundary");
    }

    // --- cross-repo edge flagging ---

    #[test]
    fn marks_cross_repo_only_for_in_repo_backed_targets() {
        // A cross-repo edge must couple to a real in-repo service/binary in another
        // repo -- not to an external stub (command / external URL) that dedup just
        // tagged with a first-seen repo.
        let mut svc_route = route_node("svc_route", "/api/users");
        svc_route.repo = Some("repob".into());
        let mut handler = file_node("h", "list()", "repob/h.py");
        handler.repo = Some("repob".into());
        let mut ext_route = route_node("ext_route", "/v1/external");
        ext_route.repo = Some("repob".into());
        let mut cmd = command_stub("npm"); // external command stub (no source_file)
        cmd.repo = Some("repob".into());
        let mut bin = file_node("bin", "tool.rs", "repob/src/bin/tool.rs");
        bin.repo = Some("repob".into());
        let mut client = file_node("client", "fetch()", "repoa/c.py");
        client.repo = Some("repoa".into());

        let nodes = vec![svc_route, handler, ext_route, cmd, bin, client];
        let edges = vec![
            edge("client", "svc_route", "calls_service"), // service w/ in-repo handler
            edge("svc_route", "h", "handled_by"),         // makes svc_route in-repo-backed
            edge("client", "ext_route", "calls_service"), // external client-only route
            edge("client", "cmd::npm", "invokes"),        // external command stub
            edge("client", "bin", "invokes"),             // resolved in-repo binary
            edge("client", "svc_route", "calls"),         // non cross-language relation
        ];
        let edges = mark_cross_repo_edges(&nodes, edges);
        let flag = |s: &str, t: &str, rel: &str| {
            edges
                .iter()
                .find(|e| e.source.0 == s && e.target.0 == t && e.relation == rel)
                .unwrap()
                .cross_repo
        };
        assert!(
            flag("client", "svc_route", "calls_service"),
            "a service with an in-repo handler in another repo is cross-repo"
        );
        assert!(
            flag("client", "bin", "invokes"),
            "a resolved in-repo binary in another repo is cross-repo"
        );
        assert!(
            !flag("client", "ext_route", "calls_service"),
            "an external client-only route (no in-repo handler) is NOT cross-repo"
        );
        assert!(
            !flag("client", "cmd::npm", "invokes"),
            "an external command stub is NOT cross-repo"
        );
        assert!(
            !flag("client", "svc_route", "calls"),
            "a non cross-language relation is left alone"
        );
    }

    // --- pyo3 cross-file module stitch ---

    fn pyo3_module_regs(id: &str, module: &str, regs: &[&str]) -> Node {
        let mut extra = Map::new();
        extra.insert("_node_type".into(), json!("pyo3_module"));
        extra.insert("_pyo3_registers".into(), json!(regs));
        Node {
            id: NodeId(id.into()),
            label: format!("pyo3:{module}"),
            file_type: FileType::Code,
            source_file: String::new(),
            source_location: None,
            community: None,
            repo: None,
            extra,
        }
    }

    fn export_node(id: &str, label: &str, path: &str) -> Node {
        let mut n = file_node(id, label, path);
        n.extra.insert("_pyo3_export".into(), json!(label));
        n
    }

    #[test]
    fn pyo3_module_links_registered_export_in_another_file() {
        // The #[pymodule] is in lib.rs; the #[pyfunction] `add` it registers is in
        // ops.rs. The stitch links the boundary to `add()` by name.
        let nodes = vec![
            pyo3_module_regs("b_mod", "mymod", &["add"]),
            export_node("ops_add", "add", "ops.rs"),
            // A registered name with no matching export, and an export not
            // registered, both stay unlinked.
            file_node("loose", "helper", "ops.rs"),
        ];
        let edges = vec![];
        let (_n, edges) = resolve_pyo3_modules(nodes, edges);
        let links: Vec<_> = edges
            .iter()
            .filter(|e| e.relation == "handled_by")
            .map(|e| (e.source.0.as_str(), e.target.0.as_str()))
            .collect();
        assert_eq!(
            links,
            vec![("b_mod", "ops_add")],
            "boundary linked only to its registered export"
        );
    }

    #[test]
    fn pyo3_unregistered_export_is_not_linked() {
        // A tagged export the module does not register must not be linked.
        let nodes = vec![
            pyo3_module_regs("b_mod", "mymod", &["add"]),
            export_node("ops_sub", "sub", "ops.rs"), // exported but not registered
        ];
        let (_n, edges) = resolve_pyo3_modules(nodes, vec![]);
        assert!(
            !edges.iter().any(|e| e.relation == "handled_by"),
            "unregistered export stays unlinked"
        );
    }

    // --- cross-file route handler resolution ---

    fn fn_node(id: &str, label: &str, path: &str) -> Node {
        let mut n = file_node(id, label, path);
        n.set_kind(NodeKind::Function);
        n
    }

    #[test]
    fn route_handler_resolved_across_files() {
        // An axum route carries the handler name; the handler fn lives in another
        // file. The pass links route -> handler with the route's method.
        let mut route = route_node("r", "/api/x");
        route.extra.insert("_route_handler".into(), json!("serve"));
        route.extra.insert("_route_method".into(), json!("GET"));
        let nodes = vec![route, fn_node("h_serve", "serve()", "handlers.rs")];
        let (_n, edges) = resolve_route_handlers(nodes, vec![]);
        let e = edges
            .iter()
            .find(|e| e.relation == "handled_by")
            .expect("handled_by edge");
        assert_eq!(e.source.0, "r");
        assert_eq!(e.target.0, "h_serve");
        assert_eq!(e.context.as_deref(), Some("GET"));
    }

    #[test]
    fn already_handled_route_is_not_relinked() {
        let mut route = route_node("r", "/api/x");
        route.extra.insert("_route_handler".into(), json!("serve"));
        let nodes = vec![route, fn_node("h_serve", "serve()", "handlers.rs")];
        // The route already has a same-file handler.
        let edges = vec![edge("r", "local_handler", "handled_by")];
        let (_n, edges) = resolve_route_handlers(nodes, edges);
        assert_eq!(
            edges.iter().filter(|e| e.relation == "handled_by").count(),
            1,
            "no duplicate handler edge"
        );
    }

    #[test]
    fn ambiguous_route_handler_name_is_skipped() {
        let mut route = route_node("r", "/api/x");
        route.extra.insert("_route_handler".into(), json!("serve"));
        // Two functions named `serve` -> ambiguous, skip.
        let nodes = vec![
            route,
            fn_node("a_serve", "serve()", "a.rs"),
            fn_node("b_serve", "serve()", "b.rs"),
        ];
        let (_n, edges) = resolve_route_handlers(nodes, vec![]);
        assert!(
            !edges.iter().any(|e| e.relation == "handled_by"),
            "ambiguous handler name is not linked"
        );
    }

    #[test]
    fn unrelated_python_import_does_not_join() {
        // A pure-Python import whose name matches no pyo3 module stays unconnected.
        let nodes = vec![
            pyo3_module("b_mathmod", "mathmod"),
            module_stub("os"),
            file_node("client_py", "client.py", "client.py"),
        ];
        let edges = vec![edge("client_py", "os", "imports")];
        let (_nodes, edges) = resolve_pyo3_imports(nodes, edges);
        assert!(
            !edges.iter().any(|e| e.relation == "calls_service"),
            "import of a non-pyo3 module must not connect"
        );
    }

    // --- sql query resolution ---

    #[test]
    fn resolve_sql_queries_dedups_stub_into_real_table() {
        use synaptic_core::{make_id, NodeId};
        let tid = NodeId(make_id(&["sql", "orders"]));
        let mut real = Node {
            id: tid.clone(),
            label: "orders".into(),
            file_type: synaptic_core::FileType::Code,
            source_file: "schema.sql".into(),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: serde_json::Map::new(),
        };
        real.set_kind(synaptic_core::NodeKind::Table);
        let stub = Node {
            id: tid.clone(),
            label: "orders".into(),
            file_type: synaptic_core::FileType::Code,
            source_file: String::new(),
            source_location: None,
            community: None,
            repo: None,
            extra: serde_json::Map::new(),
        };
        let edge = Edge {
            source: NodeId("app.list".into()),
            target: tid.clone(),
            relation: "queries".into(),
            confidence: synaptic_core::Confidence::Inferred,
            source_file: "app.py".into(),
            source_location: Some("L4".into()),
            confidence_score: Some(0.5),
            weight: 1.0,
            context: Some("sql_query".into()),
            cross_repo: false,
            extra: serde_json::Map::new(),
        };
        let (nodes, edges) = resolve_sql_queries(vec![real, stub], vec![edge]);
        let orders: Vec<_> = nodes.iter().filter(|n| n.id == tid).collect();
        assert_eq!(orders.len(), 1, "stub deduped into real node");
        assert_eq!(orders[0].source_file, "schema.sql");
        assert_eq!(
            orders[0].kind(),
            Some(synaptic_core::NodeKind::Table),
            "the enriched real node (not the bare stub) survives"
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target, tid);
    }
}
