//! Criterion benchmarks for `synaptic-workspace` federation kernels.
//!
//!   * `prefix_graph` — namespace one member's graph (D2).
//!   * `compose` — union R per-repo graphs into a federated graph (D2 core).
//!   * `build_export_surface` — extract a member's public symbol surface (D3 in).
//!   * `resolve_cross_repo` — the D3 hard path: rewire import-target external
//!     stubs to their owning repo, on a synthetic two-repo (exporter/importer)
//!     scenario.
//!
//! Run: `cargo bench -p synaptic-workspace`

use std::path::PathBuf;

use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_workspace::alias::collect_aliases;
use synaptic_workspace::coordinate::{Coordinate, Ecosystem};
use synaptic_workspace::export_surface::{build_export_surface, resolve_cross_repo, ExportSurface};
use synaptic_workspace::federate::{compose, prefix_graph};

fn node(id: String, label: String, source_file: String, repo: Option<String>) -> Node {
    Node {
        id: NodeId(id),
        label,
        file_type: FileType::Code,
        source_file,
        source_location: Some("L1".to_string()),
        community: None,
        repo,
        extra: serde_json::Map::new(),
    }
}

fn import_edge(src: String, dst: String) -> Edge {
    Edge {
        source: NodeId(src),
        target: NodeId(dst),
        relation: "imports_from".to_string(),
        confidence: Confidence::Extracted,
        source_file: "beta/src/lib.rs".to_string(),
        source_location: None,
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: serde_json::Map::new(),
    }
}

fn gd(nodes: Vec<Node>, links: Vec<Edge>) -> GraphData {
    GraphData {
        directed: false,
        multigraph: false,
        graph: serde_json::Map::new(),
        nodes,
        links,
        hyperedges: vec![],
        built_at_commit: None,
    }
}

/// A single member's graph: `n` code symbols under `tag`, ring-linked.
fn member_graph(n: usize, tag: &str) -> GraphData {
    let nodes = (0..n)
        .map(|i| {
            node(
                format!("{tag}::s{i}"),
                format!("Sym_{tag}_{i}"),
                format!("{tag}/src/mod_{}.rs", i % 32),
                Some(tag.to_string()),
            )
        })
        .collect();
    gd(nodes, vec![])
}

fn coord(name: &str) -> Coordinate {
    Coordinate {
        ecosystem: Ecosystem::Cargo,
        name: name.to_string(),
    }
}

/// Two-repo scenario: `alpha` exports `Sym{i}`; `beta` imports a quarter of them
/// through external stubs (empty `source_file`). Returns the combined graph plus
/// both export surfaces, ready for `resolve_cross_repo`.
fn cross_repo_scenario(n: usize) -> (GraphData, Vec<ExportSurface>) {
    let alpha_nodes: Vec<Node> = (0..n)
        .map(|i| {
            node(
                format!("alpha::a{i}"),
                format!("Sym{i}"),
                format!("alpha/src/mod_{}.rs", i % 32),
                Some("alpha".to_string()),
            )
        })
        .collect();
    let alpha_gd = gd(alpha_nodes.clone(), vec![]);

    let mut beta_nodes: Vec<Node> = (0..n)
        .map(|i| {
            node(
                format!("beta::b{i}"),
                format!("BSym{i}"),
                "beta/src/lib.rs".to_string(),
                Some("beta".to_string()),
            )
        })
        .collect();
    let mut beta_links = Vec::new();
    for i in (0..n).step_by(4) {
        let stub_id = format!("beta::stub{i}");
        // External stub: empty source_file, label matches an alpha symbol.
        beta_nodes.push(node(
            stub_id.clone(),
            format!("Sym{i}"),
            String::new(),
            Some("beta".to_string()),
        ));
        beta_links.push(import_edge(format!("beta::b{i}"), stub_id));
    }
    let beta_gd = gd(beta_nodes.clone(), beta_links.clone());

    let surfaces = vec![
        build_export_surface("alpha", coord("alpha"), &alpha_gd),
        build_export_surface("beta", coord("beta"), &beta_gd),
    ];

    let mut nodes = alpha_nodes;
    nodes.extend(beta_nodes);
    (gd(nodes, beta_links), surfaces)
}

