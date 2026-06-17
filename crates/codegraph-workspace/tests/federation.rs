//! Integration tests for the federation member sources that were "logic built,
//! untested offline": git-clone members (exercised against a LOCAL git repo —
//! fully offline), `path` members (already-checked-out), local `subgraph`
//! artifact members, and the remote-`subgraph` SSRF guard (blocked-IP error
//! path). Only a real *network* remote remains unexercised here, by nature.

use std::path::Path;

use codegraph_workspace::manifest::{write_manifest, RepoMember, WorkspaceManifest, WorkspaceMeta};
use codegraph_workspace::workspace_build::{build_workspace, MemberSource, WorkspaceBuildOptions};

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// Forward-slash a path so it is a valid TOML value + accepted by git on Windows.
fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("git must be available to run the federation tests");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// Create a committed git repo containing a tiny Rust crate named `name`.
fn make_git_repo(dir: &Path, name: &str) {
    write(
        dir,
        "Cargo.toml",
        &format!("[package]\nname = \"{name}\"\n"),
    );
    write(
        dir,
        "src/lib.rs",
        "pub struct Ledger;\nimpl Ledger { pub fn new() -> Ledger { Ledger } }\n",
    );
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["add", "-A"]);
    git(
        dir,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"],
    );
}

fn manifest_with(repos: Vec<RepoMember>) -> WorkspaceManifest {
    WorkspaceManifest {
        workspace: WorkspaceMeta {
            name: "demo".into(),
            default_branch: "main".into(),
            members: vec![],
        },
        repos,
    }
}

#[test]
fn git_member_is_cloned_built_and_federated() {
    let dir = tempfile::tempdir().unwrap();
    // A source repo to clone (local, fully offline).
    let src = dir.path().join("src-lib");
    make_git_repo(&src, "lib");

    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_manifest(
        &ws,
        &manifest_with(vec![RepoMember {
            name: "lib".into(),
            git: Some(fwd(&src)),
            rev: None,
            subgraph: None,
            path: None,
        }]),
    )
    .unwrap();

    let build = build_workspace(&ws, &WorkspaceBuildOptions::default()).unwrap();
    assert_eq!(build.members.len(), 1);
    assert_eq!(build.members[0].tag, "lib");
    assert_eq!(build.members[0].source, MemberSource::Remote);
    assert!(
        build
            .federated
            .nodes()
            .any(|n| n.repo.as_deref() == Some("lib")),
        "federated graph carries the cloned member's repo tag"
    );
    // The clone landed in the workspace cache and is reused on a second build.
    assert!(ws.join("codegraph-out/workspace-repos/lib").is_dir());
    let again = build_workspace(&ws, &WorkspaceBuildOptions::default()).unwrap();
    assert_eq!(again.members.len(), 1, "re-build reuses the existing clone");
}

#[test]
fn path_member_is_built_and_federated() {
    let dir = tempfile::tempdir().unwrap();
    // An already-checked-out repo, OUTSIDE the workspace root (multi-repo layout).
    let ext = dir.path().join("ext");
    write(&ext, "Cargo.toml", "[package]\nname = \"ext\"\n");
    write(&ext, "src/lib.rs", "pub fn helper() {}\n");

    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_manifest(
        &ws,
        &manifest_with(vec![RepoMember {
            name: "ext".into(),
            git: None,
            rev: None,
            subgraph: None,
            path: Some(fwd(&ext)),
        }]),
    )
    .unwrap();

    let build = build_workspace(&ws, &WorkspaceBuildOptions::default()).unwrap();
    assert_eq!(build.members[0].source, MemberSource::Local);
    assert!(build
        .federated
        .nodes()
        .any(|n| n.repo.as_deref() == Some("ext")));
}

