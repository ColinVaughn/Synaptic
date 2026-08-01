//! Durable repository-memory CLI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use synaptic_memory::{
    enforce_benchmark_gate, export_bundle, import_bundle, ingest_artifact_file, ingest_commit,
    ingest_repository_documents, refresh_repository_memory, run_benchmark_file, AccessScope,
    BenchmarkGate, MemoryKind, MemoryLifecycle, MemoryPrincipal, MemoryQuery, MemoryRecord,
    MemoryStore, SourceArtifact, SymbolAnchor, VerificationOutcome, VerificationStatus,
};

use crate::cli::MemoryAction;
use crate::commands::common::load_scoped_graph;

pub(crate) fn run_memory(action: MemoryAction) -> Result<()> {
    match action {
        MemoryAction::Ingest {
            revision,
            root,
            graph,
        } => ingest(&root, &revision, graph),
        MemoryAction::IngestDocs { root, graph } => ingest_docs(&root, graph),
        MemoryAction::ImportArtifacts { root, file, graph } => {
            import_artifacts(&root, &file, graph)
        }
        MemoryAction::Refresh { root, graph } => refresh(&root, graph),
        MemoryAction::Search {
            query,
            root,
            symbol,
            kinds,
            include_superseded,
            limit,
            json,
            peers,
            principal,
            repository_claims,
            workspace_claims,
            allow_private,
        } => search(
            &root,
            query,
            symbol,
            kinds,
            include_superseded,
            limit,
            json,
            peers,
            principal,
            repository_claims,
            workspace_claims,
            allow_private,
        ),
        MemoryAction::Record {
            root,
            idempotency_key,
            title,
            summary,
            outcome,
            source_uri,
            commit,
            branch,
            symbols,
            verification_status,
            verification_commands,
            confidence,
            scope,
            principal,
            repository_claims,
            workspace_claims,
            allow_private,
        } => record(
            &root,
            idempotency_key,
            title,
            summary,
            outcome,
            source_uri,
            commit,
            branch,
            symbols,
            verification_status,
            verification_commands,
            confidence,
            scope,
            principal,
            repository_claims,
            workspace_claims,
            allow_private,
        ),
        MemoryAction::Compact { root, json } => compact(&root, json),
        MemoryAction::Export {
            root,
            output,
            principal,
            repository_claims,
            workspace_claims,
            allow_private,
        } => export(
            &root,
            &output,
            principal,
            repository_claims,
            workspace_claims,
            allow_private,
        ),
        MemoryAction::Sync {
            root,
            bundle,
            principal,
            repository_claims,
            workspace_claims,
            allow_private,
        } => sync(
            &root,
            &bundle,
            principal,
            repository_claims,
            workspace_claims,
            allow_private,
        ),
        MemoryAction::Eval {
            root,
            manifest,
            principal,
            repository_claims,
            workspace_claims,
            allow_private,
            min_recall_at_5,
            min_mrr,
            max_candidate_fraction,
            json,
        } => evaluate(
            &root,
            &manifest,
            principal,
            repository_claims,
            workspace_claims,
            allow_private,
            BenchmarkGate {
                min_recall_at_5,
                min_mean_reciprocal_rank: min_mrr,
                max_mean_candidate_fraction: max_candidate_fraction,
            },
            json,
        ),
        MemoryAction::Status { root, json } => status(&root, json),
    }
}

fn memory_store(root: &Path) -> MemoryStore {
    MemoryStore::open(root.join(".synaptic").join("memory"))
}

fn federated_memory_store(root: &Path, peers: Vec<PathBuf>) -> MemoryStore {
    MemoryStore::open_federated(root.join(".synaptic").join("memory"), peers)
}

fn memory_principal(
    id: Option<String>,
    repositories: Vec<String>,
    workspaces: Vec<String>,
    allow_private: bool,
) -> MemoryPrincipal {
    let Some(id) = id else {
        return MemoryPrincipal::operator();
    };
    let mut principal = MemoryPrincipal::restricted(id).with_all_private(allow_private);
    for repository in repositories {
        principal = principal.with_repository(repository);
    }
    for workspace in workspaces {
        principal = principal.with_workspace(workspace);
    }
    principal
}

