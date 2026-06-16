//! Time-travel diff: build the CodeGraph at a git revision and diff two revisions.
//!
//! Each revision is materialized into a throwaway `git worktree` and built with
//! the existing `codegraph_incremental::rebuild` pipeline (detect + extract +
//! resolve + cluster), so no extraction logic is duplicated here. Built graphs are
//! cached per commit SHA under `codegraph-out/history/`. The reporting layer
//! derives added/removed dependencies, removed APIs, architectural drift, new
//! dependency cycles, and hotspots of change from the existing `graph_diff` delta
//! plus `git diff --numstat`.

use std::path::Path;

pub mod build;
pub mod git;
pub mod html;
pub mod report;
pub mod snapshot;

pub use html::to_html;
pub use report::{DiffReport, DriftReport, Hotspot, ModuleDep, ModuleDrift, RemovedApi};

/// Which revision to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rev {
    /// A committed revision (resolved SHA).
    Commit(String),
    /// The current working tree (uncommitted state).
    WorkingTree,
}

/// Options controlling a diff.
#[derive(Debug, Clone)]
pub struct DiffOptions {
    /// Build a directed graph for each revision.
    pub directed: bool,
    /// Limit reports to source files under this path prefix (repo-relative).
    pub scope: Option<String>,
    /// Max rows per ranked report section.
    pub top: usize,
    /// Path-component depth defining a "module" (e.g. 2 => `crates/foo`). A file
    /// is never its own module, so this is clamped to leave the filename out.
    pub module_depth: usize,
    /// Edge relations counted as dependencies.
    pub dep_relations: Vec<String>,
    /// Skip the per-SHA snapshot store (always rebuild).
    pub no_cache: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        DiffOptions {
            directed: false,
            scope: None,
            top: 20,
            // Depth 2 keeps sibling modules distinct in a monorepo (crates/foo vs
            // crates/bar) while collapsing to a top-level dir in a flat repo.
            module_depth: 2,
            // The codebase's structural relation vocabulary (mirrors
            // DEFAULT_AFFECTED_RELATIONS): a cross-module edge of any of these is
            // an architectural dependency. Cross-file `calls`/`references` matter
            // because per-language import resolution often leaves the import edge
            // pointing at a module stub rather than the resolved file node.
            dep_relations: [
                "calls",
                "references",
                "imports",
                "imports_from",
                "re_exports",
                "inherits",
                "extends",
                "implements",
                "uses",
                "mixes_in",
                "embeds",
                "depends_on",
                "reads_from",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            no_cache: false,
        }
    }
}

/// Errors a diff can surface.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("git error: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("rebuild error: {0}")]
    Rebuild(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Diff the code graph between two revisions. `rev2 = None` means the working tree.
pub fn diff(
    repo_root: &Path,
    rev1: &str,
    rev2: Option<&str>,
    opts: &DiffOptions,
) -> Result<DiffReport, HistoryError> {
    let sha1 = git::rev_parse(repo_root, rev1)?;
    let old = build::build_at_rev(repo_root, &sha1, opts.directed, !opts.no_cache)?;
    let (label2, new, ns) = match rev2 {
        Some(r2) => {
            let sha2 = git::rev_parse(repo_root, r2)?;
            let new = build::build_at_rev(repo_root, &sha2, opts.directed, !opts.no_cache)?;
            let ns = git::numstat(repo_root, &sha1, Some(&sha2))?;
            (sha2, new, ns)
        }
        None => {
            let new = build::build_working_tree(repo_root, opts.directed)?;
            let ns = git::numstat(repo_root, &sha1, None)?;
            ("WORKING_TREE".to_string(), new, ns)
        }
    };
    Ok(report::assemble(&sha1, &label2, &old, &new, &ns, opts))
}
