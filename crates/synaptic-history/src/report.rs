//! Derive the five time-travel report categories from two built graphs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use synaptic_core::{FileType, NodeId};
use synaptic_graph::{find_import_cycles, graph_diff, KnowledgeGraph};

use crate::DiffOptions;

/// A module-to-module dependency.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModuleDep {
    pub from: String,
    pub to: String,
}

/// A public/referenced symbol that disappeared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedApi {
    pub id: String,
    pub label: String,
    pub source_file: String,
    pub referenced_by: usize,
}

/// Per-module coupling change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleDrift {
    pub module: String,
    pub coupling_before: f64,
    pub coupling_after: f64,
    pub delta: f64,
}

/// Architectural drift summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftReport {
    pub communities_before: usize,
    pub communities_after: usize,
    pub coupling_before: f64,
    pub coupling_after: f64,
    pub modules: Vec<ModuleDrift>,
}

/// A file ranked by change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hotspot {
    pub file: String,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub nodes_added: usize,
    pub nodes_removed: usize,
    pub score: f64,
}

/// The full time-travel report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffReport {
    pub rev1: String,
    pub rev2: String,
    pub summary: String,
    pub added_dependencies: Vec<ModuleDep>,
    pub removed_dependencies: Vec<ModuleDep>,
    pub removed_apis: Vec<RemovedApi>,
    pub drift: DriftReport,
    pub new_cycles: Vec<Vec<String>>,
    pub hotspots: Vec<Hotspot>,
}

/// Top `depth` path components of a repo-relative file (forward-slashed).
pub fn module_of(source_file: &str, depth: usize) -> String {
    let norm = source_file.replace('\\', "/");
    let comps: Vec<&str> = norm.split('/').filter(|c| !c.is_empty()).collect();
    if comps.len() <= 1 {
        return "(root)".to_string();
    }
    // Always leave at least the file component out, so a file is never its own
    // module; clamp is safe because comps.len() >= 2 here.
    let take = depth.clamp(1, comps.len() - 1);
    comps[..take].join("/")
}

/// id -> module map for a graph.
fn module_index(kg: &KnowledgeGraph, depth: usize) -> HashMap<NodeId, String> {
    kg.nodes()
        .map(|n| (n.id.clone(), module_of(&n.source_file, depth)))
        .collect()
}

/// Module-level dependency set for a graph, over the given relations.
fn module_deps(
    kg: &KnowledgeGraph,
    relations: &HashSet<&str>,
    depth: usize,
) -> BTreeSet<ModuleDep> {
    let idx = module_index(kg, depth);
    let mut out = BTreeSet::new();
    for e in kg.edges() {
        if e.source == e.target || !relations.contains(e.relation.as_str()) {
            continue;
        }
        let (Some(a), Some(b)) = (idx.get(&e.source), idx.get(&e.target)) else {
            continue;
        };
        if a != b {
            out.insert(ModuleDep {
                from: a.clone(),
                to: b.clone(),
            });
        }
    }
    out
}

/// New and removed module-to-module dependencies.
pub fn dependency_delta(
    old: &KnowledgeGraph,
    new: &KnowledgeGraph,
    opts: &DiffOptions,
) -> (Vec<ModuleDep>, Vec<ModuleDep>) {
    let rels: HashSet<&str> = opts.dep_relations.iter().map(String::as_str).collect();
    let o = module_deps(old, &rels, opts.module_depth);
    let n = module_deps(new, &rels, opts.module_depth);
    let added: Vec<ModuleDep> = n.difference(&o).cloned().collect();
    let removed: Vec<ModuleDep> = o.difference(&n).cloned().collect();
    (added, removed)
}

