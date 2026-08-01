use std::path::Path;
use std::process::Command;

use synaptic_graph::KnowledgeGraph;

use crate::{
    AccessScope, MemoryKind, MemoryRecord, MemoryStore, PathChange, PathChangeKind, RecordOutcome,
    SourceArtifact, SymbolAnchor, SymbolChange, SymbolChangeKind,
};

#[derive(Debug, thiserror::Error)]
pub enum GitIngestError {
    #[error("git {command} failed: {message}")]
    Command { command: String, message: String },
    #[error("git returned malformed commit metadata for {0}")]
    MalformedMetadata(String),
    #[error(transparent)]
    Memory(#[from] crate::MemoryError),
}

pub fn ingest_commit(
    store: &MemoryStore,
    repo_root: &Path,
    revision: &str,
    graph: Option<&KnowledgeGraph>,
) -> Result<(MemoryRecord, RecordOutcome), GitIngestError> {
    let metadata = git(
        repo_root,
        &["show", "-s", "--format=%H%x00%ct%x00%s", revision],
    )?;
    let mut parts = metadata.trim_end().splitn(3, '\0');
    let sha = parts
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GitIngestError::MalformedMetadata(revision.to_string()))?;
    let occurred_at = parts
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| GitIngestError::MalformedMetadata(revision.to_string()))?;
    let title = parts
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GitIngestError::MalformedMetadata(revision.to_string()))?;
    let repository = repository_identity(repo_root);
    let idempotency_key = format!("git:{sha}");
    if let Some(existing) = store
        .all()?
        .into_iter()
        .find(|record| record.repository == repository && record.idempotency_key == idempotency_key)
    {
        return Ok((existing, RecordOutcome::AlreadyPresent));
    }

    let path_changes = parse_name_status(&git(
        repo_root,
        &[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-m",
            "-M",
            "-C",
            sha,
        ],
    )?);
    let mut changed_files: Vec<String> = path_changes
        .iter()
        .filter_map(|change| change.new_path.as_ref().or(change.old_path.as_ref()))
        .cloned()
        .collect();
    changed_files.sort();
    changed_files.dedup();

    let mut record = MemoryRecord::new(
        idempotency_key,
        MemoryKind::ChangeEpisode,
        title,
        format!(
            "Commit {sha} changed {} file(s): {}",
            changed_files.len(),
            changed_files.join(", ")
        ),
        repository,
        occurred_at,
        vec![SourceArtifact {
            kind: "git_commit".into(),
            uri: format!("git:{sha}"),
            revision: Some(sha.to_string()),
            digest: Some(sha.to_string()),
        }],
    );
    record.commit = Some(sha.to_string());
    record.path_changes = path_changes;
    record.branch = git_optional(repo_root, &["branch", "--show-current"])
        .filter(|branch| !branch.trim().is_empty())
        .map(|branch| branch.trim().to_string());
    record.access_scope = AccessScope::Repository;
    if let Some(graph) = graph {
        for node in graph.nodes() {
            let source_file = node.source_file.replace('\\', "/");
            if changed_files.iter().any(|file| file == &source_file) {
                record.affected_symbols.push(SymbolAnchor {
                    node_id: node.id.0.clone(),
                    label: node.label.clone(),
                    source_file,
                    repo: node.repo.clone(),
                    commit: Some(sha.to_string()),
                    confidence: 1.0,
                });
            }
        }
        record
            .affected_symbols
            .sort_by(|a, b| a.node_id.cmp(&b.node_id));
        if let Some(parent) = git_optional(repo_root, &["rev-parse", &format!("{sha}^")])
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            let patch = git_optional(
                repo_root,
                &[
                    "diff",
                    "--unified=0",
                    "--no-ext-diff",
                    "--no-color",
                    &parent,
                    sha,
                ],
            )
            .unwrap_or_default();
            record.symbol_changes = infer_symbol_renames(&patch, graph, &parent, sha);
        }
    }
    let outcome = store.record(&record)?;
    Ok((record, outcome))
}

#[derive(Default)]
struct PatchHunk {
    old_path: String,
    new_path: String,
    deleted: Vec<String>,
    added: Vec<String>,
}

