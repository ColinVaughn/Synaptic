//! Structured node projection for query results.
//!
//! A [`QueryResult`] holds bare node ids so plain queries stay cheap. A caller
//! that wants structured output (the MCP `structural_search` tool, a description
//! generator) resolves those ids into [`NodeView`]s on demand against the graph,
//! surfacing the node's kind, visibility, location, and captured signature.

use synaptic_core::{NodeId, Signature};
use synaptic_graph::KnowledgeGraph;

use crate::QueryResult;

/// A resolved view of a result node: its intrinsic structural metadata plus the
/// captured signature. Text fields are raw graph values; a caller emitting these
/// to an untrusted sink (e.g. an MCP response) is responsible for sanitizing
/// `label`/`id`/`file` as it already does for other tool output.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeView {
    pub id: String,
    pub label: String,
    pub kind: Option<String>,
    pub visibility: Option<String>,
    pub file: String,
    pub line: Option<String>,
    pub loc: Option<u32>,
    pub signature: Option<Signature>,
}

impl NodeView {
    /// Resolve a single node id. A dangling id (no node in `kg`) yields a view
    /// labeled by the id with empty metadata, so output stays total.
    pub fn resolve(kg: &KnowledgeGraph, id: &NodeId) -> NodeView {
        Self::from_found(id, kg.node(id))
    }

    /// Build a view from an already-looked-up node (or the dangling fallback),
    /// shared by the single-graph and federated resolution paths.
    pub fn from_found(id: &NodeId, node: Option<&synaptic_core::Node>) -> NodeView {
        match node {
            Some(n) => NodeView {
                id: n.id.0.clone(),
                label: n.label.clone(),
                kind: n.kind().map(|k| k.as_str().to_string()),
                visibility: n.visibility().map(|v| v.as_str().to_string()),
                file: n.source_file.clone(),
                line: n.source_location.clone(),
                loc: n.loc(),
                signature: n.signature(),
            },
            None => NodeView {
                id: id.0.clone(),
                label: id.0.clone(),
                kind: None,
                visibility: None,
                file: String::new(),
                line: None,
                loc: None,
                signature: None,
            },
        }
    }
}

impl QueryResult {
    /// Resolve each row of node ids into [`NodeView`]s against `kg`. Aggregate
    /// results carry no node rows, so this returns an empty `Vec` for them.
    pub fn node_views(&self, kg: &KnowledgeGraph) -> Vec<Vec<NodeView>> {
        self.rows
            .iter()
            .map(|row| row.iter().map(|id| NodeView::resolve(kg, id)).collect())
            .collect()
    }

    /// Like [`node_views`](Self::node_views) but resolving each id through
    /// `lookup`, for callers whose nodes live across several graphs (the
    /// federated per-shard server resolves each id in its owning shard).
    pub fn node_views_by(
        &self,
        mut lookup: impl FnMut(&NodeId) -> Option<synaptic_core::Node>,
    ) -> Vec<Vec<NodeView>> {
        self.rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|id| NodeView::from_found(id, lookup(id).as_ref()))
                    .collect()
            })
            .collect()
    }
}
