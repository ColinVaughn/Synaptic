//! `ShardStore`: the per-repo shard collection rooted at a directory.
//!
//! Each shard is one flat container file named `<tag>.<hash>.shard` (older
//! stores hold v1 `.redb` files, still readable); the manifest records
//! which file is current. Writing a new version creates a new file and flips the
//! manifest (RCU), then best-effort deletes the old file. Reads materialize a
//! shard fully into RAM and drop the redb handle, so no long-lived handle blocks
//! a later rewrite (the Windows replace-open-file pitfall).

use std::path::{Path, PathBuf};

use synaptic_core::{Edge, GraphData};
use synaptic_graph::KnowledgeGraph;

use crate::manifest::{ShardEntry, ShardManifest};
use crate::{codec, sanitize_tag, shard, Scope, StoreError};

/// Manifest tag recorded for the cross-repo bridge entry (stored apart from the
/// per-repo shards, so it never appears in `list_shards`).
const BRIDGE_TAG: &str = "__bridge__";
/// Filename stem for the bridge shard file.
const BRIDGE_STEM: &str = "bridge";

/// A directory-rooted collection of per-repo shards.
pub struct ShardStore {
    root: PathBuf,
    manifest: ShardManifest,
}

impl ShardStore {
    /// Open (or initialize) a store rooted at `root`. A missing directory or
    /// manifest is treated as an empty store. The manifest is validated against
    /// disk, so a schema mismatch or a missing shard file fails loudly here.
    pub fn open(root: &Path) -> Result<ShardStore, StoreError> {
        let manifest = ShardManifest::load(root)?;
        manifest.validate(root)?;
        Ok(ShardStore {
            root: root.to_path_buf(),
            manifest,
        })
    }

    /// The shard entries, in stable (tag-sorted) order.
    pub fn list_shards(&self) -> &[ShardEntry] {
        &self.manifest.shards
    }

    /// Read access to the manifest (entry lookup, schema, etc.).
    pub fn manifest(&self) -> &ShardManifest {
        &self.manifest
    }

    /// Filesystem-safe, content-versioned filename for a shard.
    fn shard_file(stem: &str, source_hash: &str) -> String {
        let mut hpart: String = source_hash
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(16)
            .collect();
        if hpart.is_empty() {
            hpart.push('0');
        }
        format!("{stem}.{hpart}.shard")
    }

    /// Write `gd` as the shard for `tag` with content hash `source_hash`.
    ///
    /// Writes a fresh versioned file, then flips the manifest to point at it, then
    /// best-effort removes the prior version. The shard file lands before the
    /// manifest, so a crash never leaves the manifest pointing at a missing file.
    pub fn write_shard(
        &mut self,
        tag: &str,
        gd: &GraphData,
        source_hash: &str,
    ) -> Result<(), StoreError> {
        self.write_shard_with_blobs(tag, gd, source_hash, &[])
    }

    /// [`write_shard`](Self::write_shard) plus pre-built index blobs, landing in
    /// the shard's single write pass (no post-hoc reopen of the new file).
    pub fn write_shard_with_blobs(
        &mut self,
        tag: &str,
        gd: &GraphData,
        source_hash: &str,
        blobs: &[(&str, &[u8])],
    ) -> Result<(), StoreError> {
        let stem = sanitize_tag(tag)?;
        let file = self.write_versioned(&stem, source_hash, gd, blobs)?;

        // Flip the manifest to the new file (and remember any old file to GC).
        let old_file = self
            .manifest
            .entry(tag)
            .map(|e| e.file.clone())
            .filter(|f| f != &file);
        self.manifest.shards.retain(|e| e.tag != tag);
        self.manifest.shards.push(ShardEntry {
            tag: tag.to_string(),
            file,
            source_hash: source_hash.to_string(),
            node_count: gd.nodes.len() as u64,
            edge_count: gd.links.len() as u64,
            directed: gd.directed,
        });
        self.manifest.shards.sort_by(|a, b| a.tag.cmp(&b.tag));
        // A freshly written shard uses the current encoding, so the store now
        // requires a reader at least this new (older binaries must refuse it).
        self.manifest.schema_version = shard::SCHEMA_VERSION;
        self.manifest.save(&self.root)?;

        // Old version is now unreferenced; remove it best-effort (a reader still
        // holding it open on Windows will keep it until it drops).
        if let Some(old) = old_file {
            let _ = std::fs::remove_file(self.root.join(old));
        }
        Ok(())
    }

