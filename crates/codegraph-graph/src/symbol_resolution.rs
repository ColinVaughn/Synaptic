//! Cross-file call resolution: turn per-file `raw_calls` into `calls` edges,
//! conservatively.
//!
//! Two passes, run after the graph is built (so the canonical node set is known):
//! 1. **Import-guided** — a `from M import name [as local]` record proves that a
//!    bare `local(...)` call targets `M`'s `name`. If exactly one node matches
//!    `(module_stem, name)`, emit an `EXTRACTED` edge (score 1.0).
//! 2. **Cross-file** — for any remaining unqualified call, if its name maps to
//!    exactly one node across the whole graph, emit an `INFERRED` edge (0.8).
//!
//! Both skip member calls (the raw fact carries no receiver) and ambiguous names.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use codegraph_core::{Confidence, Edge, ImportRecord, Node, NodeId, RawCall};
use serde_json::{json, Map};

use crate::graph::KnowledgeGraph;

/// Source-file extensions whose file-node labels must never be call targets.
const SOURCE_EXTS: &[&str] = &[
    ".py", ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".go", ".rs", ".java",
];

/// Normalize a node label into the lookup key: `foo()`→`foo`, `.bar()`→`bar`,
/// lowercased.
fn normalize_label(label: &str) -> String {
    label
        .trim()
        .trim_matches(|c| c == '(' || c == ')')
        .trim_start_matches('.')
        .to_lowercase()
}

/// A node usable as a deterministic call target: a located code symbol (not a
/// file node, not an external stub).
///
/// We exclude empty-`source_file` nodes so a real call never resolves onto a
/// `{file_type: "code", source_file: ""}` stub emitted for unresolved
/// imports/inheritance bases — a call must never resolve to an unresolved
/// external stub (see the `external_stub_nodes_do_not_absorb_calls` test).
/// Pass 1 is unaffected either way (it already requires a non-empty source stem
/// on both sides).
fn is_resolvable(n: &Node) -> bool {
    if n.file_type != codegraph_core::FileType::Code || n.source_file.is_empty() {
        return false;
    }
    let label = n.label.trim();
    if label.is_empty() || SOURCE_EXTS.iter().any(|e| label.ends_with(e)) {
        return false;
    }
    !normalize_label(label).is_empty()
}

