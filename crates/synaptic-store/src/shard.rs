//! A single repo shard, stored as one file.
//!
//! v2 (current) is a flat container: a magic tag, a raw-msgpack header (graph
//! scalars + a table of contents), then deflate-compressed chunks of nodes and
//! links plus any index blobs, all in insertion order. Order-preservation is
//! what lets `synaptic export` reproduce a byte-identical `graph.json` (the
//! regression harness). A flat file was chosen over the v1 redb database after
//! measurement: shards are written once behind an RCU rename and only ever
//! read back whole or by blob, so a B-tree bought nothing and cost plenty --
//! on a real repo half the file was redb page overhead, an empty database
//! costs ~1.5 MiB, growth steps double the file, and `compact()` *grew* small
//! files. v1 shards (redb, one raw msgpack record per row) are no longer
//! readable: redb 3.0 dropped the file format redb 2.x wrote, so no current
//! redb can open them, and the dependency went with the read path. A v1 file
//! is recognized by redb's magic and rejected with a rebuild hint -- the store
//! is derived data that `synaptic extract` regenerates in one pass.

use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;

use serde::{Deserialize, Serialize};
use synaptic_core::{GraphData, Hyperedge};
use synaptic_graph::KnowledgeGraph;

use crate::{codec, StoreError};

/// On-disk schema version. v1 = redb, one msgpack record per row (no longer
/// readable; detected and rejected with a rebuild hint). v2 = flat container
/// with deflate-compressed chunks. Writes always produce the current version.
pub const SCHEMA_VERSION: u32 = 2;

/// Records per v2 chunk. Large enough to amortize per-chunk cost and give
/// deflate a real window, small enough to keep a single chunk's decode cheap.
const CHUNK: usize = 1024;

/// Leading magic of a v2 flat shard file.
const MAGIC: &[u8; 8] = b"SYNSHRD2";

/// Leading magic of a v1 redb shard file (redb's own file header).
const V1_MAGIC: &[u8; 9] = &[b'r', b'e', b'd', b'b', 0x1A, 0x0A, 0xA9, 0x0D, 0x0A];

/// v2 header: graph scalars plus the container's table of contents. Sections
/// follow the header in this order: node chunks, link chunks, blobs; each
/// entry records its compressed byte length, so offsets are cumulative.
#[derive(Debug, Serialize, Deserialize)]
struct FlatHeader {
    schema_version: u32,
    directed: bool,
    multigraph: bool,
    built_at_commit: Option<String>,
    hyperedges: Vec<Hyperedge>,
    node_chunks: Vec<u64>,
    link_chunks: Vec<u64>,
    blobs: Vec<FlatBlob>,
}

/// One persisted index blob in a v2 file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlatBlob {
    name: String,
    source_hash: String,
    len: u64,
}

/// Error for a shard file that is not a flat container. A legacy v1 (redb)
/// file gets a rebuild hint; anything else is called out as unrecognized.
fn unreadable(path: &Path) -> StoreError {
    let mut head = [0u8; 9];
    let is_v1 = std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .map(|()| &head == V1_MAGIC)
        .unwrap_or(false);
    if is_v1 {
        StoreError::UnreadableShard(format!(
            "shard {} is in the legacy v1 (redb) format this build no longer reads; re-run `synaptic extract` to rebuild the store",
            path.display()
        ))
    } else {
        StoreError::UnreadableShard(format!("{} is not a recognized shard file", path.display()))
    }
}

/// Whether the file at `path` is a v2 flat shard (leading-magic probe only).
/// Migration uses this to force a rewrite of legacy files whose content hash
/// still matches the manifest.
pub(crate) fn is_flat(path: &Path) -> bool {
    let mut m = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut m))
        .map(|()| &m == MAGIC)
        .unwrap_or(false)
}

fn newer_version_err(v: u32) -> StoreError {
    StoreError::Manifest(format!(
        "shard schema v{v} is newer than this binary (reads up to v{SCHEMA_VERSION}); upgrade synaptic or re-run `synaptic extract`"
    ))
}

/// Write `gd` to a fresh shard file at `path`, replacing any existing file.
pub fn write(path: &Path, gd: &GraphData) -> Result<(), StoreError> {
    write_with_blobs(path, gd, "", &[])
}

