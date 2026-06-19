//! Per-file AST extraction cache. Keyed by `(path, content)` so an unchanged
//! file on a rebuild skips tree-sitter parsing. The cached value is the
//! serialized [`ExtractionResult`] under `<cache_dir>/ast/v{version}/<key>.<ext>`,
//! where `<ext>` is `mp` (MessagePack, the default `cache-binary` feature) or
//! `json` when that feature is off -- see [`CACHE_EXT`].
//! The path is part of the key because node ids and scoping
//! embed it, so two files with identical bytes at different paths must not share
//! an entry. Entries are namespaced by [`AST_CACHE_VERSION`] so a release *or* an
//! extractor-logic change auto-invalidates — see that constant.

use std::path::{Path, PathBuf};

use crate::extract_source;
use crate::result::ExtractionResult;

/// On-disk cache namespace: `{crate version}-{build fingerprint}`. Entries depend
/// on the extractor *code*, not just file contents — keying on the package version
/// alone missed extractor-*behavior* changes (a walker fix that emits different
/// nodes for the same bytes), serving stale pre-fix results from a warm cache
/// within a dev cycle (the version only moves on release). `build.rs` hashes the
/// extract crate's `src/` + enabled `lang-*` features into `CODEGRAPH_EXTRACT_BUILD_ID`,
/// so the namespace rotates the instant extraction logic recompiles and stays warm
/// across identical rebuilds.
pub const AST_CACHE_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "-",
    env!("CODEGRAPH_EXTRACT_BUILD_ID")
);

fn cache_key(path: &str, source: &[u8]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(path.as_bytes());
    h.update(&[0]); // separator so (path, content) can't be ambiguous
    h.update(source);
    // --no-columns changes the SQL output for the same bytes, so it must change
    // the key. Only perturb in the off (non-default) case, leaving every
    // existing default-mode entry valid.
    #[cfg(feature = "lang-sql")]
    if !crate::sql_semantic::emit_sql_columns() {
        h.update(b"\x01sql-no-columns");
    }
    h.finalize().to_hex().to_string()
}

/// On-disk extension for a cache entry. MessagePack and JSON entries are kept on
/// distinct paths so the two formats never collide in a shared cache dir (and a
/// stale entry of the wrong format is simply a miss, not a decode of garbage).
#[cfg(feature = "cache-binary")]
const CACHE_EXT: &str = "mp";
#[cfg(not(feature = "cache-binary"))]
const CACHE_EXT: &str = "json";

fn cache_file(cache_dir: &Path, key: &str) -> PathBuf {
    cache_dir
        .join(format!("ast/v{AST_CACHE_VERSION}"))
        .join(format!("{key}.{CACHE_EXT}"))
}

/// Decode a cache entry. MessagePack under `cache-binary`, else JSON. Returns
/// `None` on any decode error so a corrupt/wrong-format entry falls back to
/// extraction.
fn decode_result(bytes: &[u8]) -> Option<ExtractionResult> {
    #[cfg(feature = "cache-binary")]
    {
        rmp_serde::from_slice(bytes).ok()
    }
    #[cfg(not(feature = "cache-binary"))]
    {
        serde_json::from_slice(bytes).ok()
    }
}

/// Encode a result for the cache. MessagePack under `cache-binary`, else JSON.
/// Uses `to_vec_named` so `#[serde(flatten)]` fields round-trip (rmp's default
/// array encoding can't merge a flattened map). Returns `None` on serialize
/// error so a write failure is best-effort (skip the entry).
fn encode_result(res: &ExtractionResult) -> Option<Vec<u8>> {
    #[cfg(feature = "cache-binary")]
    {
        rmp_serde::to_vec_named(res).ok()
    }
    #[cfg(not(feature = "cache-binary"))]
    {
        serde_json::to_vec(res).ok()
    }
}

/// Extract `source`, using and populating an on-disk cache when `cache_dir` is
/// `Some`. Returns `None` for unsupported (or feature-disabled) extensions,
/// which are never cached. Cache I/O is best-effort: any read/write/parse error
/// falls back to a fresh extraction, so a corrupt cache never blocks a build.
pub fn cached_extract_source(
    cache_dir: Option<&Path>,
    path: &str,
    source: &[u8],
) -> Option<ExtractionResult> {
    let Some(dir) = cache_dir else {
        return extract_source(path, source);
    };
    let file = cache_file(dir, &cache_key(path, source));
    if let Ok(bytes) = std::fs::read(&file) {
        if let Some(res) = decode_result(&bytes) {
            return Some(res);
        }
    }
    let res = extract_source(path, source)?;
    if let Some(parent) = file.parent() {
        if std::fs::create_dir_all(parent).is_ok() {
            if let Some(bytes) = encode_result(&res) {
                let _ = std::fs::write(&file, bytes);
            }
        }
    }
    Some(res)
}