fn graph_path(root: &Path, explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| root.join("synaptic-out").join("graph.json"))
}

fn ingest_docs(root: &Path, graph: Option<PathBuf>) -> Result<()> {
    let path = graph_path(root, graph);
    let graph = path
        .exists()
        .then(|| load_scoped_graph(&path, None))
        .transpose()
        .with_context(|| format!("loading graph anchors from {}", path.display()))?;
    let report = ingest_repository_documents(&memory_store(root), root, graph.as_ref())
        .context("ingesting repository ADRs and procedures")?;
    println!(
        "Repository documents: {} scanned, {} created, {} already present",
        report.scanned, report.created, report.already_present
    );
    Ok(())
}

fn import_artifacts(root: &Path, file: &Path, graph: Option<PathBuf>) -> Result<()> {
    let path = graph_path(root, graph);
    let graph = path
        .exists()
        .then(|| load_scoped_graph(&path, None))
        .transpose()
        .with_context(|| format!("loading graph anchors from {}", path.display()))?;
    let report = ingest_artifact_file(&memory_store(root), file, graph.as_ref())
        .with_context(|| format!("importing memory artifacts from {}", file.display()))?;
    println!(
        "External artifacts: {} scanned, {} created, {} already present",
        report.scanned, report.created, report.already_present
    );
    Ok(())
}

fn refresh(root: &Path, graph: Option<PathBuf>) -> Result<()> {
    let path = graph_path(root, graph);
    let graph = load_scoped_graph(&path, None)
        .with_context(|| format!("loading semantic source graph {}", path.display()))?;
    let source_uri = format!(
        "file:{}",
        path.strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/")
    );
    let report = refresh_repository_memory(&memory_store(root), root, &graph, &source_uri)
        .context("refreshing repository memory")?;
    println!(
        "Repository refresh: {} document(s) scanned ({} created), {} communities ({} created)",
        report.documents.scanned,
        report.documents.created,
        report.semantic.communities,
        report.semantic.created
    );
    Ok(())
}

