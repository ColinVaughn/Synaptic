use std::fs;

use assert_cmd::Command;

#[test]
fn chart_command_writes_self_contained_architecture_html() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    let output = dir.path().join("architecture.html");
    fs::write(
        &graph,
        r#"{
          "directed": true,
          "multigraph": false,
          "graph": {},
          "nodes": [
            {"id":"api","label":"ApiServer","file_type":"code","source_file":"services/api/src/main.rs","community":0},
            {"id":"db","label":"Database","file_type":"code","source_file":"services/db/src/lib.rs","community":1}
          ],
          "links": [
            {"source":"api","target":"db","relation":"queries","confidence":"EXTRACTED","source_file":"services/api/src/main.rs","weight":1.0}
          ],
          "hyperedges": []
        }"#,
    )
    .unwrap();

    Command::cargo_bin("synaptic")
        .unwrap()
        .args(["chart", "--graph"])
        .arg(&graph)
        .args(["--out"])
        .arg(&output)
        .assert()
        .success();

    let html = fs::read_to_string(output).unwrap();
    assert!(html.contains("Synaptic / Signal Atlas"));
    assert!(html.contains("ApiServer"));
    assert!(html.contains("queries"));
    assert!(html.contains("download=\"synaptic-architecture.svg\""));
    assert!(!html.contains("<script src="));
}
