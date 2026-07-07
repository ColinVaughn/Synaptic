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

pub mod callnames;
pub mod concurrency;
pub mod freshen;
pub mod hooks;
pub mod merge_driver;
pub mod watch;
pub use callnames::{
    bare_name, call_names, callnames_path, from_raw_calls, load_callnames, save_callnames,
    CallNames,
};
pub use concurrency::{
    drain_pending, drain_queued_rounds, merge_changed_paths, queue_pending, try_acquire_lock,
    RebuildLock,
};
pub use freshen::{
    detect_changes, graph_input_files, is_extractable_markdown, manifest_path, persist_manifest,
    persist_manifest_with, snapshot_manifest, ChangeReport,
};
pub use merge_driver::{run_merge_driver, union_graphs, MergeDriverError};
pub use watch::{is_rebuildable, should_ignore_path, ChangeBatch, DEBOUNCE_MS};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use synaptic_core::{Edge, GraphData, Hyperedge, Node, NodeId};
use synaptic_detect::{classify_file, detect_inputs, DetectResult, FileType, Manifest};
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

    // On a FULL rebuild every file is re-extracted, so every extraction-owned
    // old edge comes back fresh; preserving the old set too would union stale
    // edges (a retargeted call keeps its phantom old edge because both
    // endpoints still live). Extraction-owned means BOTH endpoints are AST:
    // an edge touching a semantic node in either direction (including a
    // ghost-remapped concept link sourced at a code symbol) has no fresh
    // replacement and survives.
    let ast_ids: HashSet<&NodeId> = if full_rebuild {
        existing
            .nodes
            .iter()
            .filter(|n| is_ast(n))
            .map(|n| &n.id)
            .collect()
    } else {
        HashSet::new()
    };

    let preserved_edges: Vec<Edge> = existing
        .links
        .iter()
        .filter(|e| {
            // Keep an edge iff both endpoints survive AND it does not originate
            // from an evicted file (by source node, with the edge's own
            // source_file as a belt-and-suspenders fallback) AND, on a full
            // rebuild, it is not extraction-owned (both endpoints AST).
            live_ids.contains(&e.source)
                && live_ids.contains(&e.target)
                && !evicted_node_ids.contains(&e.source)
                && !evict_sources.contains(&e.source_file)
                && !(ast_ids.contains(&e.source) && ast_ids.contains(&e.target))
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

/// A small incremental delta may skip the full re-cluster and place new nodes
/// locally instead. Above this many unassigned nodes, re-cluster from scratch.
const LOCAL_ASSIGN_MAX: usize = 64;

/// Cap on how many unchanged files a single rebuild ripple-re-extracts when a
/// (re)introduced symbol name is widely referenced. Cache hits keep each cheap;
/// the cap bounds the pathological case (a name like `get` returning).
const RIPPLE_MAX: usize = 512;

/// Community assignment for a small incremental delta: every previously
/// assigned node keeps its exact community (no re-cluster, so ids never churn
/// mid-session); a new node joins the majority community among its neighbors
/// (two propagation rounds, so a new node chained to another new node still
/// lands with the group); anything still unplaced opens a fresh community.
/// Full rebuilds and large deltas re-cluster properly, bounding quality drift.
fn assign_locally(kg: &KnowledgeGraph, prev: &HashMap<NodeId, u32>) -> BTreeMap<u32, Vec<NodeId>> {
    let mut assign: HashMap<NodeId, u32> = kg
        .nodes()
        .filter_map(|n| prev.get(&n.id).map(|c| (n.id.clone(), *c)))
        .collect();
    let mut fresh: Vec<NodeId> = kg
        .nodes()
        .filter(|n| !assign.contains_key(&n.id))
        .map(|n| n.id.clone())
        .collect();
    fresh.sort();

    // Undirected adjacency over the new-node set only.
    let mut adj: HashMap<&NodeId, Vec<&NodeId>> = HashMap::new();
    let fresh_set: HashSet<&NodeId> = fresh.iter().collect();
    for e in kg.edges() {
        if fresh_set.contains(&e.source) {
            adj.entry(&e.source).or_default().push(&e.target);
        }
        if fresh_set.contains(&e.target) {
            adj.entry(&e.target).or_default().push(&e.source);
        }
    }

    for _ in 0..2 {
        for id in &fresh {
            if assign.contains_key(id) {
                continue;
            }
            let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
            for nb in adj.get(id).into_iter().flatten() {
                if let Some(c) = assign.get(*nb) {
                    *counts.entry(*c).or_default() += 1;
                }
            }
            // Majority community; ties break to the smallest id (BTreeMap order).
            if let Some((c, _)) = counts
                .iter()
                .max_by_key(|(c, n)| (**n, std::cmp::Reverse(**c)))
            {
                assign.insert(id.clone(), *c);
            }
        }
    }

    let mut next = prev.values().copied().max().map_or(0, |m| m + 1);
    for id in &fresh {
        if !assign.contains_key(id) {
            assign.insert(id.clone(), next);
            next += 1;
        }
    }

    let mut communities: BTreeMap<u32, Vec<NodeId>> = BTreeMap::new();
    for (id, c) in assign {
        communities.entry(c).or_default().push(id);
    }
    for v in communities.values_mut() {
        v.sort();
    }
    communities
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
    /// Manifest keys (repo-relative, POSIX) of changed files that could not be
    /// read this round (editor/AV lock); their prior nodes were kept and their
    /// keys are already dropped from [`manifest`](Self::manifest) so they
    /// re-detect and retry next round. Exposed for caller-side logging.
    pub unreadable: Vec<String>,
    /// The provenance manifest to persist once the graph is written: the prior
    /// manifest advanced for exactly what this rebuild ingested (entries for
    /// the targets hashed BEFORE extraction, deleted keys removed, unreadable
    /// keys dropped). Files outside the change set keep their prior entries,
    /// so an edit the caller didn't list still diffs as changed later instead
    /// of being stamped as ingested.
    pub manifest: Manifest,
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
    // `detect_inputs` canonicalizes the root internally and exposes it as
    // `scan_root`; stats-free (no corpus word count) since a rebuild only needs
    // the classified file lists.
    let det = detect_inputs(&opts.root);
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
    let mut deleted_keys: Vec<String> = Vec::new();
    // Manifest-key derivation shared with synaptic-detect so unreadable /
    // deleted / sidecar keys always match the manifest's own keys.
    let rel_key = |p: &Path| synaptic_detect::relative_key(p, root);
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
                    deleted_keys.push(rel_key(&abs));
                }
            }
            wanted
        }
    };
    // Provenance manifest, advanced BEFORE extraction so an edit landing
    // mid-rebuild still diffs as changed once this manifest is saved. A full
    // rebuild snapshots the whole input set; an incremental one refreshes only
    // the target entries -- a file changed on disk but NOT in the change set
    // keeps its prior entry and stays detectable, instead of being stamped as
    // ingested without ever being extracted.
    let out_dir = root.join("synaptic-out");
    let prior_manifest = Manifest::load(&manifest_path(&out_dir));
    let fresh_entries =
        Manifest::build_incremental(targets.iter().map(PathBuf::as_path), root, &prior_manifest);
    let mut manifest = if full_rebuild {
        fresh_entries
    } else {
        let mut m = prior_manifest;
        for k in &deleted_keys {
            m.0.remove(k);
        }
        m.0.extend(fresh_entries.0);
        m
    };

    // Extract targets in parallel, ordered for determinism, with portable rel
    // ids. A read failure is kept distinct from "unsupported extension": the
    // file exists but is momentarily unreadable (editor/AV lock), and treating
    // it as deleted would silently evict its symbols.
    let cache_dir = out_dir.join("cache");
    let extracted: Vec<(Option<_>, bool)> = targets
        .par_iter()
        .map(|file| {
            let rel = file.strip_prefix(root).unwrap_or(file);
            let rel_str = rel.to_string_lossy();
            match std::fs::read(file) {
                Ok(bytes) => (
                    cached_extract_source(Some(&cache_dir), rel_str.as_ref(), &bytes),
                    false,
                ),
                Err(_) => (None, true),
            }
        })
        .collect();
    // Per-file called-name sidecar: rebuilt whole on a full rebuild, otherwise
    // advanced for this round's extractions and deletions (an unreadable file
    // keeps its prior entry, like its nodes). Saved only when something
    // actually changed -- serve-path rounds are frequent and usually no-ops.
    let mut callnames = if full_rebuild {
        CallNames::new()
    } else {
        load_callnames(&out_dir)
    };
    let mut callnames_dirty = full_rebuild;
    for k in &deleted_keys {
        callnames_dirty |= callnames.remove(k).is_some();
    }

    let mut fresh_nodes = Vec::new();
    let mut fresh_edges = Vec::new();
    let mut raw_calls = Vec::new();
    let mut imports = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    for (file, (res, read_failed)) in targets.iter().zip(extracted) {
        if read_failed {
            // Keep the prior graph's view of this file: re-inject its old nodes
            // and outgoing edges as if freshly extracted (covers both change
            // modes: eviction drops the old copies, the full-rebuild rule drops
            // old AST edges, and these fresh clones survive either way).
            if let Some(ex) = existing {
                let norm = norm_source_file(&file.to_string_lossy(), Some(&root_str));
                let ids: HashSet<&NodeId> = ex
                    .nodes
                    .iter()
                    .filter(|n| norm_source_file(&n.source_file, Some(&root_str)) == norm)
                    .map(|n| &n.id)
                    .collect();
                fresh_edges.extend(ex.links.iter().filter(|e| ids.contains(&e.source)).cloned());
                fresh_nodes.extend(ex.nodes.iter().filter(|n| ids.contains(&n.id)).cloned());
            }
            unreadable.push(rel_key(file));
            continue;
        }
        let Some(r) = res else { continue };
        let key = rel_key(file);
        let names = call_names(&r.raw_calls);
        if callnames.get(&key) != Some(&names) {
            callnames.insert(key, names);
            callnames_dirty = true;
        }
        fresh_nodes.extend(r.nodes);
        fresh_edges.extend(r.edges);
        raw_calls.extend(r.raw_calls);
        imports.extend(r.imports);
    }
    let reextracted = targets.len() - unreadable.len();
    for key in &unreadable {
        // The unreadable file kept its old nodes; drop it from the manifest so
        // it re-detects and retries instead of going stale.
        manifest.0.remove(key);
    }

    // Ripple: a symbol name this change (re)introduces may be referenced by raw
    // calls in UNCHANGED files (a definition added after its caller, a rename
    // back, a move to another file). Those calls exist only in each file's own
    // extraction, so re-extract the sidecar-indexed candidates (unchanged
    // content = AST-cache hits) and feed their calls to resolution, which only
    // ADDS edges. New-ness is judged by node ID, which encodes the file, so a
    // move also triggers.
    if matches!(changes, ChangeSet::Incremental(_)) {
        if let Some(ex) = existing {
            let existing_ids: HashSet<&NodeId> = ex.nodes.iter().map(|n| &n.id).collect();
            let introduced: HashSet<&str> = fresh_nodes
                .iter()
                .filter(|n| is_ast(n) && !existing_ids.contains(&n.id))
                .map(|n| bare_name(&n.label))
                .collect();
            if !introduced.is_empty() {
                let extracted_keys: HashSet<String> = targets.iter().map(|f| rel_key(f)).collect();
                let mut candidates: Vec<&String> = callnames
                    .iter()
                    .filter(|(k, names)| {
                        !extracted_keys.contains(k.as_str())
                            && names.iter().any(|n| introduced.contains(n.as_str()))
                    })
                    .map(|(k, _)| k)
                    .collect();
                if candidates.len() > RIPPLE_MAX {
                    eprintln!(
                        "note: {} files reference newly introduced symbols; re-resolving only the first {RIPPLE_MAX}",
                        candidates.len()
                    );
                    candidates.truncate(RIPPLE_MAX);
                }
                // Rebuild the OS-separator rel path the original extraction
                // used: the path string feeds both the AST-cache key and the
                // extractor's node ids, so it must match exactly.
                let rippled: Vec<_> = candidates
                    .par_iter()
                    .filter_map(|key| {
                        let rel_os = key.replace('/', std::path::MAIN_SEPARATOR_STR);
                        let abs = root.join(&rel_os);
                        let bytes = std::fs::read(&abs).ok()?;
                        cached_extract_source(Some(&cache_dir), &rel_os, &bytes)
                    })
                    .collect();
                for r in rippled {
                    raw_calls.extend(r.raw_calls);
                    imports.extend(r.imports);
                }
            }
        }
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
    // Bind resource references + emit generated-shadow edges over the full merged
    // set, matching the full-extract pipeline (parity is asserted in tests).
    synaptic_extract::resolve_resource_refs(&mut nodes, &mut edges);

    let build_opts = BuildOptions {
        directed: opts.directed,
        root: Some(root_str.clone()),
    };
    let mut kg = build_from_parts(nodes, edges, hyper, &build_opts);

    // Cross-file symbol resolution over the (re)extracted raw calls needs the
    // built graph's index. Resolution dedups against existing edges, so this
    // only *adds* newly-resolvable calls (the prior resolved edges are already
    // preserved by the merge).
    let resolved = resolve_symbols(&kg, &raw_calls, &imports);

    // The remaining passes are pure (nodes, edges) -> (nodes, edges)
    // transforms, so run them chained on owned vecs and rebuild ONCE at the
    // end, instead of cloning + rebuilding the whole graph after each pass.
    // Same pass order as the one-shot `extract`: subprocess commands, named
    // route handlers (before the parameterized-route merge), SQL table stubs,
    // parameterized routes, pyo3 modules + imports, dynamic-dispatch evidence,
    // entity dedup.
    let hyper = kg.hyperedges.clone();
    let n: Vec<Node> = kg.nodes().cloned().collect();
    let mut e: Vec<Edge> = kg.edges().cloned().collect();
    e.extend(resolved);
    let (n, e) = resolve_command_invocations(n, e);
    let (n, e) = resolve_route_handlers(n, e);
    let (n, e) = resolve_sql_queries(n, e);
    let (n, e) = resolve_parameterized_routes(n, e);
    let (n, e) = resolve_pyo3_modules(n, e);
    let (n, e) = resolve_pyo3_imports(n, e);
    let (n, e) = link_dynamic_refs(n, e);
    let (n, e) = deduplicate_entities(n, e, &HashMap::new());
    kg = build_from_parts(n, e, hyper, &build_opts);

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

    // The sidecar reflects extraction facts, so it advances with the manifest
    // semantics: only after the rebuild is known good (best-effort write).
    if callnames_dirty {
        if let Err(e) = save_callnames(&out_dir, &callnames) {
            eprintln!("note: could not write call-name sidecar: {e}");
        }
    }

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
                unreadable,
                manifest,
            });
        }
    }

    // Cluster. A small incremental delta keeps the previous assignment and
    // places only the new nodes (exact id stability, O(delta) instead of a full
    // re-cluster); full rebuilds and large deltas re-cluster from scratch,
    // remapped to previous ids, so clustering quality never drifts far.
    let prev = existing.map(previous_communities).unwrap_or_default();
    let unassigned = kg.nodes().filter(|n| !prev.contains_key(&n.id)).count();
    let communities = if incremental && !prev.is_empty() && unassigned <= LOCAL_ASSIGN_MAX {
        assign_locally(&kg, &prev)
    } else {
        let mut c = cluster(&kg, &ClusterOptions::default());
        if !prev.is_empty() {
            c = remap_communities_to_previous(&c, &prev);
        }
        c
    };
    apply_communities(&mut kg, &communities);

    Ok(RebuildOutcome {
        kg,
        communities,
        changed: true,
        reextracted,
        evicted_sources: evict_sources.len(),
        unreadable,
        manifest,
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
    fn full_rebuild_replaces_edges_not_unions_them() {
        // A FULL rebuild re-extracts every file, so every old AST-sourced edge
        // comes back fresh; preserving the old set too would union stale edges
        // (e.g. a call retargeted announce() -> log() across a branch switch
        // keeps a phantom announce edge because both endpoints still live).
        let existing = graph_data(
            vec![
                node("caller", "caller()", "a.py", Some("ast")),
                node("announce", "announce()", "lib.py", Some("ast")),
                node("log", "log()", "lib.py", Some("ast")),
                node("concept_x", "Auth Flow", "doc.md", Some("semantic")),
            ],
            vec![
                edge_sf("caller", "announce", "calls", "a.py"),
                // Semantic-node edge: must survive (its source is not AST).
                edge_sf("concept_x", "caller", "mentions", "doc.md"),
                // Ghost-remap can leave a semantic edge SOURCED at an AST node
                // (concept merged onto a code symbol). Extraction never
                // regenerates it, so it must survive a full rebuild too: only
                // AST-to-AST edges are extraction-owned and replaceable.
                edge_sf("caller", "concept_x", "conceptually_related_to", "doc.md"),
            ],
        );
        // Full re-extract: caller now calls log().
        let fresh = vec![
            node("caller", "caller()", "a.py", Some("ast")),
            node("announce", "announce()", "lib.py", Some("ast")),
            node("log", "log()", "lib.py", Some("ast")),
        ];
        let fresh_edges = vec![edge_sf("caller", "log", "calls", "a.py")];
        let (_, edges, _) = merge_incremental(&existing, fresh, fresh_edges, &HashSet::new(), true);
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
            "stale caller->announce edge must not survive a full rebuild: {edges:?}"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.source.0 == "concept_x" && e.target.0 == "caller"),
            "semantic-sourced edge survives a full rebuild: {edges:?}"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.source.0 == "caller" && e.target.0 == "concept_x"),
            "AST-to-semantic edge (ghost-remapped concept link) survives a full rebuild: {edges:?}"
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
    fn local_assignment_places_new_nodes_with_their_neighbors() {
        // A small incremental delta must not re-cluster the whole graph: prior
        // nodes keep their exact community, a new node joins the majority
        // community among its neighbors, and a disconnected new node opens a
        // fresh community (max previous id + 1).
        let nodes = vec![
            node("x1", "x1()", "x.py", Some("ast")),
            node("x2", "x2()", "x.py", Some("ast")),
            node("y1", "y1()", "y.py", Some("ast")),
            node("n1", "n1()", "x.py", Some("ast")),
            node("n2", "n2()", "z.py", Some("ast")),
        ];
        let edges = vec![edge("x1", "x2", "calls"), edge("n1", "x2", "calls")];
        let kg = build_from_parts(
            nodes,
            edges,
            vec![],
            &BuildOptions {
                directed: false,
                root: None,
            },
        );
        let prev: HashMap<NodeId, u32> = [
            (NodeId("x1".into()), 5),
            (NodeId("x2".into()), 5),
            (NodeId("y1".into()), 9),
        ]
        .into_iter()
        .collect();

        let communities = assign_locally(&kg, &prev);
        let of = |id: &str| {
            communities
                .iter()
                .find(|(_, v)| v.iter().any(|n| n.0 == id))
                .map(|(c, _)| *c)
        };
        assert_eq!(of("x1"), Some(5), "prior nodes keep their community");
        assert_eq!(of("x2"), Some(5));
        assert_eq!(of("y1"), Some(9));
        assert_eq!(of("n1"), Some(5), "new node joins its neighbors' community");
        assert_eq!(of("n2"), Some(10), "disconnected new node opens max+1");
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

    #[cfg(windows)]
    #[test]
    fn unreadable_changed_file_keeps_its_nodes_and_reports_it() {
        // Editors/AV briefly hold exclusive locks during save. A changed file
        // that cannot be read must NOT be silently evicted (the incremental
        // path bypasses the shrink guard); its prior nodes are kept and the
        // file is reported so the caller drops it from the provenance manifest
        // and retries next round.
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def a():\n    return 1\n");
        write(root, "b.py", "def b():\n    return 2\n");
        let opts = RebuildOptions {
            root: root.to_path_buf(),
            directed: false,
            force: false,
        };
        let r1 = rebuild(&opts, &ChangeSet::Full, None).unwrap();
        let existing = r1.kg.to_graph_data();

        // Exclusive lock: any other open (incl. our fs::read) now fails.
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(root.join("b.py"))
            .unwrap();
        let r2 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("b.py")]),
            Some(&existing),
        )
        .unwrap();
        let l = labels(&r2.kg);
        assert!(l.contains("b()"), "locked file's nodes must survive: {l:?}");
        assert_eq!(
            r2.unreadable,
            vec!["b.py".to_string()],
            "unreadable file reported for manifest retry"
        );
    }

    #[test]
    fn readable_rebuild_reports_no_unreadable_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def a():\n    return 1\n");
        let opts = RebuildOptions {
            root: root.to_path_buf(),
            directed: false,
            force: false,
        };
        let r = rebuild(&opts, &ChangeSet::Full, None).unwrap();
        assert!(r.unreadable.is_empty());
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
    fn incremental_rebuild_manifest_advances_only_the_change_set() {
        // a.py and b.py both change on disk, but only a.py is in the change
        // set (e.g. the post-commit hook lists committed files while b.py has
        // an uncommitted edit). The manifest the rebuild returns must keep
        // b.py's PRIOR entry so b.py still diffs as changed later; a
        // whole-tree snapshot would stamp b.py's new state as ingested
        // without ever extracting it -- permanently invisible now that bare
        // `update` trusts the manifest.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.py", "def a():\n    return 1\n");
        write(root, "b.py", "def b():\n    return 2\n");
        let opts = RebuildOptions {
            root: root.to_path_buf(),
            directed: false,
            force: false,
        };
        let out = root.join("synaptic-out");
        let r1 = rebuild(&opts, &ChangeSet::Full, None).unwrap();
        r1.manifest.save(&manifest_path(&out)).unwrap();
        let existing = r1.kg.to_graph_data();

        write(root, "a.py", "def a():\n    return 10\n");
        write(root, "b.py", "def b():\n    return 20\n");
        let r2 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("a.py")]),
            Some(&existing),
        )
        .unwrap();
        r2.manifest.save(&manifest_path(&out)).unwrap();

        let report = detect_changes(&out, root);
        assert_eq!(
            report.diff.changed,
            vec!["b.py".to_string()],
            "the unlisted edit must still be detectable"
        );
    }

    #[test]
    fn ripple_relinks_calls_from_unchanged_files_when_a_name_returns() {
        // lib.py defines announce(); main.py calls it. Renaming announce ->
        // announce2 (incremental on lib.py only) correctly drops main's edge.
        // Renaming BACK (again only lib.py changes) must RESTORE the edge from
        // the unchanged main.py: its raw calls exist only in its own
        // extraction, so the rebuild has to ripple-re-extract it (AST-cache
        // hit) when a referenced name (re)appears.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "main.py",
            "from lib import announce\n\n\ndef probe():\n    announce()\n",
        );
        write(root, "lib.py", "def announce():\n    return 1\n");
        let opts = RebuildOptions {
            root: root.to_path_buf(),
            directed: false,
            force: false,
        };
        let r1 = rebuild(&opts, &ChangeSet::Full, None).unwrap();
        assert_eq!(
            call_targets(&r1.kg, "probe()"),
            vec!["announce()".to_string()],
            "baseline: probe calls announce"
        );
        let existing = r1.kg.to_graph_data();

        write(root, "lib.py", "def announce2():\n    return 1\n");
        let r2 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("lib.py")]),
            Some(&existing),
        )
        .unwrap();
        assert!(
            call_targets(&r2.kg, "probe()").is_empty(),
            "renamed away: edge drops"
        );
        let existing = r2.kg.to_graph_data();

        write(root, "lib.py", "def announce():\n    return 1\n");
        let r3 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("lib.py")]),
            Some(&existing),
        )
        .unwrap();
        assert_eq!(
            call_targets(&r3.kg, "probe()"),
            vec!["announce()".to_string()],
            "renamed back: the edge from UNCHANGED main.py must return"
        );
    }

    #[test]
    fn ripple_links_a_new_definition_to_preexisting_callers() {
        // main.py calls helper() before it exists anywhere. Adding helper() to
        // lib.py (incremental on lib.py only) must create main -> helper even
        // though main.py itself was not touched.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "main.py",
            "from lib import helper\n\n\ndef probe():\n    helper()\n",
        );
        write(root, "lib.py", "def unrelated():\n    return 0\n");
        let opts = RebuildOptions {
            root: root.to_path_buf(),
            directed: false,
            force: false,
        };
        let r1 = rebuild(&opts, &ChangeSet::Full, None).unwrap();
        assert!(
            call_targets(&r1.kg, "probe()").is_empty(),
            "helper doesn't exist yet"
        );
        let existing = r1.kg.to_graph_data();

        write(
            root,
            "lib.py",
            "def unrelated():\n    return 0\n\n\ndef helper():\n    return 1\n",
        );
        let r2 = rebuild(
            &opts,
            &ChangeSet::Incremental(vec![PathBuf::from("lib.py")]),
            Some(&existing),
        )
        .unwrap();
        assert_eq!(
            call_targets(&r2.kg, "probe()"),
            vec!["helper()".to_string()],
            "new definition must attract the pre-existing unresolved call"
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
