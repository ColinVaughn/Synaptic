//! msgpack (rmp-serde) value codec for shard tables, plus a canonical,
//! order-independent content hash used as the cache key for persisted index
//! blobs and the incremental "skip unchanged shard" check.

use crate::StoreError;
use serde::{de::DeserializeOwned, Serialize};

/// Encode a value to msgpack with named fields, so `#[serde(flatten)] extra`
/// maps on [`synaptic_core::node::Node`] / [`synaptic_core::edge::Edge`] survive
/// the round-trip (matches the AST cache's `to_vec_named` choice).
pub fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>, StoreError> {
    rmp_serde::to_vec_named(v).map_err(|e| StoreError::Codec(e.to_string()))
}

/// Decode a msgpack value produced by [`encode`].
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    rmp_serde::from_slice(bytes).map_err(|e| StoreError::Codec(e.to_string()))
}

/// Deflate-compress a chunk/blob (schema v2 rows). Graph payloads are highly
/// repetitive (field names, path prefixes), so this typically shrinks them
/// several-fold; miniz_oxide keeps the workspace free of C toolchains.
pub fn compress(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    // Writing to a Vec cannot fail.
    let _ = enc.write_all(bytes);
    enc.finish().unwrap_or_default()
}

/// Inverse of [`compress`].
pub fn decompress(bytes: &[u8]) -> Result<Vec<u8>, StoreError> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|e| StoreError::Codec(format!("inflate: {e}")))?;
    Ok(out)
}

/// Content hash of a shard, independent of node/edge *order*: canonical keys are
/// sorted before hashing, so two builds that emit the same set of nodes/edges in
/// a different order hash identically. Lets the incremental path skip a shard
/// whose content is unchanged even if the extractor reorders output.
pub fn source_hash(node_ids: &[String], edge_keys: &[String]) -> String {
    let mut ids: Vec<&str> = node_ids.iter().map(|s| s.as_str()).collect();
    let mut eks: Vec<&str> = edge_keys.iter().map(|s| s.as_str()).collect();
    ids.sort_unstable();
    eks.sort_unstable();
    let mut h = blake3::Hasher::new();
    for id in ids {
        h.update(id.as_bytes());
        h.update(b"\0");
    }
    h.update(b"\x01");
    for k in eks {
        h.update(k.as_bytes());
        h.update(b"\0");
    }
    h.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::confidence::Confidence;
    use synaptic_core::edge::Edge;
    use synaptic_core::file_type::FileType;
    use synaptic_core::id::NodeId;
    use synaptic_core::node::Node;

    fn node(id: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: id.into(),
            file_type: FileType::Code,
            source_file: format!("src/{id}.rs"),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    fn edge(s: &str, t: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: format!("src/{s}.rs"),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn node_round_trips() {
        let n = node("billing::Ledger");
        let bytes = encode(&n).unwrap();
        let back: Node = decode(&bytes).unwrap();
        assert_eq!(n, back);
    }

    #[test]
    fn edge_vec_round_trips() {
        let v = vec![edge("a", "b"), edge("b", "c")];
        let bytes = encode(&v).unwrap();
        let back: Vec<Edge> = decode(&bytes).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn node_with_flattened_extra_survives() {
        let mut n = node("x");
        n.extra.insert("norm_label".into(), serde_json::json!("x"));
        n.extra.insert("_origin".into(), serde_json::json!("ast"));
        let back: Node = decode(&encode(&n).unwrap()).unwrap();
        assert_eq!(back.extra.get("norm_label").unwrap(), "x");
        assert_eq!(back.extra.get("_origin").unwrap(), "ast");
    }

    #[test]
    fn source_hash_is_stable_and_order_independent() {
        let h1 = source_hash(&["b".into(), "a".into()], &["a->b".into()]);
        let h2 = source_hash(&["a".into(), "b".into()], &["a->b".into()]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn compress_round_trips_and_shrinks_repetitive_payloads() {
        let nodes: Vec<Node> = (0..500).map(|i| node(&format!("mod::func_{i}"))).collect();
        let raw = encode(&nodes).unwrap();
        let packed = compress(&raw);
        assert!(
            packed.len() * 2 < raw.len(),
            "named-field msgpack must deflate to under half: {} vs {}",
            packed.len(),
            raw.len()
        );
        assert_eq!(decompress(&packed).unwrap(), raw);
    }

    #[test]
    fn source_hash_changes_with_content() {
        let base = source_hash(&["a".into()], &[]);
        assert_ne!(base, source_hash(&["a".into(), "b".into()], &[]));
        assert_ne!(base, source_hash(&["a".into()], &["a->b".into()]));
    }
}
