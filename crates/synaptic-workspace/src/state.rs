//! Workspace-scale incremental state.
//!
//! Per member we persist a **source hash** (content fingerprint of the member's
//! files) and a **surface hash** (fingerprint of its export surface) to
//! `synaptic-out/workspace-state.json`. Source hashes let [`update_workspace`]
//! skip the whole federation when nothing changed; surface hashes let it report
//! which members' *public* surface changed — i.e. which dependents' cross-repo
//! edges could actually be affected (a member can change internally without
//! affecting anyone else).
//!
//! Change detection covers **local** members. When a workspace also declares
//! remote `[[repos]]`, a rebuild is forced (remote state can't be cheaply checked
//! offline) — reported honestly rather than silently skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use synaptic_detect::{detect, Manifest, ALL_FILE_TYPES};

use crate::export_surface::ExportSurface;
use crate::workspace_build::{
    build_workspace, resolve_members, WorkspaceBuild, WorkspaceBuildOptions,
};
use crate::{Result, WorkspaceError};

/// One member's persisted fingerprints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberState {
    pub source_hash: String,
    pub surface_hash: String,
}

/// The persisted workspace incremental state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub members: BTreeMap<String, MemberState>,
}

/// Where the state file lives under a workspace root.
pub fn state_path(root: &Path) -> PathBuf {
    root.join("synaptic-out").join("workspace-state.json")
}

/// Load the workspace state (empty default when absent or corrupt).
pub fn load_state(root: &Path) -> WorkspaceState {
    match std::fs::read(state_path(root)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => WorkspaceState::default(),
    }
}

/// Save the workspace state.
pub fn save_state(root: &Path, state: &WorkspaceState) -> Result<()> {
    let p = state_path(root);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| WorkspaceError::Io {
            context: format!("creating {}", parent.display()),
            source: e,
        })?;
    }
    let bytes = serde_json::to_vec_pretty(state).map_err(|source| WorkspaceError::Json {
        path: p.display().to_string(),
        source,
    })?;
    std::fs::write(&p, bytes).map_err(|source| WorkspaceError::Io {
        context: format!("writing {}", p.display()),
        source,
    })
}

/// Content fingerprint of a member's source tree: blake3 over its detected files'
/// `(relative path, content hash)` pairs (mtime-independent, so it is stable
/// across checkouts).
pub fn member_source_hash(member_root: &Path) -> String {
    let det = detect(member_root);
    let mut all: Vec<&Path> = Vec::new();
    for ft in ALL_FILE_TYPES {
        all.extend(det.of(ft).iter().map(PathBuf::as_path));
    }
    let manifest = Manifest::build(all, member_root);
    let mut hasher = blake3::Hasher::new();
    for (key, entry) in &manifest.0 {
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        hasher.update(entry.hash.as_bytes());
        hasher.update(b";");
    }
    hasher.finalize().to_hex().to_string()
}

/// Fingerprint of an export surface (its public API).
pub fn surface_hash(surface: &ExportSurface) -> String {
    let bytes = serde_json::to_vec(surface).unwrap_or_default();
    blake3::hash(&bytes).to_hex().to_string()
}

/// Per-member change status, computed *without* building.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberStatus {
    pub tag: String,
    /// `true` if the member is new or its source changed vs the saved state.
    pub changed: bool,
}

/// Compute each local member's change status against the saved state. Returns the
/// statuses plus whether a rebuild is forced by the presence of remote repos.
pub fn workspace_status(root: &Path) -> Result<(Vec<MemberStatus>, bool)> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let prev = load_state(&root);
    let (members, repos) = resolve_members(&root)?;
    let statuses = members
        .iter()
        .map(|m| {
            let cur = member_source_hash(&m.path);
            let changed = prev
                .members
                .get(&m.tag)
                .map(|s| s.source_hash != cur)
                .unwrap_or(true);
            MemberStatus {
                tag: m.tag.clone(),
                changed,
            }
        })
        .collect();
    Ok((statuses, !repos.is_empty()))
}

/// Result of an incremental [`update_workspace`].
pub struct UpdateOutcome {
    /// `false` when nothing changed and the existing federated graph was kept.
    pub rebuilt: bool,
    /// Local members whose source changed (or are new).
    pub changed_members: Vec<String>,
    /// Members whose *export surface* changed — the ones whose dependents' cross
    /// repo edges may need to change. Empty when `rebuilt` is false.
    pub surface_changed: Vec<String>,
    /// The federated build (present only when `rebuilt`).
    pub build: Option<WorkspaceBuild>,
    /// The fresh state to persist — present only when `rebuilt`. The caller
    /// **must** `save_state` this *after* the federated artifacts are durably
    /// written, so a mid-write failure can't leave "up to date" state pointing at
    /// outputs that never landed.
    pub new_state: Option<WorkspaceState>,
}