/// Removed code symbols that were a public API of `old`. When enrichment is
/// present, a removed `Public` node counts even with no observed
/// references; otherwise it falls back to the export-surface heuristic (a node
/// referenced from another file). `Private`/`Internal` removals are never APIs.
pub fn removed_apis(old: &KnowledgeGraph, new: &KnowledgeGraph, top: usize) -> Vec<RemovedApi> {
    let new_ids: HashSet<&NodeId> = new.nodes().map(|n| &n.id).collect();
    // Count cross-file incoming references per node in `old`.
    let mut refs: HashMap<NodeId, usize> = HashMap::new();
    let src_file: HashMap<&NodeId, &str> = old
        .nodes()
        .map(|n| (&n.id, n.source_file.as_str()))
        .collect();
    for e in old.edges() {
        if e.source == e.target {
            continue;
        }
        let (sf, tf) = (src_file.get(&e.source), src_file.get(&e.target));
        if let (Some(sf), Some(tf)) = (sf, tf) {
            if sf != tf {
                *refs.entry(e.target.clone()).or_default() += 1;
            }
        }
    }
    let mut out: Vec<RemovedApi> = old
        .nodes()
        .filter(|n| n.file_type == FileType::Code && !new_ids.contains(&n.id))
        .filter_map(|n| {
            let by = refs.get(&n.id).copied().unwrap_or(0);
            let is_api = match n.visibility() {
                Some(synaptic_core::Visibility::Public) => true,
                Some(_) => false, // explicitly non-public: not an API
                None => by > 0,   // unknown visibility: export-surface heuristic
            };
            is_api.then(|| RemovedApi {
                id: n.id.0.clone(),
                label: n.label.clone(),
                source_file: n.source_file.clone(),
                referenced_by: by,
            })
        })
        .collect();
    out.sort_by(|a, b| b.referenced_by.cmp(&a.referenced_by).then(a.id.cmp(&b.id)));
    out.truncate(top);
    out
}

/// Fraction of (undirected, distinct) edges that cross module boundaries, overall
/// and per module. Returns `(overall, per_module_coupling)`.
fn coupling(kg: &KnowledgeGraph, depth: usize) -> (f64, HashMap<String, f64>) {
    let idx = module_index(kg, depth);
    let mut total = 0u64;
    let mut crossing = 0u64;
    // per module: (incident, crossing)
    let mut per: HashMap<String, (u64, u64)> = HashMap::new();
    let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();
    for e in kg.edges() {
        if e.source == e.target {
            continue;
        }
        let key = if e.source <= e.target {
            (e.source.clone(), e.target.clone())
        } else {
            (e.target.clone(), e.source.clone())
        };
        if !seen.insert(key) {
            continue;
        }
        let (Some(a), Some(b)) = (idx.get(&e.source), idx.get(&e.target)) else {
            continue;
        };
        total += 1;
        let cross = a != b;
        if cross {
            crossing += 1;
        }
        for m in [a, b] {
            let ent = per.entry(m.clone()).or_insert((0, 0));
            ent.0 += 1;
            if cross {
                ent.1 += 1;
            }
        }
    }
    let overall = if total > 0 {
        crossing as f64 / total as f64
    } else {
        0.0
    };
    let per_module = per
        .into_iter()
        .map(|(m, (inc, cr))| (m, if inc > 0 { cr as f64 / inc as f64 } else { 0.0 }))
        .collect();
    (overall, per_module)
}

fn community_count(kg: &KnowledgeGraph) -> usize {
    kg.nodes()
        .filter_map(|n| n.community)
        .collect::<HashSet<_>>()
        .len()
}

