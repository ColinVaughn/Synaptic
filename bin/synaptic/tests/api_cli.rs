use std::fs;
use std::process::Command;

use assert_cmd::cargo::{cargo_bin_cmd, CommandCargoExt};

fn repository() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join(".synaptic")).unwrap();
    fs::write(
        repo.path().join(".synaptic/api-maintenance.toml"),
        r#"
schema = 1

[[vendors]]
id = "stripe"
packages = ["npm:stripe", "pypi:stripe"]
hosts = ["api.stripe.com"]

[[vendors.sdk_bindings]]
package = "npm:stripe"
member = "customers.create"
method = "POST"
path = "/v1/customers"

[[vendors]]
id = "other_pay"
packages = ["npm:other-pay"]
hosts = ["api.other-pay.test"]
"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("package.json"),
        r#"{"dependencies":{"stripe":"^18","other-pay":"^3","chalk":"^5"}}"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("package-lock.json"),
        r#"{
          "lockfileVersion": 3,
          "packages": {
            "node_modules/stripe": {"version":"18.2.1"},
            "node_modules/other-pay": {"version":"3.1.0"},
            "node_modules/chalk": {"version":"5.4.0"}
          }
        }"#,
    )
    .unwrap();
    repo
}

#[test]
fn api_inventory_json_reports_multiple_vendors_and_resolved_versions() {
    let repo = repository();
    let output = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "inventory",
            "--root",
            repo.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["version"], 1);
    assert_eq!(report["dependencies"].as_array().unwrap().len(), 3);
    assert_eq!(report["matched"].as_array().unwrap().len(), 2);
    assert_eq!(report["matched"][0]["vendor_id"], "other_pay");
    assert_eq!(
        report["matched"][0]["dependency"]["resolved_version"],
        "3.1.0"
    );
    assert_eq!(report["matched"][1]["vendor_id"], "stripe");
    assert_eq!(report["unmatched"][0]["package"], "npm:chalk");
}

#[test]
fn api_inventory_vendor_filter_is_not_stripe_specific() {
    let repo = repository();
    let output = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "inventory",
            "--root",
            repo.path().to_str().unwrap(),
            "--vendor",
            "other_pay",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["matched"].as_array().unwrap().len(), 1);
    assert_eq!(report["matched"][0]["vendor_id"], "other_pay");
}

