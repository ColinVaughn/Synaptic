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

/// What a query (or export) ranges over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every shard (per-repo isolation by default; cross-repo edges are not
    /// traversed unless explicitly requested).
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
