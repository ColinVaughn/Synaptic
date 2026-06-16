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

#[test]
fn search_query_and_patterns() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("m.py"),
        b"class Service:\n    def run(self):\n        return 1\n\ndef helper():\n    return 2\n",
    )
    .unwrap();

    // Build the graph.
    let ex = codegraph(&["extract", "."], root);
    assert!(
        ex.status.success(),
        "extract: {}",
        String::from_utf8_lossy(&ex.stderr)
    );

    // --list-patterns lists god-class.
    let lp = codegraph(&["search", "--list-patterns"], root);
    assert!(lp.status.success());
    assert!(String::from_utf8_lossy(&lp.stdout).contains("god-class"));

    // A CGQL query returns the class as JSON.
    let q = codegraph(&["search", "MATCH (c:class) RETURN c", "--json"], root);
    assert!(
        q.status.success(),
        "search: {}",
        String::from_utf8_lossy(&q.stderr)
    );
    let out = String::from_utf8_lossy(&q.stdout);
    assert!(out.contains("\"label\": \"Service\""), "stdout: {out}");
    assert!(out.contains("\"kind\": \"class\""), "stdout: {out}");

    // A named pattern runs without error.
    let p = codegraph(&["search", "--pattern", "god-class", "--json"], root);
    assert!(
        p.status.success(),
        "pattern: {}",
        String::from_utf8_lossy(&p.stderr)
    );

    // A parse error exits non-zero with a message.
    let bad = codegraph(&["search", "MATCH (c) WERE"], root);
    assert!(!bad.status.success());
}

#[test]
fn search_explain_saved_and_aggregation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("m.py"),
        b"class A:\n    pass\n\nclass B:\n    pass\n",
    )
    .unwrap();
    let ex = codegraph(&["extract", "."], root);
    assert!(ex.status.success());

    // --explain prints a plan without running.
    let e = codegraph(&["search", "MATCH (c:class) RETURN c", "--explain"], root);
    assert!(e.status.success());
    assert!(String::from_utf8_lossy(&e.stdout).contains("PLAN"));

    // Aggregation returns grouped scalar output.
    let agg = codegraph(
        &["search", "MATCH (c:class) RETURN count(c)", "--json"],
        root,
    );
    assert!(
        agg.status.success(),
        "agg: {}",
        String::from_utf8_lossy(&agg.stderr)
    );
    assert!(String::from_utf8_lossy(&agg.stdout).contains("\"groups\""));

    // Save, then run by name, then list.
    let s = codegraph(
        &[
            "search",
            "MATCH (c:class) RETURN c",
            "--save",
            "all_classes",
        ],
        root,
    );
    assert!(
        s.status.success(),
        "save: {}",
        String::from_utf8_lossy(&s.stderr)
    );
    let r = codegraph(&["search", "--saved", "all_classes", "--json"], root);
    assert!(
        r.status.success(),
        "saved: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let ls = codegraph(&["search", "--list-saved"], root);
    assert!(String::from_utf8_lossy(&ls.stdout).contains("all_classes"));

    // A path-traversal saved name is rejected.
    let bad = codegraph(&["search", "MATCH (c) RETURN c", "--save", "../evil"], root);
    assert!(!bad.status.success());
}
