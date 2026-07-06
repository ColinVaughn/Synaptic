//! The store manifest: `<root>/manifest.json` listing every shard, written
//! atomically (tmp + rename) and validated against what is actually on disk
//! before the store is trusted.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::shard::SCHEMA_VERSION;
use crate::StoreError;

const MANIFEST_FILE: &str = "manifest.json";
const MANIFEST_TMP: &str = "manifest.json.tmp";

/// One shard's bookkeeping. `file` is relative to the store root so the manifest
/// stays portable if the directory moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardEntry {
    /// Federation tag (e.g. `billing`, or `local` for the repo-less remainder).
    pub tag: String,
    /// Shard filename relative to the store root (e.g. `billing.redb`).
    pub file: String,
    /// Content hash of the shard; the cache key for persisted index blobs and
    /// the "skip unchanged" incremental check.
    pub source_hash: String,
    pub node_count: u64,
    pub edge_count: u64,
    pub directed: bool,
}

/// The full set of shards in a store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardManifest {
    pub schema_version: u32,
    pub shards: Vec<ShardEntry>,
    /// The cross-repo bridge shard (edges spanning two repos), stored apart from
    /// the per-repo `shards` so it is never listed/materialized as a repo and is
    /// only traversed on an opt-in cross-repo query. Absent when there are none.
    #[serde(default)]
    pub bridge: Option<ShardEntry>,
}

impl ShardManifest {
    /// An empty manifest at the current schema version.
    pub fn empty() -> ShardManifest {
        ShardManifest {
            schema_version: SCHEMA_VERSION,
            shards: Vec::new(),
            bridge: None,
        }
    }

    /// Load the manifest from `root`. A missing file is treated as an empty
    /// store at the current schema (so opening a fresh directory just works).
    pub fn load(root: &Path) -> Result<ShardManifest, StoreError> {
        let path = root.join(MANIFEST_FILE);
        if !path.exists() {
            return Ok(ShardManifest::empty());
        }
        let text = std::fs::read_to_string(&path)?;
        serde_json::from_str(&text).map_err(|e| StoreError::Manifest(e.to_string()))
    }

    /// Atomically write the manifest to `root/manifest.json`: serialize to a
    /// sibling `.tmp`, flush+fsync it, then rename over the target. A crash
    /// mid-write leaves the previous manifest intact, never a torn file.
    pub fn save(&self, root: &Path) -> Result<(), StoreError> {
        std::fs::create_dir_all(root)?;
        let tmp = root.join(MANIFEST_TMP);
        let text =
            serde_json::to_string_pretty(self).map_err(|e| StoreError::Manifest(e.to_string()))?;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, root.join(MANIFEST_FILE))?;
        Ok(())
    }

    /// Reject a store we should not trust: a schema mismatch (an old/newer store
    /// that must be re-migrated) or an entry pointing at a shard file that is not
    /// on disk. A clear error here prevents a misread or a confident-but-wrong
    /// query against a corrupt store.
    pub fn validate(&self, root: &Path) -> Result<(), StoreError> {
        self.validate_with(
            root,
            synaptic_core::max_shard_nodes(),
            synaptic_core::max_shard_bytes(),
        )
    }

    /// Validation core with explicit per-shard caps (parameterized so the caps
    /// can be unit-tested without multi-GB fixtures). Rejects a schema mismatch,
    /// a missing shard file, or a shard whose declared node count / on-disk size
    /// exceeds the cap — none of which we should trust enough to materialize.
    pub(crate) fn validate_with(
        &self,
        root: &Path,
        max_nodes: u64,
        max_bytes: u64,
    ) -> Result<(), StoreError> {
        // The recorded version is the minimum reader the store needs: shard
        // files self-describe their encoding, so an older store still opens
        // here (legacy v1 redb shards fail at read time with a rebuild hint);
        // only a NEWER store than this binary understands is rejected.
        if self.schema_version > SCHEMA_VERSION || self.schema_version == 0 {
            return Err(StoreError::Manifest(format!(
                "store schema {} is newer than this binary supports ({}); upgrade synaptic or re-run `synaptic migrate`",
                self.schema_version, SCHEMA_VERSION
            )));
        }
        for e in self.shards.iter().chain(self.bridge.iter()) {
            let meta = std::fs::metadata(root.join(&e.file)).map_err(|_| {
                StoreError::Manifest(format!(
                    "shard {:?} references missing file {:?}",
                    e.tag, e.file
                ))
            })?;
            if e.node_count > max_nodes {
                return Err(StoreError::Manifest(format!(
                    "shard {:?} declares {} nodes, over the per-shard cap {} \n                     (set SYNAPTIC_MAX_SHARD_NODES to raise it; 0 = no cap)",
                    e.tag, e.node_count, max_nodes
                )));
            }
            if meta.len() > max_bytes {
                return Err(StoreError::Manifest(format!(
                    "shard {:?} file is {} bytes, over the per-shard cap {} \n                     (set SYNAPTIC_MAX_SHARD_MB to raise it; 0 = no cap)",
                    e.tag,
                    meta.len(),
                    max_bytes
                )));
            }
        }
        Ok(())
    }

    /// The entry for `tag`, if present.
    pub fn entry(&self, tag: &str) -> Option<&ShardEntry> {
        self.shards.iter().find(|e| e.tag == tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_with_rejects_shard_over_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        // A 4-byte shard file with a 1-byte per-shard byte cap -> rejected.
        std::fs::write(dir.path().join("x.redb"), b"abcd").unwrap();
        let m = ShardManifest {
            schema_version: SCHEMA_VERSION,
            shards: vec![ShardEntry {
                tag: "x".into(),
                file: "x.redb".into(),
                source_hash: "h".into(),
                node_count: 1,
                edge_count: 0,
                directed: true,
            }],
            bridge: None,
        };
        assert!(m.validate_with(dir.path(), u64::MAX, 1).is_err());
        // generous caps -> the same shard passes
        assert!(m.validate_with(dir.path(), u64::MAX, u64::MAX).is_ok());
    }
}
