//! The cross-repo **global graph store**.
//!
//! A persistent, namespaced union of many repos' graphs, kept under a store
//! directory (default `~/.synaptic`, but injectable for tests via
//! [`GlobalStore::at`]): `global-graph.json` (the merged graph) +
//! `global-manifest.json` (per-repo bookkeeping). `add` is idempotent — a repo
//! whose source `graph.json` hash is unchanged is skipped — and re-adding a repo
//! first prunes its previous nodes so the store tracks the latest build.
//!
//! The manifest deliberately does **not** record an `added_at` timestamp
//! (cosmetic only, and time-dependent — kept out for deterministic behavior).
//! It records the source path, node/edge counts, and source hash.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use synaptic_core::{GraphData, NodeId};
use synaptic_incremental::union_graphs;

use crate::federate::{dedup_externals, prefix_graph};
use crate::{load_graph, sanitize_tag, write_graph, Result, WorkspaceError};

/// One repo's entry in the global manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalRepoEntry {
    pub source_path: String,
    pub node_count: usize,
    pub edge_count: usize,
    pub source_hash: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GlobalManifest {
    #[serde(default)]
    repos: std::collections::BTreeMap<String, GlobalRepoEntry>,
}

/// Outcome of [`GlobalStore::add`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddOutcome {
    /// Added (or replaced): `(tag, nodes_added)`.
    Added { tag: String, nodes_added: usize },
    /// Skipped because the source hash was unchanged.
    Skipped { tag: String },
}

/// A handle to a global store rooted at a directory.
pub struct GlobalStore {
    dir: PathBuf,
}

impl GlobalStore {
    /// Open (or lazily create on write) the store at `dir`.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        GlobalStore { dir: dir.into() }
    }

    /// The default store directory: `~/.synaptic` (`%USERPROFILE%\.synaptic`
    /// on Windows). Falls back to `.synaptic` in the CWD if no home is set.
    pub fn default_dir() -> PathBuf {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        match home {
            Some(h) => h.join(".synaptic"),
            None => PathBuf::from(".synaptic"),
        }
    }

    /// Path to the merged global graph.
    pub fn graph_path(&self) -> PathBuf {
        self.dir.join("global-graph.json")
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("global-manifest.json")
    }

    fn load_global(&self) -> Result<GraphData> {
        let p = self.graph_path();
        if p.exists() {
            load_graph(&p)
        } else {
            Ok(GraphData::default())
        }
    }

    fn load_manifest(&self) -> GlobalManifest {
        let p = self.manifest_path();
        match std::fs::read(&p) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(m) => m,
                Err(_) => {
                    // Corrupt manifest: back it up and start fresh.
                    let _ = std::fs::rename(&p, p.with_extension("json.corrupt"));
                    GlobalManifest::default()
                }
            },
            Err(_) => GlobalManifest::default(),
        }
    }

    fn save_manifest(&self, m: &GlobalManifest) -> Result<()> {
        let p = self.manifest_path();
        let bytes = serde_json::to_vec_pretty(m).map_err(|source| WorkspaceError::Json {
            path: p.display().to_string(),
            source,
        })?;
        std::fs::write(&p, bytes).map_err(|source| WorkspaceError::Io {
            context: format!("writing {}", p.display()),
            source,
        })
    }

    /// Add (or replace) the repo whose `graph.json` is at `source` under `tag`
    /// (sanitized). Idempotent on an unchanged source hash.
    pub fn add(&self, source: &Path, tag: &str) -> Result<AddOutcome> {
        let tag = sanitize_tag(tag);
        let bytes = std::fs::read(source).map_err(|s| WorkspaceError::Io {
            context: format!("reading {}", source.display()),
            source: s,
        })?;
        let source_hash = blake3::hash(&bytes).to_hex()[..16].to_string();

        let mut manifest = self.load_manifest();
        if let Some(existing) = manifest.repos.get(&tag) {
            if existing.source_hash == source_hash {
                return Ok(AddOutcome::Skipped { tag });
            }
        }

        let source_graph = load_graph(source)?;
        let mut global = self.load_global()?;
        prune_repo(&mut global, &tag); // replace any previous version of this repo

        let prefixed = prefix_graph(source_graph, &tag);
        // Per-repo edge count, captured before the union consumes `prefixed`.
        let repo_edge_count = prefixed.links.len();
        let global_before = global.nodes.len();
        // Union the new repo in, then collapse shared externals onto the existing
        // global externals (global is first, so its nodes win).
        let merged = dedup_externals(union_graphs(global, prefixed));
        // Net-new nodes (subtracting collapsed externals): store growth,
        // not the raw prefixed count.
        let nodes_added = merged.nodes.len().saturating_sub(global_before);

        std::fs::create_dir_all(&self.dir).map_err(|s| WorkspaceError::Io {
            context: format!("creating {}", self.dir.display()),
            source: s,
        })?;
        write_graph(&self.graph_path(), &merged)?;

        manifest.repos.insert(
            tag.clone(),
            GlobalRepoEntry {
                source_path: source.display().to_string(),
                node_count: nodes_added,
                edge_count: repo_edge_count,
                source_hash,
            },
        );
        self.save_manifest(&manifest)?;
        Ok(AddOutcome::Added { tag, nodes_added })
    }

    /// Remove a repo's nodes from the store. Returns how many nodes were pruned.
    pub fn remove(&self, tag: &str) -> Result<usize> {
        let tag = sanitize_tag(tag);
        let mut global = self.load_global()?;
        let removed = prune_repo(&mut global, &tag);
        write_graph(&self.graph_path(), &global)?;
        let mut manifest = self.load_manifest();
        manifest.repos.remove(&tag);
        self.save_manifest(&manifest)?;
        Ok(removed)
    }

    /// List the repos currently in the store (tag → entry), sorted by tag.
    pub fn list(&self) -> Vec<(String, GlobalRepoEntry)> {
        self.load_manifest().repos.into_iter().collect()
    }
}

