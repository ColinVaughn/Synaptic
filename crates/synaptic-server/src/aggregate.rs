//! Streaming accumulators that turn per-shard passes into exact global results,
//! plus a version-keyed cache so each aggregate is computed once per content
//! version. The accumulators (stats, then god-node degree, global df, community
//! map as those tools migrate) match what the equivalent whole-graph function
//! returns, so a federated result is identical to running it on the union.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use synaptic_core::{Confidence, Edge, NodeId};
use synaptic_graph::{GodNode, GraphStats, KnowledgeGraph};
use synaptic_query::QueryIndex;

/// Version-keyed cache of exact global aggregates. `version` is the provider's
/// content fingerprint (the combined shard-hash); the cache is owned by the
/// provider instance, which is rebuilt (dropping the cache) when a shard changes,
/// so a stale aggregate is never served.
pub struct AggregateCache {
    version: String,
    stats: OnceLock<GraphStats>,
    god_nodes: OnceLock<Vec<GodNode>>,
    communities: OnceLock<BTreeMap<u32, Vec<NodeId>>>,
    global_query: OnceLock<(QueryIndex, HashMap<NodeId, String>)>,
}

impl AggregateCache {
    pub fn new(version: String) -> Self {
        AggregateCache {
            version,
            stats: OnceLock::new(),
            god_nodes: OnceLock::new(),
            communities: OnceLock::new(),
            global_query: OnceLock::new(),
        }
    }

    /// The content version these aggregates are keyed by.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Get the cached global `GraphStats`, computing it once via `compute`.
    pub fn stats(&self, compute: impl FnOnce() -> GraphStats) -> &GraphStats {
        self.stats.get_or_init(compute)
    }

    /// Get the cached global god-node ranking, computing it once via `compute`.
    pub fn god_nodes(&self, compute: impl FnOnce() -> Vec<GodNode>) -> &[GodNode] {
        self.god_nodes.get_or_init(compute)
    }

    /// Get the cached global community map, computing it once via `compute`.
    pub fn communities(
        &self,
        compute: impl FnOnce() -> BTreeMap<u32, Vec<NodeId>>,
    ) -> &BTreeMap<u32, Vec<NodeId>> {
        self.communities.get_or_init(compute)
    }

    /// Get the cached global query index + node-owner map, computing once via
    /// `compute`. The owner map (node id -> shard tag) is what lets result
    /// assembly and node rendering find the shard that holds a ranked id.
    pub fn global_query(
        &self,
        compute: impl FnOnce() -> (QueryIndex, HashMap<NodeId, String>),
    ) -> &(QueryIndex, HashMap<NodeId, String>) {
        self.global_query.get_or_init(compute)
    }
}

/// Accumulates an exact global [`GraphStats`] across a streamed pass over the
/// shards plus the cross-repo bridge. Mirrors `synaptic_graph::graph_stats`
/// field-for-field, so the streamed result equals running it on the union.
#[derive(Default)]
pub struct StatsAcc {
    nodes: usize,
    edges: usize,
    communities: HashSet<u32>,
    extracted: usize,
    inferred: usize,
    ambiguous: usize,
    cross_repo: usize,
    cross_language: usize,
}

impl StatsAcc {
    /// Fold one shard's nodes + edges into the accumulator.
    pub fn add_shard(&mut self, kg: &KnowledgeGraph) {
        self.nodes += kg.node_count();
        for n in kg.nodes() {
            if let Some(c) = n.community {
                self.communities.insert(c);
            }
        }
        for e in kg.edges() {
            self.add_edge(e);
        }
    }

    /// Fold the cross-repo bridge edges in (they carry no nodes).
    pub fn add_edges(&mut self, edges: &[Edge]) {
        for e in edges {
            self.add_edge(e);
        }
    }

    fn add_edge(&mut self, e: &Edge) {
        self.edges += 1;
        match e.confidence {
            Confidence::Extracted => self.extracted += 1,
            Confidence::Inferred => self.inferred += 1,
            Confidence::Ambiguous => self.ambiguous += 1,
        }
        if e.cross_repo {
            self.cross_repo += 1;
        }
        // Cross-language coupling is counted by RELATION, same-repo included,
        // mirroring graph_stats (the 2026-07 audit's counting rule).
        if synaptic_graph::CROSS_LANGUAGE_RELATIONS.contains(&e.relation.as_str()) {
            self.cross_language += 1;
        }
    }

    /// Finalize into a `GraphStats` (distinct community count, etc.).
    pub fn finish(self) -> GraphStats {
        GraphStats {
            nodes: self.nodes,
            edges: self.edges,
            communities: self.communities.len(),
            extracted: self.extracted,
            inferred: self.inferred,
            ambiguous: self.ambiguous,
            cross_repo: self.cross_repo,
            cross_language: self.cross_language,
        }
    }
}