#[test]
fn api_inventory_missing_config_fails_with_actionable_path() {
    let repo = tempfile::tempdir().unwrap();
    let output = cargo_bin_cmd!("synaptic")
        .args(["api", "inventory", "--root", repo.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(".synaptic"), "{stderr}");
    assert!(stderr.contains("api-maintenance.toml"), "{stderr}");
}

#[test]
fn api_coverage_reports_unconfigured_surfaces_without_requiring_config() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("synaptic-out")).unwrap();
    fs::write(
        repo.path().join("package.json"),
        r#"{"dependencies":{"unknown-sdk":"^2.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("package-lock.json"),
        r#"{"lockfileVersion":3,"packages":{"node_modules/unknown-sdk":{"version":"2.1.0"}}}"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("synaptic-out/graph.json"),
        serde_json::to_vec(&serde_json::json!({
            "directed": true,
            "multigraph": true,
            "graph": {},
            "nodes": [
                {"id":"caller","label":"call","file_type":"code","source_file":"src/client.ts"},
                {"id":"candidate","label":"unknown-sdk","file_type":"concept","source_file":""}
            ],
            "links": [{
                "source":"caller",
                "target":"candidate",
                "relation":"calls_sdk",
                "confidence":"EXTRACTED",
                "confidence_score":0.95,
                "weight":1.0,
                "source_file":"src/client.ts",
                "source_location":"4:1",
                "sdk_ecosystem":"npm",
                "sdk_import":"unknown-sdk",
                "sdk_package":"npm:unknown-sdk",
                "sdk_member_chain":"widgets.create"
            }],
            "hyperedges": []
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        repo.path().join("canary.json"),
        r#"{"version":1,"environment":"test","window_start_unix_nano":1,"window_end_unix_nano":2,"observations":[{"kind":"http","protocol":"https","method":"GET","authority":"api.unknown.test","path":"/health","outcome":"unavailable","occurrences":2}]}"#,
    )
    .unwrap();

    let output = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "coverage",
            "--root",
            repo.path().to_str().unwrap(),
            "--behavioral-evidence",
            "canary.json",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["version"], 1);
    assert_eq!(report["complete"], false);
    assert_eq!(report["raw_evidence"], 2);
    assert_eq!(report["dependency_inventory"], 1);
    assert_eq!(report["counts"]["observed"], 1);
    let dependency = report["observations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|observation| observation["package"] == "npm:unknown-sdk")
        .unwrap();
    assert!(dependency["gaps"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("provider_identity")));
    assert_eq!(
        report["behavioral_review_candidates"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let enforced = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "coverage",
            "--root",
            repo.path().to_str().unwrap(),
            "--require-complete",
        ])
        .output()
        .unwrap();
    assert!(!enforced.status.success());
    assert!(
        String::from_utf8_lossy(&enforced.stderr).contains("coverage gap"),
        "{}",
        String::from_utf8_lossy(&enforced.stderr)
    );
}

#[test]
fn api_discover_finds_non_openapi_contracts_without_config() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("service.proto"),
        r#"syntax = "proto3"; service Health { rpc Check(Req) returns (Res); }"#,
    )
    .unwrap();
    let output = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "discover",
            "--root",
            repo.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["candidates_scanned"], 1);
    assert_eq!(report["contracts"][0]["format"], "protobuf");
    assert_eq!(report["contracts"][0]["operations"], 1);
}

#[test]
fn extract_and_update_persist_the_same_unconfigured_coverage_ledger() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("package.json"),
        r#"{"dependencies":{"unknown-sdk":"^2.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("client.ts"),
        r#"import client from "unknown-sdk";
