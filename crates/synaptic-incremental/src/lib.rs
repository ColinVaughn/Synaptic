//! Incremental rebuild + git integration for Synaptic.
//!
//! C1b — the **changed-files rebuild engine**:
//! given the files that changed since the last build, re-extract only those,
//! merge the fresh AST into the existing graph (preserving semantic nodes,
//! unrelated AST, and hyperedges; evicting deleted/changed sources), rebuild,
//! re-run symbol resolution + dedup, re-cluster with community remap for ID
//! stability, and guard against silent shrink. Topology-unchanged rebuilds
//! short-circuit so the caller can skip rewriting artifacts.
//!
//! This crate stays at the graph level (deps detect + extract + graph): it
//! returns the rebuilt [`KnowledgeGraph`]; the caller (CLI) reads/writes
//! `graph.json` and the other artifacts. `affected` lives in `synaptic-query`
//! (C1a); locking/hooks/merge-driver are C1c–C1e.
#![forbid(unsafe_code)]

pub mod concurrency;
pub mod hooks;
pub mod merge_driver;
pub mod watch;
pub use concurrency::{
    drain_pending, merge_changed_paths, queue_pending, try_acquire_lock, RebuildLock,
};
pub use merge_driver::{run_merge_driver, union_graphs, MergeDriverError};
pub use watch::{should_ignore_path, ChangeBatch, DEBOUNCE_MS};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use synaptic_core::{Edge, GraphData, Hyperedge, Node, NodeId};
use synaptic_detect::{classify_file, detect, FileType};
use synaptic_extract::cached_extract_source;
use synaptic_graph::{
    apply_communities, build_from_parts, cluster, deduplicate_entities, guard_shrink,
    norm_source_file, remap_communities_to_previous, resolve_command_invocations,
    resolve_parameterized_routes, resolve_pyo3_imports, resolve_pyo3_modules,
    resolve_route_handlers, resolve_sql_queries, resolve_symbols, BuildOptions, ClusterOptions,
    KnowledgeGraph,
};

/// AST-extracted node — origin stamped by the extractors (`_origin == "ast"`).
/// Semantic/concept nodes are NOT ast and so survive an AST-only rebuild.
fn is_ast(node: &Node) -> bool {
    node.extra.get("_origin").and_then(|v| v.as_str()) == Some("ast")
}

/// Merge a fresh AST extraction into the existing graph:
/// - a fresh AST node replaces the existing node with the same id;
/// - an existing node is **preserved** iff it isn't being replaced, isn't a
///   stale AST node being dropped by a full rebuild, and its `source_file` was
///   not evicted (so semantic nodes and unchanged files' AST survive);
/// - an existing edge is preserved iff both endpoints are still live (which
///   keeps prior cross-file `calls` edges whose endpoints both survive);
/// - hyperedges are carried over verbatim.
///
/// Returns `(nodes, edges, hyperedges)` ready for [`build_from_parts`].
pub fn merge_incremental(
    existing: &GraphData,
    fresh_nodes: Vec<Node>,
    fresh_edges: Vec<Edge>,
    evict_sources: &HashSet<String>,
    full_rebuild: bool,
) -> (Vec<Node>, Vec<Edge>, Vec<Hyperedge>) {
    let new_ast_ids: HashSet<NodeId> = fresh_nodes.iter().map(|n| n.id.clone()).collect();

    let preserved_nodes: Vec<Node> = existing
        .nodes
        .iter()
        .filter(|n| {
            let replaced = new_ast_ids.contains(&n.id);
            let stale_ast = full_rebuild && is_ast(n);
            let evicted = evict_sources.contains(&n.source_file);
            !(replaced || stale_ast || evicted)
        })
        .cloned()
        .collect();

    let mut live_ids = new_ast_ids;
    for n in &preserved_nodes {
        live_ids.insert(n.id.clone());
    }

    let preserved_edges: Vec<Edge> = existing
        .links
        .iter()
        .filter(|e| live_ids.contains(&e.source) && live_ids.contains(&e.target))
        .cloned()
        .collect();

    let mut nodes = fresh_nodes;
    nodes.extend(preserved_nodes);
    let mut edges = fresh_edges;
    edges.extend(preserved_edges);
    (nodes, edges, existing.hyperedges.clone())
}

