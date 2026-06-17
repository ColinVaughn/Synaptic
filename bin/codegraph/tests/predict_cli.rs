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

/// Write a minimal graph.json fixture: `main` (in app.py) calls public `helper`
/// (in helper.py), so editing helper.py puts `main` in the blast radius. Using a
/// fixture instead of `codegraph extract` keeps the test hermetic and fast.
fn write_graph(root: &Path) {
    std::fs::create_dir_all(root.join("codegraph-out")).unwrap();
    let graph = r#"{
        "directed": true,
        "multigraph": false,
        "graph": {},
        "nodes": [
            {"id": "helper", "label": "helper()", "file_type": "code",
             "source_file": "helper.py", "kind": "function", "visibility": "public"},
            {"id": "main", "label": "main()", "file_type": "code",
             "source_file": "app.py", "kind": "function"}
        ],
        "links": [
            {"source": "main", "target": "helper", "relation": "calls",
             "confidence": "EXTRACTED", "source_file": "app.py", "weight": 1.0}
        ],
        "hyperedges": []
    }"#;
    std::fs::write(root.join("codegraph-out/graph.json"), graph).unwrap();
}

#[test]
fn predict_reports_changed_nodes_and_blast_radius() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_graph(root);

    // Forecast the impact of changing helper.py (explicit path; skip the
    // git/worktree time-travel diff since the fixture is not a git repo).
    let p = codegraph(&["predict", "helper.py", "--no-diff", "--json"], root);
    assert!(
        p.status.success(),
        "predict: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    let out = String::from_utf8_lossy(&p.stdout);
    assert!(out.contains("\"changed_nodes\""), "json shape: {out}");
    assert!(out.contains("\"blast_radius\""), "json shape: {out}");
    // helper is the changed node; main is its at-risk dependent; helper is public.
    assert!(out.contains("helper"), "helper is a changed node: {out}");
    assert!(out.contains("main"), "main is in the blast radius: {out}");
    assert!(out.contains("\"public_api_breaks\""), "json shape: {out}");

    // The default (non-JSON) run writes forecast.json + forecast.md.
    let p2 = codegraph(&["predict", "helper.py", "--no-diff"], root);
    assert!(
        p2.status.success(),
        "predict md: {}",
        String::from_utf8_lossy(&p2.stderr)
    );
    assert!(root.join("codegraph-out/predict/forecast.json").exists());
    assert!(root.join("codegraph-out/predict/forecast.md").exists());
    let md = std::fs::read_to_string(root.join("codegraph-out/predict/forecast.md")).unwrap();
    assert!(md.starts_with("# Change forecast"), "md: {md}");
    assert!(md.contains("## Changed nodes"), "md: {md}");
    assert!(md.contains("helper"), "changed node label in md: {md}");
    assert!(md.contains("## Blast radius"), "md: {md}");
    assert!(md.contains("## Verify checklist"), "md: {md}");
}

#[test]
fn predict_in_a_git_repo_refines_risk_from_churn() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("run git")
    };
    if !git(&["init", "-q"]).status.success() {
        return; // git unavailable; nothing to exercise
    }
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);

    std::fs::write(root.join("helper.py"), b"def helper():\n    return 1\n").unwrap();
    write_graph(root);
    git(&["add", "-A"]);
    assert!(git(&["commit", "-q", "-m", "init"]).status.success());

    // An uncommitted edit to helper.py adds churn vs HEAD.
    std::fs::write(
        root.join("helper.py"),
        b"def helper():\n    # changed\n    return 2\n",
    )
    .unwrap();

    // No paths -> changed files come from `git diff --name-only HEAD`; --no-diff
    // skips the worktree time-travel diff but still gathers churn/history.
    let p = codegraph(&["predict", "--no-diff", "--base", "HEAD", "--json"], root);
    assert!(
        p.status.success(),
        "predict: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    let out = String::from_utf8_lossy(&p.stdout);
    assert!(out.contains("\"risk\""), "risk present: {out}");
    assert!(out.contains("\"score\""), "risk scored: {out}");
    assert!(
        out.contains("helper"),
        "helper derived as a changed node: {out}"
    );
}

