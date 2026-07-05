//! Graph safety-cap behavior at the CLI surface: extract succeeds but warns
//! when the written graph exceeds the effective caps (which the merge driver
//! and federation enforce), and stays quiet under them.

use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

fn extract(dir: &Path, envs: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::cargo_bin("synaptic").unwrap();
    cmd.args(["extract", "."]).current_dir(dir);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("run synaptic extract")
}

#[test]
fn extract_warns_when_graph_exceeds_node_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("m.py"),
        b"def a():\n    return 1\n\ndef b():\n    return a()\n",
    )
    .unwrap();

    let out = extract(root, &[("SYNAPTIC_MAX_NODES", "1")]);
    assert!(
        out.status.success(),
        "extract still succeeds over the cap: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("SYNAPTIC_MAX_NODES"),
        "warning names the override: {err}"
    );
    assert!(
        root.join("synaptic-out/graph.json").is_file(),
        "graph.json still written"
    );
}

#[test]
fn extract_is_quiet_under_the_caps() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("m.py"), b"def a():\n    return 1\n").unwrap();

    let out = extract(root, &[]);
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("SYNAPTIC_MAX"),
        "no cap warning under the defaults: {err}"
    );
}
