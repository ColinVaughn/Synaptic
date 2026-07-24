//! Watch targeting across a realistic multi-repo workspace: the platform-wide
//! layout where members are *sibling checkouts* outside the workspace tree, not
//! packages inside it. A workspace root's recursive watcher never sees those, so
//! resolution must hand back one root per out-of-tree member — and must keep
//! attributing changed paths to the right member.

use std::path::Path;

use synaptic_workspace::watch::{classify, member_for_path, resolve_watch_targets, WatchEvent};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A specops-style layout: a thin workspace root that federates one in-tree
/// package plus two sibling repositories checked out next to it.
fn make_platform_workspace(base: &Path) -> std::path::PathBuf {
    let ws = base.join("platform");
    write(
        &ws,
        "synaptic-workspace.toml",
        r#"[workspace]
name = "platform"
members = ["pkgs/*"]

[[repos]]
name = "identity"
path = "../identity"

[[repos]]
name = "billing"
path = "../billing"
"#,
    );
    write(
        &ws,
        "pkgs/shared/Cargo.toml",
        "[package]\nname=\"shared\"\n",
    );
    write(&ws, "pkgs/shared/src/lib.rs", "pub struct Token;\n");

    write(
        &base.join("identity"),
        "Cargo.toml",
        "[package]\nname=\"identity\"\n",
    );
    write(
        &base.join("identity"),
        "src/lib.rs",
        "pub fn verify() -> bool { true }\n",
    );
    write(
        &base.join("billing"),
        "Cargo.toml",
        "[package]\nname=\"billing\"\n",
    );
    write(&base.join("billing"), "src/lib.rs", "pub fn charge() {}\n");
    ws
}

#[test]
fn sibling_checkouts_each_get_a_watch_root() {
    let d = tempfile::tempdir().unwrap();
    let ws = make_platform_workspace(d.path());
    let targets = resolve_watch_targets(&ws).unwrap();

    // Three members: the in-tree package plus both sibling repos.
    let tags: std::collections::BTreeSet<&str> =
        targets.members.iter().map(|(t, _)| t.as_str()).collect();
    assert!(
        tags.contains("shared") && tags.contains("identity") && tags.contains("billing"),
        "{tags:?}"
    );

    // The in-tree package folds into the workspace root; each sibling adds one.
    assert_eq!(
        targets.roots.len(),
        3,
        "workspace root + 2 sibling checkouts: {:?}",
        targets.roots
    );
    let canon = |p: &Path| p.canonicalize().unwrap();
    for expected in [
        canon(&ws),
        canon(&d.path().join("identity")),
        canon(&d.path().join("billing")),
    ] {
        assert!(
            targets.roots.contains(&expected),
            "{} watched: {:?}",
            expected.display(),
            targets.roots
        );
    }
}

#[test]
fn changes_attribute_to_the_owning_repository() {
    let d = tempfile::tempdir().unwrap();
    let ws = make_platform_workspace(d.path());
    let targets = resolve_watch_targets(&ws).unwrap();
    let canon = |p: &Path| p.canonicalize().unwrap();

    let cases = [
        (canon(&ws).join("pkgs/shared/src/lib.rs"), "shared"),
        (
            canon(&d.path().join("identity")).join("src/lib.rs"),
            "identity",
        ),
        (
            canon(&d.path().join("billing")).join("src/lib.rs"),
            "billing",
        ),
    ];
    for (path, want) in cases {
        assert_eq!(
            member_for_path(&path, &targets.members),
            Some(want),
            "{} attributes to {want}",
            path.display()
        );
    }
}

#[test]
fn a_siblings_own_output_dir_never_self_triggers() {
    // Each member keeps its own synaptic-out/cache; if those writes counted as
    // changes the watcher would rebuild forever.
    let d = tempfile::tempdir().unwrap();
    let ws = make_platform_workspace(d.path());
    let targets = resolve_watch_targets(&ws).unwrap();
    let identity = d.path().join("identity").canonicalize().unwrap();

    assert_eq!(
        classify(&identity.join("synaptic-out/cache/x.bin"), &identity),
        None
    );
    assert_eq!(classify(&ws.join("synaptic-out/graph.json"), &ws), None);
    assert_eq!(
        classify(&identity.join("src/lib.rs"), &identity),
        Some(WatchEvent::Source(Path::new("src/lib.rs").to_path_buf())),
    );
    // Every resolved root is one the classifier can strip a path against.
    assert!(targets.roots.iter().all(|r| r.is_absolute()));
}

#[test]
fn manifest_edits_are_seen_so_members_can_be_added_at_runtime() {
    let d = tempfile::tempdir().unwrap();
    let ws = make_platform_workspace(d.path());
    assert_eq!(
        classify(&ws.join("synaptic-workspace.toml"), &ws),
        Some(WatchEvent::Manifest),
    );

    // Dropping a repo from the manifest drops its watch root on re-resolution.
    let before = resolve_watch_targets(&ws).unwrap();
    write(
        &ws,
        "synaptic-workspace.toml",
        "[workspace]\nname = \"platform\"\nmembers = [\"pkgs/*\"]\n",
    );
    let after = resolve_watch_targets(&ws).unwrap();
    assert_eq!(before.roots.len(), 3);
    assert_eq!(
        after.roots.len(),
        1,
        "removing both [[repos]] leaves only the workspace root: {:?}",
        after.roots
    );
}
