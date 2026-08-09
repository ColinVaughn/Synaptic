//! Cost of a dependency vulnerability audit.
//!
//! The workload is sized to a real repository rather than a toy: roughly the
//! 479 resolved packages and 2,698 cargo advisories Synaptic itself scans, so a
//! regression here is one an operator would actually feel.
//!
//! Groups whose names begin `baseline/` exist in both this crate and the
//! release before dependency-scope and CVSS v4 scoring were added, and are the
//! ones to compare across versions. The rest measure paths that are new, where
//! the question is whether they cost anything worth noticing at all.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use synaptic_api::{Dependency, DependencyScope, Ecosystem, PackageCoordinate};
use synaptic_vuln::{
    assess_severity, cvss_v3_base_score, cvss_v4_base_score, feature_gated_dependencies,
    manifest_features, parse_lockfile, scan, Advisory, LocalDirSource, LockfileKind,
    NoUsageEvidence, PackageGraph, ScanRequest, UsageOracle,
};

/// A `Cargo.lock` with `count` registry packages in a chain off one workspace
/// member, which is the shape that exercises both the graph walk and the
/// dependency-path search.
fn cargo_lock(count: usize) -> String {
    let mut out = String::from(
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n",
    );
    for index in 0..count {
        out.push_str(&format!(" \"dep{index}\",\n"));
    }
    out.push_str("]\n");
    for index in 0..count {
        out.push_str(&format!(
            "\n[[package]]\nname = \"dep{index}\"\nversion = \"1.0.{}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"c{index}\"\n",
            index % 50
        ));
        // Every fourth package depends on the next one, so the graph has real
        // depth rather than being a flat fan-out.
        if index % 4 == 0 && index + 1 < count {
            out.push_str(&format!("dependencies = [\n \"dep{}\",\n]\n", index + 1));
        }
    }
    out
}

/// An advisory affecting `dep<index>`, alternating between a v3 vector, a v4
/// vector, and no severity at all, which is the mix a real corpus has.
fn advisory(index: usize) -> Advisory {
    let severity = match index % 3 {
        0 => r#"[{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }]"#,
        1 => {
            r#"[{ "type": "CVSS_V4", "score": "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N" }]"#
        }
        _ => "[]",
    };
    Advisory::parse(&format!(
        r#"{{
            "id": "BENCH-{index}",
            "summary": "dep{index} is vulnerable",
            "severity": {severity},
            "affected": [
                {{
                    "package": {{ "ecosystem": "crates.io", "name": "dep{index}" }},
                    "ranges": [
                        {{ "type": "SEMVER", "events": [{{ "introduced": "0" }}, {{ "fixed": "2.0.0" }}] }}
                    ]
                }}
            ]
        }}"#
    ))
    .expect("bench advisory parses")
}

/// A corpus of `total` advisories, of which the first `affecting` name a
/// package the lockfile actually resolves.
fn corpus(total: usize, affecting: usize) -> LocalDirSource {
    let advisories = (0..total)
        .map(|index| {
            if index < affecting {
                advisory(index)
            } else {
                // Names a package nothing resolves, so it is indexed and
                // skipped exactly as most of a real corpus is.
                advisory(index + 1_000_000)
            }
        })
        .collect();
    LocalDirSource::from_advisories("bench-corpus", advisories)
}

fn direct_dependencies(count: usize) -> Vec<Dependency> {
    (0..count)
        .map(|index| {
            let scope = if index % 5 == 0 {
                DependencyScope::Development
            } else {
                DependencyScope::Runtime
            };
            let mut dependency = Dependency::new(
                PackageCoordinate::new(Ecosystem::Cargo, format!("dep{index}")),
                "Cargo.toml",
                scope,
            );
            dependency.resolved_version = Some(format!("1.0.{}", index % 50));
            dependency
        })
        .collect()
}

