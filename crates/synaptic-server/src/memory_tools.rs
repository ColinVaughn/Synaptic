//! Repository-memory MCP schemas, handlers, and compact evidence rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use synaptic_memory::{
    AccessScope, MemoryKind, MemoryLifecycle, MemoryQuery, MemoryRecord, MemorySearchHit,
    MemoryStore, RecordOutcome, SourceArtifact, SymbolAnchor, VerificationOutcome,
    VerificationStatus,
};

use crate::{Server, provider, tool_error_result};

const READ_TOOLS: &[&str] = &[
    "search_memory",
    "explain_history",
    "find_similar_change",
    "known_pitfalls",
    "explain_decision",
];

pub(crate) fn is_memory_tool(name: &str) -> bool {
    READ_TOOLS.contains(&name) || name == "record_change_outcome"
}

pub(crate) fn schemas(allow_write: bool) -> Vec<Value> {
    let mut tools = vec![
        schema(
            "search_memory",
            "Search source-grounded repository history, decisions, procedures, and failed attempts.",
            json!({
                "query": {"type":"string"},
                "symbol": {"type":"string"},
                "kinds": {"type":"array","items":{"type":"string"}},
                "include_superseded": {"type":"boolean"},
                "limit": {"type":"integer"}
            }),
            &[],
            true,
        ),
        schema(
            "explain_history",
            "Explain the revision-aware history attached to a symbol, file, or subsystem.",
            json!({
                "subject": {"type":"string"},
                "include_superseded": {"type":"boolean"},
                "limit": {"type":"integer"}
            }),
            &["subject"],
            true,
        ),
        schema(
            "find_similar_change",
            "Find previous changes with similar intent and affected symbols, including their outcomes.",
            json!({
                "description": {"type":"string"},
                "symbol": {"type":"string"},
                "limit": {"type":"integer"}
            }),
            &["description"],
            true,
        ),
        schema(
            "known_pitfalls",
            "Find active regressions, failed attempts, incidents, and rejected review findings.",
            json!({
                "subject": {"type":"string"},
                "limit": {"type":"integer"}
            }),
            &["subject"],
            true,
        ),
        schema(
            "explain_decision",
            "Explain active architecture decisions, invariants, conventions, and procedures.",
            json!({
                "subject": {"type":"string"},
                "include_superseded": {"type":"boolean"},
                "limit": {"type":"integer"}
            }),
            &["subject"],
            true,
        ),
    ];
    if allow_write {
        tools.push(schema(
            "record_change_outcome",
            "Persist a source-grounded, idempotent change outcome. Available only with --allow-memory-write.",
            json!({
                "idempotency_key": {"type":"string"},
                "title": {"type":"string"},
                "summary": {"type":"string"},
                "outcome": {"type":"string","enum":["succeeded","failed","partial","rolled_back","regressed"]},
                "source_uri": {"type":"string"},
                "commit": {"type":"string"},
                "branch": {"type":"string"},
                "affected_symbols": {"type":"array","items":{"type":"string"}},
                "verification_status": {"type":"string","enum":["unknown","passed","failed","partial"]},
                "verification_commands": {"type":"array","items":{"type":"string"}},
                "confidence": {"type":"number"},
                "scope": {"type":"string","enum":["private","repository","workspace"]},
                "workspace": {"type":"string"}
            }),
            &[
                "idempotency_key",
                "title",
                "summary",
                "outcome",
                "source_uri",
            ],
            false,
        ));
    }
    tools
}

pub(crate) fn schema_for(name: &str, allow_write: bool) -> Option<Value> {
    schemas(allow_write)
        .into_iter()
        .find(|schema| schema["name"].as_str() == Some(name))
}

fn schema(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required
        },
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        }
    })
}

impl Server {
    /// Append a bounded, source-citing memory overlay to an otherwise current
    /// graph response. This is deliberately a post-render join: historical
    /// observations never become static graph facts.
    pub(crate) fn append_memory_evidence(&self, result: &mut Value, subject: &str) {
        self.append_memory_evidence_for_subjects(result, &[subject.to_string()]);
    }