export function create() { return client.widgets.create({}); }
"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("openapi.yaml"),
        "openapi: 3.1.0\npaths:\n  /widgets:\n    get:\n      responses:\n        '200':\n          description: ok\n",
    )
    .unwrap();

    let extract = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["extract", "."])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        extract.status.success(),
        "{}",
        String::from_utf8_lossy(&extract.stderr)
    );
    let coverage_path = repo
        .path()
        .join("synaptic-out/api-maintenance/coverage.json");
    let extracted = fs::read(&coverage_path).expect("extract coverage artifact");
    let report: serde_json::Value = serde_json::from_slice(&extracted).unwrap();
    assert_eq!(report["complete"], false);
    assert!(report["observations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| { entry["kind"] == "sdk" && entry["package"] == "npm:unknown-sdk" }));
    let candidate_path = repo
        .path()
        .join("synaptic-out/api-maintenance/candidate-profile.toml");
    let candidate = fs::read_to_string(candidate_path).expect("candidate profile artifact");
    assert!(candidate.contains("enabled = false"));
    assert!(candidate.contains("openapi.yaml"));

    let update = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["update", "--full"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    assert_eq!(
        fs::read(&coverage_path).unwrap(),
        extracted,
        "full and incremental builds must persist identical coverage"
    );
}

#[test]
fn api_init_and_offline_scan_replay_a_checked_in_contract() {
    let repo = tempfile::tempdir().unwrap();
    let init = cargo_bin_cmd!("synaptic")
        .args(["api", "init", "--root", repo.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let config_path = repo.path().join(".synaptic/api-maintenance.toml");
    assert!(config_path.exists());

    fs::write(
        &config_path,
        r#"
schema = 1
[[vendors]]
id = "acme"
hosts = ["api.acme.example"]
[[vendors.sources]]
kind = "static_contract"
path = "openapi.json"
affected_versions = ">=1.0.0, <2.0.0"
"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("openapi.json"),
        r#"{"openapi":"3.0.0","paths":{"/v1/widgets":{"get":{"operationId":"listWidgets","responses":{"200":{"description":"ok"}}}}}}"#,
    )
    .unwrap();
    let first = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "scan",
            "--root",
            repo.path().to_str().unwrap(),
            "--offline",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(report["sources"][0]["disposition"], "baseline_stored");

    fs::write(
        repo.path().join("openapi.json"),
        r#"{"openapi":"3.0.0","paths":{}}"#,
    )
    .unwrap();
    let second = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "scan",
            "--root",
            repo.path().to_str().unwrap(),
            "--offline",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(report["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        report["events"][0]["changes"][0]["kind"],
        "operation_removed"
    );
    let memory = synaptic_memory::MemoryStore::open(repo.path().join(".synaptic/memory"))
        .all()
        .unwrap();
    let release = memory
        .iter()
        .find(|record| record.kind == synaptic_memory::MemoryKind::Release)
        .expect("scan must persist a source-grounded release memory record");
    assert_eq!(release.sources[0].uri, "openapi.json");
    assert_eq!(
        release.sources[0].digest.as_deref(),
        report["events"][0]["source"]["content_digest"].as_str()
    );
}

fn api_topology(repo: &tempfile::TempDir) -> (Vec<String>, Vec<String>) {
    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.path().join("synaptic-out/graph.json")).unwrap())
            .unwrap();
    let mut nodes = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| {
            matches!(
                node["_node_type"].as_str(),
                Some("api_vendor" | "api_operation")
            )
        })
        .map(|node| {
            format!(
                "{}|{}|{}|{}",
                node["id"].as_str().unwrap(),
                node["_node_type"].as_str().unwrap(),
                node["vendor"].as_str().unwrap(),
                node["canonical_path"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>();
    let mut edges = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| matches!(edge["relation"].as_str(), Some("uses_api" | "provided_by")))
        .map(|edge| {
            format!(
                "{}|{}|{}|{}",
                edge["source"].as_str().unwrap(),
                edge["relation"].as_str().unwrap(),
                edge["target"].as_str().unwrap(),
                edge["api_vendor"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>();
    nodes.sort();
    edges.sort();
    (nodes, edges)
}

#[test]
fn graph_binds_multiple_http_vendors_and_incremental_full_rebuild_matches_extract() {
    let repo = repository();
    fs::write(
        repo.path().join("client.ts"),
        r#"
export function stripeCharge() {
  return fetch("https://api.stripe.com/v1/charges", { method: "POST" });
}
export function otherCharge() {
  return fetch("https://api.other-pay.test/v1/charges", { method: "POST" });
}
export function relativeOnly() {
  return fetch("/v1/charges", { method: "POST" });
}
"#,
    )
    .unwrap();

    let extract = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["extract", "."])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        extract.status.success(),
        "{}",
        String::from_utf8_lossy(&extract.stderr)
    );
    let extracted = api_topology(&repo);
    assert_eq!(extracted.0.len(), 4, "two vendors and two operations");
    assert_eq!(
        extracted
            .1
            .iter()
            .filter(|edge| edge.contains("|uses_api|"))
            .count(),
        2,
        "absolute URLs bind, while the relative call does not"
    );
    assert!(extracted.0.iter().any(|node| node.contains("|stripe|")));
    assert!(extracted.0.iter().any(|node| node.contains("|other_pay|")));

    let update = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["update", "--full"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    assert_eq!(
        api_topology(&repo),
        extracted,
        "full extract and incremental full rebuild must produce the same API overlay"
    );

    fs::write(
        repo.path().join("client.ts"),
        r#"
export function stripeRefund() {
  return fetch("https://api.stripe.com/v1/refunds", { method: "POST" });
}
export function otherCharge() {
  return fetch("https://api.other-pay.test/v1/charges", { method: "POST" });
}
"#,
    )
    .unwrap();
    let incremental = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["update", "client.ts"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        incremental.status.success(),
        "{}",
        String::from_utf8_lossy(&incremental.stderr)
    );
    let incrementally_updated = api_topology(&repo);
    assert!(incrementally_updated
        .0
        .iter()
        .any(|node| node.ends_with("|/v1/refunds")));
    assert!(!incrementally_updated
        .0
        .iter()
        .any(|node| node.contains("|stripe|/v1/charges")));

    let fresh_extract = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["extract", "."])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        fresh_extract.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh_extract.stderr)
    );
    assert_eq!(
        api_topology(&repo),
        incrementally_updated,
        "a changed-file update must remove stale operations and match a fresh extract"
    );
}

#[test]
fn graph_binds_configured_sdk_members_and_keeps_inventory_edges_non_usage() {
    let repo = repository();
    fs::write(
        repo.path().join("client.ts"),
        r#"
import Stripe from "stripe";
const stripe = new Stripe("test_key");
export function createCustomer() {
  return stripe.customers.create({ email: "a@example.test" });
}
"#,
    )
    .unwrap();
    let output = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["extract", "."])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.path().join("synaptic-out/graph.json")).unwrap())
            .unwrap();
    let links = graph["links"].as_array().unwrap();
    let sdk_for = links
        .iter()
        .find(|edge| edge["relation"] == "sdk_for" && edge["api_vendor"] == "stripe")
        .expect("installed SDK inventory edge");
    let package = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == sdk_for["source"])
        .unwrap();
    assert_eq!(package["resolved_version"], "18.2.1");
    let usage = links
        .iter()
        .find(|edge| edge["relation"] == "uses_api" && edge["binding_basis"] == "sdk_symbol")
        .expect("SDK call operation binding");
    assert_eq!(usage["sdk_member_chain"], "customers.create");
    assert_eq!(usage["installed_sdk_version"], "18.2.1");
    let operation = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == usage["target"])
        .unwrap();
    assert_eq!(operation["canonical_path"], "/v1/customers");
}