/// The headline number: a whole scan, corpus already loaded.
fn bench_scan(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("baseline/scan");
    for packages in [100_usize, 500] {
        let graph = PackageGraph::from_cargo_lock(&cargo_lock(packages)).expect("lockfile parses");
        let source = corpus(2_700, packages / 2);
        let direct = direct_dependencies(packages / 10);
        group.bench_with_input(
            BenchmarkId::from_parameter(packages),
            &packages,
            |bencher, _| {
                bencher.iter(|| {
                    let request = ScanRequest {
                        repository_identity: "bench",
                        packages: &graph,
                        direct_dependencies: &direct,
                        source: &source,
                        policy: None,
                        usage: &NoUsageEvidence,
                        reach: None,
                        impact: None,
                        validation_commands: Vec::new(),
                        today: "2026-08-06".into(),
                        covered_ecosystems: [Ecosystem::Cargo].into_iter().collect(),
                        feature_gated: Default::default(),
                    };
                    black_box(scan(&request).expect("scan succeeds"))
                })
            },
        );
    }
    group.finish();
}

/// Reading the lockfile, which now also reads dependency scope.
fn bench_lockfile(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("baseline/lockfile");
    let cargo = cargo_lock(500);
    group.bench_function("cargo_lock_500", |bencher| {
        bencher.iter(|| black_box(PackageGraph::from_cargo_lock(black_box(&cargo)).unwrap()))
    });

    const NPM: &str = r#"{
      "name": "app",
      "lockfileVersion": 3,
      "packages": {
        "": { "name": "app", "version": "1.0.0" },
        "node_modules/lodash": { "version": "4.17.20", "dependencies": { "tiny": "^1.0.0" } },
        "node_modules/tiny": { "version": "1.0.0" },
        "node_modules/jest": { "version": "29.0.0", "dev": true }
      }
    }"#;
    group.bench_function("npm_package_lock_scope", |bencher| {
        bencher.iter(|| {
            black_box(parse_lockfile(LockfileKind::NpmPackageLock, black_box(NPM)).unwrap())
        })
    });
    group.finish();
}

/// Severity assessment, per advisory, inside the scan's inner loop.
fn bench_severity(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("severity");
    let v3 = advisory(0);
    let v4 = advisory(1);
    let none = advisory(2);

    group.bench_function("baseline/assess_v3", |bencher| {
        bencher.iter(|| black_box(assess_severity(black_box(&v3))))
    });
    group.bench_function("assess_v4", |bencher| {
        bencher.iter(|| black_box(assess_severity(black_box(&v4))))
    });
    group.bench_function("baseline/assess_unscored", |bencher| {
        bencher.iter(|| black_box(assess_severity(black_box(&none))))
    });
    group.finish();
}

/// The two scorers head to head. v4 does a table lookup and a maximal-vector
/// search where v3 evaluates a closed-form expression, so this is where a
/// surprise would show up.
fn bench_cvss(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("cvss");
    const V3: &str = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H";
    const V4: &str = "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:H/SI:H/SA:H";
    // The worst case for the maximal-vector search: a mid-table macrovector
    // whose first candidate combinations are rejected.
    const V4_DEEP: &str = "CVSS:4.0/AV:A/AC:H/AT:P/PR:L/UI:P/VC:L/VI:L/VA:H/SC:L/SI:L/SA:H";

    group.bench_function("baseline/v3_base_score", |bencher| {
        bencher.iter(|| black_box(cvss_v3_base_score(black_box(V3))))
    });
    group.bench_function("v4_base_score", |bencher| {
        bencher.iter(|| black_box(cvss_v4_base_score(black_box(V4))))
    });
    group.bench_function("v4_base_score_deep_search", |bencher| {
        bencher.iter(|| black_box(cvss_v4_base_score(black_box(V4_DEEP))))
    });
    group.finish();
}