    /// Join memory across a whole proposed change. Results are deduplicated by
    /// immutable record ID and retain which files/symbols matched the evidence.
    pub(crate) fn append_memory_evidence_for_subjects(
        &self,
        result: &mut Value,
        subjects: &[String],
    ) {
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let Some(store) = self.memory.as_ref() else {
            return;
        };
        let normalized_subjects = subjects
            .iter()
            .map(|subject| {
                subject
                    .split_once('@')
                    .map(|(label, _)| label)
                    .filter(|label| !label.is_empty())
                    .unwrap_or(subject)
                    .trim()
                    .to_string()
            })
            .filter(|subject| !subject.is_empty())
            .collect::<BTreeSet<_>>();
        if normalized_subjects.is_empty() {
            return;
        }
        let cap = if self.concise { 4 } else { 6 };
        let mut by_id = BTreeMap::<String, (MemorySearchHit, BTreeSet<String>)>::new();
        let mut query_truncated = false;
        for subject in &normalized_subjects {
            let Ok(hits) = store.search_authorized(
                &MemoryQuery {
                    symbol: Some(subject.clone()),
                    limit: cap + 1,
                    ..MemoryQuery::default()
                },
                &self.memory_principal,
            ) else {
                continue;
            };
            query_truncated |= hits.len() > cap;
            for hit in hits {
                let entry = by_id
                    .entry(hit.record.id.clone())
                    .or_insert_with(|| (hit.clone(), BTreeSet::new()));
                if hit.score > entry.0.score {
                    entry.0 = hit;
                }
                entry.1.insert(subject.clone());
            }
        }
        let mut hits = by_id.into_values().collect::<Vec<_>>();
        if hits.is_empty() {
            return;
        }
        hits.sort_by(|a, b| {
            evidence_priority(&a.0.record)
                .cmp(&evidence_priority(&b.0.record))
                .then_with(|| b.0.record.occurred_at.cmp(&a.0.record.occurred_at))
                .then_with(|| a.0.record.id.cmp(&b.0.record.id))
        });
        let truncated = query_truncated || hits.len() > cap;
        hits.truncate(cap);
        let decisions = hits
            .iter()
            .filter(|(hit, _)| hit.record.kind.is_decision())
            .count();
        let pitfalls = hits
            .iter()
            .filter(|(hit, _)| is_pitfall(&hit.record))
            .count();
        let history = hits.len().saturating_sub(decisions + pitfalls);
        let mut rendered = format!("\n\nRepository memory evidence ({}):", hits.len());
        let records: Vec<Value> = hits
            .iter()
            .map(|(hit, matched_subjects)| {
                let source = hit
                    .record
                    .sources
                    .first()
                    .map(|source| source.uri.as_str())
                    .unwrap_or("unknown source");
                let category = if is_pitfall(&hit.record) {
                    "WARNING"
                } else if hit.record.kind.is_decision() {
                    "DECISION"
                } else {
                    "HISTORY"
                };
                rendered.push_str(&format!(
                    "\n- {category} [{}] {} — {} (source: {}, confidence: {:.2})",
                    hit.record.kind.as_str(),
                    crate::sanitize_label(&hit.record.title),
                    crate::sanitize_label(&hit.record.summary),
                    crate::sanitize_label(source),
                    hit.record.confidence
                ));
                json!({
                    "id": hit.record.id,
                    "kind": hit.record.kind.as_str(),
                    "category": category.to_ascii_lowercase(),
                    "title": crate::sanitize_label(&hit.record.title),
                    "summary": crate::sanitize_label(&hit.record.summary),
                    "source_uri": crate::sanitize_label(source),
                    "commit": hit.record.commit,
                    "confidence": hit.record.confidence,
                    "verification_status": hit.record.verification.status,
                    "matched_subjects": matched_subjects
                })
            })
            .collect();
        if truncated {
            rendered.push_str("\n- ... additional matching memory omitted; call explain_history");
        }
        if let Some(text) = result
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|content| content.first_mut())
            .and_then(|content| content.get_mut("text"))
            .and_then(|value| value.as_str())
            .map(str::to_string)
        {
            result["content"][0]["text"] = json!(format!("{text}{rendered}"));
        }
        if !result
            .get("structuredContent")
            .is_some_and(Value::is_object)
        {
            result["structuredContent"] = json!({});
        }
        if let Some(structured) = result["structuredContent"].as_object_mut() {
            structured.insert(
                "memory_evidence".into(),
                json!({
                    "subjects": normalized_subjects,
                    "total": records.len(),
                    "decisions": decisions,
                    "pitfalls": pitfalls,
                    "history": history,
                    "truncated": truncated,
                    "records": records
                }),
            );
        }
    }

    pub(crate) fn dispatch_memory_tool(&self, name: &str, args: &Value) -> Value {
        let Some(store) = self.memory.as_ref() else {
            return tool_error_result("Repository memory is not configured.");
        };
        if name == "record_change_outcome" {
            if !self.allow_memory_write {
                return tool_error_result(
                    "Memory writes are disabled. Restart with --allow-memory-write.",
                );
            }
            return match self.record_change_outcome(store, args) {
                Ok(value) => value,
                Err(message) => tool_error_result(message),
            };
        }

        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(if self.concise { 8 } else { 15 }) as usize;
        let include_superseded = args
            .get("include_superseded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let string = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let mut query = MemoryQuery {
            limit,
            include_superseded,
            ..MemoryQuery::default()
        };
        let heading = match name {
            "search_memory" => {
                query.text = string("query");
                query.symbol = nonempty(string("symbol"));
                query.kinds = args
                    .get("kinds")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter_map(parse_kind)
                    .collect();
                "Repository memory"
            }
            "explain_history" => {
                query.symbol = nonempty(string("subject"));
                "History"
            }
            "find_similar_change" => {
                query.text = string("description");
                query.symbol = nonempty(string("symbol"));
                query.kinds = vec![
                    MemoryKind::ChangeEpisode,
                    MemoryKind::FailedAttempt,
                    MemoryKind::Regression,
                    MemoryKind::PullRequest,
                ];
                "Similar changes"
            }
            "known_pitfalls" => {
                let subject = string("subject");
                query.symbol = nonempty(subject.clone());
                query.text = subject;
                query.kinds = vec![
                    MemoryKind::FailedAttempt,
                    MemoryKind::Regression,
                    MemoryKind::ReviewFinding,
                    MemoryKind::CiRun,
                    MemoryKind::Incident,
                ];
                "Known pitfalls"
            }
            "explain_decision" => {
                query.symbol = nonempty(string("subject"));
                query.kinds = vec![
                    MemoryKind::ArchitectureDecision,
                    MemoryKind::Invariant,
                    MemoryKind::Convention,
                    MemoryKind::Procedure,
                ];
                "ADR / decisions"
            }
            _ => return tool_error_result(format!("Unknown memory tool: {name}")),
        };
        match store.search_authorized(&query, &self.memory_principal) {
            Ok(mut hits) => {
                if name == "known_pitfalls" {
                    hits.retain(|hit| is_pitfall(&hit.record));
                }
                memory_result(heading, hits)
            }
            Err(error) => tool_error_result(format!("Memory search failed: {error}")),
        }
    }

    fn record_change_outcome(&self, store: &MemoryStore, args: &Value) -> Result<Value, String> {
        let required = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("{key} must be a non-empty string"))
        };
        let key = required("idempotency_key")?;
        let title = required("title")?;
        let summary = required("summary")?;
        let outcome = required("outcome")?;
        let source_uri = required("source_uri")?;
        let kind = match outcome.as_str() {
            "failed" | "rolled_back" => MemoryKind::FailedAttempt,
            "regressed" => MemoryKind::Regression,
            _ => MemoryKind::ChangeEpisode,
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let repository = self
            .source_root
            .as_deref()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| "unknown".to_string());
        let commit = args
            .get("commit")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut record = MemoryRecord::new(
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
        record.commit = commit.clone();
        record.branch = args
            .get("branch")
            .and_then(Value::as_str)
            .map(str::to_string);
        record.confidence = args
            .get("confidence")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            .clamp(0.0, 1.0) as f32;
        record.access_scope = match args.get("scope").and_then(Value::as_str) {
            Some("repository") => AccessScope::Repository,
            Some("workspace") => {
                let workspace = required("workspace")?;
                AccessScope::Workspace { workspace }
            }
            _ => AccessScope::Private,
        };
        if matches!(record.access_scope, AccessScope::Private) {
            record.owner = Some(self.memory_principal.id.clone());
        }
        record.lifecycle = if outcome == "rolled_back" {
            MemoryLifecycle::Resolved
        } else {
            MemoryLifecycle::Active
        };
        record.verification = VerificationOutcome {
            status: parse_verification(
                args.get("verification_status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
            ),
            commands: strings(args.get("verification_commands")),
            notes: format!("recorded outcome: {outcome}"),
        };
        for symbol in strings(args.get("affected_symbols")) {
            record
                .affected_symbols
                .push(self.resolve_memory_anchor(&symbol, commit.as_deref()));
        }
        let write = store
            .record_with_generated_timestamps_as(&record, &self.memory_principal)
            .map_err(|error| error.to_string())?;
        let text = format!(
            "Recorded {} {} ({write:?}) from {}.",
            record.kind.as_str(),
            crate::sanitize_label(&record.title),
            crate::sanitize_label(&record.sources[0].uri)
        );
        let write_label = match write {
            RecordOutcome::Created => "created",
            RecordOutcome::AlreadyPresent => "already_present",
        };
        Ok(json!({
            "content": [{"type":"text","text":text}],
            "structuredContent": {
                "record": record,
                "write": write_label
            },
            "isError": false
        }))
    }

    fn resolve_memory_anchor(&self, query: &str, commit: Option<&str>) -> SymbolAnchor {
        let node = match self.provider.resolve(query) {
            provider::ScopedResolution::Unique(_, id) => self.provider.node_cloned(&id),
            _ => None,
        };
        match node {
            Some(node) => SymbolAnchor {
                node_id: node.id.0,
                label: node.label,
                source_file: node.source_file,
                repo: node.repo,
                commit: commit.map(str::to_string),
                confidence: 1.0,
            },
            None => SymbolAnchor {
                node_id: query.to_string(),
                label: query.to_string(),
                source_file: String::new(),
                repo: None,
                commit: commit.map(str::to_string),
                confidence: 0.5,
            },
        }
    }
}

