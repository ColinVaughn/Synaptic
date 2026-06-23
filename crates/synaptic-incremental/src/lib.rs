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
pub mod freshen;
pub mod hooks;
pub mod merge_driver;
pub mod watch;
pub use concurrency::{
    drain_pending, merge_changed_paths, queue_pending, try_acquire_lock, RebuildLock,
};
pub use freshen::{
    detect_changes, graph_input_files, is_extractable_markdown, manifest_path, persist_manifest,
    persist_manifest_with, ChangeReport,
};
pub use merge_driver::{run_merge_driver, union_graphs, MergeDriverError};
pub use watch::{should_ignore_path, ChangeBatch, DEBOUNCE_MS};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use synaptic_core::{Edge, GraphData, Hyperedge, Node, NodeId};
use synaptic_detect::{classify_file, detect, DetectResult, FileType};
use synaptic_extract::cached_extract_source;
use synaptic_graph::{
    apply_communities, build_from_parts, cluster, deduplicate_entities, guard_shrink,
    link_dynamic_refs, norm_source_file, remap_communities_to_previous,
    resolve_command_invocations, resolve_parameterized_routes, resolve_pyo3_imports,
    resolve_pyo3_modules, resolve_route_handlers, resolve_sql_queries, resolve_symbols,
    BuildOptions, ClusterOptions, KnowledgeGraph,
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
/// - an existing edge is preserved iff both endpoints are still live AND its
///   source node's file was not evicted (so prior cross-file edges from
///   *unchanged* files survive, but a re-extracted file's own outgoing edges are
///   dropped and regenerated rather than union-merged onto the surviving node);
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

    // Node ids whose *defining file* was evicted (re-extracted or deleted). Their
    // stale OUTGOING edges must be dropped: a re-extracted file's edges come back
    // fresh via `fresh_edges` and the post-merge resolution passes, so keeping the
    // old ones would union-merge stale edges onto a re-extracted node (e.g. a call
    // retargeted announce() -> log() would leave a phantom announce edge because
    // the callee node still lives). This is keyed on the source NODE's
    // `source_file` -- the same predicate node eviction uses -- NOT the edge's own
    // `source_file`, because a resolved cross-file edge can carry a
    // differently-normalized source_file (absolute vs repo-relative) than the node
    // it originates from, which would slip past an edge-keyed filter.
    let evicted_node_ids: HashSet<NodeId> = existing
        .nodes
        .iter()
        .filter(|n| evict_sources.contains(&n.source_file))
        .map(|n| n.id.clone())
        .collect();

    let preserved_edges: Vec<Edge> = existing
        .links
        .iter()
        .filter(|e| {
            // Keep an edge iff both endpoints survive AND it does not originate
            // from an evicted file (by source node, with the edge's own
            // source_file as a belt-and-suspenders fallback).
            live_ids.contains(&e.source)
                && live_ids.contains(&e.target)
                && !evicted_node_ids.contains(&e.source)
                && !evict_sources.contains(&e.source_file)
        })
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
    // `detect` canonicalizes the root internally and exposes it as `scan_root`.
    let det = detect(&opts.root);
    rebuild_with_detect(opts, changes, existing, &det)
}