/// Remove every node tagged `repo == tag` (and edges/hyperedges touching them).
/// Returns the number of nodes removed.
fn prune_repo(g: &mut GraphData, tag: &str) -> usize {
    let removed: HashSet<NodeId> = g
        .nodes
        .iter()
        .filter(|n| n.repo.as_deref() == Some(tag))
        .map(|n| n.id.clone())
        .collect();
    if removed.is_empty() {
        return 0;
    }
    g.nodes.retain(|n| !removed.contains(&n.id));
    g.links
        .retain(|e| !removed.contains(&e.source) && !removed.contains(&e.target));
    g.hyperedges
        .retain(|h| !h.nodes.iter().any(|m| removed.contains(m)));
    removed.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{FileType, Node};

    fn node(id: &str, label: &str, source_file: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: source_file.into(),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    fn write_source(dir: &Path, name: &str, nodes: Vec<Node>) -> PathBuf {
        let g = GraphData {
            nodes,
            ..Default::default()
        };
        let p = dir.join(name);
        std::fs::write(&p, serde_json::to_vec(&g).unwrap()).unwrap();
        p
    }

    #[test]
    fn add_namespaces_and_lists() {
        let d = tempfile::tempdir().unwrap();
        let store = GlobalStore::at(d.path().join("store"));
        let src = write_source(d.path(), "a.json", vec![node("main", "main", "a.rs")]);
        let out = store.add(&src, "billing").unwrap();
        assert_eq!(
            out,
            AddOutcome::Added {
                tag: "billing".into(),
                nodes_added: 1
            }
        );
        let g: GraphData =
            serde_json::from_slice(&std::fs::read(store.graph_path()).unwrap()).unwrap();
        assert_eq!(g.nodes[0].id.0, "billing::main");
        assert_eq!(g.nodes[0].repo.as_deref(), Some("billing"));
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "billing");
    }

    #[test]
    fn add_is_idempotent_on_unchanged_hash() {
        let d = tempfile::tempdir().unwrap();
        let store = GlobalStore::at(d.path().join("store"));
        let src = write_source(d.path(), "a.json", vec![node("main", "main", "a.rs")]);
        assert!(matches!(
            store.add(&src, "x").unwrap(),
            AddOutcome::Added { .. }
        ));
        assert_eq!(
            store.add(&src, "x").unwrap(),
            AddOutcome::Skipped { tag: "x".into() }
        );
    }

    #[test]
    fn readd_replaces_previous_version() {
        let d = tempfile::tempdir().unwrap();
        let store = GlobalStore::at(d.path().join("store"));
        let src = write_source(d.path(), "a.json", vec![node("main", "main", "a.rs")]);
        store.add(&src, "x").unwrap();
        // New source with a different node set (and thus different hash).
        let src2 = write_source(
            d.path(),
            "a.json",
            vec![node("main", "main", "a.rs"), node("extra", "extra", "b.rs")],
        );
        store.add(&src2, "x").unwrap();
        let g: GraphData =
            serde_json::from_slice(&std::fs::read(store.graph_path()).unwrap()).unwrap();
        assert_eq!(g.nodes.len(), 2, "old version pruned, new one present");
        let ids: Vec<&str> = g.nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(ids.contains(&"x::extra"));
    }

    #[test]
    fn remove_prunes_repo_nodes() {
        let d = tempfile::tempdir().unwrap();
        let store = GlobalStore::at(d.path().join("store"));
        let a = write_source(d.path(), "a.json", vec![node("m", "m", "a.rs")]);
        let b = write_source(d.path(), "b.json", vec![node("m", "m", "b.rs")]);
        store.add(&a, "ra").unwrap();
        store.add(&b, "rb").unwrap();
        let removed = store.remove("ra").unwrap();
        assert_eq!(removed, 1);
        let g: GraphData =
            serde_json::from_slice(&std::fs::read(store.graph_path()).unwrap()).unwrap();
        assert!(g.nodes.iter().all(|n| n.repo.as_deref() == Some("rb")));
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn shared_externals_collapse_across_repos() {
        let d = tempfile::tempdir().unwrap();
        let store = GlobalStore::at(d.path().join("store"));
        let a = write_source(
            d.path(),
            "a.json",
            vec![node("lib", "lib", "a.rs"), node("ext", "serde", "")],
        );
        let b = write_source(
            d.path(),
            "b.json",
            vec![node("app", "app", "b.rs"), node("ext", "serde", "")],
        );
        store.add(&a, "ra").unwrap();
        store.add(&b, "rb").unwrap();
        let g: GraphData =
            serde_json::from_slice(&std::fs::read(store.graph_path()).unwrap()).unwrap();
        assert_eq!(
            g.nodes.iter().filter(|n| n.label == "serde").count(),
            1,
            "one shared external across repos"
        );
    }
}
