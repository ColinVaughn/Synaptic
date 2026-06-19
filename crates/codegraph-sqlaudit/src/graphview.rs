//! Read-only SQL-shaped views over the KnowledgeGraph. All lookups are by the
//! kinds/edges Phase 1 persists (table/column/index/policy/role nodes;
//! has_column/has_index/indexes/protected_by/grants/queries/writes_to edges).
use codegraph_core::{Node, NodeId, NodeKind};
use codegraph_graph::KnowledgeGraph;
use std::collections::HashSet;

/// All nodes of one SQL kind.
pub fn nodes_of_kind(kg: &KnowledgeGraph, kind: NodeKind) -> Vec<&Node> {
    kg.nodes().filter(|n| n.kind() == Some(kind)).collect()
}

/// Targets of `relation` edges out of `id` (e.g. table -> columns).
pub fn out_targets<'a>(kg: &'a KnowledgeGraph, id: &NodeId, relation: &str) -> Vec<&'a Node> {
    kg.incident_edges(id)
        .filter(|e| &e.source == id && e.relation == relation)
        .filter_map(|e| kg.node(&e.target))
        .collect()
}

/// Columns of a table (via `has_column`).
pub fn columns_of<'a>(kg: &'a KnowledgeGraph, table: &NodeId) -> Vec<&'a Node> {
    out_targets(kg, table, "has_column")
}

/// Lowercased column names that have at least one index on this table.
pub fn indexed_columns(kg: &KnowledgeGraph, table: &NodeId) -> HashSet<String> {
    let mut cols = HashSet::new();
    for idx in out_targets(kg, table, "has_index") {
        for col in out_targets(kg, &idx.id, "indexes") {
            cols.insert(col.label.to_lowercase());
        }
    }
    cols
}

/// Policies protecting a table (via `protected_by`).
pub fn policies_of<'a>(kg: &'a KnowledgeGraph, table: &NodeId) -> Vec<&'a Node> {
    out_targets(kg, table, "protected_by")
}

/// A node-extra bool (e.g. rls_enabled), defaulting to false when absent.
pub fn table_flag(node: &Node, key: &str) -> bool {
    node.extra
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}