/// Like [`rebuild`] but reuses an existing detect result instead of walking the
/// tree again -- for the serve catch-up, which already scanned to discover the
/// change set. `det.scan_root` is the canonicalized root, so the produced graph
/// is identical to a fresh `rebuild` (the scan is the only thing reused).
pub fn rebuild_with_detect(
    opts: &RebuildOptions,
    changes: &ChangeSet,
    existing: Option<&GraphData>,
    det: &DetectResult,
) -> Result<RebuildOutcome, IncrementalError> {
    // The canonical root keeps `root.join(rel)` and rel ids/source_files
    // identical to a full `synaptic extract` (matters for changed-path matching).
    let root = det.scan_root.as_path();
    let root_str = root.to_string_lossy().into_owned();
    let code_files: Vec<PathBuf> = det.of(FileType::Code).to_vec();
    // Markdown is a Document (not Code), but it gets structural heading
    // extraction too, so extract it alongside code in every rebuild path
    // (update/watch/workspace), matching `synaptic extract`. (.NET/apex/etc. are
    // Code, so they already flow through `code_files`.)
    let md_files: Vec<PathBuf> = det
        .of(FileType::Document)
        .iter()
        .filter(|p| freshen::is_extractable_markdown(p))
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

    // Dynamic-dispatch evidence-link (matches the one-shot `extract` path).
    let before_edges = kg.edges().count();
    let (yn, ye) = link_dynamic_refs(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    if ye.len() > before_edges {
        kg = build_from_parts(yn, ye, kg.hyperedges.clone(), &build_opts);
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
    // An incremental rebuild is scoped to explicitly-changed files, so any shrink
    // is bounded to them and expected (e.g. an edit that removes a method), and
    // is authorized here; the strict guard still protects full rebuilds, where a
    // shrink signals a catastrophic empty extraction.
    let incremental = matches!(changes, ChangeSet::Incremental(_));
    let existing_n = existing.map(|g| g.nodes.len()).unwrap_or(0);
    guard_shrink(
        kg.node_count(),
        existing_n,
        opts.force,
        had_deletions || incremental,
    )?;

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

    fn edge_sf(s: &str, t: &str, rel: &str, sf: &str) -> Edge {
        Edge {
            source_file: sf.into(),
            ..edge(s, t, rel)
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
            // Edge from the UNCHANGED file b.py into a.py: a legitimately
            // preserved cross-file edge (its source node is not re-extracted).
            vec![edge_sf("b_fn", "a_fn", "calls", "b.py")],
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
        // The b_fn->a_fn edge from unchanged b.py survives (its source node was
        // not re-extracted, and both endpoints are live).
        assert!(edges
            .iter()
            .any(|e| e.source.0 == "b_fn" && e.target.0 == "a_fn"));
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
    fn merge_replaces_outgoing_edges_of_reextracted_file() {
        // A node that survives a re-extract must not keep its *old* outgoing
        // edges. When a.py is re-extracted its fresh edges come back via
        // `fresh_edges` (and post-merge resolution), so every prior edge whose
        // `source_file` was evicted must be dropped -- not union-merged with the
        // fresh set. Otherwise changing a call announce() -> log() leaves a
        // phantom caller->announce edge because the callee node still lives,
        // which inflates affected/predict_impact blast radius.
        let existing = graph_data(
            vec![
                node("caller", "caller()", "a.py", Some("ast")),
                node("announce", "announce()", "lib.py", Some("ast")),
                node("log", "log()", "lib.py", Some("ast")),
            ],
            vec![edge_sf("caller", "announce", "calls", "a.py")],
        );
        // Re-extract a.py: caller now calls log() instead of announce().
        let fresh = vec![node("caller", "caller()", "a.py", Some("ast"))];
        let fresh_edges = vec![edge_sf("caller", "log", "calls", "a.py")];
        let evict: HashSet<String> = ["a.py".to_string()].into_iter().collect();
        let (_, edges, _) = merge_incremental(&existing, fresh, fresh_edges, &evict, false);
        assert!(
            edges
                .iter()
                .any(|e| e.source.0 == "caller" && e.target.0 == "log"),
            "fresh caller->log edge present: {edges:?}"
        );
        assert!(
            !edges
                .iter()
                .any(|e| e.source.0 == "caller" && e.target.0 == "announce"),
            "stale caller->announce edge from re-extracted a.py must be dropped: {edges:?}"
        );
    }

    #[test]
    fn merge_drops_edges_from_reextracted_node_even_if_edge_source_file_differs() {
        // Root cause of the end-to-end regression: a resolved cross-file call edge
        // can carry a `source_file` normalized differently from the node it
        // originates from (e.g. absolute vs repo-relative), so keying eviction on
        // the EDGE's source_file misses it. Eviction must key on the source NODE's
        // file instead -- the probe node lives in the re-extracted file, so its
        // stale outgoing edges must be dropped regardless of the edge's own
        // source_file string.
        let existing = graph_data(
            vec![
                node("caller", "caller()", "src/a.ts", Some("ast")),
                node("announce", "announce()", "src/lib.ts", Some("ast")),
                node("log", "log()", "src/lib.ts", Some("ast")),
            ],
            // The stale edge's source_file is an ABSOLUTE path, not the
            // repo-relative "src/a.ts" that eviction normalizes to.
            vec![edge_sf("caller", "announce", "calls", "/abs/root/src/a.ts")],
        );
        let fresh = vec![node("caller", "caller()", "src/a.ts", Some("ast"))];
        let fresh_edges = vec![edge_sf("caller", "log", "calls", "src/a.ts")];
        let evict: HashSet<String> = ["src/a.ts".to_string()].into_iter().collect();
        let (_, edges, _) = merge_incremental(&existing, fresh, fresh_edges, &evict, false);
        assert!(
            edges.iter().any(|e| e.target.0 == "log"),
            "fresh edge present: {edges:?}"
        );
        assert!(
            !edges.iter().any(|e| e.target.0 == "announce"),
            "stale edge from a re-extracted node must drop despite a differing edge.source_file: {edges:?}"
        );
    }

    #[test]
    fn merge_keeps_cross_file_edge_whose_source_file_is_unchanged() {
        // The flip side of the rule above: an edge from a file that was NOT
        // re-extracted must survive even when one endpoint lives in a
        // re-extracted file (its target id is stable). Pruning is keyed on the
        // edge's own `source_file`, not its endpoints.
        let existing = graph_data(
            vec![
                node("a_fn", "a()", "a.py", Some("ast")),
                node("b_fn", "b()", "b.py", Some("ast")),
            ],
            // b.py calls into a.py; b.py is untouched this round.
            vec![edge_sf("b_fn", "a_fn", "calls", "b.py")],
        );
        let fresh = vec![node("a_fn", "a()", "a.py", Some("ast"))];
        let evict: HashSet<String> = ["a.py".to_string()].into_iter().collect();
        let (_, edges, _) = merge_incremental(&existing, fresh, vec![], &evict, false);
        assert!(
            edges
                .iter()
                .any(|e| e.source.0 == "b_fn" && e.target.0 == "a_fn"),
            "edge from unchanged b.py preserved: {edges:?}"
        );
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
    fn incremental_allows_removing_a_symbol_from_an_existing_file() {
        // Editing a file to drop a whole symbol is a net node shrink, but it is
        // bounded to that explicitly-changed file and expected, so an incremental
        // rebuild must apply it. The shrink guard only protects full rebuilds
        // (the catastrophic empty-extraction case).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "a.py",
            "def keep():\n    return 1\n\n\ndef drop_me():\n    return 2\n",
        );
        let opts = RebuildOptions {
            root: root.to_path_buf(),
            directed: false,
            force: false,
        };
        let r1 = rebuild(&opts, &ChangeSet::Full, None).unwrap();
        let existing = r1.kg.to_graph_data();
        assert!(
            labels(&r1.kg).contains("drop_me()"),
            "symbol present at first"
        );

        // Remove drop_me() from the still-existing file.
        write(root, "a.py", "def keep():\n    return 1\n");
        let r2 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("a.py")]),
            Some(&existing),
        )
        .expect("incremental rebuild must apply a bounded shrink, not error");
        let l = labels(&r2.kg);
        assert!(!l.contains("drop_me()"), "removed symbol is gone: {l:?}");
        assert!(l.contains("keep()"), "kept symbol survives: {l:?}");
        assert!(r2.changed, "topology changed");
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

    /// Sorted `calls` targets (by label) out of the node labelled `caller`.
    fn call_targets(kg: &KnowledgeGraph, caller: &str) -> Vec<String> {
        let Some(cid) = kg.nodes().find(|n| n.label == caller).map(|n| n.id.clone()) else {
            return vec![];
        };
        let mut v: Vec<String> = kg
            .edges()
            .filter(|e| e.source == cid && e.relation == "calls")
            .map(|e| {
                kg.node(&e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| e.target.0.clone())
            })
            .collect();
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn incremental_retargeting_a_call_replaces_the_old_edge() {
        // End-to-end repro of the edge-accumulation bug through the FULL rebuild
        // path (extraction -> merge -> symbol resolution), not just
        // merge_incremental: probe() in main.py calls a cross-file function;
        // retargeting that call across two incremental rebuilds must leave only
        // the latest edge, never the union announce+warn+log.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "lib.py",
            "def announce():\n    return 1\n\n\ndef warn():\n    return 2\n\n\ndef log():\n    return 3\n",
        );
        let main_calling = |callee: &str| {
            format!("from lib import announce, warn, log\n\n\ndef probe():\n    {callee}()\n")
        };
        write(root, "main.py", &main_calling("announce"));
        let opts = RebuildOptions {
            root: root.to_path_buf(),
            directed: false,
            force: false,
        };

        let r1 = rebuild(&opts, &ChangeSet::Full, None).unwrap();
        assert_eq!(
            call_targets(&r1.kg, "probe()"),
            vec!["announce()".to_string()],
            "step 1: probe calls announce only"
        );
        let mut existing = r1.kg.to_graph_data();

        // Step 2: retarget announce -> warn.
        write(root, "main.py", &main_calling("warn"));
        let r2 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("main.py")]),
            Some(&existing),
        )
        .unwrap();
        assert_eq!(
            call_targets(&r2.kg, "probe()"),
            vec!["warn()".to_string()],
            "step 2: the announce edge must be replaced, not unioned"
        );
        existing = r2.kg.to_graph_data();

        // Step 3: retarget warn -> log.
        write(root, "main.py", &main_calling("log"));
        let r3 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("main.py")]),
            Some(&existing),
        )
        .unwrap();
        assert_eq!(
            call_targets(&r3.kg, "probe()"),
            vec!["log()".to_string()],
            "step 3: only the latest call edge survives (no announce/warn residue)"
        );
    }

    #[test]
    fn incremental_retargeting_a_ts_call_replaces_the_old_edge() {
        // Same repro as above but in TypeScript (the language of the a11ycore
        // repo where the bug was reported). TS call edges can be emitted at
        // extraction rather than via cross-file symbol resolution, so this
        // exercises a different code path than the Python case.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "lib.ts",
            "export function announce(): void {}\nexport function warn(): void {}\nexport function log(): void {}\n",
        );
        let main_calling = |callee: &str| {
            format!(
                "import {{ announce, warn, log }} from './lib';\n\nexport function probe(): void {{\n  {callee}();\n}}\n"
            )
        };
        write(root, "main.ts", &main_calling("announce"));
        let opts = RebuildOptions {
            root: root.to_path_buf(),
            directed: false,
            force: false,
        };

        let r1 = rebuild(&opts, &ChangeSet::Full, None).unwrap();
        let t1 = call_targets(&r1.kg, "probe()");
        assert!(
            t1.iter().any(|t| t.contains("announce")),
            "step 1: probe calls announce: {t1:?}"
        );
        let mut existing = r1.kg.to_graph_data();

        write(root, "main.ts", &main_calling("warn"));
        let r2 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("main.ts")]),
            Some(&existing),
        )
        .unwrap();
        let t2 = call_targets(&r2.kg, "probe()");
        assert!(
            t2.iter().any(|t| t.contains("warn")) && !t2.iter().any(|t| t.contains("announce")),
            "step 2: announce edge replaced by warn, not unioned: {t2:?}"
        );
        existing = r2.kg.to_graph_data();

        write(root, "main.ts", &main_calling("log"));
        let r3 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("main.ts")]),
            Some(&existing),
        )
        .unwrap();
        let t3 = call_targets(&r3.kg, "probe()");
        assert!(
            t3.iter().any(|t| t.contains("log"))
                && !t3
                    .iter()
                    .any(|t| t.contains("announce") || t.contains("warn")),
            "step 3: only the latest call edge survives (no announce/warn residue): {t3:?}"
        );
    }
}