fn ingest(root: &Path, revision: &str, graph: Option<PathBuf>) -> Result<()> {
    let path = graph_path(root, graph);
    let graph = path
        .exists()
        .then(|| load_scoped_graph(&path, None))
        .transpose()
        .with_context(|| format!("loading graph anchors from {}", path.display()))?;
    let (record, outcome) = ingest_commit(&memory_store(root), root, revision, graph.as_ref())
        .context("ingesting Git commit")?;
    println!(
        "Memory {outcome:?}: {} ({}) — {} symbol anchor(s)",
        record.title,
        record.commit.as_deref().unwrap_or(revision),
        record.affected_symbols.len()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn search(
    root: &Path,
    text: String,
    symbol: Option<String>,
    kinds: Vec<String>,
    include_superseded: bool,
    limit: usize,
    json_output: bool,
    peers: Vec<PathBuf>,
    principal_id: Option<String>,
    repository_claims: Vec<String>,
    workspace_claims: Vec<String>,
    allow_private: bool,
) -> Result<()> {
    let kinds: Vec<MemoryKind> = kinds
        .iter()
        .map(|kind| parse_kind(kind).ok_or_else(|| anyhow::anyhow!("unknown memory kind {kind:?}")))
        .collect::<Result<_>>()?;
    let principal = memory_principal(
        principal_id,
        repository_claims,
        workspace_claims,
        allow_private,
    );
    let hits = federated_memory_store(root, peers).search_authorized(
        &MemoryQuery {
            text,
            kinds,
            symbol,
            include_superseded,
            limit,
        },
        &principal,
    )?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "total": hits.len(),
                "hits": hits
            }))?
        );
        return Ok(());
    }
    println!("{} memory result(s)", hits.len());
    for hit in hits {
        let source = hit
            .record
            .sources
            .first()
            .map(|source| source.uri.as_str())
            .unwrap_or("unknown source");
        println!(
            "- [{}] {} — {} (source: {source}, score: {:.2})",
            hit.record.kind.as_str(),
            hit.record.title,
            hit.record.summary,
            hit.score
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record(
    root: &Path,
    key: String,
    title: String,
    summary: String,
    outcome: String,
    source_uri: String,
    commit: Option<String>,
    branch: Option<String>,
    symbols: Vec<String>,
    verification_status: String,
    verification_commands: Vec<String>,
    confidence: f32,
    scope: String,
    principal_id: Option<String>,
    repository_claims: Vec<String>,
    workspace_claims: Vec<String>,
    allow_private: bool,
) -> Result<()> {
    if key.trim().is_empty()
        || title.trim().is_empty()
        || summary.trim().is_empty()
        || source_uri.trim().is_empty()
    {
        bail!("idempotency key, title, summary, and source URI must be non-empty");
    }
    let kind = match outcome.as_str() {
        "succeeded" | "partial" => MemoryKind::ChangeEpisode,
        "failed" | "rolled_back" => MemoryKind::FailedAttempt,
        "regressed" => MemoryKind::Regression,
        _ => bail!(
            "unknown outcome {outcome:?}; expected succeeded, failed, partial, rolled_back, or regressed"
        ),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let repository = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/");
    let mut memory = MemoryRecord::new(
        key,
        kind,
        title,
        summary,
        repository,
        now,
        vec![SourceArtifact {
            kind: "agent_outcome".into(),
            uri: source_uri,
            revision: commit.clone(),
            digest: None,
        }],
    );
    memory.commit = commit.clone();
    memory.branch = branch;
    memory.confidence = confidence.clamp(0.0, 1.0);
    memory.lifecycle = if outcome == "rolled_back" {
        MemoryLifecycle::Resolved
    } else {
        MemoryLifecycle::Active
    };
    memory.access_scope = match scope.as_str() {
        "private" => AccessScope::Private,
        "repository" => AccessScope::Repository,
        _ if scope.starts_with("workspace:") => AccessScope::Workspace {
            workspace: scope["workspace:".len()..].trim().to_string(),
        },
        _ => bail!("unknown scope {scope:?}; expected private, repository, or workspace:<name>"),
    };
    memory.verification = VerificationOutcome {
        status: parse_verification(&verification_status)?,
        commands: verification_commands,
        notes: format!("recorded outcome: {outcome}"),
    };
    memory.affected_symbols = symbols
        .into_iter()
        .map(|symbol| SymbolAnchor {
            node_id: symbol.clone(),
            label: symbol,
            source_file: String::new(),
            repo: None,
            commit: commit.clone(),
            confidence: 0.5,
        })
        .collect();
    let principal = memory_principal(
        principal_id,
        repository_claims,
        workspace_claims,
        allow_private,
    );
    if matches!(memory.access_scope, AccessScope::Private) {
        memory.owner = Some(principal.id.clone());
    }
    let write = memory_store(root).record_with_generated_timestamps_as(&memory, &principal)?;
    println!(
        "Memory {write:?}: [{}] {} ({})",
        memory.kind.as_str(),
        memory.title,
        memory.sources[0].uri
    );
    Ok(())
}

fn compact(root: &Path, json_output: bool) -> Result<()> {
    let store = memory_store(root);
    let report = store.compact().context("compacting repository memory")?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "store": store.root(),
                "records": report.records,
                "bytes": report.bytes
            }))?
        );
    } else {
        println!(
            "Compacted {} memory record(s) into {} bytes at {}",
            report.records,
            report.bytes,
            store.root().join("index.compact-v1.json").display()
        );
    }
    Ok(())
}

