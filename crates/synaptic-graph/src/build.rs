use std::collections::{HashMap, HashSet};
use std::path::Path;

use synaptic_core::{Edge, Hyperedge, Node, NodeId};

use crate::error::GraphError;
use crate::graph::KnowledgeGraph;
use crate::ids::{norm_source_file, normalize_id};

/// Options controlling a build.
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// Produce a directed graph (default: undirected dedup semantics).
    pub directed: bool,
    /// Repo root; absolute `source_file`s are relativized to it.
    pub root: Option<String>,
}

/// Build a `KnowledgeGraph` from extraction parts:
/// normalize nodes, reconcile + drop edges, dedup undirected duplicates.
pub fn build_from_parts(
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    hyperedges: Vec<Hyperedge>,
    opts: &BuildOptions,
) -> KnowledgeGraph {
    let mut kg = KnowledgeGraph::with_directed(opts.directed);
    kg.hyperedges = hyperedges;

    // nodes: normalize source_file, upsert (last write wins)
    for mut node in nodes {
        node.source_file = norm_source_file(&node.source_file, opts.root.as_deref());
        kg.upsert_node(node);
    }

    // Ghost remap: drop non-AST duplicates of located/AST nodes; remember the
    // canonical replacement so their edges survive.
    let remap = ghost_remap(&kg);
    let ghosts: HashSet<NodeId> = remap.keys().cloned().collect();

    // norm_to_id: normalized id -> canonical node id, including ghost mappings.
    let mut norm_to_id: HashMap<String, NodeId> = HashMap::new();
    for n in kg.nodes() {
        if ghosts.contains(&n.id) {
            continue;
        }
        norm_to_id.insert(normalize_id(n.id.as_str()), n.id.clone());
    }
    for (ghost, canonical) in &remap {
        norm_to_id.insert(normalize_id(ghost.as_str()), canonical.clone());
        norm_to_id.insert(ghost.0.clone(), canonical.clone());
    }

    kg.remove_nodes(&ghosts);
    add_edges(&mut kg, edges, &norm_to_id, opts.directed);
    kg
}

fn is_ast(node: &Node) -> bool {
    node.extra.get("_origin").and_then(|v| v.as_str()) == Some("ast")
}

fn has_location(node: &Node) -> bool {
    node.source_location
        .as_deref()
        .is_some_and(|s| !s.is_empty())
}

