use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use synaptic_graph::KnowledgeGraph;

use crate::{
    AccessScope, MemoryKind, MemoryLifecycle, MemoryLink, MemoryRecord, MemoryRelation,
    MemoryStore, RecordOutcome, SourceArtifact, SymbolAnchor,
};

const MAX_DOCUMENTS: usize = 500;
const MAX_DEPTH: usize = 8;
const DOCUMENT_ROOTS: &[(&str, MemoryKind)] = &[
    ("docs/adr", MemoryKind::ArchitectureDecision),
    ("docs/adrs", MemoryKind::ArchitectureDecision),
    ("docs/decisions", MemoryKind::ArchitectureDecision),
    ("adr", MemoryKind::ArchitectureDecision),
    ("adrs", MemoryKind::ArchitectureDecision),
    ("decisions", MemoryKind::ArchitectureDecision),
    ("docs/procedures", MemoryKind::Procedure),
    ("docs/runbooks", MemoryKind::Procedure),
    ("docs/playbooks", MemoryKind::Procedure),
    ("procedures", MemoryKind::Procedure),
    ("runbooks", MemoryKind::Procedure),
    ("playbooks", MemoryKind::Procedure),
];
const PROCEDURAL_FILES: &[(&str, MemoryKind)] = &[
    ("CONTRIBUTING.md", MemoryKind::Convention),
    ("DEVELOPMENT.md", MemoryKind::Procedure),
    ("RELEASING.md", MemoryKind::Procedure),
    ("AGENTS.md", MemoryKind::Convention),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentIngestReport {
    pub scanned: usize,
    pub created: usize,
    pub already_present: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum DocumentIngestError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Memory(#[from] crate::MemoryError),
}

/// Ingest explicit repository ADR/decision and procedure/runbook directories.
///
/// This intentionally does not summarize arbitrary Markdown. The directory is
/// the type signal, the first heading is the title, the first prose paragraph
/// is the compact summary, and `Synaptic-Symbols:` supplies grounded anchors.
pub fn ingest_repository_documents(
    store: &MemoryStore,
    repo_root: &Path,
    graph: Option<&KnowledgeGraph>,
) -> Result<DocumentIngestReport, DocumentIngestError> {
    let mut documents = Vec::<(PathBuf, MemoryKind)>::new();
    for (relative, kind) in DOCUMENT_ROOTS {
        collect_markdown(
            &repo_root.join(relative),
            *kind,
            0,
            &mut documents,
            MAX_DOCUMENTS,
        )?;
        if documents.len() >= MAX_DOCUMENTS {
            break;
        }
    }
    for (relative, kind) in PROCEDURAL_FILES {
        let path = repo_root.join(relative);
        if path.is_file() && documents.len() < MAX_DOCUMENTS {
            documents.push((path, *kind));
        }
    }
    documents.sort_by(|a, b| a.0.cmp(&b.0));
    documents.dedup_by(|a, b| a.0 == b.0);

    let repository = repository_identity(repo_root);
    let branch = git_optional(repo_root, &["branch", "--show-current"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let mut existing = store.all()?;
    let mut report = DocumentIngestReport {
        scanned: documents.len(),
        created: 0,
        already_present: 0,
    };

    for (path, kind) in documents {
        let bytes = std::fs::read(&path)?;
        let text = String::from_utf8_lossy(&bytes);
        let relative = path
            .strip_prefix(repo_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let source_uri = format!("file:{relative}");
        let digest = blake3::hash(&bytes).to_hex().to_string();
        let idempotency_key = format!("document:{relative}:{digest}");
        // The document bytes define this immutable observation. Branch/commit
        // metadata, graph-resolved anchors, and supersession links are contextual
        // enrichment and can legitimately change between retries (for example,
        // when an unchanged working-tree ADR is committed before the next
        // refresh). Preserve the first record when its source identity matches,
        // while still letting the store reject a reused key whose source does not.
        if existing.iter().any(|candidate| {
            candidate.repository == repository
                && candidate.idempotency_key == idempotency_key
                && candidate.kind == kind
                && candidate.sources.iter().any(|source| {
                    source.uri == source_uri && source.digest.as_deref() == Some(digest.as_str())
                })
        }) {
            report.already_present += 1;
            continue;
        }
        let metadata = document_revision(repo_root, &relative).unwrap_or_else(|| {
            let occurred_at = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or_else(|_| SystemTime::now())
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            (None, occurred_at)
        });
        let title = first_heading(&text).unwrap_or_else(|| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().replace(['-', '_'], " "))
                .unwrap_or_else(|| relative.clone())
        });
        let summary = first_prose_paragraph(&text)
            .unwrap_or_else(|| format!("Repository {} documented in {relative}.", kind.as_str()));
        let mut record = MemoryRecord::new(
            idempotency_key,
            kind,
            title,
            summary,
            repository.clone(),
            metadata.1,
            vec![SourceArtifact {
                kind: match kind {
                    MemoryKind::ArchitectureDecision => "adr",
                    MemoryKind::Convention => "convention",
                    _ => "procedure",
                }
                .into(),
                uri: source_uri.clone(),
                revision: metadata.0.clone(),
                digest: Some(digest),
            }],
        );
        record.commit = metadata.0;
        record.branch = branch.clone();
        record.access_scope = AccessScope::Repository;
        record.lifecycle = lifecycle(&text);
        record.affected_symbols = symbol_directives(&text)
            .into_iter()
            .map(|symbol| resolve_anchor(graph, &symbol, record.commit.as_deref()))
            .collect();

        if let Some(previous) = existing
            .iter()
            .filter(|candidate| {
                candidate.id != record.id
                    && candidate.kind == kind
                    && !matches!(
                        candidate.lifecycle,
                        MemoryLifecycle::Superseded | MemoryLifecycle::Retracted
                    )
                    && candidate
                        .sources
                        .iter()
                        .any(|source| source.uri == source_uri)
            })
            .max_by_key(|candidate| (candidate.occurred_at, candidate.recorded_at))
        {
            record.links.push(MemoryLink {
                relation: MemoryRelation::Supersedes,
                target: previous.id.clone(),
            });
        }

        match store.record_with_generated_timestamps(&record)? {
            RecordOutcome::Created => {
                report.created += 1;
                existing.push(record);
            }
            RecordOutcome::AlreadyPresent => report.already_present += 1,
        }
    }
    Ok(report)
}

fn collect_markdown(
    root: &Path,
    kind: MemoryKind,
    depth: usize,
    out: &mut Vec<(PathBuf, MemoryKind)>,
    cap: usize,
) -> std::io::Result<()> {
    if depth > MAX_DEPTH || out.len() >= cap || !root.exists() {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        if out.len() >= cap {
            break;
        }
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_markdown(&path, kind, depth + 1, out, cap)?;
        } else if file_type.is_file()
            && matches!(
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("md" | "mdx" | "qmd")
            )
        {
            out.push((path, kind));
        }
    }
    Ok(())
}

fn first_heading(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix('#')
            .map(|value| value.trim_start_matches('#').trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn first_prose_paragraph(text: &str) -> Option<String> {
    let mut paragraph = Vec::new();
    let mut in_frontmatter = false;
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "---" && paragraph.is_empty() {
            in_frontmatter = !in_frontmatter;
            continue;
        }
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_frontmatter || in_fence || trimmed.starts_with('#') || is_metadata_line(trimmed) {
            continue;
        }
        if trimmed.is_empty() {
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        paragraph.push(trimmed);
    }
    if paragraph.is_empty() {
        None
    } else {
        Some(cap_chars(&paragraph.join(" "), 1000))
    }
}

fn is_metadata_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.starts_with("status:")
        || lower.starts_with("synaptic-symbols:")
        || lower.starts_with("date:")
        || lower.starts_with("deciders:")
}

fn symbol_directives(text: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    for line in text.lines() {
        let Some((name, values)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("synaptic-symbols") {
            continue;
        }
        symbols.extend(
            values
                .split(',')
                .map(str::trim)
                .filter(|symbol| !symbol.is_empty())
                .map(str::to_string),
        );
    }
    symbols.sort();
    symbols.dedup();
    symbols
}

fn resolve_anchor(
    graph: Option<&KnowledgeGraph>,
    symbol: &str,
    commit: Option<&str>,
) -> SymbolAnchor {
    let matches = graph
        .into_iter()
        .flat_map(KnowledgeGraph::nodes)
        .filter(|node| {
            node.id.0.eq_ignore_ascii_case(symbol) || node.label.eq_ignore_ascii_case(symbol)
        })
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        let node = matches[0];
        SymbolAnchor {
            node_id: node.id.0.clone(),
            label: node.label.clone(),
            source_file: node.source_file.to_string(),
            repo: node.repo.clone(),
            commit: commit.map(str::to_string),
            confidence: 1.0,
        }
    } else {
        SymbolAnchor {
            node_id: symbol.to_string(),
            label: symbol.to_string(),
            source_file: String::new(),
            repo: None,
            commit: commit.map(str::to_string),
            confidence: 0.5,
        }
    }
}

fn lifecycle(text: &str) -> MemoryLifecycle {
    let status = text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("status")
            .then(|| value.trim().to_ascii_lowercase())
    });
    match status.as_deref() {
        Some("superseded" | "deprecated") => MemoryLifecycle::Superseded,
        Some("rejected" | "withdrawn") => MemoryLifecycle::Retracted,
        _ => MemoryLifecycle::Active,
    }
}

fn document_revision(repo_root: &Path, relative: &str) -> Option<(Option<String>, i64)> {
    let output = git_optional(
        repo_root,
        &["log", "-1", "--format=%H%x00%ct", "--", relative],
    )?;
    let mut parts = output.trim().splitn(2, '\0');
    let commit = parts.next()?.trim();
    let timestamp = parts.next()?.trim().parse().ok()?;
    (!commit.is_empty()).then(|| (Some(commit.to_string()), timestamp))
}

fn git_optional(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn repository_identity(repo_root: &Path) -> String {
    git_optional(repo_root, &["config", "--get", "remote.origin.url"])
        .filter(|remote| !remote.trim().is_empty())
        .map(|remote| remote.trim().trim_end_matches(".git").to_string())
        .unwrap_or_else(|| {
            repo_root
                .canonicalize()
                .unwrap_or_else(|_| repo_root.to_path_buf())
                .to_string_lossy()
                .replace('\\', "/")
        })
}

fn cap_chars(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let capped: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{capped}…")
    } else {
        capped
    }
}
