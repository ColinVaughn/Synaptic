//! The `synaptic-workspace.toml` model. Declares a workspace's
//! name, default branch, monorepo **members** (local package globs), and
//! multi-repo **repos** (separate repositories, federated). When the file is
//! absent the workspace is auto-discovered instead (see [`crate::discover`]).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Result, WorkspaceError};

/// The parsed `synaptic-workspace.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct WorkspaceManifest {
    pub workspace: WorkspaceMeta,
    /// Multi-repo members (separate repositories). `[[repos]]` in the TOML.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoMember>,
}

/// The `[workspace]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct WorkspaceMeta {
    pub name: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    /// Monorepo members: local package-root globs (like Cargo/pnpm workspaces).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

impl Default for WorkspaceMeta {
    fn default() -> Self {
        WorkspaceMeta {
            name: String::new(),
            default_branch: default_branch(),
            members: Vec::new(),
        }
    }
}

fn default_branch() -> String {
    "main".to_string()
}

/// A `[[repos]]` entry — a separate repository federated into the workspace.
///
/// Exactly one of `path` (already checked out), `git` (clone), or `subgraph`
/// (consume a prebuilt `graph.json` instead of cloning) drives how it is built.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RepoMember {
    pub name: String,
    /// Remote git URL to clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    /// Branch / revision for the git clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Artifact federation: a prebuilt subgraph (local path or URL) to consume
    /// instead of cloning + building.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subgraph: Option<String>,
    /// A local path to an already-checked-out repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// The conventional manifest filename at a workspace root.
pub const MANIFEST_NAME: &str = "synaptic-workspace.toml";

/// Load `root/synaptic-workspace.toml`. `Ok(None)` when the file is absent;
/// `Err` when it exists but fails to parse.
pub fn load_manifest(root: &Path) -> Result<Option<WorkspaceManifest>> {
    let path = root.join(MANIFEST_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(WorkspaceError::Io {
                context: format!("reading {}", path.display()),
                source,
            })
        }
    };
    let manifest = toml::from_str(&text).map_err(|source| WorkspaceError::Toml {
        path: path.display().to_string(),
        source,
    })?;
    Ok(Some(manifest))
}

/// Write `manifest` to `root/synaptic-workspace.toml`.
pub fn write_manifest(root: &Path, manifest: &WorkspaceManifest) -> Result<()> {
    let path = root.join(MANIFEST_NAME);
    // `toml::to_string` cannot fail for this always-serializable struct, but
    // surface any error as JSON-style for uniformity.
    let text = toml::to_string_pretty(manifest).map_err(|e| WorkspaceError::Io {
        context: format!("serializing {}", path.display()),
        source: std::io::Error::other(e.to_string()),
    })?;
    std::fs::write(&path, text).map_err(|source| WorkspaceError::Io {
        context: format!("writing {}", path.display()),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_design_example() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(MANIFEST_NAME),
            r#"
[workspace]
name = "acme-platform"
default_branch = "main"
members = ["services/*", "libs/*"]

[[repos]]
name = "billing"
git  = "https://github.com/acme/billing"
rev  = "main"

[[repos]]
name = "identity"
subgraph = "https://artifacts.acme.com/identity/latest/graph.json"
"#,
        )
        .unwrap();
        let m = load_manifest(d.path()).unwrap().unwrap();
        assert_eq!(m.workspace.name, "acme-platform");
        assert_eq!(m.workspace.default_branch, "main");
        assert_eq!(m.workspace.members, vec!["services/*", "libs/*"]);
        assert_eq!(m.repos.len(), 2);
        assert_eq!(m.repos[0].name, "billing");
        assert_eq!(
            m.repos[0].git.as_deref(),
            Some("https://github.com/acme/billing")
        );
        assert_eq!(
            m.repos[1].subgraph.as_deref().unwrap(),
            "https://artifacts.acme.com/identity/latest/graph.json"
        );
        assert!(m.repos[1].git.is_none());
    }

    #[test]
    fn absent_manifest_is_none() {
        let d = tempfile::tempdir().unwrap();
        assert!(load_manifest(d.path()).unwrap().is_none());
    }

    #[test]
    fn malformed_manifest_is_err() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(MANIFEST_NAME), "this is not = valid toml [[[").unwrap();
        assert!(load_manifest(d.path()).is_err());
    }

    #[test]
    fn default_branch_when_omitted() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(MANIFEST_NAME),
            "[workspace]\nname = \"x\"\nmembers = [\"a\"]\n",
        )
        .unwrap();
        let m = load_manifest(d.path()).unwrap().unwrap();
        assert_eq!(m.workspace.default_branch, "main");
    }

    #[test]
    fn write_then_load_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let m = WorkspaceManifest {
            workspace: WorkspaceMeta {
                name: "demo".into(),
                default_branch: "trunk".into(),
                members: vec!["crates/*".into()],
            },
            repos: vec![RepoMember {
                name: "ext".into(),
                git: Some("https://example.com/ext".into()),
                rev: None,
                subgraph: None,
                path: None,
            }],
        };
        write_manifest(d.path(), &m).unwrap();
        let back = load_manifest(d.path()).unwrap().unwrap();
        assert_eq!(back, m);
    }
}