#[test]
fn predict_surfaces_co_change_from_history() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("run git")
    };
    if !git(&["init", "-q"]).status.success() {
        return; // git unavailable
    }
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    write_graph(root);

    // Three commits in which helper.py and util.py always change together.
    for i in 0..3 {
        std::fs::write(
            root.join("helper.py"),
            format!("def helper():\n    return {i}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("util.py"),
            format!("def util():\n    return {i}\n"),
        )
        .unwrap();
        git(&["add", "-A"]);
        assert!(git(&["commit", "-q", "-m", &format!("c{i}")])
            .status
            .success());
    }
    // An uncommitted edit to helper.py only.
    std::fs::write(root.join("helper.py"), b"def helper():\n    return 99\n").unwrap();

    let p = codegraph(&["predict", "--no-diff", "--base", "HEAD", "--json"], root);
    assert!(
        p.status.success(),
        "predict: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    let out = String::from_utf8_lossy(&p.stdout);
    assert!(out.contains("\"co_change_suggestions\""), "shape: {out}");
    assert!(
        out.contains("util.py"),
        "util.py co-changes with helper.py: {out}"
    );
}

#[test]
fn predict_gate_passes_on_a_clean_change() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("run git")
    };
    if !git(&["init", "-q"]).status.success() {
        return;
    }
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(root.join("helper.py"), b"def helper():\n    return 1\n").unwrap();
    write_graph(root);
    git(&["add", "-A"]);
    assert!(git(&["commit", "-q", "-m", "init"]).status.success());
    // A trivial body edit: no new cycles, no removed APIs -> gate passes (exit 0).
    std::fs::write(root.join("helper.py"), b"def helper():\n    return 2\n").unwrap();

    let p = codegraph(&["predict", "--base", "HEAD", "--gate"], root);
    assert!(
        p.status.success(),
        "gate should pass on a clean change: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    assert!(String::from_utf8_lossy(&p.stdout).contains("Gate passed"));
}

#[test]
fn predict_gate_fails_closed_when_diff_cannot_run() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_graph(root); // a graph, but NOT a git repo -> the diff cannot run
    let p = codegraph(&["predict", "helper.py", "--gate"], root);
    assert!(
        !p.status.success(),
        "gate must fail closed when it cannot verify the change"
    );
    assert!(String::from_utf8_lossy(&p.stderr).contains("could not run the time-travel diff"));
}

#[test]
fn predict_errors_without_a_graph() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // No graph.json present -> a clear failure, not a panic.
    let p = codegraph(&["predict", "x.py", "--no-diff"], root);
    assert!(!p.status.success());
}

#[test]
fn predict_edit_forecasts_a_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_graph(root); // no git repo needed: --edit is pure-graph analytic mode

    let p = codegraph(&["predict", "--edit", "delete:helper", "--json"], root);
    assert!(
        p.status.success(),
        "predict --edit: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    let out = String::from_utf8_lossy(&p.stdout);
    assert!(out.contains("\"removes_node\": true"), "deletion: {out}");
    // helper is public in the fixture, so deleting it removes a public API.
    assert!(
        out.contains("\"removed_public_api\": true"),
        "public: {out}"
    );
    // main() depends on helper, so it breaks.
    assert!(out.contains("main"), "the caller breaks: {out}");
}

#[test]
fn predict_edit_writes_markdown() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_graph(root);
    let p = codegraph(&["predict", "--edit", "signature:helper"], root);
    assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
    assert!(root.join("codegraph-out/predict/editforecast.md").exists());
    let md = std::fs::read_to_string(root.join("codegraph-out/predict/editforecast.md")).unwrap();
    assert!(md.starts_with("# Edit forecast"), "md: {md}");
}

#[test]
fn predict_edit_rejects_a_bad_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_graph(root);
    // Unknown edit kind -> a clear error, not a panic.
    let p = codegraph(&["predict", "--edit", "frobnicate:helper", "--json"], root);
    assert!(!p.status.success());
    assert!(String::from_utf8_lossy(&p.stderr).contains("unknown edit kind"));
}