    /// Write `gd` to a fresh content-versioned `<stem>.<hash>.shard` and return
    /// its filename. Temp-write then rename; the final name is content-versioned
    /// so it is not an in-use file (the RCU swap). Single-writer-per-stem is
    /// assumed.
    fn write_versioned(
        &self,
        stem: &str,
        source_hash: &str,
        gd: &GraphData,
        blobs: &[(&str, &[u8])],
    ) -> Result<String, StoreError> {
        std::fs::create_dir_all(&self.root)?;
        let file = Self::shard_file(stem, source_hash);
        let final_path = self.root.join(&file);
        let tmp = self.root.join(format!("{stem}.writing.tmp"));
        shard::write_with_blobs(&tmp, gd, source_hash, blobs)?;
        if final_path.exists() {
            std::fs::remove_file(&final_path)?;
        }
        std::fs::rename(&tmp, &final_path)?;
        Ok(file)
    }

    /// Store the cross-repo bridge (edges spanning two repos). Replaces any prior
    /// bridge; an empty edge set clears it. Skips the write when the content is
    /// unchanged. The bridge is kept out of `shards`, so it is never listed or
    /// materialized as a repo — only `export_cross_repo` grafts it back.
    pub fn write_bridge(&mut self, edges: &[Edge], directed: bool) -> Result<(), StoreError> {
        if edges.is_empty() {
            if let Some(old) = self.manifest.bridge.take() {
                let _ = std::fs::remove_file(self.root.join(old.file));
                self.manifest.save(&self.root)?;
            }
            return Ok(());
        }
        let hash = bridge_hash(edges);
        if self
            .manifest
            .bridge
            .as_ref()
            .is_some_and(|e| e.source_hash == hash)
        {
            return Ok(()); // unchanged
        }
        let gd = GraphData {
            directed,
            links: edges.to_vec(),
            ..GraphData::default()
        };
        let file = self.write_versioned(BRIDGE_STEM, &hash, &gd, &[])?;
        let old = self
            .manifest
            .bridge
            .replace(ShardEntry {
                tag: BRIDGE_TAG.to_string(),
                file: file.clone(),
                source_hash: hash,
                node_count: 0,
                edge_count: edges.len() as u64,
                directed,
            })
            .map(|e| e.file)
            .filter(|f| f != &file);
        // The fresh bridge file uses the current encoding (see write_shard).
        self.manifest.schema_version = shard::SCHEMA_VERSION;
        self.manifest.save(&self.root)?;
        if let Some(o) = old {
            let _ = std::fs::remove_file(self.root.join(o));
        }
        Ok(())
    }

    /// Number of cross-repo bridge edges (0 if none), from the manifest — no I/O.
    pub fn bridge_edge_count(&self) -> u64 {
        self.manifest.bridge.as_ref().map_or(0, |e| e.edge_count)
    }

    /// Per-shard byte/row breakdown (bridge included, tagged `bridge`), for the
    /// `store_report` example and size regressions.
    pub fn stats(&self) -> Result<Vec<(String, shard::ShardStats)>, StoreError> {
        let mut out = Vec::new();
        for e in &self.manifest.shards {
            out.push((e.tag.clone(), shard::shard_stats(&self.root.join(&e.file))?));
        }
        if let Some(b) = &self.manifest.bridge {
            out.push((b.tag.clone(), shard::shard_stats(&self.root.join(&b.file))?));
        }
        Ok(out)
    }

    /// The cross-repo bridge edges (empty if there is no bridge).
    pub fn read_bridge_edges(&self) -> Result<Vec<Edge>, StoreError> {
        match &self.manifest.bridge {
            Some(e) => Ok(shard::read_graph_data(&self.root.join(&e.file))?.links),
            None => Ok(Vec::new()),
        }
    }

    /// Materialize every repo shard **plus** the cross-repo bridge into one graph
    /// (the unified view, the default for federated queries when bridge edges
    /// exist). [`export_graph`](Self::export_graph) is the bridge-less export.
    pub fn export_cross_repo(&self) -> Result<KnowledgeGraph, StoreError> {
        let mut union = GraphData::default();
        for (i, e) in self.manifest.shards.iter().enumerate() {
            let gd = shard::read_graph_data(&self.root.join(&e.file))?;
            if i == 0 {
                union.directed = gd.directed;
                union.multigraph = gd.multigraph;
                union.built_at_commit = gd.built_at_commit.clone();
            }
            union.nodes.extend(gd.nodes);
            union.links.extend(gd.links);
            union.hyperedges.extend(gd.hyperedges);
        }
        union.links.extend(self.read_bridge_edges()?);
        Ok(KnowledgeGraph::from_graph_data(union))
    }

