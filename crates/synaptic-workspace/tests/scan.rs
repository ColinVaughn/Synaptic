//! Integration coverage: scan result feeds federate_repos end-to-end.
use std::path::Path;

use synaptic_workspace::manifest::RepoMember;
use synaptic_workspace::scan::{discover_sibling_repos, ScanOptions};
use synaptic_workspace::workspace_build::{federate_repos, WorkspaceBuildOptions};

fn touch(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

#[test]
fn scan_then_federate_carries_both_repo_tags() {
    let d = tempfile::tempdir().unwrap();
    let parent = d.path();
    // The "current" repo (excluded) plus two sibling repos.
    touch(parent, "ws/Cargo.toml", "[package]\nname=\"ws\"\n");
    touch(parent, "billing/.git/HEAD", "x\n");
    touch(
        parent,
        "billing/Cargo.toml",
        "[package]\nname=\"billing\"\n",
    );
    touch(parent, "billing/src/lib.rs", "pub struct Ledger;\n");
    touch(parent, "identity/.git/HEAD", "x\n");
    touch(
        parent,
        "identity/Cargo.toml",
        "[package]\nname=\"identity\"\n",
    );
    touch(parent, "identity/src/lib.rs", "pub struct User;\n");

    let ws = parent.join("ws");
    let res = discover_sibling_repos(parent, &ScanOptions::default(), Some(&ws));
    let names: Vec<&str> = res.repos.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"billing") && names.contains(&"identity"),
        "{names:?}"
    );

    let repos: Vec<RepoMember> = res
        .repos
        .iter()
        .map(|c| RepoMember {
            name: c.name.clone(),
            git: None,
            rev: None,
            subgraph: None,
            path: Some(c.path.to_string_lossy().into_owned()),
        })
        .collect();
    let build = federate_repos(&ws, &repos, &WorkspaceBuildOptions::default()).unwrap();
    let tags: std::collections::BTreeSet<Option<&str>> =
        build.federated.nodes().map(|n| n.repo.as_deref()).collect();
    assert!(
        tags.contains(&Some("billing")) && tags.contains(&Some("identity")),
        "{tags:?}"
    );
}