/// Architectural drift between two graphs.
pub fn drift(old: &KnowledgeGraph, new: &KnowledgeGraph, depth: usize, top: usize) -> DriftReport {
    let (cb, mb) = coupling(old, depth);
    let (ca, ma) = coupling(new, depth);
    let mut modules: Vec<ModuleDrift> = Vec::new();
    let names: BTreeSet<&String> = mb.keys().chain(ma.keys()).collect();
    for m in names {
        let before = mb.get(m).copied().unwrap_or(0.0);
        let after = ma.get(m).copied().unwrap_or(0.0);
        let delta = after - before;
        if delta.abs() > 1e-9 {
            modules.push(ModuleDrift {
                module: m.clone(),
                coupling_before: before,
                coupling_after: after,
                delta,
            });
        }
    }
    modules.sort_by(|a, b| {
        b.delta
            .partial_cmp(&a.delta)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    modules.truncate(top);
    DriftReport {
        communities_before: community_count(old),
        communities_after: community_count(new),
        coupling_before: cb,
        coupling_after: ca,
        modules,
    }
}

/// Dependency cycles present in `new` but not in `old` (canonicalized vecs).
pub fn new_cycles(old: &KnowledgeGraph, new: &KnowledgeGraph, top: usize) -> Vec<Vec<String>> {
    let old_set: HashSet<Vec<String>> = find_import_cycles(old, 12, 1000)
        .into_iter()
        .map(|c| c.cycle)
        .collect();
    find_import_cycles(new, 12, 1000)
        .into_iter()
        .map(|c| c.cycle)
        .filter(|c| !old_set.contains(c))
        .take(top)
        .collect()
}

/// Rank files by change: line churn (numstat) + graph node churn.
pub fn hotspots(
    old: &KnowledgeGraph,
    new: &KnowledgeGraph,
    numstat: &[(usize, usize, String)],
    delta_new_nodes: &[NodeId],
    delta_removed_nodes: &[NodeId],
    top: usize,
) -> Vec<Hotspot> {
    let new_file: HashMap<&NodeId, &str> = new
        .nodes()
        .map(|n| (&n.id, n.source_file.as_str()))
        .collect();
    let old_file: HashMap<&NodeId, &str> = old
        .nodes()
        .map(|n| (&n.id, n.source_file.as_str()))
        .collect();
    let mut nodes_added: HashMap<String, usize> = HashMap::new();
    let mut nodes_removed: HashMap<String, usize> = HashMap::new();
    for id in delta_new_nodes {
        if let Some(f) = new_file.get(id) {
            *nodes_added.entry(f.to_string()).or_default() += 1;
        }
    }
    for id in delta_removed_nodes {
        if let Some(f) = old_file.get(id) {
            *nodes_removed.entry(f.to_string()).or_default() += 1;
        }
    }
    let mut files: BTreeMap<String, Hotspot> = BTreeMap::new();
    let blank = |p: &str| Hotspot {
        file: p.to_string(),
        lines_added: 0,
        lines_removed: 0,
        nodes_added: 0,
        nodes_removed: 0,
        score: 0.0,
    };
    for (a, d, p) in numstat {
        let h = files.entry(p.clone()).or_insert_with(|| blank(p));
        h.lines_added += a;
        h.lines_removed += d;
    }
    // Fold in graph churn for files that may not appear in numstat.
    for f in nodes_added.keys().chain(nodes_removed.keys()) {
        files.entry(f.clone()).or_insert_with(|| blank(f));
    }
    for (f, h) in files.iter_mut() {
        h.nodes_added = nodes_added.get(f).copied().unwrap_or(0);
        h.nodes_removed = nodes_removed.get(f).copied().unwrap_or(0);
        h.score = (h.lines_added + h.lines_removed) as f64
            + 3.0 * (h.nodes_added + h.nodes_removed) as f64;
    }
    let mut out: Vec<Hotspot> = files.into_values().filter(|h| h.score > 0.0).collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.file.cmp(&b.file))
    });
    out.truncate(top);
    out
}

/// Apply a scope prefix filter to a built graph by dropping out-of-scope nodes.
fn scoped(kg: &KnowledgeGraph, scope: &Option<String>) -> KnowledgeGraph {
    let Some(prefix) = scope else {
        return clone_kg(kg);
    };
    let prefix = prefix.replace('\\', "/");
    let keep: HashSet<NodeId> = kg
        .nodes()
        .filter(|n| n.source_file.replace('\\', "/").starts_with(&prefix))
        .map(|n| n.id.clone())
        .collect();
    let mut gd = kg.to_graph_data();
    gd.nodes.retain(|n| keep.contains(&n.id));
    gd.links
        .retain(|e| keep.contains(&e.source) && keep.contains(&e.target));
    KnowledgeGraph::from_graph_data(gd)
}

fn clone_kg(kg: &KnowledgeGraph) -> KnowledgeGraph {
    KnowledgeGraph::from_graph_data(kg.to_graph_data())
}