#[test]
fn graph_binds_default_import_constructors_via_stable_default_member() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join(".synaptic")).unwrap();
    fs::write(
        repo.path().join(".synaptic/api-maintenance.toml"),
        r#"
schema = 1
[[vendors]]
id = "redis"
packages = ["npm:ioredis"]
[[vendors.sdk_bindings]]
package = "npm:ioredis"
member = "default"
method = "POST"
path = "/redis/client"
"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("package.json"),
        r#"{"dependencies":{"ioredis":"^5"}}"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("package-lock.json"),
        r#"{"lockfileVersion":3,"packages":{"node_modules/ioredis":{"version":"5.10.1"}}}"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("queue.ts"),
        r#"
import QueueConnection from "ioredis";
export function connect() {
  return new QueueConnection("redis://localhost");
}
"#,
    )
    .unwrap();

    let output = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["extract", "."])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.path().join("synaptic-out/graph.json")).unwrap())
            .unwrap();
    let usage = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|edge| edge["relation"] == "uses_api" && edge["api_vendor"] == "redis")
        .expect("default import constructor must bind to the configured operation");
    assert_eq!(usage["sdk_member_chain"], "default");
    assert_eq!(usage["installed_sdk_version"], "5.10.1");
    assert_eq!(usage["source_file"], "queue.ts");
}