fn export(
    root: &Path,
    output: &Path,
    principal_id: Option<String>,
    repository_claims: Vec<String>,
    workspace_claims: Vec<String>,
    allow_private: bool,
) -> Result<()> {
    let principal = memory_principal(
        principal_id,
        repository_claims,
        workspace_claims,
        allow_private,
    );
    let report = export_bundle(&memory_store(root), output, &principal)
        .with_context(|| format!("exporting memory bundle to {}", output.display()))?;
    println!(
        "Exported {} record(s), {} bytes, digest {} to {}",
        report.records,
        report.bytes,
        report.digest,
        output.display()
    );
    Ok(())
}

fn sync(
    root: &Path,
    bundle: &Path,
    principal_id: Option<String>,
    repository_claims: Vec<String>,
    workspace_claims: Vec<String>,
    allow_private: bool,
) -> Result<()> {
    let principal = memory_principal(
        principal_id,
        repository_claims,
        workspace_claims,
        allow_private,
    );
    let report = import_bundle(&memory_store(root), bundle, &principal)
        .with_context(|| format!("synchronizing memory bundle {}", bundle.display()))?;
    println!(
        "Synchronized {} record(s): {} created, {} already present",
        report.records, report.created, report.already_present
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn evaluate(
    root: &Path,
    manifest: &Path,
    principal_id: Option<String>,
    repository_claims: Vec<String>,
    workspace_claims: Vec<String>,
    allow_private: bool,
    gate: BenchmarkGate,
    json_output: bool,
) -> Result<()> {
    let principal = memory_principal(
        principal_id,
        repository_claims,
        workspace_claims,
        allow_private,
    );
    let report = run_benchmark_file(&memory_store(root), manifest, &principal)
        .with_context(|| format!("evaluating {}", manifest.display()))?;
    enforce_benchmark_gate(&report, gate)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Memory evaluation: {} cases | recall@1 {:.3} | recall@5 {:.3} | MRR {:.3} | candidate fraction {:.3}",
            report.cases,
            report.recall_at_1,
            report.recall_at_5,
            report.mean_reciprocal_rank,
            report.mean_candidate_fraction
        );
        for miss in &report.misses {
            println!("  MISS {miss}");
        }
    }
    Ok(())
}

fn status(root: &Path, json_output: bool) -> Result<()> {
    let store = memory_store(root);
    let records = store.all()?;
    let mut by_kind = BTreeMap::<String, usize>::new();
    for record in &records {
        *by_kind.entry(record.kind.as_str().to_string()).or_default() += 1;
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "store": store.root(),
                "records": records.len(),
                "by_kind": by_kind
            }))?
        );
    } else {
        println!(
            "Repository memory: {} record(s) at {}",
            records.len(),
            store.root().display()
        );
        for (kind, count) in by_kind {
            println!("  {kind}: {count}");
        }
    }
    Ok(())
}

fn parse_verification(value: &str) -> Result<VerificationStatus> {
    Ok(match value {
        "unknown" => VerificationStatus::Unknown,
        "passed" => VerificationStatus::Passed,
        "failed" => VerificationStatus::Failed,
        "partial" => VerificationStatus::Partial,
        _ => bail!(
            "unknown verification status {value:?}; expected unknown, passed, failed, or partial"
        ),
    })
}

fn parse_kind(value: &str) -> Option<MemoryKind> {
    Some(match value {
        "change_episode" => MemoryKind::ChangeEpisode,
        "issue" => MemoryKind::Issue,
        "incident" => MemoryKind::Incident,
        "pull_request" => MemoryKind::PullRequest,
        "review_finding" => MemoryKind::ReviewFinding,
        "ci_run" => MemoryKind::CiRun,
        "architecture_decision" => MemoryKind::ArchitectureDecision,
        "invariant" => MemoryKind::Invariant,
        "convention" => MemoryKind::Convention,
        "procedure" => MemoryKind::Procedure,
        "failed_attempt" => MemoryKind::FailedAttempt,
        "regression" => MemoryKind::Regression,
        "release" => MemoryKind::Release,
        "customer_report" => MemoryKind::CustomerReport,
        "agent_task" => MemoryKind::AgentTask,
        "semantic_summary" => MemoryKind::SemanticSummary,
        _ => return None,
    })
}
