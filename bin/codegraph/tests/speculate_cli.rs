use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

fn codegraph(args: &[&str], dir: &Path) -> std::process::Output {
    Command::cargo_bin("codegraph")
        .unwrap()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run codegraph")
}

fn git(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .output()
        .expect("run git")
}

/// A graph where the test `test_helper` (in tests/, so it is detected as a test)
/// calls public `helper` (in src/helper.py). Editing helper.py therefore puts
/// tests/test_helper.py in the at-risk test set.
fn write_graph(root: &Path) {
    std::fs::create_dir_all(root.join("codegraph-out")).unwrap();
    let graph = r#"{
        "directed": true,
        "multigraph": false,
        "graph": {},
        "nodes": [
            {"id": "helper", "label": "helper()", "file_type": "code",
             "source_file": "src/helper.py", "kind": "function", "visibility": "public"},
            {"id": "t", "label": "test_helper()", "file_type": "code",
             "source_file": "tests/test_helper.py", "kind": "function"}
        ],
        "links": [
            {"source": "t", "target": "helper", "relation": "calls",
             "confidence": "EXTRACTED", "source_file": "tests/test_helper.py", "weight": 1.0}
        ],
        "hyperedges": []
    }"#;
    std::fs::write(root.join("codegraph-out/graph.json"), graph).unwrap();
}

/// A committed repo with src/helper.py + tests/test_helper.py and the graph.
fn repo(root: &Path) -> bool {
    if !git(root, &["init", "-q"]).status.success() {
        return false; // git unavailable
    }
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("src/helper.py"), b"def helper():\n    return 1\n").unwrap();
    std::fs::write(root.join("tests/test_helper.py"), b"# exercises helper\n").unwrap();
    write_graph(root);
    git(root, &["add", "src", "tests"]);
    assert!(git(root, &["commit", "-q", "-m", "init", "--no-gpg-sign"])
        .status
        .success());
    true
}

#[test]
fn speculate_runs_at_risk_tests_and_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    if !repo(root) {
        return;
    }
    // An uncommitted edit to helper.py is the change to speculate.
    std::fs::write(root.join("src/helper.py"), b"def helper():\n    return 2\n").unwrap();

    // ls-files passes for the tracked at-risk test file -> outcome passed.
    let p = codegraph(
        &[
            "speculate",
            "src/helper.py",
            "--base",
            "HEAD",
            "--test-cmd",
            "git ls-files --error-unmatch {files}",
            "--no-detect",
            "--json",
        ],
        root,
    );
    assert!(
        p.status.success(),
        "speculate: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    let out = String::from_utf8_lossy(&p.stdout);
    assert!(out.contains("\"outcome\": \"passed\""), "passed: {out}");
    assert!(
        out.contains("tests/test_helper.py"),
        "ran the at-risk test: {out}"
    );
}

#[test]
fn speculate_fails_when_the_check_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    if !repo(root) {
        return;
    }
    std::fs::write(root.join("src/helper.py"), b"def helper():\n    return 2\n").unwrap();

    let p = codegraph(
        &[
            "speculate",
            "src/helper.py",
            "--base",
            "HEAD",
            "--check-cmd",
            "git not-a-real-subcommand",
            "--test-cmd",
            "git --version",
            "--no-detect",
        ],
        root,
    );
    assert!(!p.status.success(), "a failed check must exit non-zero");
    assert!(
        String::from_utf8_lossy(&p.stderr).contains("speculation failed"),
        "stderr: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    // The report is still written for the agent to read.
    assert!(root.join("codegraph-out/speculate/report.md").exists());
}

#[test]
fn speculate_reports_no_changes_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    if !repo(root) {
        return;
    }
    // No edit: the working tree is clean.
    let p = codegraph(
        &[
            "speculate",
            "src/helper.py",
            "--base",
            "HEAD",
            "--no-detect",
        ],
        root,
    );
    assert!(
        p.status.success(),
        "clean tree is not a failure: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    assert!(
        String::from_utf8_lossy(&p.stdout).contains("no changes"),
        "stdout: {}",
        String::from_utf8_lossy(&p.stdout)
    );
}

#[test]
fn speculate_errors_without_a_graph() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // No graph.json -> a clear failure, not a panic.
    let p = codegraph(&["speculate", "x.py", "--no-detect"], root);
    assert!(!p.status.success());
}