/// Assemble the full report from two built graphs + numstat.
pub fn assemble(
    rev1: &str,
    rev2: &str,
    old: &KnowledgeGraph,
    new: &KnowledgeGraph,
    numstat: &[(usize, usize, String)],
    opts: &DiffOptions,
) -> DiffReport {
    let old = scoped(old, &opts.scope);
    let new = scoped(new, &opts.scope);
    let delta = graph_diff(&old, &new);
    let (added_dependencies, removed_dependencies) = dependency_delta(&old, &new, opts);
    DiffReport {
        rev1: rev1.to_string(),
        rev2: rev2.to_string(),
        summary: delta.summary.clone(),
        added_dependencies,
        removed_dependencies,
        removed_apis: removed_apis(&old, &new, opts.top),
        drift: drift(&old, &new, opts.module_depth, opts.top),
        new_cycles: new_cycles(&old, &new, opts.top),
        hotspots: hotspots(
            &old,
            &new,
            numstat,
            &delta.new_nodes,
            &delta.removed_nodes,
            opts.top,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, GraphData, Node};

    fn n(id: &str, sf: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: id.into(),
            file_type: FileType::Code,
            source_file: sf.into(),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }
    fn e(s: &str, t: &str, sf: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: sf.into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }
    fn imp(s: &str, t: &str, sf: &str) -> Edge {
        let mut x = e(s, t, sf);
        x.relation = "imports_from".into();
        x
    }
    fn kg(nodes: Vec<Node>, edges: Vec<Edge>) -> KnowledgeGraph {
        let gd = GraphData {
            nodes,
            links: edges,
            ..GraphData::default()
        };
        KnowledgeGraph::from_graph_data(gd)
    }

    #[test]
    fn module_of_uses_top_dir() {
        assert_eq!(module_of("src/a/b.rs", 1), "src");
        assert_eq!(module_of("main.rs", 1), "(root)");
        assert_eq!(module_of("crates/x/src/y.rs", 2), "crates/x");
    }

    #[test]
    fn removed_apis_flags_cross_file_referenced_deletions() {
        // old: b (in b.py) is called from a.py; new: b is gone.
        let old = kg(
            vec![n("a", "a.py"), n("b", "b.py")],
            vec![e("a", "b", "a.py")],
        );
        let new = kg(vec![n("a", "a.py")], vec![]);
        let removed = removed_apis(&old, &new, 10);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id, "b");
        assert_eq!(removed[0].referenced_by, 1);
    }

    #[test]
    fn removed_apis_uses_visibility_when_present() {
        // old: a public `pubsym` (no refs) and a private `priv_` referenced cross-file.
        let mut pubsym = n("pubsym", "lib.rs");
        pubsym.set_visibility(synaptic_core::Visibility::Public);
        let mut privsym = n("priv_", "lib.rs");
        privsym.set_visibility(synaptic_core::Visibility::Private);
        let old = kg(
            vec![n("a", "a.rs"), pubsym, privsym],
            vec![e("a", "priv_", "a.rs")], // priv_ referenced from a.rs
        );
        let new = kg(vec![n("a", "a.rs")], vec![]); // both removed
        let removed = removed_apis(&old, &new, 10);
        let ids: Vec<&str> = removed.iter().map(|r| r.id.as_str()).collect();
        assert!(
            ids.contains(&"pubsym"),
            "public symbol is a removed API even with 0 refs"
        );
        assert!(
            !ids.contains(&"priv_"),
            "private symbol is not an API despite refs"
        );
    }

    #[test]
    fn drift_detects_new_cross_module_coupling() {
        // old: a.py and lib/b.py with no cross edge. new: a.py -> lib/b.py.
        let old = kg(vec![n("a", "a.py"), n("b", "lib/b.py")], vec![]);
        let new = kg(
            vec![n("a", "a.py"), n("b", "lib/b.py")],
            vec![e("a", "b", "a.py")],
        );
        let d = drift(&old, &new, 1, 10);
        assert!(d.coupling_after > d.coupling_before);
    }

    #[test]
    fn new_cycles_reports_only_introduced_cycles() {
        // old: x.py -> y.py (no cycle). new: also y.py -> x.py (2-cycle).
        let nodes = || vec![n("x", "x.py"), n("y", "y.py")];
        let old = kg(nodes(), vec![imp("x", "y", "x.py")]);
        let new = kg(nodes(), vec![imp("x", "y", "x.py"), imp("y", "x", "y.py")]);
        let cycles = new_cycles(&old, &new, 10);
        assert_eq!(cycles.len(), 1, "exactly one new cycle");
    }
}
