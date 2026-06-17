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

#[test]
fn eval_replay_scores_a_commit_against_ground_truth() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    if !git(root, &["init", "-q"]).status.success() {
        return; // git unavailable
    }
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("src/calc.py"),
        b"def add(a, b):\n    return a + b\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tests/test_calc.py"),
        b"from calc import add\n\n\ndef test_add():\n    assert add(1, 2) == 3\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    assert!(git(root, &["commit", "-q", "-m", "init", "--no-gpg-sign"])
        .status
        .success());
    let c1 = String::from_utf8(git(root, &["rev-parse", "HEAD"]).stdout).unwrap();
    let c1 = c1.trim();

    // A second commit that changes the source AND its test together (the ground
    // truth the forecast is scored against).
    std::fs::write(
        root.join("src/calc.py"),
        b"def add(a, b):\n    return b + a\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tests/test_calc.py"),
        b"from calc import add\n\n\ndef test_add():\n    assert add(2, 1) == 3\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    assert!(
        git(root, &["commit", "-q", "-m", "tweak add", "--no-gpg-sign"])
            .status
            .success()
    );

    // Replay c1..HEAD (the one tweak commit). A floor of 0 always passes, so this
    // exercises the gate path too.
    let p = codegraph(
        &["eval", "replay", c1, "--min-test-recall", "0", "--json"],
        root,
    );
    assert!(
        p.status.success(),
        "eval replay: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    let out = String::from_utf8_lossy(&p.stdout);
    assert!(out.contains("\"commits\""), "report shape: {out}");
    assert!(out.contains("\"selectivity_pct\""), "report shape: {out}");
    // The tweak commit edited exactly one test file -> one relevant test scored.
    assert!(out.contains("\"relevant\""), "scores present: {out}");
    assert!(
        out.contains("test_calc.py"),
        "the edited test is in the commit's changed files: {out}"
    );
}

#[test]
fn eval_replay_handles_an_empty_range() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    if !git(root, &["init", "-q"]).status.success() {
        return;
    }
    std::fs::write(root.join("a.py"), b"def f():\n    return 1\n").unwrap();
    git(root, &["add", "-A"]);
    assert!(git(root, &["commit", "-q", "-m", "init", "--no-gpg-sign"])
        .status
        .success());
    // HEAD..HEAD is empty: a clean, zero-commit report, not an error.
    let p = codegraph(&["eval", "replay", "HEAD", "--json"], root);
    assert!(
        p.status.success(),
        "empty range is not a failure: {}",
        String::from_utf8_lossy(&p.stderr)
    );
    assert!(String::from_utf8_lossy(&p.stdout).contains("\"commits\": []"));
}
