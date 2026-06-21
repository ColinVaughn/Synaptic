//! Plan-generation benchmark: a hot target referenced by many call sites across
//! many files, on a graph of a few thousand nodes.

use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion};
use serde_json::Map;
use synaptic_core::{Confidence, Edge, GraphData, Node, NodeId, NodeKind, Span};
use synaptic_graph::KnowledgeGraph;
use synaptic_refactor::{plan_rename, RenameOptions};

const FILES: usize = 40;
const CALLS_PER_FILE: usize = 3;
const FILLER_NODES: usize = 2000;

fn code_node(id: &str, label: &str, file: &str, kind: NodeKind) -> Node {
    let mut n = Node {
        id: NodeId(id.into()),
        label: label.into(),
        file_type: synaptic_core::FileType::Code,
        source_file: file.into(),
        source_location: Some("L1".into()),
        community: None,
        repo: None,
        extra: Map::new(),
    };
    n.set_kind(kind);
    n.set_span(Span {
        start_line: 1,
        start_col: 1,
        end_line: 3,
        end_col: 2,
    });
    n
}

fn call_edge(src: &str, tgt: &str, file: &str) -> Edge {
    Edge {
        source: NodeId(src.into()),
        target: NodeId(tgt.into()),
        relation: "calls".into(),
        confidence: Confidence::Extracted,
        source_file: file.into(),
        source_location: Some("L2".into()),
        confidence_score: Some(1.0),
        weight: 1.0,
        context: Some("call".into()),
        cross_repo: false,
        extra: Map::new(),
    }
}

/// Build the graph and materialize the referencing files under `root` so the
/// AST-cache recovery path has real bytes to read.
fn fixture(root: &Path) -> KnowledgeGraph {
    let mut nodes = vec![code_node(
        "models::User",
        "User",
        "models.py",
        NodeKind::Class,
    )];
    std::fs::write(root.join("models.py"), b"class User:\n    pass\n").unwrap();
    let mut links = Vec::new();
    for f in 0..FILES {
        let file = format!("svc_{f}.py");
        let mut body = String::from("from models import User\n");
        for c in 0..CALLS_PER_FILE {
            let caller = format!("svc_{f}::fn_{c}");
            nodes.push(code_node(
                &caller,
                &format!("fn_{c}()"),
                &file,
                NodeKind::Function,
            ));
            links.push(call_edge(&caller, "models::User", &file));
            body.push_str(&format!("def fn_{c}():\n    return User()\n"));
        }
        std::fs::write(root.join(&file), body.as_bytes()).unwrap();
    }
    for i in 0..FILLER_NODES {
        nodes.push(code_node(
            &format!("filler::n{i}"),
            &format!("n{i}()"),
            "filler.py",
            NodeKind::Function,
        ));
    }
    KnowledgeGraph::from_graph_data(GraphData {
        directed: true,
        multigraph: false,
        graph: Map::new(),
        nodes,
        links,
        hyperedges: vec![],
        built_at_commit: None,
    })
}

fn bench_plan(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let kg = fixture(dir.path());
    let opts = RenameOptions::default();
    c.bench_function("plan_rename_hot_target", |b| {
        b.iter(|| {
            let plan = plan_rename(&kg, "User", "Account", dir.path(), &opts).unwrap();
            criterion::black_box(plan.blast_radius.edit_count)
        })
    });
}

criterion_group!(benches, bench_plan);
criterion_main!(benches);
