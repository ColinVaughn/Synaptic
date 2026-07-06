//! `common` command(s) split from main.rs.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use synaptic_core::{GraphData, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_query::{QueryIndex, ReverseImpactIndex, DEFAULT_AFFECTED_RELATIONS};
use synaptic_server::{PreparedIndexes, Server};
use synaptic_store::{Scope, ShardStore, AFFECTED_INDEX_BLOB, QUERY_INDEX_BLOB};

/// Which on-disk representation read commands load from. `Json` is today's
/// single `graph.json`; `Sharded` is the per-repo shard store. Selected by the
/// `SYNAPTIC_STORE` env var (default `json`) so every read command switches
/// uniformly without a per-command flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreBackend {
    Json,
    Sharded,
}

/// Resolve the backend for a given graph + store location.
///
/// `SYNAPTIC_STORE=redb`/`json` forces a backend. Unset is **auto**: prefer the
/// shard store, but only when it exists and is at least as fresh as `graph.json`
/// (a store older than the graph would serve stale results). Otherwise fall back
/// to json. So a graph.json-only user is unchanged, a user who ran `synaptic
/// migrate` gets the store automatically, and a re-extract without re-migrate
/// safely falls back to json rather than serving a stale store.
pub(crate) fn resolve_backend(graph_path: &Path, store_dir: &Path) -> StoreBackend {
    match std::env::var("SYNAPTIC_STORE").ok().as_deref() {
        Some("redb") => StoreBackend::Sharded,
        Some("json") => StoreBackend::Json,
        _ => {
            if store_is_fresh(store_dir, graph_path) {
                StoreBackend::Sharded
            } else {
                StoreBackend::Json
            }
        }
    }
}

/// True when a usable, non-stale store exists: its `manifest.json` is present and
/// at least as new as `graph.json` (or `graph.json` is absent, making the store
/// the only source).
fn store_is_fresh(store_dir: &Path, graph_path: &Path) -> bool {
    let Ok(store_meta) = std::fs::metadata(store_dir.join("manifest.json")) else {
        return false;
    };
    match std::fs::metadata(graph_path) {
        // No graph.json -> the store is the only source; use it.
        Err(_) => true,
        Ok(graph_meta) => match (store_meta.modified(), graph_meta.modified()) {
            (Ok(s), Ok(g)) => s >= g,
            _ => false,
        },
    }
}

/// The shard store directory for a given `graph.json` path: a `store/` sibling
/// of the graph (i.e. `synaptic-out/store`).
pub(crate) fn store_dir_for(graph_path: &Path) -> PathBuf {
    graph_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("store")
}

/// Load `GraphData` for a scope from an explicit backend. The pure core of the
/// read path: `Json` parses `graph.json` (optionally repo-filtered, exactly as
/// today); `Sharded` materializes the scope from the shard store.
pub(crate) fn load_graph_data_backend(
    backend: StoreBackend,
    graph_path: &Path,
    store_dir: &Path,
    repo: Option<&str>,
) -> Result<GraphData> {
    match backend {
        StoreBackend::Json => {
            let text = fs::read_to_string(graph_path).with_context(|| {
                format!(
                    "reading {} (run `synaptic extract` first?)",
                    graph_path.display()
                )
            })?;
            let gd: GraphData = serde_json::from_str(&text).context("parsing graph.json")?;
            match repo {
                Some(r) => Ok(synaptic_workspace::repo_scope::filter_repo(&gd, r)),
                None => Ok(gd),
            }
        }
        StoreBackend::Sharded => {
            let store = ShardStore::open(store_dir)
                .with_context(|| format!("opening shard store at {}", store_dir.display()))?;
            // An unscoped query grafts the cross-repo bridge by default when the
            // store has bridge edges; SYNAPTIC_CROSS_REPO=0 isolates per repo.
            // Both notes go to stderr so machine-read stdout stays clean.
            let kg = match repo {
                Some(r) => store
                    .export_graph(&Scope::Repo(r.to_string()))
                    .with_context(|| format!("materializing repo {r:?} from the shard store"))?,
                None => {
                    let bridge = store.bridge_edge_count();
                    if synaptic_store::cross_repo_mode().resolve(bridge > 0) {
                        if bridge > 0 {
                            eprintln!(
                                "[synaptic] including {bridge} cross-repo edge(s); set \
                                 SYNAPTIC_CROSS_REPO=0 to isolate per repo"
                            );
                        }
                        store
                            .export_cross_repo()
                            .context("materializing all repos + cross-repo bridge")?
                    } else {
                        if bridge > 0 {
                            eprintln!(
                                "[synaptic] {bridge} cross-repo edge(s) not traversed \
                                 (SYNAPTIC_CROSS_REPO=0)"
                            );
                        }
                        store
                            .export_graph(&Scope::All)
                            .context("materializing all repos from the shard store")?
                    }
                }
            };
            Ok(kg.to_graph_data())
        }
    }
}