/// Runtime reachability, computed once per scan over the whole graph.
fn bench_reachability(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("reachability");
    for packages in [100_usize, 500] {
        let graph = PackageGraph::from_cargo_lock(&cargo_lock(packages)).expect("lockfile parses");
        let development = (0..packages / 10)
            .map(|index| PackageCoordinate::new(Ecosystem::Cargo, format!("dep{index}")))
            .collect();
        group.bench_with_input(
            BenchmarkId::from_parameter(packages),
            &packages,
            |bencher, _| {
                bencher.iter(|| black_box(graph.runtime_reachable_keys(black_box(&development))))
            },
        );
    }
    group.finish();
}

/// Cargo feature resolution, run once per scan.
fn bench_features(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("features");
    const MANIFEST: &str = r#"
[package]
name = "app"

[dependencies]
serde = { version = "1", optional = true }
tokio = { version = "1", optional = true }
anyhow = "1"

[dev-dependencies]
tempfile = "3"

[features]
default = ["json"]
json = ["dep:serde"]
async = ["dep:tokio", "json"]
"#;
    group.bench_function("manifest_features", |bencher| {
        bencher.iter(|| black_box(manifest_features(black_box(MANIFEST))))
    });

    // The walk over a repository's manifests, which is the part that touches
    // the filesystem.
    let directory = tempfile::tempdir().expect("tempdir");
    for index in 0..30 {
        let member = directory.path().join(format!("crate{index}"));
        std::fs::create_dir_all(&member).expect("member dir");
        std::fs::write(member.join("Cargo.toml"), MANIFEST).expect("member manifest");
    }
    group.bench_function("feature_gated_dependencies_30_manifests", |bencher| {
        bencher.iter(|| black_box(feature_gated_dependencies(black_box(directory.path()))))
    });
    group.finish();
}

/// The graph-backed usage oracle.
///
/// The scan asks it three questions for every advisory that names a resolved
/// package, so its per-query cost is multiplied by the finding count. Deriving
/// the answer per call rather than indexing once measured 563 us here; the
/// index build is paid a single time.
fn bench_usage_oracle(criterion: &mut Criterion) {
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};

    fn node(id: &str, label: &str, source_file: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: source_file.into(),
            source_location: None,
            community: None,
            repo: None,
            extra: Default::default(),
            ..Default::default()
        }
    }

    // A graph on the order of a real one: 12,000 first-party nodes, 1,000 SDK
    // stubs, and an edge into each stub.
    let mut nodes = Vec::new();
    let mut links = Vec::new();
    for index in 0..12_000 {
        nodes.push(node(
            &format!("n{index}"),
            &format!("fn{index}"),
            "src/lib.rs",
        ));
    }
    for index in 0..1_000 {
        let id = format!("stub{index}");
        nodes.push(node(&id, &format!("Sdk: cargo:dep{index}#Value.get"), ""));
        links.push(Edge {
            source: NodeId(format!("n{index}")),
            target: NodeId(id),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: "src/lib.rs".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Default::default(),
        });
    }
    let graph = GraphData {
        nodes,
        links,
        ..Default::default()
    };

    let mut group = criterion.benchmark_group("usage_oracle");
    group.bench_function("build_index_once", |bencher| {
        bencher.iter(|| black_box(synaptic_vuln::GraphUsageOracle::new(black_box(&graph))))
    });

    let oracle = synaptic_vuln::GraphUsageOracle::new(&graph);
    let package = PackageCoordinate::new(Ecosystem::Cargo, "dep500");
    group.bench_function("first_party_usage", |bencher| {
        bencher.iter(|| black_box(oracle.first_party_usage(black_box(&package))))
    });
    // What one advisory match actually costs: the three questions the scan asks.
    group.bench_function("three_queries_per_finding", |bencher| {
        bencher.iter(|| {
            black_box(oracle.first_party_usage(black_box(&package)));
            black_box(oracle.dynamic_hazard(black_box(&package)));
            black_box(oracle.reachable_functions(black_box(&package), &["Value.get".to_string()]))
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_scan,
    bench_usage_oracle,
    bench_lockfile,
    bench_severity,
    bench_cvss,
    bench_reachability,
    bench_features
);
criterion_main!(benches);
