use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .output()
        .expect("git")
        .status
        .success();
    assert!(ok, "git {:?}", args);
}

#[test]
fn diff_command_emits_json() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("a.py"), b"def f():\n    return 1\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "c1", "--no-gpg-sign"]);
    std::fs::write(
        root.join("a.py"),
        b"def f():\n    return 2\n\ndef g():\n    return f()\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "c2", "--no-gpg-sign"]);

    let out = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["diff", "HEAD~1", "HEAD", "--root"])
        .arg(root)
        .arg("--json")
        .output()
        .expect("run synaptic diff");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"hotspots\""), "stdout: {stdout}");

    // --html writes a self-contained report.
    let html_path = root.join("report.html");
    let h = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["diff", "HEAD~1", "HEAD", "--root"])
        .arg(root)
        .arg("--html")
        .arg(&html_path)
        .output()
        .expect("run synaptic diff --html");
    assert!(
        h.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&h.stderr)
    );
    let html = std::fs::read_to_string(&html_path).expect("html written");
    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "html: {}",
        &html[..html.len().min(60)]
    );
    assert!(html.contains("Architectural drift"));
}
