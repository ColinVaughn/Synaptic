//! Speculative execution for CodeGraph.
//!
//! Given a proposed change and the tests a forecast flagged as at-risk, this
//! crate materializes the change in a throwaway `git worktree`, runs only those
//! tests plus a build/type-check, and reports the actual pass/fail outcome. It is
//! the ground-truth half of the prediction system: the graph narrows *what to
//! check*, the sandbox *confirms* it. The user's real working tree is never
//! touched and the worktree is removed on completion.
//!
//! This is deliberately an opt-in library/CLI surface, never an MCP tool: it runs
//! commands, which would break the server's read-only invariant.
#![forbid(unsafe_code)]

mod detect;
mod render;
mod run;
mod speculate;
mod worktree;

pub use detect::{detect_commands, DetectedCommands};
pub use render::render_markdown;
pub use run::{CommandResult, CommandStatus};
pub use speculate::{speculate, Change, Outcome, SpeculateOptions, SpeculateReport};

/// Errors speculative execution can surface.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("git error: {0}")]
    Git(String),
    #[error("could not apply the proposed change: {0}")]
    Apply(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