#[cfg(all(test, feature = "lang-python"))]
mod tests {
    use super::*;

    const SRC: &[u8] = b"def f(x):\n    return x\n";

    #[test]
    fn cache_miss_then_hit_is_identical_and_writes_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        let r1 = cached_extract_source(Some(cache), "a.py", SRC).unwrap();
        // A cache file was written for this (path, content).
        let key = cache_key("a.py", SRC);
        assert!(cache_file(cache, &key).exists(), "cache file written");
        // Second call hits the cache and returns an identical result.
        let r2 = cached_extract_source(Some(cache), "a.py", SRC).unwrap();
        assert_eq!(r1, r2);
        // The captured signature (stored in node.extra) survives the round-trip:
        // a regression guard that the cache format preserves flattened metadata.
        let f = r2
            .nodes
            .iter()
            .find(|n| n.label == "f()")
            .expect("f() node");
        let sig = f
            .signature()
            .expect("signature survives the cache round-trip");
        assert_eq!(sig.params.first().map(|p| p.name.as_str()), Some("x"));
    }

    #[test]
    fn different_path_or_content_is_a_distinct_entry() {
        assert_ne!(cache_key("a.py", SRC), cache_key("b.py", SRC));
        assert_ne!(
            cache_key("a.py", SRC),
            cache_key("a.py", b"def g(): pass\n")
        );
    }

    #[test]
    fn corrupt_cache_entry_falls_back_to_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        let file = cache_file(cache, &cache_key("a.py", SRC));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"{ not valid json").unwrap();
        // Falls back to a real extraction instead of erroring.
        let r = cached_extract_source(Some(cache), "a.py", SRC).unwrap();
        assert!(r.nodes.iter().any(|n| n.label == "f()"));
    }

    #[test]
    fn no_cache_dir_extracts_directly() {
        let r = cached_extract_source(None, "a.py", SRC).unwrap();
        assert!(r.nodes.iter().any(|n| n.label == "f()"));
    }
}

// Viability + equivalence gate for the `cache-binary` (MessagePack) format. The
// cache stores `ExtractionResult`, whose `Node`/`Edge` use `#[serde(flatten)]`
// over a `serde_json::Value` map -- both need a self-describing format, and rmp's
// default array encoding can't merge a flattened map (hence `to_vec_named`). These
// prove the encode/decode path used by `encode_result`/`decode_result` round-trips
// that shape losslessly AND decodes to the same value JSON does, so the format
// swap can't silently drop or alter data (esp. floats).
#[cfg(all(test, feature = "cache-binary"))]
mod msgpack_format_tests {
    use super::*;
    use codegraph_core::{Confidence, Edge, FileType, Node, NodeId};

    fn sample_result() -> ExtractionResult {
        let mut node_extra = serde_json::Map::new();
        node_extra.insert("kind".into(), serde_json::json!("class"));
        node_extra.insert("_origin".into(), serde_json::json!("ast"));
        // Nested object + array + a float inside the flattened extra map.
        node_extra.insert(
            "span".into(),
            serde_json::json!({"start_line": 1, "end_line": 9, "ratio": 0.3333333333333333}),
        );
        node_extra.insert("tags".into(), serde_json::json!(["a", "b", 3]));

        let node = Node {
            id: NodeId("auth".into()),
            label: "AuthService".into(),
            file_type: FileType::Code,
            source_file: "src/auth.rs".into(),
            source_location: Some("L42".into()),
            community: Some(7),
            repo: None,
            extra: node_extra,
        };

        let mut edge_extra = serde_json::Map::new();
        edge_extra.insert("note".into(), serde_json::json!("via trait"));
        let edge = Edge {
            source: NodeId("auth".into()),
            target: NodeId("db".into()),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: "src/auth.rs".into(),
            source_location: Some("L50".into()),
            confidence_score: Some(0.875),
            weight: 2.5,
            context: Some("call".into()),
            cross_repo: false,
            extra: edge_extra,
        };

        ExtractionResult {
            nodes: vec![node],
            edges: vec![edge],
            raw_calls: vec![],
            imports: vec![],
        }
    }

