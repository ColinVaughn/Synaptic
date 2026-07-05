//! End-to-end: `synaptic migrate` builds a redb store, and read commands return
//! identical output whether they load from graph.json or from the store.

use assert_cmd::Command;
use std::fs;

const GRAPH_JSON: &str = r#"{
  "directed": true,
  "multigraph": false,
  "graph": {},
  "nodes": [
    {"id": "alpha", "label": "alpha", "file_type": "code", "source_file": "src/alpha.rs"},
    {"id": "beta", "label": "beta", "file_type": "code", "source_file": "src/beta.rs"}
  ],
  "links": [
    {"source": "alpha", "target": "beta", "relation": "calls",
     "confidence": "EXTRACTED", "source_file": "src/alpha.rs", "weight": 1.0}
  ],
  "hyperedges": []
}"#;

#[test]
fn migrate_builds_store_then_query_matches_across_backends() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("synaptic-out");
    fs::create_dir_all(&out).unwrap();
    let graph = out.join("graph.json");
    fs::write(&graph, GRAPH_JSON).unwrap();

    // migrate graph.json -> shard store
    Command::cargo_bin("synaptic")
        .unwrap()
        .args(["migrate", "--graph"])
        .arg(&graph)
        .assert()
        .success();
    assert!(
        out.join("store").join("manifest.json").exists(),
        "migrate must write the store manifest"
    );

    // query via json (forced, so the auto-default's fresh store can't pick redb)
    let json_out = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["query", "alpha", "--graph"])
        .arg(&graph)
        .env("SYNAPTIC_STORE", "json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    // query via redb backend (loads from the migrated store)
    let redb_out = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["query", "alpha", "--graph"])
        .arg(&graph)
        .env("SYNAPTIC_STORE", "redb")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        json_out, redb_out,
        "query output must be identical across the json and redb backends"
    );
    // sanity: the query actually returned the seed
    assert!(String::from_utf8_lossy(&json_out).contains("alpha"));
}

const CROSS_REPO_GRAPH: &str = r#"{
  "directed": true,
  "multigraph": false,
  "graph": {},
  "nodes": [
    {"id": "biller", "label": "biller", "file_type": "code",
     "source_file": "billing/biller.rs", "repo": "billing"},
    {"id": "webcaller", "label": "webcaller", "file_type": "code",
     "source_file": "web/webcaller.rs", "repo": "web"}
  ],
  "links": [
    {"source": "webcaller", "target": "biller", "relation": "calls",
     "confidence": "EXTRACTED", "source_file": "web/webcaller.rs",
     "weight": 1.0, "cross_repo": true}
  ],
  "hyperedges": []
}"#;

#[test]
fn cross_repo_affected_default_on_and_opt_out() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("synaptic-out");
    fs::create_dir_all(&out).unwrap();
    let graph = out.join("graph.json");
    fs::write(&graph, CROSS_REPO_GRAPH).unwrap();

    Command::cargo_bin("synaptic")
        .unwrap()
        .args(["migrate", "--graph"])
        .arg(&graph)
        .assert()
        .success();

    // Default (unset): the store has bridge edges, so the cross-repo caller
    // is a dependent with no opt-in needed; stderr names the opt-out.
    let def_out = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["affected", "biller", "--graph"])
        .arg(&graph)
        .env("SYNAPTIC_STORE", "redb")
        .env_remove("SYNAPTIC_CROSS_REPO")
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(
        String::from_utf8_lossy(&def_out.stdout).contains("webcaller"),
        "bridge edges present: the default must traverse them"
    );
    assert!(
        String::from_utf8_lossy(&def_out.stderr).contains("SYNAPTIC_CROSS_REPO=0"),
        "default traversal should name the isolation opt-out"
    );

    // Opt-out: per-repo isolation, with a note that edges were skipped.
    let iso_out = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["affected", "biller", "--graph"])
        .arg(&graph)
        .env("SYNAPTIC_STORE", "redb")
        .env("SYNAPTIC_CROSS_REPO", "0")
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(
        !String::from_utf8_lossy(&iso_out.stdout).contains("webcaller"),
        "SYNAPTIC_CROSS_REPO=0 must not traverse the cross-repo bridge"
    );
    assert!(
        String::from_utf8_lossy(&iso_out.stderr).contains("not traversed"),
        "isolation should say the cross-repo edges were skipped"
    );

    // Legacy opt-in still forces traversal.
    let cross = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["affected", "biller", "--graph"])
        .arg(&graph)
        .env("SYNAPTIC_STORE", "redb")
        .env("SYNAPTIC_CROSS_REPO", "1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8_lossy(&cross).contains("webcaller"),
        "SYNAPTIC_CROSS_REPO=1 must graft the bridge so the cross-repo caller appears"
    );
}

#[test]
fn extract_store_flag_builds_usable_store() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/app.py"),
        "def run():\n    helper()\n\ndef helper():\n    return 1\n",
    )
    .unwrap();

    // extract with --store builds graph.json AND the sharded store in one step
    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .arg("--store")
        .assert()
        .success();

    let store = root.join("synaptic-out").join("store");
    assert!(
        store.join("manifest.json").exists(),
        "extract --store must build the store with no separate migrate"
    );

    // the freshly built store serves a redb-backed query
    Command::cargo_bin("synaptic")
        .unwrap()
        .args(["query", "run", "--graph"])
        .arg(root.join("synaptic-out").join("graph.json"))
        .env("SYNAPTIC_STORE", "redb")
        .assert()
        .success();
}

#[test]
fn migrate_persists_usable_indexes() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("synaptic-out");
    fs::create_dir_all(&out).unwrap();
    let graph = out.join("graph.json");
    fs::write(&graph, GRAPH_JSON).unwrap();

    Command::cargo_bin("synaptic")
        .unwrap()
        .args(["migrate", "--graph"])
        .arg(&graph)
        .assert()
        .success();

    // The migrated shard carries pre-built, deserializable derived indexes, so a
    // later `serve` loads them instead of rebuilding.
    let store = synaptic_store::ShardStore::open(&out.join("store")).unwrap();
    let entry = &store.list_shards()[0];
    let qi = store
        .get_index_blob(&entry.tag, "query_index", &entry.source_hash)
        .unwrap()
        .expect("migrate should persist a query_index blob");
    synaptic_query::QueryIndex::from_bytes(&qi).expect("query_index blob deserializes");
    let ai = store
        .get_index_blob(&entry.tag, "affected_index", &entry.source_hash)
        .unwrap()
        .expect("migrate should persist an affected_index blob");
    synaptic_query::ReverseImpactIndex::from_bytes(&ai).expect("affected_index blob deserializes");
}
