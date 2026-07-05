//! A re-extract (write of a new shard version) must not disturb an
//! already-materialized graph, and a fresh open must see the new version.
//!
//! Phase 1 reads materialize a shard fully into RAM and close the redb handle,
//! and writes use versioned filenames flipped via the manifest (RCU), so a
//! re-extract never replaces a file another handle holds open (the Windows
//! sharing pitfall).

use serde_json::Map;
use synaptic_core::{FileType, GraphData, Node, NodeId};
use synaptic_store::ShardStore;

fn graph(ids: &[&str]) -> GraphData {
    let mut gd = GraphData {
        directed: true,
        ..GraphData::default()
    };
    gd.nodes = ids
        .iter()
        .map(|id| Node {
            id: NodeId((*id).into()),
            label: (*id).into(),
            file_type: FileType::Code,
            source_file: format!("src/{id}.rs"),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        })
        .collect();
    gd
}

#[test]
fn rewrite_does_not_disturb_already_materialized_graph() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ShardStore::open(dir.path()).unwrap();
    store.write_shard("x", &graph(&["a", "b"]), "v1").unwrap();

    let g1 = store.materialize("x").unwrap();
    assert_eq!(g1.node_count(), 2);

    // overwrite with a different (smaller) version
    store.write_shard("x", &graph(&["a"]), "v2").unwrap();

    // the already-materialized graph is in RAM and unaffected by the swap
    assert_eq!(
        g1.node_count(),
        2,
        "held graph must be stable across a rewrite"
    );

    // a fresh open sees the new version
    let store = ShardStore::open(dir.path()).unwrap();
    assert_eq!(store.materialize("x").unwrap().node_count(), 1);
    assert_eq!(store.list_shards()[0].source_hash, "v2");
    assert_eq!(
        store.list_shards().len(),
        1,
        "tag still has exactly one shard"
    );
}