fn infer_symbol_renames(
    patch: &str,
    graph: &KnowledgeGraph,
    parent: &str,
    revision: &str,
) -> Vec<SymbolChange> {
    let mut hunks = Vec::<PatchHunk>::new();
    let mut old_path = String::new();
    let mut new_path = String::new();
    let mut current: Option<PatchHunk> = None;
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("--- ") {
            old_path = patch_path(path);
        } else if let Some(path) = line.strip_prefix("+++ ") {
            new_path = patch_path(path);
        } else if line.starts_with("@@") {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            current = Some(PatchHunk {
                old_path: old_path.clone(),
                new_path: new_path.clone(),
                ..PatchHunk::default()
            });
        } else if let Some(hunk) = current.as_mut() {
            if let Some(value) = line.strip_prefix('-') {
                hunk.deleted.push(value.to_string());
            } else if let Some(value) = line.strip_prefix('+') {
                hunk.added.push(value.to_string());
            }
        }
    }
    if let Some(hunk) = current {
        hunks.push(hunk);
    }

    let mut changes = Vec::new();
    for hunk in hunks {
        for deleted in &hunk.deleted {
            let old_tokens = identifiers(deleted);
            for added in &hunk.added {
                let new_tokens = identifiers(added);
                if old_tokens.len() != new_tokens.len() {
                    continue;
                }
                let differences = old_tokens
                    .iter()
                    .zip(&new_tokens)
                    .enumerate()
                    .filter(|(_, (old, new))| old != new)
                    .collect::<Vec<_>>();
                if differences.len() != 1 {
                    continue;
                }
                let (_, (old_name, new_name)) = differences[0];
                let Some(node) = graph.nodes().find(|node| {
                    node.source_file.replace('\\', "/") == hunk.new_path
                        && canonical_label(&node.label) == new_name.as_str()
                }) else {
                    continue;
                };
                changes.push(SymbolChange {
                    kind: SymbolChangeKind::Renamed,
                    old: SymbolAnchor {
                        node_id: old_name.clone(),
                        label: old_name.clone(),
                        source_file: hunk.old_path.clone(),
                        repo: node.repo.clone(),
                        commit: Some(parent.to_string()),
                        confidence: 0.85,
                    },
                    new: SymbolAnchor {
                        node_id: node.id.0.clone(),
                        label: node.label.clone(),
                        source_file: hunk.new_path.clone(),
                        repo: node.repo.clone(),
                        commit: Some(revision.to_string()),
                        confidence: 1.0,
                    },
                    confidence: 0.9,
                });
            }
        }
    }
    changes.sort_by(|a, b| {
        a.old
            .source_file
            .cmp(&b.old.source_file)
            .then_with(|| a.old.label.cmp(&b.old.label))
            .then_with(|| a.new.label.cmp(&b.new.label))
    });
    changes.dedup_by(|a, b| {
        a.old.label == b.old.label
            && a.old.source_file == b.old.source_file
            && a.new.label == b.new.label
            && a.new.source_file == b.new.source_file
    });
    changes
}

fn patch_path(value: &str) -> String {
    let value = value.trim();
    if value == "/dev/null" {
        String::new()
    } else {
        value
            .strip_prefix("a/")
            .or_else(|| value.strip_prefix("b/"))
            .unwrap_or(value)
            .replace('\\', "/")
    }
}

fn identifiers(line: &str) -> Vec<String> {
    let mut identifiers = Vec::new();
    let mut current = String::new();
    for ch in line.chars() {
        if ch == '_' || ch.is_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            identifiers.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        identifiers.push(current);
    }
    identifiers
}

fn canonical_label(label: &str) -> &str {
    label
        .trim_start_matches('.')
        .strip_suffix("()")
        .unwrap_or_else(|| label.trim_start_matches('.'))
}

fn parse_name_status(output: &str) -> Vec<PathChange> {
    let mut changes = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let fields: Vec<&str> = line.split('\t').collect();
        let status = fields.first().copied().unwrap_or("");
        let code = status.chars().next().unwrap_or('M');
        let normalized = |value: &str| value.trim().replace('\\', "/");
        let change = match code {
            'R' if fields.len() >= 3 => PathChange {
                kind: PathChangeKind::Renamed,
                old_path: Some(normalized(fields[1])),
                new_path: Some(normalized(fields[2])),
            },
            'C' if fields.len() >= 3 => PathChange {
                kind: PathChangeKind::Copied,
                old_path: Some(normalized(fields[1])),
                new_path: Some(normalized(fields[2])),
            },
            'A' if fields.len() >= 2 => PathChange {
                kind: PathChangeKind::Added,
                old_path: None,
                new_path: Some(normalized(fields[1])),
            },
            'D' if fields.len() >= 2 => PathChange {
                kind: PathChangeKind::Deleted,
                old_path: Some(normalized(fields[1])),
                new_path: None,
            },
            'T' if fields.len() >= 2 => PathChange {
                kind: PathChangeKind::TypeChanged,
                old_path: Some(normalized(fields[1])),
                new_path: Some(normalized(fields[1])),
            },
            _ if fields.len() >= 2 => PathChange {
                kind: PathChangeKind::Modified,
                old_path: Some(normalized(fields[1])),
                new_path: Some(normalized(fields[1])),
            },
            _ => continue,
        };
        changes.push(change);
    }
    changes.sort_by(|a, b| {
        a.new_path
            .as_deref()
            .or(a.old_path.as_deref())
            .cmp(&b.new_path.as_deref().or(b.old_path.as_deref()))
    });
    changes.dedup();
    changes
}

fn git(repo_root: &Path, args: &[&str]) -> Result<String, GitIngestError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .map_err(|error| GitIngestError::Command {
            command: args.join(" "),
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(GitIngestError::Command {
            command: args.join(" "),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_optional(repo_root: &Path, args: &[&str]) -> Option<String> {
    git(repo_root, args).ok()
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