/// Topology fingerprint for the "unchanged → don't rewrite" short-circuit:
/// the sorted node-id set + sorted
/// `(source, target, relation)` edge triples. Deliberately ignores community,
/// `norm_label`, and confidence scores, which are derived, not structural.
pub fn topology(gd: &GraphData) -> (Vec<String>, Vec<(String, String, String)>) {
    let mut ids: Vec<String> = gd.nodes.iter().map(|n| n.id.0.clone()).collect();
    ids.sort();
    ids.dedup();
    let mut edges: Vec<(String, String, String)> = gd
        .links
        .iter()
        .map(|e| (e.source.0.clone(), e.target.0.clone(), e.relation.clone()))
        .collect();
    edges.sort();
    edges.dedup();
    (ids, edges)
}

/// Per-node previous community assignment, for [`remap_communities_to_previous`].
fn previous_communities(gd: &GraphData) -> HashMap<NodeId, u32> {
    gd.nodes
        .iter()
        .filter_map(|n| n.community.map(|c| (n.id.clone(), c)))
        .collect()
}

/// What changed since the last build.
#[derive(Debug, Clone)]
pub enum ChangeSet {
    /// Rebuild every code file from scratch (drops stale AST, keeps semantic).
    Full,
    /// Only these paths changed (repo-relative or absolute). Each is re-extracted
    /// if it still exists and is a code file, otherwise evicted.
    Incremental(Vec<PathBuf>),
}

/// Options for [`rebuild`].
#[derive(Debug, Clone)]
pub struct RebuildOptions {
    /// Repo root.
    pub root: PathBuf,
    /// Build a directed graph.
    pub directed: bool,
    /// Bypass the shrink guard.
    pub force: bool,
}

/// Result of a [`rebuild`].
#[derive(Debug)]
pub struct RebuildOutcome {
    /// The rebuilt, clustered graph.
    pub kg: KnowledgeGraph,
    /// Community assignment (id-stable vs the previous build where possible).
    pub communities: BTreeMap<u32, Vec<NodeId>>,
    /// `false` when the rebuilt topology equals the prior graph's — the caller
    /// may skip rewriting artifacts.
    pub changed: bool,
    /// How many files were re-extracted.
    pub reextracted: usize,
    /// How many source files were evicted.
    pub evicted_sources: usize,
}

