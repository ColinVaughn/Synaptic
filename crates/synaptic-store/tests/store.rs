//! `ShardStore`: open / list / write / materialize / prune across shards.

use serde_json::Map;
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_store::ShardStore;

fn node(id: &str) -> Node {
    Node {
        id: NodeId(id.into()),
        label: id.into(),
        file_type: FileType::Code,
        source_file: format!("src/{id}.rs"),
        source_location: None,
        community: None,
        repo: None,
        extra: Map::new(),
    }
}

fn graph(ids: &[&str]) -> GraphData {
    let mut gd = GraphData {
        directed: true,
        ..GraphData::default()
    };
    gd.nodes = ids.iter().map(|i| node(i)).collect();
    // a single self-relation edge between the first two ids, if present
    if ids.len() >= 2 {
        gd.links.push(Edge {
            source: NodeId(ids[0].into()),
            target: NodeId(ids[1].into()),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: format!("src/{}.rs", ids[0]),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        });
    }
    gd
}

#[test]
fn open_empty_root_has_no_shards() {
    let dir = tempfile::tempdir().unwrap();
    let store = ShardStore::open(dir.path()).unwrap();
    assert!(store.list_shards().is_empty());
}

#[test]
fn write_list_materialize_prune_persists() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ShardStore::open(dir.path()).unwrap();
    store
        .write_shard("billing", &graph(&["a", "b"]), "h1")
        .unwrap();
    store.write_shard("web", &graph(&["c"]), "h2").unwrap();

    // reopen from disk to prove the manifest + files persisted
    let store = ShardStore::open(dir.path()).unwrap();
    assert_eq!(store.list_shards().len(), 2);
    assert_eq!(store.materialize("billing").unwrap().node_count(), 2);
    assert_eq!(store.materialize("billing").unwrap().edge_count(), 1);
    assert_eq!(store.materialize("web").unwrap().node_count(), 1);
    assert_eq!(store.manifest().entry("billing").unwrap().source_hash, "h1");

    // prune drops the entry and its file
    let mut store = ShardStore::open(dir.path()).unwrap();
    store.prune_shard("web").unwrap();
    let store = ShardStore::open(dir.path()).unwrap();
    assert_eq!(store.list_shards().len(), 1);
    assert!(store.materialize("web").is_err());
}

#[test]
fn remigrate_skips_unchanged_shard_and_rewrites_changed() {
    use synaptic_store::migrate;
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");

    let g1 = graph(&["a", "b"]);
    let mut store = ShardStore::open(&store_dir).unwrap();
    let r1 = migrate::migrate_into(&mut store, &g1).unwrap();
    assert_eq!(r1.skipped, 0, "first migrate writes everything");
    let file1 = store.list_shards()[0].file.clone();

    // Re-migrate identical content: the shard is skipped, its file unchanged.
    let mut store = ShardStore::open(&store_dir).unwrap();
    let r2 = migrate::migrate_into(&mut store, &g1).unwrap();
    assert_eq!(r2.skipped, 1, "unchanged shard is skipped");
    assert_eq!(store.list_shards()[0].file, file1, "file not rewritten");

    // Migrate changed content: the shard is rewritten (new versioned file).
    let g2 = graph(&["a", "b", "c"]);
    let mut store = ShardStore::open(&store_dir).unwrap();
    let r3 = migrate::migrate_into(&mut store, &g2).unwrap();
    assert_eq!(r3.skipped, 0, "changed shard is not skipped");
    assert_ne!(
        store.list_shards()[0].file,
        file1,
        "changed shard gets a new file"
    );
    assert_eq!(store.materialize("local").unwrap().node_count(), 3);
}

#[test]
fn index_blobs_keyed_by_name_and_hash() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ShardStore::open(dir.path()).unwrap();
    store
        .write_shard("billing", &graph(&["a", "b"]), "h1")
        .unwrap();

    // get before any put -> miss (blob table absent)
    assert!(store
        .get_index_blob("billing", "query_index", "h1")
        .unwrap()
        .is_none());

    store
        .put_index_blob("billing", "query_index", "h1", b"INDEXBYTES")
        .unwrap();
    assert_eq!(
        store
            .get_index_blob("billing", "query_index", "h1")
            .unwrap()
            .as_deref(),
        Some(&b"INDEXBYTES"[..])
    );

    // different hash (shard changed) -> stale -> miss
    assert!(store
        .get_index_blob("billing", "query_index", "h2")
        .unwrap()
        .is_none());
    // different index name -> miss
    assert!(store
        .get_index_blob("billing", "reverse_impact", "h1")
        .unwrap()
        .is_none());
}

#[test]
fn shards_are_listed_in_a_stable_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ShardStore::open(dir.path()).unwrap();
    store.write_shard("zebra", &graph(&["a"]), "h").unwrap();
    store.write_shard("apple", &graph(&["b"]), "h").unwrap();
    store.write_shard("mango", &graph(&["c"]), "h").unwrap();
    let tags: Vec<&str> = store.list_shards().iter().map(|e| e.tag.as_str()).collect();
    assert_eq!(tags, vec!["apple", "mango", "zebra"]);
}
