use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use synaptic_graph::KnowledgeGraph;

use crate::{
    AccessScope, DocumentIngestError, DocumentIngestReport, MemoryKind, MemoryLifecycle,
    MemoryLink, MemoryRecord, MemoryRelation, MemoryStore, RecordOutcome, SourceArtifact,
    SymbolAnchor, ingest_repository_documents,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticRefreshReport {
    pub communities: usize,
    pub created: usize,
    pub already_present: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRefreshReport {
    pub documents: DocumentIngestReport,
    pub semantic: SemanticRefreshReport,
}

#[derive(Debug, thiserror::Error)]
pub enum RepositoryRefreshError {
    #[error(transparent)]
    Documents(#[from] DocumentIngestError),
    #[error(transparent)]
    Memory(#[from] crate::MemoryError),
}

/// Refresh source-grounded procedures/conventions and deterministic semantic
/// summaries for every labeled graph community.
pub fn refresh_repository_memory(
    store: &MemoryStore,
    repo_root: &Path,
    graph: &KnowledgeGraph,
    graph_source_uri: &str,
) -> Result<RepositoryRefreshReport, RepositoryRefreshError> {
    let documents = ingest_repository_documents(store, repo_root, Some(graph))?;
    let repository = repository_identity(repo_root);
    let semantic = generate_semantic_summaries(store, graph, &repository, graph_source_uri)?;
    Ok(RepositoryRefreshReport {
        documents,
        semantic,
    })
}

pub fn generate_semantic_summaries(
    store: &MemoryStore,
    graph: &KnowledgeGraph,
    repository: &str,
    graph_source_uri: &str,
) -> Result<SemanticRefreshReport, crate::MemoryError> {
    let mut communities = BTreeMap::<u32, Vec<_>>::new();
    for node in graph.nodes().filter(|node| !node.source_file.is_empty()) {
        if let Some(community) = node.community {
            communities.entry(community).or_default().push(node);
        }
    }
    let existing = store.all()?;
    let occurred_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut report = SemanticRefreshReport {
        communities: communities.len(),
        created: 0,
        already_present: 0,
    };
    for (community, mut nodes) in communities {
        nodes.sort_by(|a, b| {
            a.source_file
                .cmp(&b.source_file)
                .then_with(|| a.label.cmp(&b.label))
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut hasher = blake3::Hasher::new();
        hasher.update(community.to_string().as_bytes());
        for node in &nodes {
            hasher.update(b"\0");
            hasher.update(node.id.0.as_bytes());
            hasher.update(b"\0");
            hasher.update(node.label.as_bytes());
            hasher.update(b"\0");
            hasher.update(node.source_file.as_bytes());
        }
        let digest = hasher.finalize().to_hex().to_string();
        let prefix = format!("semantic:community:{community}:");
        let labels = nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<BTreeSet<_>>();
        let files = nodes
            .iter()
            .map(|node| node.source_file.as_str())
            .collect::<BTreeSet<_>>();
        let key_symbols = labels.iter().take(12).copied().collect::<Vec<_>>();
        let primary_files = files.iter().take(8).copied().collect::<Vec<_>>();
        let mut record = MemoryRecord::new(
            format!("{prefix}{digest}"),
            MemoryKind::SemanticSummary,
            format!(
                "Community {community}: {}",
                key_symbols
                    .iter()
                    .take(3)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!(
                "Community {community} contains {} symbols across {} files. Key symbols: {}. Primary files: {}.",
                nodes.len(),
                files.len(),
                key_symbols.join(", "),
                primary_files.join(", ")
            ),
            repository,
            occurred_at,
            vec![SourceArtifact {
                kind: "synaptic_graph".into(),
                uri: graph_source_uri.to_string(),
                revision: graph.built_at_commit.clone(),
                digest: Some(digest),
            }],
        );
        record.commit = graph.built_at_commit.clone();
        record.access_scope = AccessScope::Repository;
        record.affected_symbols = nodes
            .iter()
            .take(50)
            .map(|node| SymbolAnchor {
                node_id: node.id.0.clone(),
                label: node.label.clone(),
                source_file: node.source_file.replace('\\', "/"),
                repo: node.repo.clone(),
                commit: graph.built_at_commit.clone(),
                confidence: 1.0,
            })
            .collect();
        if let Some(previous) = existing
            .iter()
            .filter(|candidate| {
                candidate.kind == MemoryKind::SemanticSummary
                    && candidate.id != record.id
                    && candidate.idempotency_key.starts_with(&prefix)
                    && !matches!(
                        candidate.lifecycle,
                        MemoryLifecycle::Superseded | MemoryLifecycle::Retracted
                    )
            })
            .max_by_key(|candidate| (candidate.occurred_at, candidate.recorded_at))
        {
            record.links.push(MemoryLink {
                relation: MemoryRelation::Supersedes,
                target: previous.id.clone(),
            });
        }
        match store.record_with_generated_timestamps(&record)? {
            RecordOutcome::Created => report.created += 1,
            RecordOutcome::AlreadyPresent => report.already_present += 1,
        }
    }
    Ok(report)
}

fn repository_identity(repo_root: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo_root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|remote| !remote.is_empty());
    output
        .map(|remote| remote.trim_end_matches(".git").to_string())
        .unwrap_or_else(|| {
            repo_root
                .canonicalize()
                .unwrap_or_else(|_| repo_root.to_path_buf())
                .to_string_lossy()
                .replace('\\', "/")
        })
}
