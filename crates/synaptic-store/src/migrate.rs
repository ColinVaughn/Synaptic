//! Split a flat `graph.json` (`GraphData`) into per-repo shards.
//!
//! Nodes group by their `repo` tag (`None` -> the `local` shard). An edge whose
//! endpoints live in the same shard stays in that shard; an edge that crosses a
//! repo boundary goes to the cross-repo `bridge` (traversed only on an opt-in
//! cross-repo query). Node and edge order is preserved within each shard so a
//! single-repo migrate -> export reproduces a byte-identical `graph.json`.

use std::collections::{BTreeMap, HashMap, HashSet};

use synaptic_core::{Edge, GraphData, NodeId};

use crate::{codec, ShardStore, StoreError};

/// The default shard tag for nodes with no federation `repo`.
pub const LOCAL: &str = "local";

/// The result of splitting a graph into shards plus the cross-repo bridge.
pub struct Split {
    /// `(tag, shard graph)` pairs, in tag-sorted order.
    pub shards: Vec<(String, GraphData)>,
    /// Edges whose endpoints live in different shards.
    pub bridge: Vec<Edge>,
}

/// Summary of a migrate, returned to the caller (e.g. to report bridge size).
pub struct MigrateReport {
    pub shard_tags: Vec<String>,
    pub bridge_edges: usize,
    /// How many shards were unchanged and skipped (incremental rebuild).
    pub skipped: usize,
}

fn base_shard(gd: &GraphData) -> GraphData {
    GraphData {
        directed: gd.directed,
        multigraph: gd.multigraph,
        graph: serde_json::Map::new(),
        nodes: Vec::new(),
        links: Vec::new(),
        hyperedges: Vec::new(),
        built_at_commit: gd.built_at_commit.clone(),
    }
}

/// Group `gd` into per-repo shards and a cross-repo bridge.
pub fn split(gd: &GraphData) -> Split {
    let node_repo: HashMap<&NodeId, &str> = gd
        .nodes
        .iter()
        .map(|n| (&n.id, n.repo.as_deref().unwrap_or(LOCAL)))
        .collect();
    let repo_of =
        |id: &NodeId| -> String { node_repo.get(id).copied().unwrap_or(LOCAL).to_string() };

    let mut shards: BTreeMap<String, GraphData> = BTreeMap::new();

    for n in &gd.nodes {
        let tag = n.repo.as_deref().unwrap_or(LOCAL).to_string();
        shards
            .entry(tag)
            .or_insert_with(|| base_shard(gd))
            .nodes
            .push(n.clone());
    }

    let mut bridge = Vec::new();
    for e in &gd.links {
        let sr = repo_of(&e.source);
        let tr = repo_of(&e.target);
        if sr == tr {
            shards
                .entry(sr)
                .or_insert_with(|| base_shard(gd))
                .links
                .push(e.clone());
        } else {
            bridge.push(e.clone());
        }
    }

    // A hyperedge whose members all live in one shard belongs to that shard;
    // a cross-repo hyperedge has no single home and is omitted from per-shard
    // storage (rare; the underlying nodes still exist in their own shards).
    for h in &gd.hyperedges {
        let repos: HashSet<String> = h.nodes.iter().map(&repo_of).collect();
        if repos.len() == 1 {
            let tag = repos.into_iter().next().expect("len == 1");
            shards
                .entry(tag)
                .or_insert_with(|| base_shard(gd))
                .hyperedges
                .push(h.clone());
        }
    }

    Split {
        shards: shards.into_iter().collect(),
        bridge,
    }
}

/// Content hash for a shard: its node ids + edge keys, order-independent.
pub(crate) fn shard_hash(gd: &GraphData) -> String {
    let ids: Vec<String> = gd.nodes.iter().map(|n| n.id.0.clone()).collect();
    let eks: Vec<String> = gd
        .links
        .iter()
        .map(|e| format!("{}>{}:{}", e.source.0, e.target.0, e.relation))
        .collect();
    codec::source_hash(&ids, &eks)
}

/// Split `gd` and write every shard into `store`. Cross-repo bridge edges are
/// reported but stored separately (see the bridge pseudo-shard).
pub fn migrate_into(store: &mut ShardStore, gd: &GraphData) -> Result<MigrateReport, StoreError> {
    let split = split(gd);
    let mut shard_tags = Vec::new();
    let mut skipped = 0;
    for (tag, shard) in &split.shards {
        let hash = shard_hash(shard);
        // Incremental: a shard whose content hash already matches the store is
        // unchanged, so skip rewriting it (and rebuilding its indexes).
        let unchanged = store
            .manifest()
            .entry(tag)
            .is_some_and(|e| e.source_hash == hash);
        if unchanged {
            skipped += 1;
        } else {
            store.write_shard(tag, shard, &hash)?;
        }
        shard_tags.push(tag.clone());
    }
    // Store the cross-repo bridge (edges spanning two repos) apart from the
    // per-repo shards; queries graft it by default when it is non-empty
    // (SYNAPTIC_CROSS_REPO=0 isolates).
    store.write_bridge(&split.bridge, gd.directed)?;
    Ok(MigrateReport {
        shard_tags,
        bridge_edges: split.bridge.len(),
        skipped,
    })
}