/// Load `GraphData` for a scope, resolving the backend (env override or auto).
pub(crate) fn load_graph_data(graph_path: &Path, repo: Option<&str>) -> Result<GraphData> {
    let store_dir = store_dir_for(graph_path);
    load_graph_data_backend(
        resolve_backend(graph_path, &store_dir),
        graph_path,
        &store_dir,
        repo,
    )
}

/// Build (or incrementally update) the sharded on-disk store from a graph, then
/// pre-build its per-shard indexes. Shared by `synaptic migrate` and the
/// `--store` build flag so the two never drift.
pub(crate) fn write_store(
    gd: &GraphData,
    store_dir: &Path,
) -> Result<synaptic_store::migrate::MigrateReport> {
    let mut store = ShardStore::open(store_dir)
        .with_context(|| format!("opening shard store at {}", store_dir.display()))?;
    // Binary shard files must never ride into a commit when synaptic-out is
    // tracked (the merge-driver workflow commits graph.json). Best-effort;
    // the dir may not exist yet (shard writes create it lazily).
    let gitignore = store_dir.join(".gitignore");
    if !gitignore.exists() {
        let _ = std::fs::create_dir_all(store_dir);
        let _ = std::fs::write(&gitignore, "*\n");
    }
    // Indexes are built from the in-memory shard split and land inside each
    // shard's single write pass; the old flow re-read every fresh shard from
    // disk just to index it, then reopened the file twice more for the blobs.
    let report = synaptic_store::migrate::migrate_into_indexed(&mut store, gd, |_tag, shard_gd| {
        let kg = synaptic_graph::KnowledgeGraph::from_graph_data(shard_gd.clone());
        let codec_err = |e: String| synaptic_store::StoreError::Codec(e);
        let qi = QueryIndex::build(&kg)
            .to_bytes()
            .map_err(|e| codec_err(e.to_string()))?;
        let ai = ReverseImpactIndex::build(&kg, DEFAULT_AFFECTED_RELATIONS)
            .to_bytes()
            .map_err(|e| codec_err(e.to_string()))?;
        Ok(vec![
            (QUERY_INDEX_BLOB.to_string(), qi),
            (AFFECTED_INDEX_BLOB.to_string(), ai),
        ])
    })
    .context("writing shards")?;
    // Safety net for stores written by other tools/versions: fill any missing
    // blobs (fast no-op when the write above already carried them).
    persist_shard_indexes(&store).context("persisting shard indexes")?;
    Ok(report)
}

/// Build + persist each shard's derived indexes (query + reverse-impact) so a
/// later `serve` deserializes them instead of rebuilding (H1). Runs at migrate
/// time, where the cost is paid once rather than on every server start.
pub(crate) fn persist_shard_indexes(store: &ShardStore) -> Result<()> {
    let tags: Vec<(String, String)> = store
        .list_shards()
        .iter()
        .map(|e| (e.tag.clone(), e.source_hash.clone()))
        .collect();
    for (tag, hash) in tags {
        // Incremental: indexes already persisted for this exact content are
        // reused. Existence only — fetching would inflate megabytes per no-op.
        if store.has_index_blob(&tag, QUERY_INDEX_BLOB, &hash)?
            && store.has_index_blob(&tag, AFFECTED_INDEX_BLOB, &hash)?
        {
            continue;
        }
        let kg = store.materialize(&tag)?;
        let qi = QueryIndex::build(&kg)
            .to_bytes()
            .context("serializing query index")?;
        let ai = ReverseImpactIndex::build(&kg, DEFAULT_AFFECTED_RELATIONS)
            .to_bytes()
            .context("serializing affected index")?;
        store.put_index_blobs(
            &tag,
            &hash,
            &[(QUERY_INDEX_BLOB, &qi), (AFFECTED_INDEX_BLOB, &ai)],
        )?;
    }
    Ok(())
}