fn basename(source_file: &str) -> String {
    Path::new(source_file)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Find ghost nodes (non-AST duplicates of AST/located nodes by `(basename,
/// label)`) and return `ghost_id → canonical_id`.
fn ghost_remap(kg: &KnowledgeGraph) -> HashMap<NodeId, NodeId> {
    // Pass 1: canonical (basename,label) -> id. AST always overwrites; located
    // non-AST only if unseen.
    let mut loc: HashMap<(String, String), NodeId> = HashMap::new();
    for n in kg.nodes() {
        let label = n.label.trim();
        let base = basename(&n.source_file);
        if label.is_empty() || base.is_empty() {
            continue;
        }
        if has_location(n) || is_ast(n) {
            let key = (base, label.to_string());
            if is_ast(n) || !loc.contains_key(&key) {
                loc.insert(key, n.id.clone());
            }
        }
    }
    // Pass 2: non-AST nodes whose (basename,label) matches a *different* canonical id.
    let mut remap: HashMap<NodeId, NodeId> = HashMap::new();
    for n in kg.nodes() {
        if is_ast(n) {
            continue;
        }
        let label = n.label.trim();
        let base = basename(&n.source_file);
        if label.is_empty() || base.is_empty() {
            continue;
        }
        let key = (base, label.to_string());
        if let Some(canonical) = loc.get(&key) {
            if canonical != &n.id {
                remap.insert(n.id.clone(), canonical.clone());
            }
        }
    }
    remap
}

/// Refuse a rebuild that silently shrinks the graph. `force` or
/// `had_explicit_deletions` bypass the guard.
pub fn guard_shrink(
    new_nodes: usize,
    existing_nodes: usize,
    force: bool,
    had_explicit_deletions: bool,
) -> Result<(), GraphError> {
    if force || had_explicit_deletions || existing_nodes == 0 {
        return Ok(());
    }
    if new_nodes < existing_nodes {
        return Err(GraphError::Shrink {
            existing: existing_nodes,
            new: new_nodes,
        });
    }
    Ok(())
}

/// Resolve an endpoint to an existing node id: identity if present, else via a
/// raw ghost-id mapping, else via the normalized-id map; `None` if unresolved.
fn resolve(
    kg: &KnowledgeGraph,
    ep: &NodeId,
    norm_to_id: &HashMap<String, NodeId>,
) -> Option<NodeId> {
    if kg.contains_node(ep) {
        return Some(ep.clone());
    }
    if let Some(id) = norm_to_id.get(ep.as_str()) {
        return Some(id.clone());
    }
    norm_to_id.get(&normalize_id(ep.as_str())).cloned()
}

fn add_edges(
    kg: &mut KnowledgeGraph,
    mut edges: Vec<Edge>,
    norm_to_id: &HashMap<String, NodeId>,
    directed: bool,
) {
    // Deterministic order: (source, target, relation).
    edges.sort_by(|a, b| {
        (a.source.as_str(), a.target.as_str(), a.relation.as_str()).cmp(&(
            b.source.as_str(),
            b.target.as_str(),
            b.relation.as_str(),
        ))
    });

    // Simple-graph dedup keyed by (pair, relation): collapses exact duplicates and,
    // when undirected, the reverse same-relation duplicate (first-direction-wins,
    // via the sorted order). Distinct relations between the same pair are kept
    // (a deliberate improvement over NetworkX's collapse-all-edges-per-pair, so a
    // `calls` and an `imports` between two nodes both survive). O(E), not O(E²).
    let mut seen: HashSet<(NodeId, NodeId, String)> = HashSet::new();
    for mut edge in edges {
        let (Some(src), Some(tgt)) = (
            resolve(kg, &edge.source, norm_to_id),
            resolve(kg, &edge.target, norm_to_id),
        ) else {
            continue; // dangling (external/stdlib), drop
        };
        edge.source = src.clone();
        edge.target = tgt.clone();

        let (si, ti) = match (kg.index_of(&src), kg.index_of(&tgt)) {
            (Some(si), Some(ti)) => (si, ti),
            _ => continue,
        };

        let key = if directed || src <= tgt {
            (src.clone(), tgt.clone(), edge.relation.clone())
        } else {
            (tgt.clone(), src.clone(), edge.relation.clone())
        };
        if !seen.insert(key) {
            continue; // duplicate (exact, or undirected reverse same-relation)
        }
        kg.add_edge_raw(si, ti, edge);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, FileType};

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

    fn edge(s: &str, t: &str, rel: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            source_file: "a.py".into(),
            source_location: Some("L1".into()),
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn counts_nodes_and_keeps_valid_edges() {
        let kg = build_from_parts(
            vec![node("a", "a", "a.py"), node("b", "b", "b.py")],
            vec![edge("a", "b", "calls")],
            vec![],
            &BuildOptions::default(),
        );
        assert_eq!(kg.node_count(), 2);
        assert_eq!(kg.edge_count(), 1);
    }

    #[test]
    fn duplicate_node_id_last_write_wins() {
        let kg = build_from_parts(
            vec![node("n1", "First", "a.py"), node("n1", "Second", "a.py")],
            vec![],
            vec![],
            &BuildOptions::default(),
        );
        assert_eq!(kg.node_count(), 1);
        assert_eq!(kg.node(&NodeId("n1".into())).unwrap().label, "Second");
    }

    #[test]
    fn located_node_is_not_clobbered_by_empty_source_stub() {
        // A .NET ProjectReference / bash `source` target emits a stub whose id
        // equals the real file's node id. Whichever order they merge in, the
        // located node's source_file + label must survive.
        let mut stub = node("lib_csproj", "Lib", ""); // empty source_file
        stub.source_location = None;
        let real = node("lib_csproj", "Lib.csproj", "src/Lib/Lib.csproj");
        for (a, b) in [(stub.clone(), real.clone()), (real.clone(), stub.clone())] {
            let kg = build_from_parts(vec![a, b], vec![], vec![], &BuildOptions::default());
            let n = kg.node(&NodeId("lib_csproj".into())).unwrap();
            assert_eq!(n.source_file, "src/Lib/Lib.csproj", "located source kept");
            assert_eq!(n.label, "Lib.csproj", "located label kept");
        }
    }

    #[test]
    fn backslash_source_files_collapse_to_one_form() {
        let kg = build_from_parts(
            vec![
                node("n1", "A", "src\\middleware\\auth.py"),
                node("n2", "B", "src/middleware/auth.py"),
            ],
            vec![],
            vec![],
            &BuildOptions::default(),
        );
        let forms: std::collections::HashSet<_> =
            kg.nodes().map(|n| n.source_file.clone()).collect();
        assert_eq!(
            forms,
            ["src/middleware/auth.py".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn dangling_edge_is_dropped() {
        let kg = build_from_parts(
            vec![node("a", "a", "a.py")],
            vec![edge("a", "stdlib_thing", "calls")],
            vec![],
            &BuildOptions::default(),
        );
        assert_eq!(kg.edge_count(), 0);
    }

    #[test]
    fn mismatched_case_endpoint_is_reconciled() {
        // Node id is normalized ("auth_login"); edge uses a differently-cased id.
        let kg = build_from_parts(
            vec![node("auth_login", "login", "a.py"), node("b", "b", "b.py")],
            vec![edge("Auth_Login", "b", "calls")],
            vec![],
            &BuildOptions::default(),
        );
        assert_eq!(kg.edge_count(), 1);
        let e = kg.edges().next().unwrap();
        assert_eq!(e.source, NodeId("auth_login".into())); // remapped to canonical
    }

    #[test]
    fn undirected_bidirectional_pair_collapses_first_direction_wins() {
        let kg = build_from_parts(
            vec![
                node("a_handler", "a", "a.ts"),
                node("z_emitter", "z", "z.ts"),
            ],
            vec![
                edge("a_handler", "z_emitter", "calls"),
                edge("z_emitter", "a_handler", "calls"),
            ],
            vec![],
            &BuildOptions::default(),
        );
        assert_eq!(kg.edge_count(), 1);
        let e = kg.edges().next().unwrap();
        // Sorted order puts ("a_handler",...) first, so that direction survives.
        assert_eq!(e.source, NodeId("a_handler".into()));
        assert_eq!(e.target, NodeId("z_emitter".into()));
    }

    #[test]
    fn exact_duplicate_edges_collapse_to_one() {
        // Same (a,b,calls) twice -> one edge, in both modes (simple-graph semantics).
        for directed in [false, true] {
            let kg = build_from_parts(
                vec![node("a", "a", "a.py"), node("b", "b", "b.py")],
                vec![edge("a", "b", "calls"), edge("a", "b", "calls")],
                vec![],
                &BuildOptions {
                    directed,
                    root: None,
                },
            );
            assert_eq!(kg.edge_count(), 1, "directed={directed}");
        }
    }

    #[test]
    fn distinct_relations_between_pair_are_kept() {
        let kg = build_from_parts(
            vec![node("a", "a", "a.py"), node("b", "b", "b.py")],
            vec![edge("a", "b", "calls"), edge("a", "b", "imports")],
            vec![],
            &BuildOptions::default(),
        );
        assert_eq!(kg.edge_count(), 2);
    }

    #[test]
    fn directed_keeps_both_directions() {
        let opts = BuildOptions {
            directed: true,
            root: None,
        };
        let kg = build_from_parts(
            vec![
                node("a_handler", "a", "a.ts"),
                node("z_emitter", "z", "z.ts"),
            ],
            vec![
                edge("a_handler", "z_emitter", "calls"),
                edge("z_emitter", "a_handler", "calls"),
            ],
            vec![],
            &opts,
        );
        assert_eq!(kg.edge_count(), 2);
    }

    #[test]
    fn hyperedges_are_attached() {
        let kg = build_from_parts(
            vec![node("a", "a", "a.py")],
            vec![],
            vec![Hyperedge {
                id: "h1".into(),
                label: "grp".into(),
                nodes: vec![NodeId("a".into())],
                relation: None,
                confidence: None,
            }],
            &BuildOptions::default(),
        );
        assert_eq!(kg.hyperedges.len(), 1);
        assert_eq!(kg.to_graph_data().hyperedges.len(), 1);
    }

    fn ast_node(id: &str, label: &str, sf: &str) -> Node {
        let mut n = node(id, label, sf);
        n.extra.insert("_origin".into(), serde_json::json!("ast"));
        n
    }

    fn semantic_node(id: &str, label: &str, sf: &str) -> Node {
        // No _origin, no source_location -> eligible to be a ghost.
        let mut n = node(id, label, sf);
        n.source_location = None;
        n
    }

    #[test]
    fn semantic_ghost_is_removed_and_edges_repoint_to_ast_node() {
        // AST node `mingpt_bpe_get_pairs` and a semantic duplicate `bpe_get_pairs`
        // share (basename="bpe.py", label="get_pairs"). The ghost is removed and
        // an edge targeting it is re-pointed to the AST node.
        let nodes = vec![
            ast_node("mingpt_bpe_get_pairs", "get_pairs", "mingpt/bpe.py"),
            semantic_node("bpe_get_pairs", "get_pairs", "mingpt/bpe.py"),
            node("caller", "caller", "mingpt/bpe.py"),
        ];
        let edges = vec![edge("caller", "bpe_get_pairs", "calls")];
        let kg = build_from_parts(nodes, edges, vec![], &BuildOptions::default());

        // Ghost removed: only the AST node + caller remain (3 - 1 ghost = 2).
        assert!(!kg.contains_node(&NodeId("bpe_get_pairs".into())));
        assert!(kg.contains_node(&NodeId("mingpt_bpe_get_pairs".into())));
        assert_eq!(kg.node_count(), 2);
        // Edge re-pointed to the canonical AST node.
        assert_eq!(kg.edge_count(), 1);
        let e = kg.edges().next().unwrap();
        assert_eq!(e.target, NodeId("mingpt_bpe_get_pairs".into()));
    }

    #[test]
    fn ast_nodes_are_never_ghosts() {
        // Two AST nodes with same (basename,label) but different ids: neither is
        // removed (AST nodes are never treated as ghosts).
        let nodes = vec![
            ast_node("a_dup", "dup", "x.py"),
            ast_node("b_dup", "dup", "x.py"),
        ];
        let kg = build_from_parts(nodes, vec![], vec![], &BuildOptions::default());
        assert_eq!(kg.node_count(), 2);
    }

    #[test]
    fn guard_shrink_refuses_smaller_graph() {
        assert_eq!(
            guard_shrink(3, 5, false, false),
            Err(crate::error::GraphError::Shrink {
                existing: 5,
                new: 3
            })
        );
    }

    #[test]
    fn guard_shrink_allows_growth_or_equal() {
        assert!(guard_shrink(5, 5, false, false).is_ok());
        assert!(guard_shrink(7, 5, false, false).is_ok());
    }

    #[test]
    fn guard_shrink_force_and_declared_deletions_bypass() {
        assert!(guard_shrink(1, 100, true, false).is_ok());
        assert!(guard_shrink(1, 100, false, true).is_ok());
    }
}