/// Errors a rebuild can surface.
#[derive(Debug, thiserror::Error)]
pub enum IncrementalError {
    /// The rebuild would shrink the graph without an explicit deletion or `force`.
    #[error(transparent)]
    Graph(#[from] synaptic_graph::GraphError),
}

/// Run a changed-files (or full) rebuild against `existing` (the prior
/// `graph.json` as [`GraphData`], or `None` for a from-scratch build).
///
/// Mirrors `synaptic extract`'s assembly (build → resolve → dedup → cluster)
/// but on the *merged* node/edge set, and adds the incremental-specific
/// community remap, topology short-circuit, and shrink guard. The semantic/LLM
/// passes are intentionally NOT run here (AST-only); existing semantic nodes are
/// preserved through the merge.
pub fn rebuild(
    opts: &RebuildOptions,
    changes: &ChangeSet,
    existing: Option<&GraphData>,
) -> Result<RebuildOutcome, IncrementalError> {
    // Canonicalize the root so `detect`'s yielded paths and our `root.join(rel)`
    // share one representation (matters for the changed-path to code-file match,
    // and keeps rel ids/source_files identical to a full `synaptic extract`).
    let root = opts
        .root
        .canonicalize()
        .unwrap_or_else(|_| opts.root.clone());
    let root = root.as_path();
    let root_str = root.to_string_lossy().into_owned();
    let det = detect(root);
    let code_files: Vec<PathBuf> = det.of(FileType::Code).to_vec();
    // Markdown is a Document (not Code), but it gets structural heading
    // extraction too, so extract it alongside code in every rebuild path
    // (update/watch/workspace), matching `synaptic extract`. (.NET/apex/etc. are
    // Code, so they already flow through `code_files`.)
    let md_files: Vec<PathBuf> = det
        .of(FileType::Document)
        .iter()
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("md") | Some("mdx") | Some("qmd")
            )
        })
        .cloned()
        .collect();
    let extract_set: HashSet<&Path> = code_files
        .iter()
        .chain(md_files.iter())
        .map(PathBuf::as_path)
        .collect();

    // Decide what to extract and what to evict.
    let mut full_rebuild = false;
    let mut evict_sources: HashSet<String> = HashSet::new();
    let mut had_deletions = false;
    let targets: Vec<PathBuf> = match changes {
        ChangeSet::Full => {
            full_rebuild = true;
            // Reconcile against the current code files: evict existing nodes whose
            // code-extension source_file no longer exists (deleted since the last
            // run, #1007). The stale-AST drop already covers AST nodes for deleted
            // files; this additionally catches non-AST nodes, and is restricted to
            // code extensions so doc-sourced semantic nodes are never wrongly
            // evicted.
            if let Some(ex) = existing {
                let current: HashSet<String> = code_files
                    .iter()
                    .map(|p| norm_source_file(&p.to_string_lossy(), Some(&root_str)))
                    .collect();
                for n in &ex.nodes {
                    if n.source_file.is_empty()
                        || classify_file(Path::new(&n.source_file)) != Some(FileType::Code)
                    {
                        continue;
                    }
                    let norm = norm_source_file(&n.source_file, Some(&root_str));
                    if !current.contains(&norm) {
                        evict_sources.insert(n.source_file.clone());
                        evict_sources.insert(norm);
                        had_deletions = true;
                    }
                }
            }
            code_files.iter().chain(md_files.iter()).cloned().collect()
        }
        ChangeSet::Incremental(paths) => {
            let mut wanted: Vec<PathBuf> = Vec::new();
            for p in paths {
                let abs = if p.is_absolute() {
                    p.clone()
                } else {
                    root.join(p)
                };
                // Evict the old nodes for this source regardless: a re-extracted
                // file's fresh nodes come back via the AST id set.
                evict_sources.insert(norm_source_file(&abs.to_string_lossy(), Some(&root_str)));
                if abs.exists() && extract_set.contains(abs.as_path()) {
                    wanted.push(abs);
                } else {
                    had_deletions = true;
                }
            }
            wanted
        }
    };
    let reextracted = targets.len();

    // Extract targets in parallel, ordered for determinism, with portable rel ids.
    let cache_dir = root.join("synaptic-out").join("cache");
    let extracted: Vec<_> = targets
        .par_iter()
        .map(|file| {
            let rel = file.strip_prefix(root).unwrap_or(file);
            let rel_str = rel.to_string_lossy();
            std::fs::read(file)
                .ok()
                .and_then(|bytes| cached_extract_source(Some(&cache_dir), rel_str.as_ref(), &bytes))
        })
        .collect();
    let mut fresh_nodes = Vec::new();
    let mut fresh_edges = Vec::new();
    let mut raw_calls = Vec::new();
    let mut imports = Vec::new();
    for r in extracted.into_iter().flatten() {
        fresh_nodes.extend(r.nodes);
        fresh_edges.extend(r.edges);
        raw_calls.extend(r.raw_calls);
        imports.extend(r.imports);
    }

    let empty = GraphData {
        directed: opts.directed,
        multigraph: false,
        graph: serde_json::Map::new(),
        nodes: vec![],
        links: vec![],
        hyperedges: vec![],
        built_at_commit: None,
    };
    let base = existing.unwrap_or(&empty);
    let (mut nodes, mut edges, hyper) =
        merge_incremental(base, fresh_nodes, fresh_edges, &evict_sources, full_rebuild);

    // Bind JS/TS imports to real nodes over the full merged set: relative code
    // imports to file nodes, `@/...` aliases to real files (tsconfig paths), and
    // non-code imports (css/json/assets) to classified asset nodes.
    let aliases = synaptic_extract::load_alias_resolver(root, &det.ts_config_files);
    synaptic_extract::resolve_imports(&mut nodes, &mut edges, &aliases);

    let build_opts = BuildOptions {
        directed: opts.directed,
        root: Some(root_str.clone()),
    };
    let mut kg = build_from_parts(nodes, edges, hyper, &build_opts);

    // Cross-file symbol resolution over the (re)extracted raw calls. Resolution
    // dedups against existing edges, so this only *adds* newly-resolvable calls
    // (the prior resolved edges are already preserved by the merge).
    let resolved = resolve_symbols(&kg, &raw_calls, &imports);
    if !resolved.is_empty() {
        let n: Vec<Node> = kg.nodes().cloned().collect();
        let mut e: Vec<Edge> = kg.edges().cloned().collect();
        e.extend(resolved);
        kg = build_from_parts(n, e, kg.hyperedges.clone(), &build_opts);
    }

    // Cross-language: retarget subprocess command stubs to in-repo file targets,
    // matching the one-shot `extract` path so an incremental update produces the
    // same edges (e.g. subprocess.run("tool") -> src/bin/tool.rs).
    let before_cmd = kg.node_count();
    let hyper = kg.hyperedges.clone();
    let (cn, ce) =
        resolve_command_invocations(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    if cn.len() < before_cmd {
        kg = build_from_parts(cn, ce, hyper, &build_opts);
    }

    // Cross-language: resolve named route handlers across files (axum), matching
    // the one-shot `extract` path. Before the parameterized-route merge.
    let before_edges = kg.edges().count();
    let hyper = kg.hyperedges.clone();
    let (hn, he) =
        resolve_route_handlers(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    if he.len() > before_edges {
        kg = build_from_parts(hn, he, hyper, &build_opts);
    }

    // Cross-language: collapse SQL table stubs into real table nodes, matching the
    // one-shot `extract` path.
    let before_sql = kg.node_count();
    let hyper = kg.hyperedges.clone();
    let (sn, se) =
        resolve_sql_queries(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    if sn.len() < before_sql {
        kg = build_from_parts(sn, se, hyper, &build_opts);
    }

    // Cross-language: merge concrete client route paths into the parameterized
    // server route they match, matching the one-shot `extract` path.
    let before_routes = kg.node_count();
    let hyper = kg.hyperedges.clone();
    let (rn, re) =
        resolve_parameterized_routes(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    if rn.len() < before_routes {
        kg = build_from_parts(rn, re, hyper, &build_opts);
    }

    // Cross-language pyo3: stitch #[pymodule] boundaries to their registered
    // #[pyfunction]/#[pyclass] definitions (across files), then connect Python
    // importers -- matching the one-shot `extract` path.
    let before_edges = kg.edges().count();
    let hyper = kg.hyperedges.clone();
    let (pn, pe) =
        resolve_pyo3_modules(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    let (pn, pe) = resolve_pyo3_imports(pn, pe);
    if pe.len() > before_edges {
        kg = build_from_parts(pn, pe, hyper, &build_opts);
    }

    // Dedup near-duplicate non-code entities (a no-op on a code-only graph).
    let before = kg.node_count();
    let (dn, de) = deduplicate_entities(
        kg.nodes().cloned().collect(),
        kg.edges().cloned().collect(),
        &HashMap::new(),
    );
    if dn.len() < before {
        kg = build_from_parts(dn, de, kg.hyperedges.clone(), &build_opts);
    }

    // Refuse a silent shrink (unless forced or an explicit deletion happened).
    let existing_n = existing.map(|g| g.nodes.len()).unwrap_or(0);
    guard_shrink(kg.node_count(), existing_n, opts.force, had_deletions)?;

    // No-change short-circuit: identical topology means reuse the previous
    // community assignment, skip re-clustering, and tell the caller nothing needs
    // rewriting.
    if let Some(prev) = existing {
        if topology(&kg.to_graph_data()) == topology(prev) {
            let mut communities: BTreeMap<u32, Vec<NodeId>> = BTreeMap::new();
            for (id, c) in previous_communities(prev) {
                communities.entry(c).or_default().push(id);
            }
            for v in communities.values_mut() {
                v.sort();
            }
            apply_communities(&mut kg, &communities);
            return Ok(RebuildOutcome {
                kg,
                communities,
                changed: false,
                reextracted,
                evicted_sources: evict_sources.len(),
            });
        }
    }

    // Cluster, remapping ids to the previous assignment for cross-build stability.
    let mut communities = cluster(&kg, &ClusterOptions::default());
    if let Some(prev) = existing {
        communities = remap_communities_to_previous(&communities, &previous_communities(prev));
    }
    apply_communities(&mut kg, &communities);

    Ok(RebuildOutcome {
        kg,
        communities,
        changed: true,
        reextracted,
        evicted_sources: evict_sources.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};
    use synaptic_core::{Confidence, FileType as CoreFt};

    fn node(id: &str, label: &str, sf: &str, origin: Option<&str>) -> Node {
        let mut extra = Map::new();
        if let Some(o) = origin {
            extra.insert("_origin".into(), json!(o));
        }
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: CoreFt::Code,
            source_file: sf.into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra,
        }
    }

    fn edge(s: &str, t: &str, rel: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            source_file: "x".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn graph_data(nodes: Vec<Node>, links: Vec<Edge>) -> GraphData {
        GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links,
            hyperedges: vec![],
            built_at_commit: None,
        }
    }

    #[test]
    fn merge_preserves_semantic_and_unchanged_evicts_changed() {
        // Existing: a.py AST (a_fn), b.py AST (b_fn), one semantic concept.
        let existing = graph_data(
            vec![
                node("a_fn", "a()", "a.py", Some("ast")),
                node("b_fn", "b()", "b.py", Some("ast")),
                node("concept_x", "Auth Flow", "doc.md", Some("semantic")),
            ],
            vec![edge("a_fn", "b_fn", "calls")],
        );
        // a.py changed: re-extract yields a_fn + a_new; b.py untouched.
        let fresh = vec![
            node("a_fn", "a()", "a.py", Some("ast")),
            node("a_new", "a2()", "a.py", Some("ast")),
        ];
        let evict: HashSet<String> = ["a.py".to_string()].into_iter().collect();
        let (nodes, edges, _) = merge_incremental(&existing, fresh, vec![], &evict, false);
        let ids: HashSet<&str> = nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(
            ids.contains("a_fn") && ids.contains("a_new"),
            "fresh a.py nodes"
        );
        assert!(ids.contains("b_fn"), "unchanged b.py preserved");
        assert!(ids.contains("concept_x"), "semantic node preserved");
        // The a_fn->b_fn calls edge survives (both endpoints live).
        assert!(edges
            .iter()
            .any(|e| e.source.0 == "a_fn" && e.target.0 == "b_fn"));
    }

    #[test]
    fn merge_drops_edges_to_deleted_nodes() {
        let existing = graph_data(
            vec![
                node("a_fn", "a()", "a.py", Some("ast")),
                node("b_fn", "b()", "b.py", Some("ast")),
            ],
            vec![edge("a_fn", "b_fn", "calls")],
        );
        // b.py deleted (evicted, no fresh replacement).
        let evict: HashSet<String> = ["b.py".to_string()].into_iter().collect();
        let (nodes, edges, _) = merge_incremental(&existing, vec![], vec![], &evict, false);
        let ids: HashSet<&str> = nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(ids.contains("a_fn") && !ids.contains("b_fn"));
        assert!(edges.is_empty(), "edge to deleted node dropped: {edges:?}");
    }

    #[test]
    fn full_rebuild_drops_stale_ast_keeps_semantic() {
        let existing = graph_data(
            vec![
                node("stale_fn", "old()", "gone.py", Some("ast")),
                node("concept_x", "Auth Flow", "doc.md", Some("semantic")),
            ],
            vec![],
        );
        // Full rebuild: fresh AST no longer includes stale_fn.
        let fresh = vec![node("a_fn", "a()", "a.py", Some("ast"))];
        let (nodes, _, _) = merge_incremental(&existing, fresh, vec![], &HashSet::new(), true);
        let ids: HashSet<&str> = nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(ids.contains("a_fn"), "fresh AST present");
        assert!(
            !ids.contains("stale_fn"),
            "stale AST dropped on full rebuild"
        );
        assert!(ids.contains("concept_x"), "semantic survives full rebuild");
    }

    #[test]
    fn full_rebuild_evicts_non_ast_nodes_for_deleted_code_files() {
        // The stale-AST drop alone misses a NON-AST node attached to a deleted
        // code file (#1007). A full rebuild must reconcile against current code
        // files and evict it, while leaving doc-sourced semantic nodes intact.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.py"), "def a():\n    return 1\n").unwrap();
        let existing = graph_data(
            vec![
                // Doc-sourced semantic node: must survive (not a code file).
                node("concept_x", "Auth Flow", "notes.md", Some("semantic")),
                // Non-AST node on a code file that no longer exists: must be evicted.
                node("stale_b", "Inferred B", "b.py", None),
            ],
            vec![],
        );
        let opts = RebuildOptions {
            root: root.to_path_buf(),
            directed: false,
            force: true, // the graph legitimately shrinks (b.py gone)
        };
        let out = rebuild(&opts, &ChangeSet::Full, Some(&existing)).unwrap();
        let gd = out.kg.to_graph_data();
        let ids: HashSet<&str> = gd.nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(
            !ids.contains("stale_b"),
            "non-AST node on a deleted code file must be evicted: {ids:?}"
        );
        assert!(
            ids.contains("concept_x"),
            "doc-sourced semantic node must survive a full rebuild: {ids:?}"
        );
        assert!(
            gd.nodes.iter().any(|n| n.source_file == "a.py"),
            "a.py is re-extracted: {ids:?}"
        );
    }

    #[test]
    fn topology_ignores_community_and_scores() {
        let a = graph_data(
            vec![node("x", "X", "x.py", Some("ast"))],
            vec![edge("x", "x", "calls")],
        );
        let mut b = a.clone();
        b.nodes[0].community = Some(99); // different community only
        assert_eq!(topology(&a), topology(&b));
    }

    // integration: the full rebuild orchestration on a real temp project

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn labels(kg: &KnowledgeGraph) -> HashSet<String> {
        kg.nodes().map(|n| n.label.clone()).collect()
    }

    #[test]
    fn rebuild_full_then_incremental_preserves_and_evicts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def a():\n    return 1\n");
        write(root, "b.py", "def b():\n    return 2\n");

        let opts = RebuildOptions {
            root: root.to_path_buf(),
            directed: false,
            force: false,
        };

        // Full build from scratch.
        let r1 = rebuild(&opts, &ChangeSet::Full, None).unwrap();
        assert!(r1.changed);
        let l1 = labels(&r1.kg);
        assert!(l1.contains("a()") && l1.contains("b()"), "{l1:?}");
        let existing = r1.kg.to_graph_data();

        // a.py changes (adds c()); b.py untouched. Incremental rebuild.
        write(
            root,
            "a.py",
            "def a():\n    return 1\n\n\ndef c():\n    return 3\n",
        );
        let r2 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("a.py")]),
            Some(&existing),
        )
        .unwrap();
        assert_eq!(r2.reextracted, 1, "only a.py re-extracted");
        assert!(r2.changed, "topology changed (c() added)");
        let l2 = labels(&r2.kg);
        assert!(l2.contains("c()"), "new function present: {l2:?}");
        assert!(l2.contains("a()"), "a() still present");
        assert!(l2.contains("b()"), "unchanged b.py preserved: {l2:?}");

        // Re-running with no actual change: topology unchanged, so changed=false.
        let existing2 = r2.kg.to_graph_data();
        let r3 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("a.py")]),
            Some(&existing2),
        )
        .unwrap();
        assert!(!r3.changed, "identical topology short-circuits");

        // Delete b.py and rebuild: b's nodes evicted.
        std::fs::remove_file(root.join("b.py")).unwrap();
        let r4 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("b.py")]),
            Some(&existing2),
        )
        .unwrap();
        let l4 = labels(&r4.kg);
        assert!(!l4.contains("b()"), "deleted file's node evicted: {l4:?}");
        assert!(
            l4.contains("a()") && l4.contains("c()"),
            "a.py survives: {l4:?}"
        );
    }
}
