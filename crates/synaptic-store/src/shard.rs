//! A single repo shard, stored as one redb database file.
//!
//! Nodes and links are stored under **sequence keys** (`0, 1, 2, …`) rather than
//! keyed by id, so redb's ordered B-tree iteration reproduces the original
//! insertion order. That order-preservation is what lets `synaptic export`
//! reproduce a byte-identical `graph.json` (the regression harness).

use std::path::Path;

use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use synaptic_core::{GraphData, Hyperedge};
use synaptic_graph::KnowledgeGraph;

use crate::{codec, StoreError};

/// On-disk schema version for a shard `.redb` file. Bump on any breaking change
/// to the table layout; the manifest records it so an old store is rejected with
/// a clear "re-migrate" error rather than misread.
pub const SCHEMA_VERSION: u32 = 1;

/// Scalars + small collections that do not need per-element keys.
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
/// `seq -> msgpack(Node)`; iteration order == insertion order.
const NODES: TableDefinition<u64, &[u8]> = TableDefinition::new("nodes");
/// `seq -> msgpack(Edge)`; iteration order == insertion order.
const LINKS: TableDefinition<u64, &[u8]> = TableDefinition::new("links");
/// `(index_name, source_hash) -> opaque producer-owned bytes`. The store never
/// learns an index's internals; the producing crate serializes its own bytes.
const INDEX_BLOBS: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("index_blobs");

const META_KEY: &str = "meta";

/// Graph-level scalars carried alongside the node/link tables.
#[derive(Debug, Serialize, Deserialize)]
struct ShardMeta {
    schema_version: u32,
    directed: bool,
    multigraph: bool,
    built_at_commit: Option<String>,
    hyperedges: Vec<Hyperedge>,
}

fn re<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Redb(e.to_string())
}

/// Write `gd` to a fresh shard database at `path`, replacing any existing file.
pub fn write(path: &Path, gd: &GraphData) -> Result<(), StoreError> {
    // Start from a clean file so a rewrite never leaves stale rows behind.
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let db = Database::create(path).map_err(re)?;
    let txn = db.begin_write().map_err(re)?;
    {
        let mut meta = txn.open_table(META).map_err(re)?;
        let m = ShardMeta {
            schema_version: SCHEMA_VERSION,
            directed: gd.directed,
            multigraph: gd.multigraph,
            built_at_commit: gd.built_at_commit.clone(),
            hyperedges: gd.hyperedges.clone(),
        };
        meta.insert(META_KEY, codec::encode(&m)?.as_slice())
            .map_err(re)?;

        // open_table creates the (possibly empty) table so reads can open it.
        let mut nodes = txn.open_table(NODES).map_err(re)?;
        for (i, n) in gd.nodes.iter().enumerate() {
            nodes
                .insert(i as u64, codec::encode(n)?.as_slice())
                .map_err(re)?;
        }
        let mut links = txn.open_table(LINKS).map_err(re)?;
        for (i, e) in gd.links.iter().enumerate() {
            links
                .insert(i as u64, codec::encode(e)?.as_slice())
                .map_err(re)?;
        }
    }
    txn.commit().map_err(re)?;
    Ok(())
}

/// Read a shard database back into a `GraphData`, preserving node/link order.
pub fn read_graph_data(path: &Path) -> Result<GraphData, StoreError> {
    let db = Database::open(path).map_err(re)?;
    let txn = db.begin_read().map_err(re)?;

    let meta_t = txn.open_table(META).map_err(re)?;
    let m: ShardMeta = match meta_t.get(META_KEY).map_err(re)? {
        Some(v) => codec::decode(v.value())?,
        None => return Err(StoreError::Manifest("shard is missing its meta row".into())),
    };

    let nodes_t = txn.open_table(NODES).map_err(re)?;
    let mut nodes = Vec::new();
    for row in nodes_t.iter().map_err(re)? {
        let (_seq, v) = row.map_err(re)?;
        nodes.push(codec::decode(v.value())?);
    }

    let links_t = txn.open_table(LINKS).map_err(re)?;
    let mut links = Vec::new();
    for row in links_t.iter().map_err(re)? {
        let (_seq, v) = row.map_err(re)?;
        links.push(codec::decode(v.value())?);
    }

    Ok(GraphData {
        directed: m.directed,
        multigraph: m.multigraph,
        graph: serde_json::Map::new(),
        nodes,
        links,
        hyperedges: m.hyperedges,
        built_at_commit: m.built_at_commit,
    })
}

/// Materialize a shard into the in-memory [`KnowledgeGraph`] used by every
/// query. Identical to loading the same `graph.json`: it reuses
/// [`KnowledgeGraph::from_graph_data`] rather than reimplementing the build, so
/// a materialized shard is byte-for-byte what today's load path produces.
pub fn materialize(path: &Path) -> Result<KnowledgeGraph, StoreError> {
    Ok(KnowledgeGraph::from_graph_data(read_graph_data(path)?))
}

/// Store a producer-owned index blob under `(name, source_hash)` in the shard.
pub fn put_index_blob(
    path: &Path,
    name: &str,
    source_hash: &str,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let db = Database::open(path).map_err(re)?;
    let txn = db.begin_write().map_err(re)?;
    {
        let mut t = txn.open_table(INDEX_BLOBS).map_err(re)?;
        t.insert((name, source_hash), bytes).map_err(re)?;
    }
    txn.commit().map_err(re)?;
    Ok(())
}

/// Fetch the index blob for `(name, source_hash)`. Returns `None` when the shard
/// has no such blob (including when the shard's content has changed, so the
/// caller's `source_hash` no longer matches what was persisted — a stale-blob miss).
pub fn get_index_blob(
    path: &Path,
    name: &str,
    source_hash: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    let db = Database::open(path).map_err(re)?;
    let txn = db.begin_read().map_err(re)?;
    // The blob table is created lazily on first put; absent == no blobs yet.
    let t = match txn.open_table(INDEX_BLOBS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(re(e)),
    };
    match t.get((name, source_hash)).map_err(re)? {
        Some(v) => Ok(Some(v.value().to_vec())),
        None => Ok(None),
    }
}