/// Write `gd` plus any pre-built index blobs in one pass. The caller stages
/// `path` (the store's RCU tmp-then-rename), so this writes directly and
/// fsyncs before returning.
pub fn write_with_blobs(
    path: &Path,
    gd: &GraphData,
    source_hash: &str,
    blobs: &[(&str, &[u8])],
) -> Result<(), StoreError> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut header = FlatHeader {
        schema_version: SCHEMA_VERSION,
        directed: gd.directed,
        multigraph: gd.multigraph,
        built_at_commit: gd.built_at_commit.clone(),
        hyperedges: gd.hyperedges.clone(),
        node_chunks: Vec::new(),
        link_chunks: Vec::new(),
        blobs: Vec::new(),
    };
    let mut body: Vec<u8> = Vec::new();
    for chunk in gd.nodes.chunks(CHUNK) {
        let packed = codec::compress(&codec::encode(&chunk)?);
        header.node_chunks.push(packed.len() as u64);
        body.extend_from_slice(&packed);
    }
    for chunk in gd.links.chunks(CHUNK) {
        let packed = codec::compress(&codec::encode(&chunk)?);
        header.link_chunks.push(packed.len() as u64);
        body.extend_from_slice(&packed);
    }
    for (name, bytes) in blobs {
        let packed = codec::compress(bytes);
        header.blobs.push(FlatBlob {
            name: name.to_string(),
            source_hash: source_hash.to_string(),
            len: packed.len() as u64,
        });
        body.extend_from_slice(&packed);
    }
    write_flat(path, &header, &body)
}

/// Emit magic + header + body to `path` and fsync.
fn write_flat(path: &Path, header: &FlatHeader, body: &[u8]) -> Result<(), StoreError> {
    let head = codec::encode(header)?;
    let mut f = std::fs::File::create(path)?;
    f.write_all(MAGIC)?;
    f.write_all(&(head.len() as u32).to_le_bytes())?;
    f.write_all(&head)?;
    f.write_all(body)?;
    f.sync_all()?;
    Ok(())
}

/// Read a v2 file's header plus the absolute offset where its body starts.
/// Returns `None` when the file is not a flat shard (legacy v1 or foreign).
fn read_flat_header(f: &mut std::fs::File) -> Result<Option<(FlatHeader, u64)>, StoreError> {
    let mut magic = [0u8; 8];
    match f.read_exact(&mut magic) {
        Ok(()) if &magic == MAGIC => {}
        _ => return Ok(None),
    }
    let mut len4 = [0u8; 4];
    f.read_exact(&mut len4)?;
    let hlen = u32::from_le_bytes(len4) as usize;
    let mut head = vec![0u8; hlen];
    f.read_exact(&mut head)?;
    let header: FlatHeader = codec::decode(&head)?;
    if header.schema_version > SCHEMA_VERSION {
        return Err(newer_version_err(header.schema_version));
    }
    Ok(Some((header, 12 + hlen as u64)))
}

/// Read a shard back into a `GraphData`, preserving node/link order.
pub fn read_graph_data(path: &Path) -> Result<GraphData, StoreError> {
    let mut f = std::fs::File::open(path)?;
    if let Some((header, _body_start)) = read_flat_header(&mut f)? {
        // Chunks follow the header sequentially; the file cursor is there.
        let mut nodes = Vec::new();
        for len in &header.node_chunks {
            let mut buf = vec![0u8; *len as usize];
            f.read_exact(&mut buf)?;
            nodes.extend(codec::decode::<Vec<synaptic_core::Node>>(
                &codec::decompress(&buf)?,
            )?);
        }
        let mut links = Vec::new();
        for len in &header.link_chunks {
            let mut buf = vec![0u8; *len as usize];
            f.read_exact(&mut buf)?;
            links.extend(codec::decode::<Vec<synaptic_core::Edge>>(
                &codec::decompress(&buf)?,
            )?);
        }
        return Ok(GraphData {
            directed: header.directed,
            multigraph: header.multigraph,
            graph: serde_json::Map::new(),
            nodes,
            links,
            hyperedges: header.hyperedges,
            built_at_commit: header.built_at_commit,
        });
    }
    drop(f);
    Err(unreadable(path))
}

/// Materialize a shard into the in-memory [`KnowledgeGraph`] used by every
/// query. Identical to loading the same `graph.json`: it reuses
/// [`KnowledgeGraph::from_graph_data`] rather than reimplementing the build, so
/// a materialized shard is byte-for-byte what today's load path produces.
pub fn materialize(path: &Path) -> Result<KnowledgeGraph, StoreError> {
    Ok(KnowledgeGraph::from_graph_data(read_graph_data(path)?))
}

/// Byte/row breakdown of one shard file: how much is node chunks, link chunks,
/// index blobs, and structural overhead (`file_bytes` minus the section sums).
/// Feeds the `store_report` example and size regressions.
#[derive(Debug, Default, Clone)]
pub struct ShardStats {
    pub file_bytes: u64,
    pub node_rows: u64,
    pub node_value_bytes: u64,
    pub link_rows: u64,
    pub link_value_bytes: u64,
    pub meta_value_bytes: u64,
    pub index_blob_rows: u64,
    pub index_blob_bytes: u64,
}