/// Incremental workspace update: skip the whole federation when no local member
/// changed (and no remote repos force a rebuild), else do a full federated build
/// and refresh the state. Whether a member's *surface* changed is reported so a
/// caller can reason about cross-repo impact.
pub fn update_workspace(root: &Path, opts: &WorkspaceBuildOptions) -> Result<UpdateOutcome> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let (statuses, has_remote) = workspace_status(&root)?;
    let prev = load_state(&root);

    let changed_members: Vec<String> = statuses
        .iter()
        .filter(|s| s.changed)
        .map(|s| s.tag.clone())
        .collect();
    let graph_exists = root.join("synaptic-out").join("graph.json").is_file();

    if changed_members.is_empty() && !has_remote && graph_exists {
        return Ok(UpdateOutcome {
            rebuilt: false,
            changed_members,
            surface_changed: Vec::new(),
            build: None,
            new_state: None,
        });
    }

    // Full federated rebuild + the fresh state to persist (the CALLER saves it
    // after writing artifacts; see UpdateOutcome::new_state).
    let build = build_workspace(&root, opts)?;
    let new_state = compute_state(&root, &build)?;
    let surface_changed = new_state
        .members
        .iter()
        .filter(|(tag, st)| {
            prev.members.get(*tag).map(|p| &p.surface_hash) != Some(&st.surface_hash)
        })
        .map(|(tag, _)| tag.clone())
        .collect();

    Ok(UpdateOutcome {
        rebuilt: true,
        changed_members,
        surface_changed,
        build: Some(build),
        new_state: Some(new_state),
    })
}

/// Compute the workspace state (source + surface hashes) for a finished build.
fn compute_state(root: &Path, build: &WorkspaceBuild) -> Result<WorkspaceState> {
    let surface_by_repo: BTreeMap<&str, &ExportSurface> = build
        .surfaces
        .iter()
        .map(|s| (s.repo.as_str(), s))
        .collect();
    let (members, _) = resolve_members(root)?;
    let mut state = WorkspaceState::default();
    for m in &members {
        let surf = surface_by_repo
            .get(m.tag.as_str())
            .map(|s| surface_hash(s))
            .unwrap_or_default();
        state.members.insert(
            m.tag.clone(),
            MemberState {
                source_hash: member_source_hash(&m.path),
                surface_hash: surf,
            },
        );
    }
    Ok(state)
}

/// Record the workspace state for a full (non-incremental) build, so a later
/// `--changed` run can short-circuit. Used by `synaptic workspace build`.
pub fn record_state(root: &Path, build: &WorkspaceBuild) -> Result<()> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let state = compute_state(&root, build)?;
    save_state(&root, &state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn make_monorepo(root: &Path) {
        touch(
            root,
            "synaptic-workspace.toml",
            "[workspace]\nname = \"demo\"\nmembers = [\"pkgs/*\"]\n",
        );
        touch(root, "pkgs/lib/Cargo.toml", "[package]\nname = \"lib\"\n");
        touch(
            root,
            "pkgs/lib/src/lib.rs",
            "pub struct Ledger;\nimpl Ledger { pub fn new() -> Ledger { Ledger } }\n",
        );
        touch(root, "pkgs/app/Cargo.toml", "[package]\nname = \"app\"\n");
        touch(
            root,
            "pkgs/app/src/lib.rs",
            "use lib::Ledger;\npub fn run() { let _ = Ledger::new(); }\n",
        );
    }

    #[test]
    fn first_update_builds_then_second_is_a_noop() {
        let d = tempfile::tempdir().unwrap();
        make_monorepo(d.path());
        // Write the federated graph.json so the no-op short-circuit can fire.
        let first = update_workspace(d.path(), &WorkspaceBuildOptions::default()).unwrap();
        assert!(first.rebuilt);
        crate::write_graph(
            &d.path().join("synaptic-out").join("graph.json"),
            &first.build.unwrap().federated.to_graph_data(),
        )
        .unwrap();
        // The caller persists state AFTER writing artifacts.
        save_state(d.path(), first.new_state.as_ref().unwrap()).unwrap();

        let second = update_workspace(d.path(), &WorkspaceBuildOptions::default()).unwrap();
        assert!(!second.rebuilt, "unchanged workspace → no rebuild");
        assert!(second.changed_members.is_empty());
    }

    #[test]
    fn changing_a_member_triggers_rebuild() {
        let d = tempfile::tempdir().unwrap();
        make_monorepo(d.path());
        let first = update_workspace(d.path(), &WorkspaceBuildOptions::default()).unwrap();
        let new_state = first.new_state.clone().unwrap();
        crate::write_graph(
            &d.path().join("synaptic-out").join("graph.json"),
            &first.build.unwrap().federated.to_graph_data(),
        )
        .unwrap();
        save_state(d.path(), &new_state).unwrap();

        // Add a new function to `app` so its source changes.
        touch(
            d.path(),
            "pkgs/app/src/lib.rs",
            "use lib::Ledger;\npub fn run() { let _ = Ledger::new(); }\npub fn extra() {}\n",
        );
        let upd = update_workspace(d.path(), &WorkspaceBuildOptions::default()).unwrap();
        assert!(upd.rebuilt);
        assert_eq!(upd.changed_members, vec!["app"]);
    }

    #[test]
    fn status_reports_changed_without_building() {
        let d = tempfile::tempdir().unwrap();
        make_monorepo(d.path());
        // No state yet, so both members are "changed" (new).
        let (statuses, has_remote) = workspace_status(d.path()).unwrap();
        assert!(!has_remote);
        assert_eq!(statuses.len(), 2);
        assert!(statuses.iter().all(|s| s.changed));
    }
}