#[test]
fn local_subgraph_member_is_federated() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // A prebuilt subgraph artifact at a local path under the workspace.
    write(
        &ws,
        "published/graph.json",
        r#"{"directed":false,"multigraph":false,"graph":{},
            "nodes":[{"id":"Widget","label":"Widget","file_type":"code","source_file":"w.rs"}],
            "links":[],"hyperedges":[]}"#,
    );
    write_manifest(
        &ws,
        &manifest_with(vec![RepoMember {
            name: "ui".into(),
            git: None,
            rev: None,
            subgraph: Some("published/graph.json".into()),
            path: None,
        }]),
    )
    .unwrap();

    let build = build_workspace(&ws, &WorkspaceBuildOptions::default()).unwrap();
    assert_eq!(build.members[0].source, MemberSource::Remote);
    assert!(
        build
            .federated
            .nodes()
            .any(|n| n.id.0 == "ui::Widget" && n.repo.as_deref() == Some("ui")),
        "the published subgraph's nodes are namespaced + tagged"
    );
}

#[test]
fn cross_repo_parameterized_route_connects() {
    // Two repos: a server with a parameterized Flask route + handler, and a client
    // calling the concrete path. After federation, the concrete client route is
    // matched to the server's template, and the cross-repo calls_service edge is
    // flagged. Exercises the finalize() cross-repo route pass.
    let dir = tempfile::tempdir().unwrap();
    let server = dir.path().join("server");
    write(
        &server,
        "app.py",
        "from flask import Flask\napp = Flask(__name__)\n\n@app.get(\"/users/<int:uid>\")\ndef get_user(uid):\n    return {}\n",
    );
    let client = dir.path().join("client");
    write(
        &client,
        "main.py",
        "import requests\n\ndef load():\n    return requests.get(\"http://svc/users/42\").json()\n",
    );

    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_manifest(
        &ws,
        &manifest_with(vec![
            RepoMember {
                name: "server".into(),
                git: None,
                rev: None,
                subgraph: None,
                path: Some(fwd(&server)),
            },
            RepoMember {
                name: "client".into(),
                git: None,
                rev: None,
                subgraph: None,
                path: Some(fwd(&client)),
            },
        ]),
    )
    .unwrap();

    let build = build_workspace(&ws, &WorkspaceBuildOptions::default()).unwrap();
    let nodes: Vec<_> = build.federated.nodes().collect();
    assert!(
        nodes.iter().any(|n| n.label == "/users/<int:uid>"),
        "server's template route survives federation"
    );
    assert!(
        !nodes.iter().any(|n| n.label == "/users/42"),
        "concrete client route merged into the server's template"
    );
    assert!(
        build
            .federated
            .edges()
            .any(|e| e.relation == "calls_service" && e.cross_repo),
        "the client -> route edge spans repos and is flagged cross_repo"
    );
}

#[test]
fn remote_subgraph_blocked_ip_is_rejected_by_the_ssrf_guard() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // The cloud-metadata / link-local IP is blocked at URL validation, no network
    // needed, so the remote-fetch guard is exercised fully offline.
    write_manifest(
        &ws,
        &manifest_with(vec![RepoMember {
            name: "ext".into(),
            git: None,
            rev: None,
            subgraph: Some("http://169.254.169.254/graph.json".into()),
            path: None,
        }]),
    )
    .unwrap();

    let err = build_workspace(&ws, &WorkspaceBuildOptions::default())
        .err()
        .expect("a blocked-IP subgraph must error, not silently succeed");
    let msg = format!("{err}");
    assert!(msg.contains("ext"), "error names the member: {msg}");
}

/// Real-network remote git clone — opt-in. Run with:
///   CODEGRAPH_NET_TESTS=1 cargo test -p codegraph-workspace --test federation -- --ignored
#[test]
#[ignore = "network: set CODEGRAPH_NET_TESTS=1 and run with --ignored"]
fn remote_git_member_is_cloned_over_network() {
    if std::env::var("CODEGRAPH_NET_TESTS").is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_manifest(
        &ws,
        &manifest_with(vec![RepoMember {
            name: "octocat-hello".into(),
            // tiny, stable public repo
            git: Some("https://github.com/octocat/Hello-World".into()),
            rev: None,
            subgraph: None,
            path: None,
        }]),
    )
    .unwrap();
    let build = build_workspace(&ws, &WorkspaceBuildOptions::default()).unwrap();
    assert_eq!(build.members.len(), 1);
    assert_eq!(build.members[0].source, MemberSource::Remote);
}
