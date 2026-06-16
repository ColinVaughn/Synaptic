use std::path::Path;
use std::process::Command;

use codegraph_history::{diff, DiffOptions};

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
fn diff_reports_added_dependency_and_hotspot() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q"]);
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(root.join("lib/util.py"), b"def helper():\n    return 1\n").unwrap();
    std::fs::write(root.join("app.py"), b"def main():\n    return 0\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "c1", "--no-gpg-sign"]);

    // c2: app.py now imports + calls lib.util.helper -> new cross-module dependency.
    std::fs::write(
        root.join("app.py"),
        b"from lib.util import helper\n\ndef main():\n    return helper()\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "c2", "--no-gpg-sign"]);

    let report = diff(root, "HEAD~1", Some("HEAD"), &DiffOptions::default()).unwrap();
    assert_eq!(report.rev1.len(), 40);
    assert!(
        report.hotspots.iter().any(|h| h.file == "app.py"),
        "app.py is a hotspot, got {:?}",
        report.hotspots
    );
    // A new module dependency app(root) -> lib should appear.
    assert!(
        !report.added_dependencies.is_empty(),
        "expected a new module dependency, got {:?}",
        report.added_dependencies
    );
}