/// Construct the MCP server honoring the `SYNAPTIC_STORE` backend. The sharded path
/// loads a single-repo store's persisted indexes (building + persisting any that
/// are missing), so startup deserializes instead of rebuilding.
pub(crate) fn build_server(path: &Path) -> Result<Server> {
    let store_dir = store_dir_for(path);
    match resolve_backend(path, &store_dir) {
        StoreBackend::Json => {
            let gd = load_graph_data_backend(StoreBackend::Json, path, &store_dir, None)?;
            Ok(Server::from_graph_data(gd, Some(path.to_path_buf())))
        }
        StoreBackend::Sharded => {
            let store = ShardStore::open(&store_dir)
                .with_context(|| format!("opening shard store at {}", store_dir.display()))?;
            // Federated (multi-shard) store: serve shard-aware. Shards load on
            // demand behind the LRU and every tool fans out, so the union is
            // never materialized in RAM.
            if store.list_shards().len() > 1 {
                return Ok(Server::from_shard_store(store, Some(path.to_path_buf())));
            }
            let gd = load_graph_data_backend(StoreBackend::Sharded, path, &store_dir, None)?;

            // Single-repo store: load (or rebuild + persist) that shard's indexes.
            // Federated (multi-shard) serve materializes the union and rebuilds.
            let single = match store.list_shards() {
                [only] => Some((only.tag.clone(), only.source_hash.clone())),
                _ => None,
            };
            let prepared = match &single {
                Some((tag, hash)) => PreparedIndexes {
                    query_index: store
                        .get_index_blob(tag, QUERY_INDEX_BLOB, hash)?
                        .and_then(|b| QueryIndex::from_bytes(&b).ok()),
                    affected_index: store
                        .get_index_blob(tag, AFFECTED_INDEX_BLOB, hash)?
                        .and_then(|b| ReverseImpactIndex::from_bytes(&b).ok()),
                },
                None => PreparedIndexes::default(),
            };
            let need_q = prepared.query_index.is_none();
            let need_a = prepared.affected_index.is_none();
            let server = Server::from_graph_data_with(gd, Some(path.to_path_buf()), prepared);
            if let Some((tag, hash)) = single {
                if need_q {
                    store.put_index_blob(
                        &tag,
                        QUERY_INDEX_BLOB,
                        &hash,
                        &server.query_index().to_bytes()?,
                    )?;
                }
                if need_a {
                    store.put_index_blob(
                        &tag,
                        AFFECTED_INDEX_BLOB,
                        &hash,
                        &server.affected_index().to_bytes()?,
                    )?;
                }
            }
            Ok(server)
        }
    }
}

/// Run a single-file writer against `path` and report it.
pub(crate) fn write_file(
    label: &str,
    path: &Path,
    write: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    write(path).with_context(|| format!("writing {label}"))?;
    println!("Wrote {}", path.display());
    Ok(())
}

pub(crate) fn default_graph_path(graph: Option<PathBuf>) -> PathBuf {
    graph.unwrap_or_else(|| PathBuf::from("synaptic-out/graph.json"))
}

/// Warn on stderr when a just-written graph.json exceeds the effective safety
/// caps. The write itself succeeds, but the merge driver and federation refuse
/// over-cap files, so surface the env override here instead of at merge time.
pub(crate) fn warn_if_over_caps(path: &Path, node_count: usize) {
    let node_cap = synaptic_core::max_nodes();
    if node_count > node_cap {
        eprintln!(
            "warning: graph has {node_count} nodes, over the {node_cap}-node cap; \
             merge and federation will refuse it (set SYNAPTIC_MAX_NODES to raise it; 0 = no cap)"
        );
    }
    let byte_cap = synaptic_core::max_graph_bytes();
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > byte_cap {
            eprintln!(
                "warning: {} is {} bytes, over the {byte_cap}-byte graph cap; \
                 merge and federation will refuse it (set SYNAPTIC_MAX_GRAPH_MB to raise it; 0 = no cap)",
                path.display(),
                meta.len()
            );
        }
    }
}

pub(crate) fn load_graph(path: &Path) -> Result<KnowledgeGraph> {
    Ok(KnowledgeGraph::from_graph_data(load_graph_data(
        path, None,
    )?))
}

