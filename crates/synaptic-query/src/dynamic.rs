//! The honesty layer over dynamic dispatch. `DynamicHazardIndex` catalogs opaque
//! reflection/dynamic-dispatch sites (and the evidence-linked node set) once per
//! graph load; `dependents_caveat` turns a bare "0 dependents" into an honest note
//! when a symbol with no static dependents sits in a scope that uses dynamic
//! dispatch, so "0 dependents" stops reading as "safe to change".

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use synaptic_core::{Confidence, DynamicKind, Node, NodeId};
use synaptic_graph::KnowledgeGraph;

use crate::DEFAULT_AFFECTED_RELATIONS;

/// A dynamic-dispatch site, flattened for indexing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteRef {
    pub kind: DynamicKind,
    pub line: u32,
    pub key: Option<String>,
}

/// Query-independent catalog of dynamic-dispatch hazards, built once per load.
pub struct DynamicHazardIndex {
    /// source_file -> opaque (unkeyed) sites in it.
    file_opaque: HashMap<String, Vec<SiteRef>>,
    /// nodes an evidence-link resolved to (reachable only dynamically).
    referenced: HashSet<NodeId>,
}

impl DynamicHazardIndex {
    /// Build the index by scanning every node's `dynamic_sites`.
    pub fn build(kg: &KnowledgeGraph) -> Self {
        let mut file_opaque: HashMap<String, Vec<SiteRef>> = HashMap::new();
        let mut referenced = HashSet::new();
        for n in kg.nodes() {
            if n.dynamically_referenced() {
                referenced.insert(n.id.clone());
            }
            if n.source_file.is_empty() {
                continue;
            }
            for s in n.dynamic_sites() {
                if s.key.is_none() {
                    file_opaque
                        .entry(n.source_file.clone())
                        .or_default()
                        .push(SiteRef {
                            kind: s.kind,
                            line: s.line,
                            key: None,
                        });
                }
            }
        }
        Self {
            file_opaque,
            referenced,
        }
    }

    /// Total opaque sites across the graph (for stats).
    pub fn opaque_total(&self) -> usize {
        self.file_opaque.values().map(Vec::len).sum()
    }
}

/// The honest caveat attached to a "0 dependents" answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DynamicCaveat {
    /// Opaque dynamic-dispatch sites in the node's own file.
    pub opaque_sites_in_scope: usize,
    /// Distinct site kinds present (snake_case).
    pub kinds: Vec<String>,
    /// An evidence-link named this symbol from a dynamic site elsewhere.
    pub dynamically_referenced: bool,
    /// One-line human message.
    pub message: String,
}

/// Return a caveat when `node` has zero high-confidence (`Extracted`) reverse-impact
/// dependents AND there is a reason dynamic dispatch could still reach it: it was
/// evidence-linked, or its own file contains opaque dynamic-dispatch sites. Returns
/// `None` when the node has real static dependents or no dynamic-hazard signal.
pub fn dependents_caveat(
    kg: &KnowledgeGraph,
    idx: &DynamicHazardIndex,
    node: &Node,
) -> Option<DynamicCaveat> {
    if has_extracted_dependent(kg, &node.id) {
        return None;
    }
    let referenced = idx.referenced.contains(&node.id);
    let sites = (!node.source_file.is_empty())
        .then(|| idx.file_opaque.get(&node.source_file))
        .flatten();
    let opaque = sites.map_or(0, Vec::len);
    if !referenced && opaque == 0 {
        return None;
    }
    let mut kinds: Vec<String> = sites
        .into_iter()
        .flatten()
        .map(|s| kind_str(s.kind))
        .collect();
    kinds.sort();
    kinds.dedup();
    let message = build_message(referenced, opaque, &kinds);
    Some(DynamicCaveat {
        opaque_sites_in_scope: opaque,
        kinds,
        dynamically_referenced: referenced,
        message,
    })
}