#[test]
fn api_impact_and_repair_dry_run_emit_a_bounded_brief() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join(".synaptic/api-maintenance/events")).unwrap();
    fs::create_dir_all(repo.path().join("synaptic-out")).unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    fs::write(
        repo.path().join(".synaptic/api-maintenance.toml"),
        r#"
schema = 1
allowed_paths = ["src/"]
[[vendors]]
id = "acme"
packages = ["npm:acme-sdk"]
hosts = ["api.acme.example"]
"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("package.json"),
        r#"{"dependencies":{"acme-sdk":"^1"}}"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("package-lock.json"),
        r#"{"lockfileVersion":3,"packages":{"node_modules/acme-sdk":{"version":"1.5.0"}}}"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("src/client.ts"),
        "export function create() { return oldApi(); }\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("tests/client.test.ts"),
        "test('create', create);\n",
    )
    .unwrap();
    let event = serde_json::json!({
        "version":1,"id":"event_acme_1","vendor":"acme","occurred_at":1,
        "source":{"uri":"fixture","revision":"2","content_digest":"digest","fetched_at":1,"adapter_version":1,"evidence_kind":"openapi"},
        "changes":[{"change_id":"change_1","kind":"path_or_method_changed","affected_versions":{"requirement":">=1.0.0, <2.0.0"},"old_operation":{"id":"api_operation:acme:old","vendor":"acme","protocol":"https","method":"POST","canonical_path":"/v1/widgets"},"new_operation":{"id":"api_operation:acme:new","vendor":"acme","protocol":"https","method":"POST","canonical_path":"/v2/widgets"},"old_sdk_symbols":[],"new_sdk_symbols":[],"migration_summary":"move","evidence":[],"confidence":1.0}]
    });
    fs::write(
        repo.path()
            .join(".synaptic/api-maintenance/events/event_acme_1.json"),
        serde_json::to_vec_pretty(&event).unwrap(),
    )
    .unwrap();
    let graph = serde_json::json!({
        "directed":true,"built_at_commit":"base123",
        "nodes":[
            {"id":"api_operation:acme:old","label":"POST /v1/widgets","file_type":"concept","source_file":"","_node_type":"api_operation","vendor":"acme"},
            {"id":"fn:create","label":"create","file_type":"code","source_file":"src/client.ts","kind":"function"},
            {"id":"test:create","label":"create test","file_type":"code","source_file":"tests/client.test.ts","kind":"function","_is_test":true}
        ],
        "links":[
            {"source":"fn:create","target":"api_operation:acme:old","relation":"uses_api","confidence":"EXTRACTED","confidence_score":1.0,"source_file":"src/client.ts","binding_basis":"sdk_symbol","sdk_package":"npm:acme-sdk","sdk_member_chain":"widgets.create","installed_sdk_version":"1.5.0"},
            {"source":"test:create","target":"fn:create","relation":"calls","confidence":"EXTRACTED","source_file":"tests/client.test.ts"}
        ]
    });
    fs::write(
        repo.path().join("synaptic-out/graph.json"),
        serde_json::to_vec(&graph).unwrap(),
    )
    .unwrap();

    let output = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "repair",
            "--event",
            "event_acme_1",
            "--root",
            repo.path().to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["dry_run"], true);
    assert_eq!(
        value["run"]["id"], value["repair_brief"]["id"],
        "the public run id must address every repair artifact"
    );
    let run_id = value["run"]["id"].as_str().unwrap();
    assert!(
        repo.path()
            .join("synaptic-out/api-maintenance")
            .join(run_id)
            .join("repair-brief.json")
            .is_file(),
        "verify/publish must be able to resolve artifacts by run id"
    );
    assert_eq!(value["repair_brief"]["allowed_files"][0], "src/client.ts");
    assert_eq!(
        value["repair_brief"]["required_tests"][0],
        "tests/client.test.ts"
    );

    let replay = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "run",
            "--root",
            repo.path().to_str().unwrap(),
            "--offline",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay: serde_json::Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(replay["scan"]["events"].as_array().unwrap().len(), 0);
    assert_eq!(
        replay["runs"].as_array().unwrap().len(),
        1,
        "stored pending events must be resumed even when the scan finds nothing new"
    );
}

