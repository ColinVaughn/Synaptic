//! Integration coverage for manifest-presence fallback + module discovery.
use std::path::Path;

use synaptic_workspace::discover::{discover_members, discover_project_roots};

fn touch(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

#[test]
fn fallback_finds_polyglot_project_roots() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    // No workspace build-file at root; two independent projects in subdirs.
    touch(r, "frontend/package.json", "{\"name\":\"web\"}");
    touch(r, "frontend/src/app.ts", "export const x = 1;\n");
    touch(r, "backend/pyproject.toml", "[project]\nname=\"svc\"\n");
    touch(r, "backend/svc/__init__.py", "x = 1\n");
    let tags: Vec<String> = discover_project_roots(r)
        .into_iter()
        .map(|m| m.tag)
        .collect();
    assert!(
        tags.contains(&"frontend".to_string()) && tags.contains(&"backend".to_string()),
        "{tags:?}"
    );
}

#[test]
fn fallback_does_not_double_count_nested_dirs() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    // A project root with a manifest-less subdir; only the root counts.
    touch(r, "svc/go.mod", "module x\n");
    touch(r, "svc/internal/db/conn.go", "package db\n");
    let roots = discover_project_roots(r);
    assert_eq!(
        roots.len(),
        1,
        "{:?}",
        roots.iter().map(|m| &m.tag).collect::<Vec<_>>()
    );
    assert_eq!(roots[0].tag, "svc");
}

#[test]
fn fallback_empty_repo_finds_nothing() {
    let d = tempfile::tempdir().unwrap();
    touch(d.path(), "README.md", "# hi\n");
    assert!(discover_project_roots(d.path()).is_empty());
}

#[test]
fn discover_members_falls_back_when_no_workspace_file() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "frontend/package.json", "{\"name\":\"web\"}");
    touch(r, "backend/pyproject.toml", "[project]\nname=\"svc\"\n");
    let tags: Vec<String> = discover_members(r).into_iter().map(|m| m.tag).collect();
    assert!(
        tags.contains(&"frontend".to_string()) && tags.contains(&"backend".to_string()),
        "{tags:?}"
    );
}

#[test]
fn nested_workspace_members_are_expanded() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    // Root npm workspace; one member is itself a pnpm workspace.
    touch(r, "package.json", "{\"workspaces\":[\"packages/*\"]}");
    touch(
        r,
        "packages/inner/pnpm-workspace.yaml",
        "packages:\n  - 'sub/*'\n",
    );
    touch(
        r,
        "packages/inner/sub/leaf/package.json",
        "{\"name\":\"leaf\"}",
    );
    let tags: Vec<String> = discover_members(r).into_iter().map(|m| m.tag).collect();
    assert!(
        tags.contains(&"leaf".to_string()),
        "nested member expanded: {tags:?}"
    );
}

#[test]
fn nested_cargo_workspace_is_expanded() {
    // Cargo workspace whose member `pkgs/inner` is itself a Cargo workspace;
    // exercises the `[workspace]` marker in could_be_nested_workspace.
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "Cargo.toml", "[workspace]\nmembers = [\"pkgs/*\"]\n");
    touch(
        r,
        "pkgs/inner/Cargo.toml",
        "[workspace]\nmembers = [\"sub/*\"]\n",
    );
    touch(
        r,
        "pkgs/inner/sub/leaf/Cargo.toml",
        "[package]\nname = \"leaf\"\n",
    );
    let tags: Vec<String> = discover_members(r).into_iter().map(|m| m.tag).collect();
    assert!(
        tags.contains(&"leaf".to_string()),
        "nested cargo member expanded: {tags:?}"
    );
}

#[test]
fn fallback_includes_submodule_project_roots() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(
        r,
        ".gitmodules",
        "[submodule \"libs/auth\"]\n\tpath = libs/auth\n\turl = https://x\n",
    );
    touch(r, "libs/auth/Cargo.toml", "[package]\nname=\"auth\"\n");
    touch(r, "app/package.json", "{\"name\":\"app\"}");
    let tags: Vec<String> = discover_project_roots(r)
        .into_iter()
        .map(|m| m.tag)
        .collect();
    assert!(
        tags.contains(&"auth".to_string()) && tags.contains(&"app".to_string()),
        "{tags:?}"
    );
}

#[test]
fn discover_members_prefers_workspace_file_over_fallback() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    // A real Cargo workspace: fallback must NOT also fire.
    touch(r, "Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    touch(r, "crates/a/Cargo.toml", "[package]\nname=\"a\"\n");
    touch(r, "extra/package.json", "{\"name\":\"extra\"}"); // not a member of the cargo ws
    let tags: Vec<String> = discover_members(r).into_iter().map(|m| m.tag).collect();
    assert_eq!(
        tags,
        vec!["a"],
        "only declared members, no fallback: {tags:?}"
    );
}