fn is_pitfall(record: &MemoryRecord) -> bool {
    record.kind.is_pitfall()
        || (record.kind == MemoryKind::CiRun
            && record.verification.status == VerificationStatus::Failed)
}

fn evidence_priority(record: &MemoryRecord) -> u8 {
    if is_pitfall(record) {
        0
    } else if record.kind.is_decision() {
        1
    } else {
        2
    }
}

fn memory_result(heading: &str, hits: Vec<MemorySearchHit>) -> Value {
    let total = hits.len();
    let mut text = format!("{heading}: {total} result(s)");
    for hit in &hits {
        let source = hit
            .record
            .sources
            .first()
            .map(|source| source.uri.as_str())
            .unwrap_or("unknown source");
        let tag = match hit.record.kind {
            MemoryKind::ArchitectureDecision => "ADR",
            MemoryKind::FailedAttempt => "FAILED ATTEMPT",
            MemoryKind::Regression => "REGRESSION",
            MemoryKind::Invariant => "INVARIANT",
            MemoryKind::Convention => "CONVENTION",
            MemoryKind::Procedure => "PROCEDURE",
            _ => hit.record.kind.as_str(),
        };
        text.push_str(&format!(
            "\n- [{tag}] {} — {} (source: {}, confidence: {:.2})",
            crate::sanitize_label(&hit.record.title),
            crate::sanitize_label(&hit.record.summary),
            crate::sanitize_label(source),
            hit.record.confidence
        ));
    }
    json!({
        "content": [{"type":"text","text":text}],
        "structuredContent": {"total": total, "hits": hits},
        "isError": false
    })
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn parse_verification(value: &str) -> VerificationStatus {
    match value {
        "passed" => VerificationStatus::Passed,
        "failed" => VerificationStatus::Failed,
        "partial" => VerificationStatus::Partial,
        _ => VerificationStatus::Unknown,
    }
}

fn parse_kind(value: &str) -> Option<MemoryKind> {
    Some(match value {
        "change_episode" => MemoryKind::ChangeEpisode,
        "issue" => MemoryKind::Issue,
        "incident" => MemoryKind::Incident,
        "pull_request" => MemoryKind::PullRequest,
        "review_finding" => MemoryKind::ReviewFinding,
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