#[test]
fn api_check_plan_reports_every_polyglot_project_and_fails_closed_on_gaps() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("rust/src")).unwrap();
    fs::create_dir_all(repo.path().join("php/src")).unwrap();
    fs::write(
        repo.path().join("rust/Cargo.toml"),
        "[package]\nname='sample'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(repo.path().join("rust/src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::write(
        repo.path().join("php/composer.json"),
        r#"{"scripts":{"check":"phpstan analyse","test":"phpunit"}}"#,
    )
    .unwrap();
    fs::write(repo.path().join("php/src/client.php"), "<?php\n").unwrap();

    let output = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "check-plan",
            "--root",
            repo.path().to_str().unwrap(),
            "--json",
            "--require-complete",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let ecosystems = plan["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|project| project["ecosystem"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        ecosystems,
        std::collections::BTreeSet::from(["php-composer", "rust"])
    );
    assert!(plan["gaps"].as_array().unwrap().is_empty());

    fs::write(
        repo.path().join("package.json"),
        r#"{"scripts":{"build":"vite build"}}"#,
    )
    .unwrap();
    fs::write(repo.path().join("app.ts"), "export const value = 1;\n").unwrap();
    let incomplete = cargo_bin_cmd!("synaptic")
        .args([
            "api",
            "check-plan",
            "--root",
            repo.path().to_str().unwrap(),
            "--require-complete",
        ])
        .output()
        .unwrap();
    assert!(!incomplete.status.success());
    assert!(String::from_utf8_lossy(&incomplete.stderr).contains("verification plan is incomplete"));
}