/// A federation member tree for the alias walk: `members` members, each with an
/// import-map (referencing two siblings), a `package.json`, and 8 noise `.js` files
/// (≈10 candidate files/member). Import-map-only so the workload is identical to the
/// pre-refactor `member_alias_map` walk — isolating the multi-parser dispatch overhead.
fn build_alias_tree(members: usize) -> (tempfile::TempDir, Vec<(String, PathBuf)>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let write = |path: PathBuf, body: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    };
    let mut list = Vec::new();
    for m in 0..members {
        let tag = format!("app{m}");
        let mroot = root.join(&tag);
        let (a, b) = ((m + 1) % members, (m + 2) % members);
        write(
            mroot.join("public/index.ejs"),
            &format!(
                r#"<script type="systemjs-importmap"></script>
                   <script>const m = {{ imports: {{
                     "@scope/App{a}": `/app{a}/dist/index.js`,
                     "@scope/App{b}": `/app{b}/dist/index.js`
                   }} }}</script>"#
            ),
        );
        for f in 0..8 {
            write(
                mroot.join(format!("src/file{f}.js")),
                "export function thing() { return 1 + 2 + 3; }\nconst k = [1,2,3].map(x => x*2);\n",
            );
        }
        write(mroot.join("package.json"), "{\"name\":\"app\"}");
        list.push((tag, mroot));
    }
    (dir, list)
}

fn bench_collect_aliases(c: &mut Criterion) {
    let mut group = c.benchmark_group("workspace");
    for &members in &[16usize, 64] {
        let (_dir, list) = build_alias_tree(members);
        group.throughput(Throughput::Elements(members as u64));
        group.bench_with_input(
            BenchmarkId::new("collect_aliases", members),
            &members,
            |b, _| b.iter(|| black_box(collect_aliases(&list))),
        );
    }
    group.finish();
}

fn bench_workspace(c: &mut Criterion) {
    let mut group = c.benchmark_group("workspace");

    for &n in &[1_000usize, 10_000] {
        let member = member_graph(n, "alpha");
        group.throughput(Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("prefix_graph", n), &n, |b, _| {
            b.iter_batched(
                || member.clone(),
                |g| black_box(prefix_graph(g, "alpha")),
                BatchSize::SmallInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("build_export_surface", n), &n, |b, _| {
            b.iter(|| black_box(build_export_surface("alpha", coord("alpha"), &member)));
        });
    }

    // compose: 16 members of 500 nodes each (~8k-node federated graph).
    let subgraphs: Vec<(String, GraphData)> = (0..16)
        .map(|r| {
            let tag = format!("repo{r}");
            let g = member_graph(500, &tag);
            (tag, g)
        })
        .collect();
    group.throughput(Throughput::Elements(16 * 500));
    group.bench_function("compose/16x500", |b| {
        b.iter_batched(
            || subgraphs.clone(),
            |subs| black_box(compose(subs)),
            BatchSize::SmallInput,
        );
    });

    // resolve_cross_repo: the D3 hot path, two repos.
    for &n in &[1_000usize, 5_000] {
        let (combined, surfaces) = cross_repo_scenario(n);
        group.throughput(Throughput::Elements(combined.nodes.len() as u64));
        group.bench_with_input(BenchmarkId::new("resolve_cross_repo", n), &n, |b, _| {
            b.iter_batched(
                || combined.clone(),
                |g| {
                    black_box(resolve_cross_repo(
                        g,
                        &surfaces,
                        &synaptic_workspace::alias::AliasMap::default(),
                    ))
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_workspace, bench_collect_aliases);
criterion_main!(benches);
