//! The shard manifest: atomic save/load and validation against disk.

use synaptic_store::manifest::{ShardEntry, ShardManifest};
use synaptic_store::shard::SCHEMA_VERSION;
use synaptic_store::DEFAULT_MAX_SHARD_NODES;

fn entry(tag: &str, file: &str) -> ShardEntry {
    ShardEntry {
        tag: tag.into(),
        file: file.into(),
        source_hash: "h".into(),
        node_count: 1,
        edge_count: 0,
        directed: true,
    }
}

fn manifest(shards: Vec<ShardEntry>) -> ShardManifest {
    ShardManifest {
        schema_version: SCHEMA_VERSION,
        shards,
        bridge: None,
    }
}

#[test]
fn save_load_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    manifest(vec![entry("billing", "billing.redb")])
        .save(dir.path())
        .unwrap();
    let back = ShardManifest::load(dir.path()).unwrap();
    assert_eq!(back.shards.len(), 1);
    assert_eq!(back.shards[0].tag, "billing");
    assert_eq!(back.schema_version, SCHEMA_VERSION);
}

#[test]
fn load_missing_is_empty_current_schema() {
    let dir = tempfile::tempdir().unwrap();
    let m = ShardManifest::load(dir.path()).unwrap();
    assert!(m.shards.is_empty());
    assert_eq!(m.schema_version, SCHEMA_VERSION);
}

#[test]
fn validate_rejects_wrong_schema() {
    let dir = tempfile::tempdir().unwrap();
    ShardManifest {
        schema_version: 999,
        shards: vec![],
        bridge: None,
    }
    .save(dir.path())
    .unwrap();
    let loaded = ShardManifest::load(dir.path()).unwrap();
    assert!(loaded.validate(dir.path()).is_err());
}

#[test]
fn validate_accepts_an_older_store_schema() {
    // v1 stores stay openable: shard files self-describe their encoding, so
    // the manifest version is a floor (minimum reader), not an equality.
    let dir = tempfile::tempdir().unwrap();
    ShardManifest {
        schema_version: 1,
        shards: vec![],
        bridge: None,
    }
    .save(dir.path())
    .unwrap();
    let loaded = ShardManifest::load(dir.path()).unwrap();
    loaded.validate(dir.path()).expect("schema 1 must validate");
}

#[test]
fn validate_rejects_missing_shard_file() {
    let dir = tempfile::tempdir().unwrap();
    let m = manifest(vec![entry("x", "x.redb")]); // x.redb never created
    m.save(dir.path()).unwrap();
    assert!(m.validate(dir.path()).is_err());
}

#[test]
fn validate_passes_when_files_present() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.redb"), b"not really redb, just present").unwrap();
    let m = manifest(vec![entry("x", "x.redb")]);
    m.save(dir.path()).unwrap();
    assert!(m.validate(dir.path()).is_ok());
}

#[test]
fn validate_rejects_shard_over_node_cap() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.redb"), b"present").unwrap();
    let m = manifest(vec![ShardEntry {
        tag: "big".into(),
        file: "big.redb".into(),
        source_hash: "h".into(),
        node_count: DEFAULT_MAX_SHARD_NODES + 1,
        edge_count: 0,
        directed: true,
    }]);
    m.save(dir.path()).unwrap();
    assert!(m.validate(dir.path()).is_err());
}

#[test]
fn validate_allows_aggregate_over_legacy_cap() {
    // Three shards, each well under the per-shard cap, together far exceeding the
    // legacy 100k single-graph node limit -> the federation now validates, since
    // the store has no aggregate gate (the ceiling the rewrite removes).
    let dir = tempfile::tempdir().unwrap();
    let mut shards = Vec::new();
    for t in ["a", "b", "c"] {
        std::fs::write(dir.path().join(format!("{t}.redb")), b"present").unwrap();
        shards.push(ShardEntry {
            tag: t.into(),
            file: format!("{t}.redb"),
            source_hash: "h".into(),
            node_count: 60_000, // 3 x 60k = 180k aggregate, over the legacy 100k cap
            edge_count: 0,
            directed: true,
        });
    }
    let m = manifest(shards);
    m.save(dir.path()).unwrap();
    assert!(m.validate(dir.path()).is_ok());
}

#[test]
fn save_is_atomic_no_tmp_left() {
    let dir = tempfile::tempdir().unwrap();
    manifest(vec![]).save(dir.path()).unwrap();
    assert!(!dir.path().join("manifest.json.tmp").exists());
    assert!(dir.path().join("manifest.json").exists());
}