    #[test]
    fn msgpack_roundtrips_flatten_value_and_floats_losslessly() {
        let original = sample_result();
        // Exercise the exact functions the cache uses.
        let bytes = encode_result(&original).expect("encode");
        let back = decode_result(&bytes).expect("decode");
        assert_eq!(back, original, "MessagePack round-trip must be lossless");
    }

    #[test]
    fn msgpack_roundtrips_sql_metadata() {
        // SQL extraction stores its facts (dialect, data_type, pk/fk target, RLS
        // flags) as flattened entries in node.extra -- string values AND bools. This
        // guards the binary cache for the SQL layer specifically, not just generic
        // code metadata: a bool that decoded as a string, or a dropped flatten key,
        // would silently corrupt the SQL audit on a warm cache.
        let mut table_extra = serde_json::Map::new();
        table_extra.insert("kind".into(), serde_json::json!("table"));
        table_extra.insert("dialect".into(), serde_json::json!("sqlserver"));
        table_extra.insert("rls_enabled".into(), serde_json::json!(true));
        table_extra.insert("rls_forced".into(), serde_json::json!(false));
        let table = Node {
            id: NodeId("sql:orders".into()),
            label: "orders".into(),
            file_type: FileType::Code,
            source_file: "schema.sql".into(),
            source_location: None,
            community: Some(0),
            repo: None,
            extra: table_extra,
        };

        let mut col_extra = serde_json::Map::new();
        col_extra.insert("kind".into(), serde_json::json!("column"));
        col_extra.insert("data_type".into(), serde_json::json!("uuid"));
        col_extra.insert("pk".into(), serde_json::json!(true));
        col_extra.insert("fk_target".into(), serde_json::json!("customers"));
        let col = Node {
            id: NodeId("sql:orders:col:id".into()),
            label: "id".into(),
            file_type: FileType::Code,
            source_file: "schema.sql".into(),
            source_location: None,
            community: Some(0),
            repo: None,
            extra: col_extra,
        };

        let result = ExtractionResult {
            nodes: vec![table, col],
            edges: vec![],
            raw_calls: vec![],
            imports: vec![],
        };

        // Exercise the exact cache encode/decode path.
        let back = decode_result(&encode_result(&result).expect("encode")).expect("decode");
        assert_eq!(
            back, result,
            "SQL metadata must survive the binary cache round-trip"
        );
        // The bool flags must stay bools (not coerce to string/int) so the typed
        // accessors the SQL audit reads still see them.
        let t = back.nodes.iter().find(|n| n.label == "orders").unwrap();
        assert_eq!(t.extra.get("rls_enabled"), Some(&serde_json::json!(true)));
        assert_eq!(
            t.extra.get("dialect").and_then(|v| v.as_str()),
            Some("sqlserver")
        );
        let c = back.nodes.iter().find(|n| n.label == "id").unwrap();
        assert_eq!(c.extra.get("pk"), Some(&serde_json::json!(true)));
        assert_eq!(
            c.extra.get("data_type").and_then(|v| v.as_str()),
            Some("uuid")
        );
    }

    #[test]
    fn msgpack_and_json_produce_identical_values() {
        let original = sample_result();

        let json_bytes = serde_json::to_vec(&original).expect("json serialize");
        let from_json: ExtractionResult =
            serde_json::from_slice(&json_bytes).expect("json deserialize");

        let mp_bytes = encode_result(&original).expect("encode");
        let from_mp = decode_result(&mp_bytes).expect("decode");

        // The format swap must be observationally identical to the JSON baseline.
        assert_eq!(
            from_mp, from_json,
            "MessagePack and JSON must decode to the same value"
        );
    }
}

#[cfg(test)]
mod version_tests {
    use super::AST_CACHE_VERSION;

    #[test]
    fn version_includes_build_fingerprint() {
        // `{crate version}-{16-hex build id}`: a `-` separator with a non-empty
        // fingerprint suffix proves build.rs wired CODEGRAPH_EXTRACT_BUILD_ID in.
        let (version, build_id) = AST_CACHE_VERSION
            .rsplit_once('-')
            .expect("namespace is `version-buildid`");
        assert!(!version.is_empty(), "crate version present");
        assert_eq!(build_id.len(), 16, "16-hex build fingerprint: {build_id:?}");
        assert!(
            build_id.bytes().all(|b| b.is_ascii_hexdigit()),
            "fingerprint is hex: {build_id:?}"
        );
    }
}