#[test]
fn graph_binds_sdk_calls_for_every_applicable_language_family() {
    let repo = tempfile::tempdir().unwrap();
    let write = |relative: &str, contents: &str| {
        let path = repo.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    };
    let vendors: &[(&str, &str, &[&str], &[&str])] = &[
        (
            "jvm",
            "maven:com.stripe:stripe-java",
            &["com.stripe"],
            &["StripeClient.builder"],
        ),
        (
            "dotnet",
            "nuget:stripe.net",
            &["Stripe"],
            &["StripeClient.Create"],
        ),
        (
            "php",
            "composer:stripe/stripe-php",
            &["Stripe"],
            &["StripeClient.create"],
        ),
        ("ruby", "gem:stripe", &["stripe"], &["Customer.create"]),
        (
            "swift",
            "swift:stripe-ios",
            &["StripePaymentSheet"],
            &["PaymentSheet.create"],
        ),
        (
            "dart",
            "pub:stripe_sdk",
            &["stripe_sdk"],
            &["Stripe.create"],
        ),
        (
            "elixir",
            "hex:stripity_stripe",
            &["Stripe"],
            &["Stripe.Customer.create"],
        ),
        ("lua", "luarocks:stripe", &["stripe"], &["customers.create"]),
        ("julia", "julia:stripe", &["Stripe"], &["create_customer"]),
        ("zig", "zig:stripe", &["stripe"], &["Client.init"]),
        (
            "powershell",
            "powershell:stripe",
            &["Stripe"],
            &["New-StripeCustomer"],
        ),
        (
            "native",
            "vcpkg:stripe",
            &["stripe"],
            &["Client.create", "stripe_customer_create"],
        ),
        (
            "objc",
            "cocoapods:stripepaymentsheet",
            &["Stripe"],
            &["STPAPIClient.sharedClient"],
        ),
        (
            "apex",
            "salesforce:stripesdk",
            &["stripe"],
            &["Client.create"],
        ),
        (
            "pascal",
            "nuget:stripe.pascal",
            &["StripePascal"],
            &["StripeClient.Create"],
        ),
        (
            "fortran",
            "fpm:stripe",
            &["stripe"],
            &["stripe_create_customer"],
        ),
        (
            "asp",
            "com:stripe.client",
            &["Stripe.Client"],
            &["Client.CreateCustomer"],
        ),
    ];
    let mut config = "schema = 1\n".to_string();
    for (vendor, package, imports, members) in vendors {
        config.push_str(&format!(
            "\n[[vendors]]\nid = \"{vendor}\"\npackages = [\"{package}\"]\n"
        ));
        for (index, member) in members.iter().enumerate() {
            let imports = imports
                .iter()
                .map(|import| format!("\"{import}\""))
                .collect::<Vec<_>>()
                .join(", ");
            config.push_str(&format!(
                "[[vendors.sdk_bindings]]\npackage = \"{package}\"\nimports = [{imports}]\nmember = \"{member}\"\nmethod = \"POST\"\npath = \"/{vendor}/{index}\"\n"
            ));
        }
    }
    write(".synaptic/api-maintenance.toml", &config);

    write(
        "jvm/pom.xml",
        "<project><dependencies><dependency><groupId>com.stripe</groupId><artifactId>stripe-java</artifactId><version>29.3.0</version></dependency></dependencies></project>",
    );
    write(
        "src/Client.java",
        "import com.stripe.StripeClient; class Client { void run() { StripeClient.builder(); } }",
    );
    write(
        "src/Client.kt",
        "import com.stripe.StripeClient\nfun run() { StripeClient.builder() }",
    );
    write(
        "src/Client.groovy",
        "import com.stripe.StripeClient\nStripeClient.builder()",
    );
    write(
        "src/Client.scala",
        "import com.stripe.StripeClient\nobject Client { StripeClient.builder() }",
    );

    write(
        "dotnet/Client.csproj",
        "<Project><ItemGroup><PackageReference Include=\"Stripe.net\" Version=\"48.5.0\"/><PackageReference Include=\"Stripe.Pascal\" Version=\"1.2.3\"/></ItemGroup></Project>",
    );
    write(
        "src/Client.cs",
        "using Stripe; class Client { void Run() { StripeClient.Create(); } }",
    );
    write(
        "src/Client.razor",
        "@using Stripe\n@code { void Run() { StripeClient.Create(); } }",
    );
    write(
        "src/client.pas",
        "uses StripePascal; begin StripeClient.Create(); end.",
    );

    write(
        "php/composer.json",
        r#"{"require":{"stripe/stripe-php":"^17"}}"#,
    );
    write(
        "php/composer.lock",
        r#"{"packages":[{"name":"stripe/stripe-php","version":"17.4.0"}]}"#,
    );
    write(
        "src/client.php",
        "<?php use Stripe\\StripeClient; StripeClient::create();",
    );
    write("ruby/Gemfile", "gem 'stripe', '~> 13'\n");
    write("ruby/Gemfile.lock", "GEM\n  specs:\n    stripe (13.1.0)\n");
    write(
        "src/client.rb",
        "require 'stripe'\nStripe::Customer.create()",
    );
    write(
        "swift/Package.swift",
        r#"let package = Package(name: "App", dependencies: [.package(url: "https://github.com/stripe/stripe-ios.git", from: "24.0.0")])"#,
    );
    write(
        "swift/Package.resolved",
        r#"{"version":2,"pins":[{"identity":"stripe-ios","state":{"version":"24.3.0"}}]}"#,
    );
    write(
        "src/Client.swift",
        "import StripePaymentSheet\nPaymentSheet.create()",
    );
    write("dart/pubspec.yaml", "dependencies:\n  stripe_sdk: ^1.0.0\n");
    write(
        "dart/pubspec.lock",
        "packages:\n  stripe_sdk:\n    dependency: direct main\n    version: 1.2.0\n",
    );
    write(
        "src/client.dart",
        "import 'package:stripe_sdk/stripe_sdk.dart';\nvoid run() { Stripe.create(); }",
    );
    write(
        "elixir/mix.exs",
        "defp deps do\n  [{:stripity_stripe, \"~> 3.2\"}]\nend",
    );
    write(
        "elixir/mix.lock",
        r#"%{"stripity_stripe": {:hex, :stripity_stripe, "3.2.0", "hash"}}"#,
    );
    write(
        "src/client.ex",
        "defmodule Client do\n alias Stripe.Customer\n def run, do: Customer.create(%{})\nend",
    );
    write(
        "lua/client.rockspec",
        "package='client'\nversion='1.0-1'\ndependencies={ 'stripe >= 1.2' }",
    );
    write(
        "lua/luarocks.lock",
        r#"return { ["stripe"] = { ["1.2.3-1"] = {} } }"#,
    );
    write(
        "src/client.lua",
        "local stripe = require('stripe')\nstripe.customers.create()",
    );
    write("julia/Project.toml", "[deps]\nStripe = \"uuid\"\n");
    write(
        "julia/Manifest.toml",
        "[[deps.Stripe]]\nversion = \"0.4.0\"\n",
    );
    write("src/client.jl", "using Stripe\nStripe.create_customer()");
    write(
        "zig/build.zig.zon",
        r#".{ .dependencies = .{ .stripe = .{ .url = "https://example.test/stripe/v1.2.3.tar.gz", .hash = "hash" } } }"#,
    );
    write(
        "src/client.zig",
        "const stripe = @import(\"stripe\");\npub fn run() void { stripe.Client.init(); }",
    );
    write(
        "powershell/Client.psd1",
        "@{ RequiredModules = @(@{ ModuleName='Stripe'; ModuleVersion='1.2.3' }) }",
    );
    write("src/client.ps1", "Import-Module Stripe\nNew-StripeCustomer");
    write(
        "native/vcpkg.json",
        r#"{"dependencies":["stripe"],"overrides":[{"name":"stripe","version":"1.2.3"}]}"#,
    );
    write(
        "src/client.cpp",
        "#include <stripe/stripe.h>\nvoid run() { stripe::Client::create(); }",
    );
    write(
        "src/client.c",
        "#include <stripe/stripe.h>\nvoid run() { stripe_customer_create(); }",
    );
    write("objc/Podfile", "pod 'StripePaymentSheet', '~> 24.0'\n");
    write(
        "objc/Podfile.lock",
        "PODS:\n  - StripePaymentSheet (24.3.0)\n",
    );
    write(
        "src/Client.m",
        "@import Stripe;\nvoid run() { [STPAPIClient sharedClient]; }",
    );
    write(
        "apex/sfdx-project.json",
        r#"{"packageDirectories":[{"path":"force-app","dependencies":[{"package":"StripeSDK@1.2.3-1"}]}]}"#,
    );
    write(
        "src/Client.cls",
        "import stripe.Client; public class Example { void run() { Client.create(); } }",
    );
    write(
        "fortran/fpm.toml",
        "[dependencies]\nstripe = { git='https://example.test/stripe', tag='v1.2.3' }\n",
    );
    write(
        "src/client.f90",
        "program client\nuse stripe\ncall stripe_create_customer()\nend program",
    );
    write(
        "src/client.asp",
        "<% Set client = Server.CreateObject(\"Stripe.Client\")\nclient.CreateCustomer %>",
    );

    let output = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["extract", "."])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.path().join("synaptic-out/graph.json")).unwrap())
            .unwrap();
    let usages = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["relation"] == "uses_api")
        .collect::<Vec<_>>();
    let has_source = |edge: &&serde_json::Value, source: &str| {
        edge["source_file"] == source
            || edge["sites"].as_array().is_some_and(|sites| {
                sites
                    .iter()
                    .any(|site| site["source_file"].as_str() == Some(source))
            })
    };
    for source in [
        "src/Client.java",
        "src/Client.kt",
        "src/Client.groovy",
        "src/Client.scala",
        "src/Client.cs",
        "src/Client.razor",
        "src/client.pas",
        "src/client.php",
        "src/client.rb",
        "src/Client.swift",
        "src/client.dart",
        "src/client.ex",
        "src/client.lua",
        "src/client.jl",
        "src/client.zig",
        "src/client.ps1",
        "src/client.cpp",
        "src/client.c",
        "src/Client.m",
        "src/Client.cls",
        "src/client.f90",
        "src/client.asp",
    ] {
        assert!(
            usages.iter().any(|edge| has_source(edge, source)),
            "{source} did not bind; usages={usages:?}"
        );
    }
    assert!(
        usages
            .iter()
            .filter(|edge| edge["source_file"] != "src/client.asp")
            .all(|edge| edge["installed_sdk_version"].is_string()),
        "every registry-backed language must carry an installed version: {usages:?}"
    );
}
