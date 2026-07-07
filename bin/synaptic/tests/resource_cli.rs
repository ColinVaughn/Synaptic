//! End-to-end: `synaptic extract` indexes data/resource files, binds their
//! references to code + other resources, flags generated-vs-source collisions,
//! and the incremental `update --full` pipeline produces the same resource graph.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;

fn synaptic(args: &[&str], dir: &Path) -> std::process::Output {
    Command::cargo_bin("synaptic")
        .unwrap()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run synaptic")
}

fn write(root: &Path, rel: &str, body: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn read_graph(root: &Path) -> Value {
    let s = std::fs::read_to_string(root.join("synaptic-out/graph.json")).unwrap();
    serde_json::from_str(&s).unwrap()
}

/// A tiny mod-shaped tree: a class, two models (one referencing the other by
/// logical id), a loot table referencing the class, and a generated copy that
/// shadows the source model.
fn scaffold(root: &Path) {
    write(
        root,
        "src/main/java/com/mymod/SkeletonBoss.java",
        b"package com.mymod;\npublic class SkeletonBoss {}\n",
    );
    write(
        root,
        "src/main/resources/data/mymod/loot_tables/boss.json",
        br#"{"type":"com.mymod.SkeletonBoss","pools":[]}"#,
    );
    write(
        root,
        "src/main/resources/assets/mymod/models/block/base.json",
        b"{}",
    );
    write(
        root,
        "src/main/resources/assets/mymod/models/block/x.json",
        br#"{"parent":"mymod:block/base"}"#,
    );
    write(
        root,
        "src/main/generated/assets/mymod/models/block/x.json",
        b"{}",
    );
}

fn resource_metrics(g: &Value) -> (usize, usize, usize) {
    let nodes = g["nodes"].as_array().unwrap();
    let res_nodes = nodes
        .iter()
        .filter(|n| n["_node_type"] == "resource")
        .count();
    let links = g["links"].as_array().unwrap();
    let refs = links
        .iter()
        .filter(|e| e["relation"] == "references" && e["context"] == "resource_ref")
        .count();
    let shadows = links.iter().filter(|e| e["relation"] == "shadows").count();
    (res_nodes, refs, shadows)
}

#[test]
fn extract_indexes_resources_binds_refs_and_flags_shadows() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    scaffold(root);

    let ex = synaptic(&["extract", "."], root);
    assert!(
        ex.status.success(),
        "extract stderr: {}",
        String::from_utf8_lossy(&ex.stderr)
    );

    let g = read_graph(root);
    let nodes = g["nodes"].as_array().unwrap();
    let links = g["links"].as_array().unwrap();
    let src_of: HashMap<&str, &str> = nodes
        .iter()
        .map(|n| {
            (
                n["id"].as_str().unwrap(),
                n["source_file"].as_str().unwrap_or(""),
            )
        })
        .collect();

    // Resource files are indexed as nodes.
    let (res_nodes, refs, shadows) = resource_metrics(&g);
    assert!(
        res_nodes >= 4,
        "expected >=4 resource nodes, got {res_nodes}"
    );
    assert!(refs >= 2, "expected resolved resource refs, got {refs}");
    assert_eq!(shadows, 1, "one generated->source shadow edge");

    // (c) The loot table's FQN "com.mymod.SkeletonBoss" bound to the class node (last segment).
    let boss_class = nodes
        .iter()
        .find(|n| n["label"] == "SkeletonBoss")
        .and_then(|n| n["id"].as_str())
        .expect("SkeletonBoss class node exists");
    assert!(
        links.iter().any(|e| e["relation"] == "references"
            && e["context"] == "resource_ref"
            && e["target"] == boss_class),
        "loot table references the SkeletonBoss class"
    );

    // (b) x.json's "mymod:block/base" bound to base.json's resource node.
    assert!(
        links.iter().any(|e| {
            e["relation"] == "references"
                && e["context"] == "resource_ref"
                && e["target"]
                    .as_str()
                    .and_then(|t| src_of.get(t))
                    .is_some_and(|s| s.replace('\\', "/").ends_with("models/block/base.json"))
        }),
        "x.json references base.json by logical id"
    );

    // The shadow edge runs generated -> source.
    let shadow = links
        .iter()
        .find(|e| e["relation"] == "shadows")
        .expect("shadow edge");
    let s_src = src_of[shadow["source"].as_str().unwrap()].replace('\\', "/");
    let s_tgt = src_of[shadow["target"].as_str().unwrap()].replace('\\', "/");
    assert!(
        s_src.contains("/generated/"),
        "shadow source is generated: {s_src}"
    );
    assert!(
        s_tgt.contains("/resources/"),
        "shadow target is source: {s_tgt}"
    );

    // Readiness surfaces the collision.
    let audit = synaptic(&["audit", "readiness", "--json"], root);
    assert!(
        audit.status.success(),
        "{}",
        String::from_utf8_lossy(&audit.stderr)
    );
    let report: Value = serde_json::from_slice(&audit.stdout).unwrap();
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["rule_id"] == "READY-RESOURCE-SHADOW"),
        "readiness reports the shadow: {report}"
    );
}

#[test]
fn no_resources_flag_restores_code_only_graph() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    scaffold(root);
    let ex = synaptic(&["extract", ".", "--no-resources"], root);
    assert!(
        ex.status.success(),
        "{}",
        String::from_utf8_lossy(&ex.stderr)
    );
    let (res_nodes, refs, shadows) = resource_metrics(&read_graph(root));
    assert_eq!(
        (res_nodes, refs, shadows),
        (0, 0, 0),
        "--no-resources indexes no resources"
    );
}

#[test]
fn incremental_full_rebuild_matches_extract_on_resources() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    scaffold(root);

    let ex = synaptic(&["extract", "."], root);
    assert!(
        ex.status.success(),
        "{}",
        String::from_utf8_lossy(&ex.stderr)
    );
    let full = resource_metrics(&read_graph(root));

    // The incremental pipeline's full-rebuild path must produce the same resource
    // nodes, resolved references, and shadow edges as a fresh extract (the known
    // full-vs-incremental drift hazard).
    let up = synaptic(&["update", "--full"], root);
    assert!(
        up.status.success(),
        "{}",
        String::from_utf8_lossy(&up.stderr)
    );
    let incr = resource_metrics(&read_graph(root));

    assert_eq!(
        full, incr,
        "resource graph parity: extract {full:?} vs update --full {incr:?}"
    );
}