/// Measure [`ShardStats`] for the shard at `path`.
pub fn shard_stats(path: &Path) -> Result<ShardStats, StoreError> {
    let mut s = ShardStats {
        file_bytes: std::fs::metadata(path)?.len(),
        ..ShardStats::default()
    };
    let mut f = std::fs::File::open(path)?;
    if let Some((header, body_start)) = read_flat_header(&mut f)? {
        s.meta_value_bytes = body_start.saturating_sub(8);
        s.node_rows = header.node_chunks.len() as u64;
        s.node_value_bytes = header.node_chunks.iter().sum();
        s.link_rows = header.link_chunks.len() as u64;
        s.link_value_bytes = header.link_chunks.iter().sum();
        s.index_blob_rows = header.blobs.len() as u64;
        s.index_blob_bytes = header.blobs.iter().map(|b| b.len).sum();
        return Ok(s);
    }
    drop(f);
    Err(unreadable(path))
}

/// Store a producer-owned index blob under `(name, source_hash)` in the shard.
pub fn put_index_blob(
    path: &Path,
    name: &str,
    source_hash: &str,
    bytes: &[u8],
) -> Result<(), StoreError> {
    put_index_blobs(path, source_hash, &[(name, bytes)])
}

/// Store several index blobs for one `source_hash`. Rewrites the container
/// once (tmp + rename, so readers never see a torn file) — the lazy
/// serve-time persistence path, at most once per shard content.
pub fn put_index_blobs(
    path: &Path,
    source_hash: &str,
    entries: &[(&str, &[u8])],
) -> Result<(), StoreError> {
    let mut f = std::fs::File::open(path)?;
    if let Some((mut header, _body_start)) = read_flat_header(&mut f)? {
        // Keep every section byte, drop blobs being replaced, append the new.
        let mut body = Vec::new();
        f.read_to_end(&mut body)?;
        let chunk_bytes: u64 =
            header.node_chunks.iter().sum::<u64>() + header.link_chunks.iter().sum::<u64>();
        let mut kept = body[..chunk_bytes as usize].to_vec();
        let mut off = chunk_bytes as usize;
        let mut kept_blobs = Vec::new();
        for b in &header.blobs {
            let next = off + b.len as usize;
            let replaced = entries
                .iter()
                .any(|(n, _)| *n == b.name && source_hash == b.source_hash);
            if !replaced {
                kept.extend_from_slice(&body[off..next]);
                kept_blobs.push(b.clone());
            }
            off = next;
        }
        for (name, bytes) in entries {
            let packed = codec::compress(bytes);
            kept_blobs.push(FlatBlob {
                name: name.to_string(),
                source_hash: source_hash.to_string(),
                len: packed.len() as u64,
            });
            kept.extend_from_slice(&packed);
        }
        header.blobs = kept_blobs;
        drop(f);
        let tmp = path.with_extension("blobs.tmp");
        write_flat(&tmp, &header, &kept)?;
        std::fs::rename(&tmp, path)?;
        return Ok(());
    }
    drop(f);
    Err(unreadable(path))
}

/// Whether the shard holds a blob for `(name, source_hash)` — a table-of-
/// contents lookup only, so callers probing for staleness never pay the
/// blob's read + inflate.
pub fn has_index_blob(path: &Path, name: &str, source_hash: &str) -> Result<bool, StoreError> {
    let mut f = std::fs::File::open(path)?;
    if let Some((header, _)) = read_flat_header(&mut f)? {
        return Ok(header
            .blobs
            .iter()
            .any(|b| b.name == name && b.source_hash == source_hash));
    }
    drop(f);
    Err(unreadable(path))
}

