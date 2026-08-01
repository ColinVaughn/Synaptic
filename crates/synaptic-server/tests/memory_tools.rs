use serde_json::{json, Map, Value};
use synaptic_core::{FileType, GraphData, Node, NodeId};
use synaptic_memory::{
    AccessScope, MemoryKind, MemoryPrincipal, MemoryRecord, MemoryStore, SourceArtifact,
    SymbolAnchor,
};
use synaptic_prs::CommandRunner;
use synaptic_server::Server;

fn server(store: MemoryStore, write: bool) -> Server {
    let node = Node {
        id: NodeId("refresh_token".into()),
        label: "refresh_token".into(),
        file_type: FileType::Code,
        source_file: "src/auth/token.rs".into(),
        source_location: Some("L10".into()),
        community: Some(0),
        repo: None,
        extra: Map::new(),
    };
    Server::from_graph_data(
        GraphData {
            nodes: vec![node],
            built_at_commit: Some("abc123".into()),
            ..GraphData::default()
        },
        None,
    )
    .with_memory_store(store)
    .with_allow_memory_write(write)
}

fn call(server: &Server, name: &str, arguments: Value) -> Value {
    server
        .dispatch_request(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }))
        .unwrap()
}

fn tool_names(server: &Server) -> Vec<String> {
    let response = server
        .dispatch_request(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        }))
        .unwrap();
    response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn memory_read_tools_are_advertised_but_write_is_gated() {
    let dir = tempfile::tempdir().unwrap();
    let read_only = server(MemoryStore::open(dir.path()), false);
    let names = tool_names(&read_only);
    for expected in [
        "search_memory",
        "explain_history",
        "find_similar_change",
        "known_pitfalls",
        "explain_decision",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }
    assert!(!names.contains(&"record_change_outcome".to_string()));

    let writable = server(MemoryStore::open(dir.path()), true);
    assert!(tool_names(&writable).contains(&"record_change_outcome".to_string()));
}

