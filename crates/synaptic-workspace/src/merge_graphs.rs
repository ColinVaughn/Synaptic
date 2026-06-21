//! `synaptic merge-graphs` — compose several `graph.json` files into one
//! namespaced graph.
//!
//! Each input's repo tag is derived from its grandparent directory name (so
//! `<repo>/synaptic-out/graph.json` → `<repo>`). Inputs are prefixed and
//! composed verbatim — **no** external dedup, unlike the global store.

use std::path::{Path, PathBuf};

use crate::federate::compose_no_dedup;
use crate::{load_graph, sanitize_tag, write_graph, Result};

/// What `merge_graph_files` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeReport {
    pub out: PathBuf,
    pub tags: Vec<String>,
    pub node_count: usize,
    pub edge_count: usize,
}

/// Derive a repo tag for a `graph.json` path: the grandparent directory name
/// (`<repo>/synaptic-out/graph.json` → `repo`), falling back to the file stem,
/// then `repo`. Always sanitized so it is namespacing-safe.
pub fn tag_for(path: &Path) -> String {
    let raw = path
        .parent()
        .and_then(Path::parent)
        .and_then(|g| g.file_name())
        .or_else(|| path.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    sanitize_tag(&raw)
}

/// Compose `paths` into one namespaced graph written to `out`.
pub fn merge_graph_files(paths: &[PathBuf], out: &Path) -> Result<MergeReport> {
    let mut subgraphs = Vec::with_capacity(paths.len());
    let mut tags = Vec::with_capacity(paths.len());
    // Disambiguate repeated tags so two inputs from same-named dirs don't collide.
    let mut counts = std::collections::HashMap::new();
    for p in paths {
        let base = tag_for(p);
        let n = counts.entry(base.clone()).or_insert(0usize);
        let tag = if *n == 0 {
            base.clone()
        } else {
            format!("{base}-{}", *n + 1)
        };
        *n += 1;
        let g = load_graph(p)?;
        subgraphs.push((tag.clone(), g));
        tags.push(tag);
    }
    let merged = compose_no_dedup(subgraphs);
    write_graph(out, &merged)?;
    Ok(MergeReport {
        out: out.to_path_buf(),
        tags,
        node_count: merged.nodes.len(),
        edge_count: merged.links.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{FileType, GraphData, Node, NodeId};

    fn node(id: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: id.into(),
            file_type: FileType::Code,
            source_file: format!("{id}.rs"),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    fn write_graph_at(path: &Path, ids: &[&str]) {
        let g = GraphData {
            nodes: ids.iter().map(|i| node(i)).collect(),
            ..Default::default()
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&g).unwrap()).unwrap();
    }

    #[test]
    fn tag_is_the_grandparent_dir() {
        assert_eq!(
            tag_for(Path::new("/x/billing/synaptic-out/graph.json")),
            "billing"
        );
        // No grandparent: fall back to file stem.
        assert_eq!(tag_for(Path::new("only.json")), "only");
    }

    #[test]
    fn merges_two_graphs_with_namespaced_ids() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("billing/synaptic-out/graph.json");
        let b = d.path().join("identity/synaptic-out/graph.json");
        write_graph_at(&a, &["main", "Ledger"]);
        write_graph_at(&b, &["main", "User"]);
        let out = d.path().join("merged.json");

        let report = merge_graph_files(&[a, b], &out).unwrap();
        assert_eq!(report.tags, vec!["billing", "identity"]);
        let merged: GraphData = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        let ids: Vec<&str> = merged.nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(ids.contains(&"billing::main"), "{ids:?}");
        assert!(ids.contains(&"identity::main"), "{ids:?}");
        assert!(ids.contains(&"billing::Ledger") && ids.contains(&"identity::User"));
        assert_eq!(merged.nodes.len(), 4, "no collision across repos");
    }
}
