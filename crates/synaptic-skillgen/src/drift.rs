//! Drift-guard for the rendered skill artifacts, exposing the `--check`/`--bless`
//! workflow.
//!
//! Our render is pure `@@SLOT@@` substitution, but committing a golden
//! `expected/` tree makes any hand-edit of the embedded template (or a stale
//! snapshot) visible in diffs and catchable in CI. [`check_drift`] re-renders
//! every artifact and byte-diffs it against the committed tree; [`bless`]
//! rewrites the tree.
//!
//! The tree lives next to this crate's source
//! (`crates/synaptic-skillgen/expected/`), resolved via `CARGO_MANIFEST_DIR`.
//! Both are therefore dev/CI tools run from the repo checkout — an installed
//! binary has a stale manifest dir and will report the snapshots missing, by
//! design. The unit test below runs `check_drift` so a normal `cargo test`
//! fails the moment the template and snapshots diverge.

use std::path::{Path, PathBuf};

use crate::{always_on_section, render_skill, Platform};

/// A rendered skill artifact: a flat snapshot filename + its content.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedArtifact {
    pub name: String,
    pub content: String,
}

/// Every artifact whose content we lock: the per-platform skill render plus the
/// shared always-on section.
pub fn render_all() -> Vec<RenderedArtifact> {
    let mut out: Vec<RenderedArtifact> = Platform::all()
        .iter()
        .map(|p| RenderedArtifact {
            name: format!("skill-{}.md", p.key()),
            content: render_skill(*p),
        })
        .collect();
    out.push(RenderedArtifact {
        name: "always-on.md".to_string(),
        content: always_on_section(),
    });
    out
}

/// The committed snapshot directory (next to this crate's source).
fn expected_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("expected")
}

/// Compare ignoring line-ending style — a snapshot checked out as CRLF (e.g.
/// `core.autocrlf=true`, despite `.gitattributes`) must not read as drift.
fn eol_eq(a: &str, b: &str) -> bool {
    a.replace("\r\n", "\n") == b.replace("\r\n", "\n")
}

/// Re-render and diff every artifact against the committed `expected/` tree
/// (line-ending-insensitive). `Ok(())` when clean; `Err(messages)` lists each
/// drift + how to fix it.
pub fn check_drift() -> Result<(), Vec<String>> {
    let dir = expected_dir();
    let mut problems = Vec::new();
    for art in render_all() {
        let path = dir.join(&art.name);
        match std::fs::read_to_string(&path) {
            Ok(committed) if eol_eq(&committed, &art.content) => {}
            Ok(_) => problems.push(format!(
                "expected/{} is out of date (run `synaptic skill bless`)",
                art.name
            )),
            Err(_) => problems.push(format!(
                "missing expected/{} (run `synaptic skill bless`)",
                art.name
            )),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Rewrite the `expected/` tree from the current render. Returns paths written.
pub fn bless() -> std::io::Result<Vec<PathBuf>> {
    let dir = expected_dir();
    std::fs::create_dir_all(&dir)?;
    let mut written = Vec::new();
    for art in render_all() {
        let path = dir.join(&art.name);
        std::fs::write(&path, &art.content)?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_one_artifact_per_platform_plus_always_on() {
        let arts = render_all();
        assert_eq!(arts.len(), Platform::all().len() + 1);
        assert!(arts.iter().any(|a| a.name == "skill-claude.md"));
        assert!(arts.iter().any(|a| a.name == "always-on.md"));
        // No unfilled slots in any render.
        assert!(arts.iter().all(|a| !a.content.contains("@@")));
    }

    #[test]
    fn expected_tree_is_in_sync() {
        // CI guard: a template edit without `synaptic skill bless` fails here.
        if let Err(problems) = check_drift() {
            panic!("skill artifacts drifted:\n{}", problems.join("\n"));
        }
    }
}
