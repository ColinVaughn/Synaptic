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
fn affected_cli_bounds_output_with_limit_and_verbose() {
    // A function called by many others (a hub). `affected --limit` must truncate
    // with a per-depth breakdown + "+N more"; `--verbose` must list all.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut src = String::from("def core():\n    return 1\n");
    for i in 0..6 {
        src.push_str(&format!("def f{i}():\n    return core()\n"));
    }
    std::fs::write(root.join("m.py"), src.as_bytes()).unwrap();
    let ex = codegraph(&["extract", "."], root);
    assert!(ex.status.success());

    let capped = codegraph(&["affected", "core", "--limit", "2"], root);
    let out = String::from_utf8_lossy(&capped.stdout);
    assert!(
        out.contains("Total: 6") && out.contains("depth 1:"),
        "breakdown: {out}"
    );
    assert!(
        out.contains("more; pass --verbose"),
        "truncation note: {out}"
    );
    let entries = out.lines().filter(|l| l.starts_with("- ")).count();
    assert_eq!(entries, 2, "limit caps listed entries: {out}");

    let full = codegraph(&["affected", "core", "--verbose"], root);
    let fout = String::from_utf8_lossy(&full.stdout);
    assert!(
        !fout.contains("more; pass --verbose"),
        "verbose not truncated: {fout}"
    );
    assert_eq!(
        fout.lines().filter(|l| l.starts_with("- ")).count(),
        6,
        "verbose lists all 6: {fout}"
    );
}

#[test]
fn explain_reports_ambiguity_with_candidates() {
    // Two `helper` functions in different files make the bare name ambiguous. The
    // CLI must report candidates (shared resolver), not "Node not found".
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("a.py"), b"def helper():\n    return 1\n").unwrap();
    std::fs::write(root.join("b.py"), b"def helper():\n    return 2\n").unwrap();
    let ex = codegraph(&["extract", "."], root);
    assert!(ex.status.success());

    let e = codegraph(&["explain", "helper"], root);
    let out = String::from_utf8_lossy(&e.stdout);
    assert!(
        out.contains("is ambiguous") && out.contains("candidates"),
        "expected an ambiguity message with candidates, got: {out}"
    );
    assert!(
        !out.contains("Node not found"),
        "old misleading message must be gone: {out}"
    );
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
