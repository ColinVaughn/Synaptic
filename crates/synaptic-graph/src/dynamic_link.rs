//! Evidence-link reflection/dynamic-dispatch sites to their unique target. A site
//! whose key is a string literal (e.g. `getattr(o, "run")`) is matched by name to a
//! single defining symbol; when exactly one matches, a low-confidence `dynamic_ref`
//! edge is added so the target shows up as a (caveated) dependent and is flagged
//! `dynamically_referenced`. Ambiguous or unmatched keys stay catalog-only.

use std::collections::{HashMap, HashSet};

use serde_json::Map;
use synaptic_core::{Confidence, DynamicKind, Edge, Node, NodeId, NodeKind};

/// The dynamic-evidence-link pass. Mirrors the cross-language resolver signature
/// `(nodes, edges) -> (nodes, edges)` so it slots into the build pipeline.
pub fn link_dynamic_refs(mut nodes: Vec<Node>, mut edges: Vec<Edge>) -> (Vec<Node>, Vec<Edge>) {
    // name -> indices of linkable target nodes (definitions, not stubs).
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if is_linkable_target(n) {
            by_name.entry(symbol_name(&n.label)).or_default().push(i);
        }
    }

    // Idempotent on re-runs: do not duplicate an existing dynamic_ref pair.
    let mut linked: HashSet<(NodeId, NodeId)> = edges
        .iter()
        .filter(|e| e.relation == "dynamic_ref")
        .map(|e| (e.source.clone(), e.target.clone()))
        .collect();

    // Collect (source_idx, target_idx, kind) for unique matches.
    let mut links: Vec<(usize, usize, DynamicKind)> = Vec::new();
    for (si, n) in nodes.iter().enumerate() {
        for site in n.dynamic_sites() {
            let Some(key) = site.key.as_deref() else {
                continue;
            };
            let name = key_name(key);
            let Some(cands) = by_name.get(&name) else {
                continue;
            };
            let pool: Vec<usize> = cands.iter().copied().filter(|&ti| ti != si).collect();
            let Some(ti) = pick_unique(&nodes, &pool, n.repo.as_deref()) else {
                continue;
            };
            if linked.insert((n.id.clone(), nodes[ti].id.clone())) {
                links.push((si, ti, site.kind));
            }
        }
    }

    // Apply: flag targets + add edges.
    for (si, ti, kind) in links {
        nodes[ti].set_dynamically_referenced(true);
        let mut extra = Map::new();
        extra.insert(
            "kind".to_string(),
            serde_json::to_value(kind).expect("DynamicKind serializes"),
        );
        extra.insert("via".to_string(), serde_json::json!("dynamic"));
        edges.push(Edge {
            source: nodes[si].id.clone(),
            target: nodes[ti].id.clone(),
            relation: "dynamic_ref".to_string(),
            confidence: Confidence::Inferred,
            source_file: nodes[si].source_file.clone(),
            source_location: None,
            confidence_score: Some(Confidence::Inferred.default_score()),
            weight: 1.0,
            context: Some("dynamic".to_string()),
            cross_repo: false,
            extra,
        });
    }
    (nodes, edges)
}

/// A node a reflection key can resolve to: a real definition (not an import stub)
/// of a callable or type.
fn is_linkable_target(n: &Node) -> bool {
    if n.is_external_stub() {
        return false;
    }
    matches!(
        n.kind(),
        Some(
            NodeKind::Function
                | NodeKind::Method
                | NodeKind::Constructor
                | NodeKind::Class
                | NodeKind::Struct
                | NodeKind::Interface
                | NodeKind::Enum
        )
    )
}

/// Label up to `(`, lowercased: `runJob(a)` -> `runjob`, `Foo` -> `foo`.
fn symbol_name(label: &str) -> String {
    label
        .split('(')
        .next()
        .unwrap_or(label)
        .trim()
        .to_ascii_lowercase()
}

/// Last path/namespace segment of a reflection key, lowercased: `com.x.Y` -> `y`.
fn key_name(key: &str) -> String {
    key.rsplit(['.', ':', '/', '\\'])
        .next()
        .unwrap_or(key)
        .trim()
        .to_ascii_lowercase()
}

/// The single matching target: prefer a unique same-repo match; fall back to a
/// unique global match only when there is no same-repo candidate. Ambiguity (>1)
/// yields `None` (catalog-only).
fn pick_unique(nodes: &[Node], pool: &[usize], src_repo: Option<&str>) -> Option<usize> {
    let same: Vec<usize> = pool
        .iter()
        .copied()
        .filter(|&i| nodes[i].repo.as_deref() == src_repo)
        .collect();
    if same.len() == 1 {
        return Some(same[0]);
    }
    if same.is_empty() && pool.len() == 1 {
        return Some(pool[0]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaptic_core::{DynamicSite, FileType};

    fn func(id: &str, label: &str, repo: Option<&str>) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: format!("{id}.ts"),
            source_location: None,
            community: None,
            repo: repo.map(|s| s.to_string()),
            extra: Map::new(),
        };
        n.set_kind(NodeKind::Function);
        n
    }

    fn site(key: &str) -> DynamicSite {
        DynamicSite {
            kind: DynamicKind::Reflection,
            line: 1,
            key: Some(key.into()),
            snippet: format!("h['{key}']()"),
        }
    }

    #[test]
    fn literal_key_site_links_unique_target_and_flags_it() {
        let mut caller = func("caller", "caller()", None);
        caller.push_dynamic_site(site("runJob"));
        let target = func("runJob", "runJob()", None);
        let (nodes, edges) = link_dynamic_refs(vec![caller, target], vec![]);
        assert!(edges.iter().any(|e| e.relation == "dynamic_ref"
            && e.source == NodeId("caller".into())
            && e.target == NodeId("runJob".into())
            && e.confidence == Confidence::Inferred));
        let t = nodes
            .iter()
            .find(|n| n.id == NodeId("runJob".into()))
            .unwrap();
        assert!(t.dynamically_referenced());
    }

    #[test]
    fn ambiguous_key_stays_catalog_only() {
        let mut caller = func("caller", "caller()", None);
        caller.push_dynamic_site(site("run"));
        let a = func("a", "run()", None);
        let b = func("b", "run()", None);
        let (_n, edges) = link_dynamic_refs(vec![caller, a, b], vec![]);
        assert!(!edges.iter().any(|e| e.relation == "dynamic_ref"));
    }

    #[test]
    fn idempotent_on_rerun() {
        let mut caller = func("caller", "caller()", None);
        caller.push_dynamic_site(site("runJob"));
        let target = func("runJob", "runJob()", None);
        let (n1, e1) = link_dynamic_refs(vec![caller, target], vec![]);
        let before = e1.iter().filter(|e| e.relation == "dynamic_ref").count();
        let (_n2, e2) = link_dynamic_refs(n1, e1);
        let after = e2.iter().filter(|e| e.relation == "dynamic_ref").count();
        assert_eq!(before, after, "re-run must not duplicate the edge");
    }
}