/// Any incoming impact edge of `Extracted` confidence from a real (non-stub) node:
/// a statically-proven dependent. Structural containment relations (`defines`,
/// `contains`) are not in [`DEFAULT_AFFECTED_RELATIONS`], so they do not count.
fn has_extracted_dependent(kg: &KnowledgeGraph, id: &NodeId) -> bool {
    kg.edges().any(|e| {
        e.target == *id
            && e.confidence == Confidence::Extracted
            && DEFAULT_AFFECTED_RELATIONS.contains(&e.relation.as_str())
            && kg.node(&e.source).is_some_and(|s| !s.is_external_stub())
    })
}

fn kind_str(k: DynamicKind) -> String {
    k.as_str().to_string()
}

fn build_message(referenced: bool, opaque: usize, kinds: &[String]) -> String {
    let kinds_str = if kinds.is_empty() {
        String::new()
    } else {
        format!(" ({})", kinds.join(", "))
    };
    match (referenced, opaque) {
        (true, 0) => "0 static dependents, but a dynamic site names this symbol by string \
             (evidence-linked dynamic_ref) -- reachable dynamically, not provably unused."
            .to_string(),
        (true, n) => format!(
            "0 static dependents, but this symbol is named by a dynamic site and its file has \
             {n} opaque dynamic-dispatch site(s){kinds_str} -- not provably unused."
        ),
        (false, n) => format!(
            "0 static dependents, but {n} opaque dynamic-dispatch site(s){kinds_str} in this file \
             may reach it -- not provably unused."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{DynamicSite, Edge, FileType, GraphData, NodeKind};

    fn node(id: &str, file: &str) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: format!("{id}()"),
            file_type: FileType::Code,
            source_file: file.into(),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
        };
        n.set_kind(NodeKind::Function);
        n
    }

    fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> KnowledgeGraph {
        KnowledgeGraph::from_graph_data(GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links: edges,
            hyperedges: vec![],
            built_at_commit: None,
        })
    }

    fn opaque_site() -> DynamicSite {
        DynamicSite {
            kind: DynamicKind::Reflection,
            line: 3,
            key: None,
            snippet: "h[k]()".into(),
        }
    }

    #[test]
    fn caveat_fires_for_zero_dep_node_in_scope_with_opaque_sites() {
        let mut hub = node("hub", "a.ts");
        hub.push_dynamic_site(opaque_site());
        let orphan = node("orphan", "a.ts"); // same file, no static dependents
        let kg = graph(vec![hub, orphan.clone()], vec![]);
        let idx = DynamicHazardIndex::build(&kg);
        let c = dependents_caveat(&kg, &idx, kg.node(&NodeId("orphan".into())).unwrap())
            .expect("caveat");
        assert!(c.opaque_sites_in_scope >= 1);
        assert!(c.message.contains("not provably unused"));
    }

    #[test]
    fn no_caveat_when_node_has_static_dependents() {
        let used = node("used", "a.ts");
        let caller = node("caller", "a.ts");
        // caller -> used (calls), Extracted
        let e = Edge {
            source: NodeId("caller".into()),
            target: NodeId("used".into()),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: "a.ts".into(),
            source_location: Some("L2".into()),
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        };
        // give the file an opaque site so only the dependent check suppresses it
        let mut other = node("other", "a.ts");
        other.push_dynamic_site(opaque_site());
        let kg = graph(vec![used, caller, other], vec![e]);
        let idx = DynamicHazardIndex::build(&kg);
        assert!(dependents_caveat(&kg, &idx, kg.node(&NodeId("used".into())).unwrap()).is_none());
    }

    #[test]
    fn no_caveat_without_any_dynamic_signal() {
        let orphan = node("orphan", "clean.ts");
        let kg = graph(vec![orphan], vec![]);
        let idx = DynamicHazardIndex::build(&kg);
        assert!(dependents_caveat(&kg, &idx, kg.node(&NodeId("orphan".into())).unwrap()).is_none());
    }

    #[test]
    fn caveat_fires_for_dynamically_referenced_node() {
        let mut tgt = node("handler", "b.ts");
        tgt.set_dynamically_referenced(true);
        let kg = graph(vec![tgt], vec![]);
        let idx = DynamicHazardIndex::build(&kg);
        let c = dependents_caveat(&kg, &idx, kg.node(&NodeId("handler".into())).unwrap())
            .expect("caveat");
        assert!(c.dynamically_referenced);
    }
}