#[test]
fn records_and_retrieves_a_failed_change_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let server = server(MemoryStore::open(dir.path()), true);
    let recorded = call(
        &server,
        "record_change_outcome",
        json!({
            "idempotency_key": "agent-task-42",
            "title": "Refresh lock attempt failed",
            "summary": "Holding the session mutex across network I/O deadlocked token refresh.",
            "outcome": "failed",
            "source_uri": "agent://task/42",
            "commit": "abc123",
            "affected_symbols": ["refresh_token"],
            "verification_status": "failed",
            "verification_commands": ["cargo test auth_refresh"],
            "confidence": 0.95,
            "scope": "private"
        }),
    );
    assert_eq!(recorded["result"]["isError"], false, "{recorded}");
    assert_eq!(
        recorded["result"]["structuredContent"]["record"]["kind"],
        "failed_attempt"
    );
    assert_eq!(
        recorded["result"]["structuredContent"]["record"]["affected_symbols"][0]["node_id"],
        "refresh_token"
    );

    let searched = call(
        &server,
        "search_memory",
        json!({"query": "session mutex deadlock", "symbol": "refresh_token"}),
    );
    assert_eq!(searched["result"]["isError"], false, "{searched}");
    assert_eq!(
        searched["result"]["structuredContent"]["total"], 1,
        "{searched}"
    );
    assert!(searched["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("agent://task/42"));

    for (tool, args) in [
        ("explain_history", json!({"subject": "refresh_token"})),
        ("known_pitfalls", json!({"subject": "refresh_token"})),
        (
            "find_similar_change",
            json!({"description": "token refresh deadlock", "symbol": "refresh_token"}),
        ),
    ] {
        let result = call(&server, tool, args);
        assert_eq!(
            result["result"]["structuredContent"]["total"], 1,
            "{tool}: {result}"
        );
    }
}

#[test]
fn record_change_outcome_retries_ignore_only_server_generated_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let server = server(MemoryStore::open(dir.path()), true);
    let args = json!({
        "idempotency_key": "retryable-task",
        "title": "Retryable result",
        "summary": "The same source-grounded outcome may be delivered more than once.",
        "outcome": "succeeded",
        "source_uri": "agent://task/retryable",
        "affected_symbols": ["refresh_token"],
        "verification_status": "passed"
    });

    let first = call(&server, "record_change_outcome", args.clone());
    assert_eq!(
        first["result"]["structuredContent"]["write"], "created",
        "{first}"
    );
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let retry = call(&server, "record_change_outcome", args);
    assert_eq!(retry["result"]["isError"], false, "{retry}");
    assert_eq!(
        retry["result"]["structuredContent"]["write"], "already_present",
        "{retry}"
    );

    let changed = call(
        &server,
        "record_change_outcome",
        json!({
            "idempotency_key": "retryable-task",
            "title": "Retryable result",
            "summary": "Different content must still conflict.",
            "outcome": "succeeded",
            "source_uri": "agent://task/retryable"
        }),
    );
    assert_eq!(changed["result"]["isError"], true, "{changed}");
    assert!(changed["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("different content"));
}

#[test]
fn explain_decision_returns_active_grounded_decisions_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let mut decision = MemoryRecord::new(
        "adr-014",
        MemoryKind::ArchitectureDecision,
        "Retain dynamic refresh entrypoint",
        "Production loads refresh_token by name, so it must remain public.",
        "repo",
        100,
        vec![SourceArtifact {
            kind: "adr".into(),
            uri: "docs/adr/014.md".into(),
            revision: Some("abc123".into()),
            digest: None,
        }],
    );
    decision.access_scope = AccessScope::Repository;
    decision.affected_symbols.push(SymbolAnchor {
        node_id: "refresh_token".into(),
        label: "refresh_token".into(),
        source_file: "src/auth/token.rs".into(),
        repo: None,
        commit: Some("abc123".into()),
        confidence: 1.0,
    });
    store.record(&decision).unwrap();
    let server = server(store, false);

    let result = call(
        &server,
        "explain_decision",
        json!({"subject": "refresh_token"}),
    );
    assert_eq!(result["result"]["structuredContent"]["total"], 1);
    assert!(result["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("ADR"));
    assert!(result["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("docs/adr/014.md"));
}

#[test]
fn graph_node_and_impact_responses_include_bounded_memory_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let anchor = SymbolAnchor {
        node_id: "refresh_token".into(),
        label: "refresh_token".into(),
        source_file: "src/auth/token.rs".into(),
        repo: None,
        commit: Some("abc123".into()),
        confidence: 1.0,
    };
    let mut decision = MemoryRecord::new(
        "adr-014-impact",
        MemoryKind::ArchitectureDecision,
        "Retain dynamic refresh entrypoint",
        "Production loads refresh_token by name, so it must remain public.",
        "repo",
        100,
        vec![SourceArtifact {
            kind: "adr".into(),
            uri: "docs/adr/014.md".into(),
            revision: Some("abc123".into()),
            digest: None,
        }],
    );
    decision.affected_symbols.push(anchor.clone());
    store.record(&decision).unwrap();
    let mut failure = MemoryRecord::new(
        "regression-impact",
        MemoryKind::Regression,
        "Static cleanup removed refresh",
        "Removing the apparently unused entrypoint caused a production regression.",
        "repo",
        101,
        vec![SourceArtifact {
            kind: "incident".into(),
            uri: "incident:INC-42".into(),
            revision: Some("abc123".into()),
            digest: None,
        }],
    );
    failure.affected_symbols.push(anchor);
    store.record(&failure).unwrap();
    let server = server(store, false);

    for (tool, arguments) in [
        (
            "get_node",
            json!({"label": "refresh_token@src/auth/token.rs"}),
        ),
        (
            "affected",
            json!({"label": "refresh_token@src/auth/token.rs"}),
        ),
    ] {
        let result = call(&server, tool, arguments);
        let text = result["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("Repository memory evidence"),
            "{tool}: {text}"
        );
        assert!(text.contains("docs/adr/014.md"), "{tool}: {text}");
        assert!(text.contains("incident:INC-42"), "{tool}: {text}");
        assert_eq!(
            result["result"]["structuredContent"]["memory_evidence"]["total"], 2,
            "{tool}: {result}"
        );
        assert_eq!(
            result["result"]["structuredContent"]["memory_evidence"]["decisions"],
            1
        );
        assert_eq!(
            result["result"]["structuredContent"]["memory_evidence"]["pitfalls"],
            1
        );
    }
}

#[test]
fn configured_principal_filters_mcp_reads_and_owns_private_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let mut private = MemoryRecord::new(
        "alice-only",
        MemoryKind::AgentTask,
        "Alice-only refresh note",
        "Private authentication investigation.",
        "repo",
        100,
        vec![SourceArtifact {
            kind: "agent_task".into(),
            uri: "agent://alice/1".into(),
            revision: None,
            digest: None,
        }],
    );
    private.owner = Some("alice".into());
    store.record(&private).unwrap();

    let alice = server(store.clone(), true)
        .with_memory_principal(MemoryPrincipal::restricted("alice").with_repository("repo"));
    let bob = server(store.clone(), false)
        .with_memory_principal(MemoryPrincipal::restricted("bob").with_repository("repo"));
    assert_eq!(
        call(
            &alice,
            "search_memory",
            json!({"query": "Alice-only refresh"})
        )["result"]["structuredContent"]["total"],
        1
    );
    assert_eq!(
        call(
            &bob,
            "search_memory",
            json!({"query": "Alice-only refresh"})
        )["result"]["structuredContent"]["total"],
        0
    );

    let written = call(
        &alice,
        "record_change_outcome",
        json!({
            "idempotency_key": "alice-write",
            "title": "Alice private outcome",
            "summary": "A principal-owned private result.",
            "outcome": "succeeded",
            "source_uri": "agent://alice/2",
            "scope": "private"
        }),
    );
    assert_eq!(
        written["result"]["structuredContent"]["record"]["owner"], "alice",
        "{written}"
    );
    assert_eq!(
        call(
            &bob,
            "search_memory",
            json!({"query": "principal-owned private"})
        )["result"]["structuredContent"]["total"],
        0
    );
}

#[test]
fn multi_file_forecast_aggregates_deduplicated_memory_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let make_record = |key: &str, title: &str, symbol: &str, file: &str| {
        let mut record = MemoryRecord::new(
            key,
            MemoryKind::Regression,
            title,
            format!("Prior failure involving {file}."),
            "repo",
            100,
            vec![SourceArtifact {
                kind: "incident".into(),
                uri: format!("incident:{key}"),
                revision: Some("abc123".into()),
                digest: None,
            }],
        );
        record.access_scope = AccessScope::Repository;
        record.affected_symbols.push(SymbolAnchor {
            node_id: symbol.into(),
            label: symbol.into(),
            source_file: file.into(),
            repo: None,
            commit: Some("abc123".into()),
            confidence: 1.0,
        });
        record
    };
    store
        .record(&make_record(
            "auth-regression",
            "Authentication regression",
            "refresh_session",
            "src/auth.rs",
        ))
        .unwrap();
    store
        .record(&make_record(
            "billing-regression",
            "Billing regression",
            "render_invoice",
            "src/billing.rs",
        ))
        .unwrap();
    let graph = GraphData {
        nodes: vec![
            Node {
                id: NodeId("refresh_session".into()),
                label: "refresh_session".into(),
                file_type: FileType::Code,
                source_file: "src/auth.rs".into(),
                source_location: Some("L1".into()),
                community: Some(0),
                repo: None,
                extra: Map::new(),
            },
            Node {
                id: NodeId("render_invoice".into()),
                label: "render_invoice".into(),
                file_type: FileType::Code,
                source_file: "src/billing.rs".into(),
                source_location: Some("L1".into()),
                community: Some(1),
                repo: None,
                extra: Map::new(),
            },
        ],
        ..GraphData::default()
    };
    let server = Server::from_graph_data(graph, None)
        .with_memory_store(store)
        .with_memory_principal(MemoryPrincipal::restricted("reviewer").with_repository("repo"));

    let result = call(
        &server,
        "predict_impact",
        json!({"files": ["src/auth.rs", "src/billing.rs"]}),
    );
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert_eq!(
        result["result"]["structuredContent"]["memory_evidence"]["total"], 2,
        "{result}"
    );
    assert_eq!(
        result["result"]["structuredContent"]["memory_evidence"]["subjects"],
        json!(["src/auth.rs", "src/billing.rs"])
    );
    let text = result["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Authentication regression"), "{text}");
    assert!(text.contains("Billing regression"), "{text}");
}

struct PullRequestRunner;

impl CommandRunner for PullRequestRunner {
    fn run(&self, program: &str, args: &[&str]) -> Option<String> {
        if program != "gh" {
            return None;
        }
        match (args.first().copied(), args.get(1).copied()) {
            (Some("repo"), Some("view")) => Some(r#"{"defaultBranchRef":{"name":"main"}}"#.into()),
            (Some("pr"), Some("view")) => Some(
                json!({
                    "title": "Change authentication",
                    "headRefName": "feature/auth",
                    "baseRefName": "main",
                    "author": {"login": "alice"},
                    "isDraft": false,
                    "reviewDecision": "APPROVED",
                    "statusCheckRollup": [{"conclusion": "SUCCESS"}],
                    "updatedAt": "2026-07-29T00:00:00Z"
                })
                .to_string(),
            ),
            (Some("pr"), Some("diff")) => Some("src/auth.rs\n".into()),
            _ => None,
        }
    }
}

#[test]
fn pull_request_impact_aggregates_memory_for_changed_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let mut incident = MemoryRecord::new(
        "auth-incident",
        MemoryKind::Incident,
        "Authentication rollout incident",
        "A previous change to auth.rs caused a production incident.",
        "repo",
        100,
        vec![SourceArtifact {
            kind: "incident".into(),
            uri: "incident:AUTH-9".into(),
            revision: None,
            digest: None,
        }],
    );
    incident.access_scope = AccessScope::Repository;
    incident.affected_symbols.push(SymbolAnchor {
        node_id: "refresh_session".into(),
        label: "refresh_session".into(),
        source_file: "src/auth.rs".into(),
        repo: None,
        commit: None,
        confidence: 1.0,
    });
    store.record(&incident).unwrap();
    let graph = GraphData {
        nodes: vec![Node {
            id: NodeId("refresh_session".into()),
            label: "refresh_session".into(),
            file_type: FileType::Code,
            source_file: "src/auth.rs".into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
        }],
        ..GraphData::default()
    };
    let server = Server::from_graph_data(graph, None)
        .with_runner(Box::new(PullRequestRunner))
        .with_memory_store(store)
        .with_memory_principal(MemoryPrincipal::restricted("reviewer").with_repository("repo"));

    let result = call(&server, "get_pr_impact", json!({"pr_number": 42}));
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert_eq!(
        result["result"]["structuredContent"]["changed_files"],
        json!(["src/auth.rs"])
    );
    assert_eq!(
        result["result"]["structuredContent"]["memory_evidence"]["total"], 1,
        "{result}"
    );
    assert!(result["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Authentication rollout incident"));
}

struct WorkingTreeRunner;

impl CommandRunner for WorkingTreeRunner {
    fn run(&self, program: &str, args: &[&str]) -> Option<String> {
        if program != "git" {
            return None;
        }
        match args.first().copied() {
            Some("rev-parse") => Some("true\n".into()),
            Some("diff") => Some("src/auth.rs\n".into()),
            _ => None,
        }
    }
}

#[test]
fn working_changes_aggregate_memory_for_the_branch_file_set() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let mut incident = MemoryRecord::new(
        "working-auth-incident",
        MemoryKind::Incident,
        "Working-tree authentication pitfall",
        "A prior auth.rs edit failed in production.",
        "repo",
        100,
        vec![SourceArtifact {
            kind: "incident".into(),
            uri: "incident:WORK-9".into(),
            revision: None,
            digest: None,
        }],
    );
    incident.access_scope = AccessScope::Repository;
    incident.affected_symbols.push(SymbolAnchor {
        node_id: "refresh_session".into(),
        label: "refresh_session".into(),
        source_file: "src/auth.rs".into(),
        repo: None,
        commit: None,
        confidence: 1.0,
    });
    store.record(&incident).unwrap();
    let graph = GraphData {
        nodes: vec![Node {
            id: NodeId("refresh_session".into()),
            label: "refresh_session".into(),
            file_type: FileType::Code,
            source_file: "src/auth.rs".into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
        }],
        ..GraphData::default()
    };
    let server = Server::from_graph_data(graph, None)
        .with_runner(Box::new(WorkingTreeRunner))
        .with_memory_store(store)
        .with_memory_principal(MemoryPrincipal::restricted("reviewer").with_repository("repo"));
    let result = call(&server, "working_changes_impact", json!({"base": "main"}));
    assert_eq!(
        result["result"]["structuredContent"]["changed_files"],
        json!(["src/auth.rs"])
    );
    assert_eq!(
        result["result"]["structuredContent"]["memory_evidence"]["total"], 1,
        "{result}"
    );
}
