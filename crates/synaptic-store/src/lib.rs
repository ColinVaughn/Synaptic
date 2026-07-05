//! redb-backed, per-repo sharded graph store.
//!
//! Replaces the load-everything-into-one-petgraph model with a per-repo shard
//! store: one redb database file per repository under `synaptic-out/store/`,
//! materialized into the existing [`synaptic_graph::KnowledgeGraph`] one shard
//! at a time. See `docs/superpowers/specs/2026-06-23-sharded-ondisk-graph-store-design.md`.

pub mod codec;
pub mod manifest;
pub mod migrate;
pub mod shard;
pub mod store;
pub mod tag;

pub use store::ShardStore;

/// Per-shard ceilings. The store has no *aggregate* cap (each repo is its own
/// shard, materialized one at a time), so a federation can far exceed the legacy
/// 100k single-graph limit. The per-shard bounds are DoS guards: a single
/// hostile/huge shard cannot force an unbounded materialization. Defaults live in
/// `synaptic_core::limits` (5M nodes / 2 GiB) and honor the
/// `SYNAPTIC_MAX_SHARD_NODES` / `SYNAPTIC_MAX_SHARD_MB` env overrides (`0` = no cap).
pub use synaptic_core::limits::{DEFAULT_MAX_SHARD_BYTES, DEFAULT_MAX_SHARD_NODES};

/// Index-blob name for a shard's persisted `QueryIndex`. Shared so the writer
/// (CLI `persist_shard_indexes`) and the reader (server `ShardProvider`) cannot drift.
pub const QUERY_INDEX_BLOB: &str = "query_index";
/// Index-blob name for a shard's persisted `ReverseImpactIndex`.
pub const AFFECTED_INDEX_BLOB: &str = "affected_index";

/// Env var steering cross-repo bridge traversal on unscoped federated queries.
pub const CROSS_REPO_ENV: &str = "SYNAPTIC_CROSS_REPO";

/// How federated queries treat the cross-repo bridge: follow it when the store
/// actually holds bridge edges (`Auto`, the unset default), always (`On`), or
/// never (`Off`, per-repo isolation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossRepoMode {
    /// Detect: traverse the bridge exactly when the store has bridge edges.
    Auto,
    /// Always traverse (a no-op on a store with no bridge edges).
    On,
    /// Never traverse: per-repo isolation.
    Off,
}

impl CrossRepoMode {
    /// The effective traversal decision given whether bridge edges exist.
    pub fn resolve(self, has_bridge: bool) -> bool {
        match self {
            CrossRepoMode::On => true,
            CrossRepoMode::Off => false,
            CrossRepoMode::Auto => has_bridge,
        }
    }
}

/// Parse a raw [`CROSS_REPO_ENV`] value: `1`/`true`/`yes`/`on` force traversal,
/// `0`/`false`/`no`/`off` isolate, anything else (unset included) auto-detects.
pub fn parse_cross_repo(raw: Option<&str>) -> CrossRepoMode {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("1") | Some("true") | Some("yes") | Some("on") => CrossRepoMode::On,
        Some("0") | Some("false") | Some("no") | Some("off") => CrossRepoMode::Off,
        _ => CrossRepoMode::Auto,
    }
}

/// [`parse_cross_repo`] over the process env ([`CROSS_REPO_ENV`]).
pub fn cross_repo_mode() -> CrossRepoMode {
    parse_cross_repo(std::env::var(CROSS_REPO_ENV).ok().as_deref())
}

/// What a query (or export) ranges over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every shard's own content; the cross-repo bridge is grafted separately
    /// (see `ShardStore::export_cross_repo` and [`CrossRepoMode`]).
    All,
    /// A single repo shard.
    Repo(String),
}

/// Errors surfaced by the store layer.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A shard tag could not be mapped to a safe on-disk name.
    #[error("invalid shard tag: {0:?}")]
    BadTag(String),
    /// An error from the underlying redb engine.
    #[error("redb: {0}")]
    Redb(String),
    /// A filesystem error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A (de)serialization error from the value codec.
    #[error("codec: {0}")]
    Codec(String),
    /// A manifest load/validate error.
    #[error("manifest: {0}")]
    Manifest(String),
}

pub use tag::sanitize_tag;

#[cfg(test)]
mod cross_repo_tests {
    use super::*;

    #[test]
    fn parse_cross_repo_is_tristate() {
        // Unset, empty, or unrecognized values mean auto-detection.
        assert_eq!(parse_cross_repo(None), CrossRepoMode::Auto);
        assert_eq!(parse_cross_repo(Some("")), CrossRepoMode::Auto);
        assert_eq!(parse_cross_repo(Some("auto")), CrossRepoMode::Auto);
        assert_eq!(parse_cross_repo(Some("maybe")), CrossRepoMode::Auto);
        // Truthy forms force traversal on.
        for v in ["1", "true", "yes", "on", " ON ", "True"] {
            assert_eq!(parse_cross_repo(Some(v)), CrossRepoMode::On, "{v:?}");
        }
        // Falsy forms isolate per repo.
        for v in ["0", "false", "no", "off", " Off "] {
            assert_eq!(parse_cross_repo(Some(v)), CrossRepoMode::Off, "{v:?}");
        }
    }

    #[test]
    fn auto_resolves_from_bridge_presence() {
        assert!(CrossRepoMode::Auto.resolve(true));
        assert!(!CrossRepoMode::Auto.resolve(false));
        // Explicit settings ignore detection.
        assert!(CrossRepoMode::On.resolve(false));
        assert!(!CrossRepoMode::Off.resolve(true));
    }
}