/// Load a graph, optionally scoped to one federated member (`--repo`). Scoping
/// drops nodes from other repos + the cross-repo edges that span them. Honors
/// the `SYNAPTIC_STORE` backend: under `redb` a `--repo` scope materializes just
/// that shard.
pub(crate) fn load_scoped_graph(path: &Path, repo: Option<&str>) -> Result<KnowledgeGraph> {
    Ok(KnowledgeGraph::from_graph_data(load_graph_data(
        path, repo,
    )?))
}

/// Resolve a user-supplied name/id to a single node, or a human-readable error
/// message. Uses the same shared resolver as the MCP server, so the CLI and MCP
/// report ambiguity identically (candidate ids instead of a bare "not found").
pub(crate) fn resolve_or_message(
    kg: &KnowledgeGraph,
    arg: &str,
) -> std::result::Result<NodeId, String> {
    match synaptic_query::resolve_detailed(kg, arg) {
        synaptic_query::Resolution::Unique(id) => Ok(id),
        synaptic_query::Resolution::Ambiguous(ids) => {
            // List each candidate with its file + degree inline so the user can pick
            // one without a follow-up lookup. Shared with the MCP server via
            // candidate_details. Enrich only the shown prefix; `+N more` conveys the
            // rest from ids.len().
            let shown = ids.len().min(10);
            let lines: String = synaptic_query::candidate_details(kg, &ids[..shown])
                .iter()
                .map(|c| {
                    let file = if c.file.is_empty() {
                        "-"
                    } else {
                        c.file.as_str()
                    };
                    format!("\n  {} [{}] (degree {})", c.id.0, file, c.degree)
                })
                .collect();
            let more = if ids.len() > 10 {
                format!("\n  +{} more", ids.len() - 10)
            } else {
                String::new()
            };
            Err(format!(
                "'{arg}' is ambiguous - {} candidates:{lines}{more}\nPass a node id (or qualify as name@file) to disambiguate.",
                ids.len(),
            ))
        }
        synaptic_query::Resolution::NotFound => Err(format!("No node matches '{arg}'.")),
    }
}

pub(crate) fn label_or_id(kg: &KnowledgeGraph, id: &NodeId) -> String {
    kg.node(id)
        .map(|n| n.label.clone())
        .unwrap_or_else(|| id.0.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, Node};

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

    fn write_sample_graph(dir: &Path) -> PathBuf {
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![node("a"), node("b"), node("c")],
            links: vec![Edge {
                source: NodeId("a".into()),
                target: NodeId("b".into()),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                source_file: "src/a.rs".into(),
                source_location: None,
                confidence_score: None,
                weight: 1.0,
                context: None,
                cross_repo: false,
                extra: Map::new(),
            }],
            hyperedges: vec![],
            built_at_commit: Some("c0ffee".into()),
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        let path = dir.join("graph.json");
        synaptic_output::to_json(&kg, &path).unwrap();
        path
    }

    #[test]
    fn json_and_sharded_backends_load_equal_graphs() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = write_sample_graph(tmp.path());
        let store_dir = tmp.path().join("store");

        // migrate the graph.json into the shard store
        let gd: GraphData =
            serde_json::from_str(&fs::read_to_string(&graph_path).unwrap()).unwrap();
        let mut store = ShardStore::open(&store_dir).unwrap();
        synaptic_store::migrate::migrate_into(&mut store, &gd).unwrap();

        let j = load_graph_data_backend(StoreBackend::Json, &graph_path, &store_dir, None).unwrap();
        let r =
            load_graph_data_backend(StoreBackend::Sharded, &graph_path, &store_dir, None).unwrap();

        // Same materialized graph from either backend.
        let kj = KnowledgeGraph::from_graph_data(j);
        let kr = KnowledgeGraph::from_graph_data(r);
        assert_eq!(kj.node_count(), kr.node_count());
        assert_eq!(kj.edge_count(), kr.edge_count());

        let dump = |kg: &KnowledgeGraph| {
            let mut ns: Vec<String> = kg.nodes().map(|n| n.id.0.clone()).collect();
            ns.sort();
            let mut es: Vec<(String, String, String)> = kg
                .edges()
                .map(|e| (e.source.0.clone(), e.target.0.clone(), e.relation.clone()))
                .collect();
            es.sort();
            (ns, es)
        };
        assert_eq!(dump(&kj), dump(&kr));
    }
}