    /// Remove a shard: delete its file and drop its manifest entry.
    pub fn prune_shard(&mut self, tag: &str) -> Result<(), StoreError> {
        if let Some(pos) = self.manifest.shards.iter().position(|e| e.tag == tag) {
            let file = self.manifest.shards[pos].file.clone();
            self.manifest.shards.remove(pos);
            self.manifest.save(&self.root)?;
            let _ = std::fs::remove_file(self.root.join(file));
        }
        Ok(())
    }

    /// Read a shard back into a `GraphData`.
    pub fn read_graph_data(&self, tag: &str) -> Result<GraphData, StoreError> {
        shard::read_graph_data(&self.shard_path(tag)?)
    }

    /// Materialize a shard into the in-memory [`KnowledgeGraph`] every query uses.
    pub fn materialize(&self, tag: &str) -> Result<KnowledgeGraph, StoreError> {
        shard::materialize(&self.shard_path(tag)?)
    }

    /// Materialize a scope into one [`KnowledgeGraph`].
    ///
    /// `Scope::Repo` is exactly that shard. `Scope::All` with a single shard is
    /// that shard (so a single-repo store round-trips byte-identically); with
    /// several shards it unions them in tag order. The cross-repo bridge is not
    /// included here — this export is per-repo content only (see
    /// [`export_cross_repo`](Self::export_cross_repo) for the grafted view).
    pub fn export_graph(&self, scope: &Scope) -> Result<KnowledgeGraph, StoreError> {
        match scope {
            Scope::Repo(tag) => self.materialize(tag),
            Scope::All => {
                let tags: Vec<String> =
                    self.manifest.shards.iter().map(|e| e.tag.clone()).collect();
                match tags.as_slice() {
                    [] => Ok(KnowledgeGraph::from_graph_data(GraphData::default())),
                    [only] => self.materialize(only),
                    _ => {
                        let mut union = GraphData::default();
                        for (i, t) in tags.iter().enumerate() {
                            let gd = self.read_graph_data(t)?;
                            if i == 0 {
                                union.directed = gd.directed;
                                union.multigraph = gd.multigraph;
                                union.built_at_commit = gd.built_at_commit.clone();
                            }
                            union.nodes.extend(gd.nodes);
                            union.links.extend(gd.links);
                            union.hyperedges.extend(gd.hyperedges);
                        }
                        Ok(KnowledgeGraph::from_graph_data(union))
                    }
                }
            }
        }
    }

    /// Store a producer-owned index blob (e.g. a serialized `QueryIndex`) for a
    /// shard, keyed by index name + the shard's content hash.
    pub fn put_index_blob(
        &self,
        tag: &str,
        name: &str,
        source_hash: &str,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        shard::put_index_blob(&self.shard_path(tag)?, name, source_hash, bytes)
    }

    /// Store several index blobs for one shard in a single file rewrite
    /// (the lazy persistence path; blobs built at write time ride along in
    /// [`write_shard_with_blobs`](Self::write_shard_with_blobs) instead).
    pub fn put_index_blobs(
        &self,
        tag: &str,
        source_hash: &str,
        entries: &[(&str, &[u8])],
    ) -> Result<(), StoreError> {
        shard::put_index_blobs(&self.shard_path(tag)?, source_hash, entries)
    }

    /// Fetch a persisted index blob, or `None` if absent/stale (hash mismatch).
    pub fn get_index_blob(
        &self,
        tag: &str,
        name: &str,
        source_hash: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        shard::get_index_blob(&self.shard_path(tag)?, name, source_hash)
    }

    /// Whether a blob exists for `(name, source_hash)` — no read or inflate.
    pub fn has_index_blob(
        &self,
        tag: &str,
        name: &str,
        source_hash: &str,
    ) -> Result<bool, StoreError> {
        shard::has_index_blob(&self.shard_path(tag)?, name, source_hash)
    }

    fn shard_path(&self, tag: &str) -> Result<PathBuf, StoreError> {
        let entry = self
            .manifest
            .entry(tag)
            .ok_or_else(|| StoreError::Manifest(format!("no shard for tag {tag:?}")))?;
        Ok(self.root.join(&entry.file))
    }
}

/// Content hash of the bridge edge set (order-independent), so an unchanged
/// bridge is skipped on re-migrate.
fn bridge_hash(edges: &[Edge]) -> String {
    let keys: Vec<String> = edges
        .iter()
        .map(|e| format!("{}>{}:{}", e.source.0, e.target.0, e.relation))
        .collect();
    codec::source_hash(&[], &keys)
}