fn source_stem(source_file: &str) -> String {
    Path::new(source_file)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `normalized_label -> node ids` for conservative cross-file resolution.
fn build_label_index(kg: &KnowledgeGraph) -> HashMap<String, Vec<NodeId>> {
    let mut idx: HashMap<String, Vec<NodeId>> = HashMap::new();
    for n in kg.nodes() {
        if !is_resolvable(n) {
            continue;
        }
        idx.entry(normalize_label(&n.label))
            .or_default()
            .push(n.id.clone());
    }
    idx
}

/// `(module_stem, normalized_label) -> node ids` — stricter than the label index;
/// import evidence resolves calls that global label uniqueness alone cannot.
fn build_symbol_index(kg: &KnowledgeGraph) -> HashMap<(String, String), Vec<NodeId>> {
    let mut idx: HashMap<(String, String), Vec<NodeId>> = HashMap::new();
    for n in kg.nodes() {
        if !is_resolvable(n) {
            continue;
        }
        let stem = source_stem(&n.source_file);
        if stem.is_empty() {
            continue;
        }
        idx.entry((stem, normalize_label(&n.label)))
            .or_default()
            .push(n.id.clone());
    }
    idx
}

#[allow(clippy::too_many_arguments)]
fn calls_edge(
    caller: NodeId,
    target: NodeId,
    confidence: Confidence,
    score: f32,
    context: &str,
    source_file: String,
    source_location: Option<String>,
) -> Edge {
    Edge {
        source: caller,
        target,
        relation: "calls".to_string(),
        confidence,
        confidence_score: Some(score),
        source_file,
        source_location,
        weight: 1.0,
        context: Some(context.to_string()),
        cross_repo: false,
        extra: Map::new(),
    }
}

fn is_bash_file(path: &str) -> bool {
    path.ends_with(".sh") || path.ends_with(".bash")
}

/// Bash-specific resolution: a call in a file
/// that `source`d another file resolves to a function defined in a **sourced**
/// file, scoped to that sourced set. This resolves calls the generic passes miss
/// (a name that's globally ambiguous but unique among the sourced files) and at
/// EXTRACTED confidence (the source relationship proves the target). Emits an
/// edge only when the callee matches exactly one function across all sourced
/// files.
fn resolve_bash_sources(
    kg: &KnowledgeGraph,
    raw_calls: &[RawCall],
    sourced: &HashMap<NodeId, HashSet<NodeId>>,
    known: &mut HashSet<(NodeId, NodeId, String)>,
) -> Vec<Edge> {
    // No bash `source` edges, nothing to do (the common non-bash case pays only
    // the cheap `is_bash_file` check folded into the caller's edge loop, plus this
    // early return, no extra graph scans).
    if sourced.is_empty() {
        return Vec::new();
    }
    // The file-node id for a path, collapses any slash style, so it equals both
    // the `imports_from` target id and a function's owning-file id.
    let file_id = |path: &str| NodeId(codegraph_core::make_id(&[path]));

    // functions_by_file[file_id][normalized label] = bash function node ids.
    let mut functions_by_file: HashMap<NodeId, HashMap<String, Vec<NodeId>>> = HashMap::new();
    for n in kg.nodes() {
        if !is_bash_file(&n.source_file) || !n.label.trim().ends_with("()") {
            continue;
        }
        let key = normalize_label(&n.label);
        if key.is_empty() {
            continue;
        }
        functions_by_file
            .entry(file_id(&n.source_file))
            .or_default()
            .entry(key)
            .or_default()
            .push(n.id.clone());
    }

    let mut out = Vec::new();
    for rc in raw_calls {
        if rc.is_member_call || !is_bash_file(&rc.source_file) {
            continue;
        }
        let callee = normalize_label(&rc.callee);
        if callee.is_empty() {
            continue;
        }
        let Some(srcset) = sourced.get(&file_id(&rc.source_file)) else {
            continue;
        };
        let matches: Vec<NodeId> = srcset
            .iter()
            .filter_map(|sf| functions_by_file.get(sf))
            .filter_map(|byname| byname.get(&callee))
            .flatten()
            .cloned()
            .collect();
        if matches.len() != 1 || rc.caller == matches[0] {
            continue;
        }
        let target = matches
            .into_iter()
            .next()
            .expect("exactly one match (len checked above)");
        if !known.insert((rc.caller.clone(), target.clone(), "calls".to_string())) {
            continue;
        }
        let mut edge = calls_edge(
            rc.caller.clone(),
            target,
            Confidence::Extracted,
            1.0,
            "bash_source_call",
            rc.source_file.clone(),
            rc.source_location.clone(),
        );
        edge.extra
            .insert("metadata".to_string(), json!({ "resolver": "bash_source" }));
        out.push(edge);
    }
    out
}

/// Resolve `raw_calls` against the built graph, returning the new `calls` edges
/// (bash sourced-calls + import-guided EXTRACTED, then single-candidate cross-file
/// INFERRED). Endpoints are canonical node ids; the caller adds them to the graph
/// (which drops any whose endpoints don't exist).
pub fn resolve_symbols(
    kg: &KnowledgeGraph,
    raw_calls: &[RawCall],
    imports: &[ImportRecord],
) -> Vec<Edge> {
    // Single edge pass: seed dedup with existing (source, target, relation)
    // triples (so we never duplicate an intra-file `calls` edge emitted at
    // extraction) AND collect bash `source` edges for Pass 0, no extra graph scan.
    let mut known: HashSet<(NodeId, NodeId, String)> = HashSet::new();
    let mut bash_sourced: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for e in kg.edges() {
        known.insert((e.source.clone(), e.target.clone(), e.relation.clone()));
        if e.relation == "imports_from" && is_bash_file(&e.source_file) {
            bash_sourced
                .entry(e.source.clone())
                .or_default()
                .insert(e.target.clone());
        }
    }
    // Pass 0: bash sourced-function calls (EXTRACTED, sourced-scoped)
    // Runs first so its EXTRACTED edges win and dedup blocks a weaker INFERRED
    // duplicate from the generic cross-file pass.
    let mut out: Vec<Edge> = resolve_bash_sources(kg, raw_calls, &bash_sourced, &mut known);

    // Pass 1: import-guided (EXTRACTED, 1.0)
    let symbol_index = build_symbol_index(kg);
    let mut aliases_by_file: HashMap<&str, HashMap<&str, &ImportRecord>> = HashMap::new();
    for imp in imports {
        aliases_by_file
            .entry(imp.source_file.as_str())
            .or_default()
            .insert(imp.local_name.as_str(), imp);
    }
    for rc in raw_calls {
        if rc.is_member_call {
            continue;
        }
        let callee = rc.callee.trim();
        if callee.is_empty() {
            continue;
        }
        let Some(aliases) = aliases_by_file.get(rc.source_file.as_str()) else {
            continue;
        };
        let Some(imported) = aliases.get(callee) else {
            continue;
        };
        let key = (
            imported.module_stem.clone(),
            imported.imported_name.to_lowercase(),
        );
        let Some(cands) = symbol_index.get(&key) else {
            continue;
        };
        if cands.len() != 1 {
            continue;
        }
        let target = cands[0].clone();
        if rc.caller == target {
            continue;
        }
        if !known.insert((rc.caller.clone(), target.clone(), "calls".to_string())) {
            continue;
        }
        let mut edge = calls_edge(
            rc.caller.clone(),
            target,
            Confidence::Extracted,
            1.0,
            "import_guided_call",
            rc.source_file.clone(),
            // Empty string is treated as "absent", so fall back to the import
            // site's location.
            rc.source_location
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| imported.source_location.clone()),
        );
        // Provenance block: sanitized `metadata` on the import-guided edge.
        edge.extra.insert(
            "metadata".to_string(),
            json!({
                "resolver": "python_import_guided",
                "local_name": imported.local_name,
                "imported_name": imported.imported_name,
                "module_stem": imported.module_stem,
                "import_source_location": imported.source_location,
            }),
        );
        out.push(edge);
    }

    // Pass 2: cross-file single-candidate (INFERRED, 0.8)
    let label_index = build_label_index(kg);
    for rc in raw_calls {
        if rc.is_member_call {
            continue;
        }
        let callee = rc.callee.trim();
        if callee.is_empty() {
            continue;
        }
        let Some(cands) = label_index.get(&callee.to_lowercase()) else {
            continue;
        };
        if cands.len() != 1 {
            continue;
        }
        let target = cands[0].clone();
        if rc.caller == target {
            continue;
        }
        if !known.insert((rc.caller.clone(), target.clone(), "calls".to_string())) {
            continue;
        }
        out.push(calls_edge(
            rc.caller.clone(),
            target,
            Confidence::Inferred,
            0.8,
            "call",
            rc.source_file.clone(),
            rc.source_location.clone(),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{FileType, GraphData};

    fn node(id: &str, label: &str, sf: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: sf.into(),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    fn kg(nodes: Vec<Node>, links: Vec<Edge>) -> KnowledgeGraph {
        KnowledgeGraph::from_graph_data(GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links,
            hyperedges: vec![],
            built_at_commit: None,
        })
    }

    fn raw(caller: &str, callee: &str, member: bool, sf: &str) -> RawCall {
        RawCall {
            caller: NodeId(caller.into()),
            callee: callee.into(),
            is_member_call: member,
            source_file: sf.into(),
            source_location: Some("L2".into()),
            span: None,
        }
    }

    fn imp(local: &str, imported: &str, stem: &str, sf: &str) -> ImportRecord {
        ImportRecord {
            local_name: local.into(),
            imported_name: imported.into(),
            module_stem: stem.into(),
            source_file: sf.into(),
            source_location: Some("L1".into()),
        }
    }

    #[test]
    fn import_guided_resolves_extracted() {
        // a.py: `from helper import transform`; caller calls transform().
        let g = kg(
            vec![
                node("a_caller", "caller()", "a.py"),
                node("helper_transform", "transform()", "helper.py"),
            ],
            vec![],
        );
        let edges = resolve_symbols(
            &g,
            &[raw("a_caller", "transform", false, "a.py")],
            &[imp("transform", "transform", "helper", "a.py")],
        );
        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        assert_eq!(e.source, NodeId("a_caller".into()));
        assert_eq!(e.target, NodeId("helper_transform".into()));
        assert_eq!(e.confidence, Confidence::Extracted);
        assert_eq!(e.confidence_score, Some(1.0));
        assert_eq!(e.context.as_deref(), Some("import_guided_call"));
        // Provenance metadata is carried on the import-guided edge.
        let meta = e.extra.get("metadata").expect("metadata present");
        assert_eq!(meta["resolver"], "python_import_guided");
        assert_eq!(meta["imported_name"], "transform");
        assert_eq!(meta["module_stem"], "helper");
    }

    fn imports_from_edge(src: &str, tgt: &str, sf: &str) -> Edge {
        Edge {
            source: NodeId(src.into()),
            target: NodeId(tgt.into()),
            relation: "imports_from".into(),
            confidence: Confidence::Extracted,
            confidence_score: Some(1.0),
            source_file: sf.into(),
            source_location: Some("L1".into()),
            weight: 1.0,
            context: Some("import".into()),
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn bash_sourced_call_resolves_extracted_and_scoped() {
        // a/app.sh sources a/lib.sh and calls greet(); greet is defined in lib.sh
        // AND (ambiguously) in an UNsourced b/other.sh. The sourced scope picks
        // lib.sh's greet at EXTRACTED, where the global pass would refuse (the
        // name is globally ambiguous).
        let app = codegraph_core::make_id(&["a/app.sh"]);
        let lib = codegraph_core::make_id(&["a/lib.sh"]);
        let g = kg(
            vec![
                node(&app, "app.sh", "a/app.sh"),
                node(&lib, "lib.sh", "a/lib.sh"), // sourced file's own node (edge target)
                node("app_run", "run()", "a/app.sh"),
                node("lib_greet", "greet()", "a/lib.sh"),
                node("other_greet", "greet()", "b/other.sh"), // not sourced
            ],
            vec![imports_from_edge(&app, &lib, "a/app.sh")],
        );
        let edges = resolve_symbols(&g, &[raw("app_run", "greet", false, "a/app.sh")], &[]);
        let calls: Vec<_> = edges.iter().filter(|e| e.relation == "calls").collect();
        assert_eq!(calls.len(), 1, "{edges:?}");
        assert_eq!(calls[0].target, NodeId("lib_greet".into()));
        assert_eq!(calls[0].confidence, Confidence::Extracted);
        assert_eq!(calls[0].context.as_deref(), Some("bash_source_call"));
    }

    #[test]
    fn cross_file_single_candidate_is_inferred() {
        // No import record, so falls to the global single-candidate pass.
        let g = kg(
            vec![
                node("a_caller", "caller()", "a.py"),
                node("helper_transform", "transform()", "helper.py"),
            ],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("a_caller", "transform", false, "a.py")], &[]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].confidence, Confidence::Inferred);
        assert_eq!(edges[0].confidence_score, Some(0.8));
        assert_eq!(edges[0].context.as_deref(), Some("call"));
    }

    #[test]
    fn ambiguous_label_is_not_resolved() {
        // Two `transform()` definitions: cross-file refuses to guess.
        let g = kg(
            vec![
                node("a_caller", "caller()", "a.py"),
                node("h1_transform", "transform()", "h1.py"),
                node("h2_transform", "transform()", "h2.py"),
            ],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("a_caller", "transform", false, "a.py")], &[]);
        assert!(
            edges.is_empty(),
            "ambiguous name must not resolve: {edges:?}"
        );
    }

    #[test]
    fn member_calls_are_skipped() {
        let g = kg(
            vec![
                node("a_caller", "caller()", "a.py"),
                node("helper_transform", "transform()", "helper.py"),
            ],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("a_caller", "transform", true, "a.py")], &[]);
        assert!(edges.is_empty());
    }

    #[test]
    fn existing_calls_edge_is_not_duplicated() {
        let mut existing = calls_edge(
            NodeId("a_caller".into()),
            NodeId("helper_transform".into()),
            Confidence::Extracted,
            1.0,
            "call",
            "a.py".into(),
            None,
        );
        existing.confidence_score = None;
        let g = kg(
            vec![
                node("a_caller", "caller()", "a.py"),
                node("helper_transform", "transform()", "helper.py"),
            ],
            vec![existing],
        );
        let edges = resolve_symbols(&g, &[raw("a_caller", "transform", false, "a.py")], &[]);
        assert!(
            edges.is_empty(),
            "should not duplicate the existing calls edge"
        );
    }

    #[test]
    fn external_stub_nodes_do_not_absorb_calls() {
        // An import stub (empty source_file, label "os") must not be a call target.
        let g = kg(
            vec![node("a_caller", "caller()", "a.py"), {
                let mut n = node("os", "os", "");
                n.source_location = None;
                n
            }],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("a_caller", "os", false, "a.py")], &[]);
        assert!(edges.is_empty());
    }
}