/// Fetch the index blob for `(name, source_hash)`. Returns `None` when the shard
/// has no such blob (including when the shard's content has changed, so the
/// caller's `source_hash` no longer matches what was persisted — a stale-blob miss).
pub fn get_index_blob(
    path: &Path,
    name: &str,
    source_hash: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    let mut f = std::fs::File::open(path)?;
    if let Some((header, body_start)) = read_flat_header(&mut f)? {
        let mut off = body_start
            + header.node_chunks.iter().sum::<u64>()
            + header.link_chunks.iter().sum::<u64>();
        for b in &header.blobs {
            if b.name == name && b.source_hash == source_hash {
                f.seek(SeekFrom::Start(off))?;
                let mut buf = vec![0u8; b.len as usize];
                f.read_exact(&mut buf)?;
                return Ok(Some(codec::decompress(&buf)?));
            }
            off += b.len;
        }
        return Ok(None);
    }
    drop(f);
    Err(unreadable(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, Node, NodeId};

    fn gd(n: usize) -> GraphData {
        let nodes: Vec<Node> = (0..n)
            .map(|i| Node {
                id: NodeId(format!("src/mod_{}.rs::f_{i}", i % 7)),
                label: format!("f_{i}()"),
                file_type: FileType::Code,
                source_file: format!("src/mod_{}.rs", i % 7),
                source_location: Some(format!("L{}", i + 1)),
                community: Some((i % 3) as u32),
                repo: None,
                extra: Map::new(),
            })
            .collect();
        let links: Vec<Edge> = (0..n.saturating_sub(1))
            .map(|i| Edge {
                source: nodes[i].id.clone(),
                target: nodes[i + 1].id.clone(),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                source_file: nodes[i].source_file.clone(),
                source_location: None,
                confidence_score: None,
                weight: 1.0,
                context: None,
                cross_repo: false,
                extra: Map::new(),
            })
            .collect();
        GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links,
            hyperedges: vec![],
            built_at_commit: Some("t".into()),
        }
    }

    #[test]
    fn chunked_write_round_trips_across_chunk_boundaries() {
        // > 2 chunks of nodes and links, so order must survive chunk seams.
        let g = gd(2_500);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.shard");
        write(&p, &g).unwrap();
        let back = read_graph_data(&p).unwrap();
        assert_eq!(back.nodes, g.nodes, "node order and content survive");
        assert_eq!(back.links, g.links, "link order and content survive");
    }

    #[test]
    fn legacy_v1_shard_is_rejected_with_rebuild_hint() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.redb");
        let mut bytes = V1_MAGIC.to_vec();
        bytes.extend_from_slice(&[0u8; 64]);
        std::fs::write(&p, &bytes).unwrap();
        let err = read_graph_data(&p).unwrap_err().to_string();
        assert!(err.contains("legacy v1"), "{err}");
        assert!(err.contains("synaptic extract"), "{err}");
        // Blob probes fail the same way instead of pretending the blob is
        // merely absent, which would mask the unreadable shard.
        assert!(get_index_blob(&p, "query_index", "h1").is_err());
        assert!(has_index_blob(&p, "query_index", "h1").is_err());
        assert!(put_index_blob(&p, "query_index", "h1", b"x").is_err());
        assert!(shard_stats(&p).is_err());
        assert!(!is_flat(&p));
    }

    #[test]
    fn foreign_file_is_rejected_as_unrecognized() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.shard");
        std::fs::write(&p, b"definitely not a shard").unwrap();
        let err = read_graph_data(&p).unwrap_err().to_string();
        assert!(err.contains("not a recognized shard file"), "{err}");
    }

    #[test]
    fn blobs_round_trip_in_one_write_and_lazily() {
        let g = gd(10);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.shard");
        write_with_blobs(&p, &g, "h2", &[("a", b"alpha".as_slice())]).unwrap();
        assert_eq!(get_index_blob(&p, "a", "h2").unwrap().unwrap(), b"alpha");
        // Lazy add + replace: "a" is replaced, "b" appended, graph untouched.
        put_index_blobs(&p, "h2", &[("a", b"alpha2".as_slice()), ("b", b"beta")]).unwrap();
        assert_eq!(get_index_blob(&p, "a", "h2").unwrap().unwrap(), b"alpha2");
        assert_eq!(get_index_blob(&p, "b", "h2").unwrap().unwrap(), b"beta");
        assert!(get_index_blob(&p, "a", "stale-hash").unwrap().is_none());
        assert_eq!(read_graph_data(&p).unwrap().nodes, g.nodes);
    }

    #[test]
    fn newer_schema_is_rejected_with_upgrade_hint() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.shard");
        let header = FlatHeader {
            schema_version: SCHEMA_VERSION + 1,
            directed: true,
            multigraph: false,
            built_at_commit: None,
            hyperedges: vec![],
            node_chunks: vec![],
            link_chunks: vec![],
            blobs: vec![],
        };
        write_flat(&p, &header, &[]).unwrap();
        let err = read_graph_data(&p).unwrap_err().to_string();
        assert!(err.contains("newer than this binary"), "{err}");
    }

    #[test]
    fn chunking_plus_deflate_shrinks_the_file() {
        let g = gd(5_000);
        let raw_payload: usize = g
            .nodes
            .iter()
            .map(|n| codec::encode(n).unwrap().len())
            .chain(g.links.iter().map(|e| codec::encode(e).unwrap().len()))
            .sum();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.shard");
        write(&p, &g).unwrap();
        let file = std::fs::metadata(&p).unwrap().len() as usize;
        assert!(
            file * 2 < raw_payload,
            "v2 file ({file} B) must be under half the raw per-record payload ({raw_payload} B)"
        );
    }
}
