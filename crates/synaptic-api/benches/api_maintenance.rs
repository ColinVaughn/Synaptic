use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use serde_json::{json, Map};
use synaptic_api::{
    analyze_api_coverage, analyze_api_coverage_with_runtime, diff_contracts, evaluate_relevance,
    normalize_openapi, ApiChangeEvent, ApiContract, ApiInventory, ApiMaintenanceConfig, Dependency,
    DependencyScope, Ecosystem, PackageCoordinate, RuntimeEvidenceReport, RuntimeSurfaceEvidence,
    RuntimeSurfaceKind, SourceArtifact, VendorRegistry, VersionRange,
};
use synaptic_core::{FileType, GraphData, Node, NodeId};

fn contracts(operation_count: usize) -> (Vec<u8>, Vec<u8>) {
    let mut before = Map::new();
    let mut after = Map::new();
    for index in 0..operation_count {
        let path = format!("/v1/resources/{index}");
        let operation = json!({
            "post": {
                "operationId": format!("createResource{index}"),
                "requestBody": {"content":{"application/json":{"schema":{
                    "type":"object", "required":["name"],
                    "properties":{"name":{"type":"string"},"tag":{"type":"string"}}
                }}}},
                "responses":{"200":{"content":{"application/json":{"schema":{
                    "type":"object","properties":{"id":{"type":"string"},"status":{"type":"string"}}
                }}}}}
            }
        });
        before.insert(path.clone(), operation.clone());
        if index % 100 != 0 {
            after.insert(path, operation);
        }
    }
    (
        serde_json::to_vec(&json!({"openapi":"3.1.0","paths":before})).unwrap(),
        serde_json::to_vec(&json!({"openapi":"3.1.0","paths":after})).unwrap(),
    )
}

fn renamed_contracts(operation_count: usize) -> (ApiContract, ApiContract) {
    let mut before = Map::new();
    let mut after = Map::new();
    for index in 0..operation_count {
        let path = format!("/v1/resources/{index}");
        before.insert(
            path.clone(),
            json!({"post":{"operationId":format!("createResource{index}"),"responses":{"200":{"description":"ok"}}}}),
        );
        after.insert(
            path,
            json!({"post":{"operationId":format!("createResourceV2_{index}"),"responses":{"200":{"description":"ok"}}}}),
        );
    }
    let before = serde_json::to_vec(&json!({"openapi":"3.1.0","paths":before})).unwrap();
    let after = serde_json::to_vec(&json!({"openapi":"3.1.0","paths":after})).unwrap();
    (
        normalize_openapi("vendor", &before).unwrap(),
        normalize_openapi("vendor", &after).unwrap(),
    )
}

fn benchmark_contract_diff(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("api_contract_diff");
    for operation_count in [100, 1_000] {
        let (before, after) = contracts(operation_count);
        group.bench_with_input(
            BenchmarkId::from_parameter(operation_count),
            &operation_count,
            |bencher, _| {
                bencher.iter(|| {
                    let before = normalize_openapi("vendor", black_box(&before)).unwrap();
                    let after = normalize_openapi("vendor", black_box(&after)).unwrap();
                    diff_contracts(
                        &before,
                        &after,
                        SourceArtifact {
                            uri: "benchmark://contract".into(),
                            revision: "2".into(),
                            etag: None,
                            last_modified: None,
                            content_digest: after.digest.clone(),
                            fetched_at: 1,
                            adapter_version: 1,
                            evidence_kind: "openapi".into(),
                        },
                        VersionRange::any(),
                    )
                    .unwrap()
                });
            },
        );
    }
    group.finish();

    let mut group = criterion.benchmark_group("api_contract_diff_renamed");
    for operation_count in [100, 1_000] {
        let (before, after) = renamed_contracts(operation_count);
        group.bench_with_input(
            BenchmarkId::from_parameter(operation_count),
            &operation_count,
            |bencher, _| {
                bencher.iter(|| {
                    diff_contracts(
                        black_box(&before),
                        black_box(&after),
                        SourceArtifact {
                            uri: "benchmark://renamed-contract".into(),
                            revision: "2".into(),
                            etag: None,
                            last_modified: None,
                            content_digest: after.digest.clone(),
                            fetched_at: 1,
                            adapter_version: 1,
                            evidence_kind: "openapi".into(),
                        },
                        VersionRange::any(),
                    )
                    .unwrap()
                });
            },
        );
    }
    group.finish();
}

fn relevance_fixture(binding_count: usize) -> (VendorRegistry, ApiChangeEvent, GraphData) {
    let registry = VendorRegistry::new(
        ApiMaintenanceConfig::parse(
            r#"schema = 1
require_resolved_version = true
[[vendors]]
id = "vendor"
packages = ["npm:vendor-sdk"]
hosts = ["api.vendor.test"]
"#,
        )
        .unwrap(),
    )
    .unwrap();
    let operation =
        synaptic_api::ApiOperationAnchor::new("vendor", "https", "POST", "/v1/resources");
    let links = (0..binding_count)
        .map(|index| {
            json!({
                "source":format!("caller:{index}"), "target":operation.id,
                "relation":"uses_api", "confidence":"INFERRED", "confidence_score":0.99,
                "source_file":format!("src/client_{}.ts", index % 1_000),
                "source_location":format!("L{}", index + 1), "binding_basis":"absolute_url_host",
                "installed_sdk_version":"1.5.0"
            })
        })
        .collect::<Vec<_>>();
    let graph: GraphData = serde_json::from_value(json!({
        "directed":true,
        "nodes":[{"id":operation.id,"label":"POST /v1/resources","file_type":"concept","source_file":"","_node_type":"api_operation","vendor":"vendor"}],
        "links":links
    }))
    .unwrap();
    let event: ApiChangeEvent = serde_json::from_value(json!({
        "version":1,"id":"benchmark_event","vendor":"vendor","occurred_at":1,
        "source":{"uri":"benchmark://contract","revision":"2","content_digest":"digest","fetched_at":1,"adapter_version":1,"evidence_kind":"openapi"},
        "changes":[{"change_id":"change","kind":"operation_removed","affected_versions":{"requirement":"*"},"old_operation":operation,"old_sdk_symbols":[],"new_sdk_symbols":[],"migration_summary":"removed","evidence":[],"confidence":1.0}]
    })).unwrap();
    (registry, event, graph)
}

fn benchmark_relevance(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("api_relevance");
    for binding_count in [1_000, 10_000] {
        let (registry, event, graph) = relevance_fixture(binding_count);
        group.bench_with_input(
            BenchmarkId::from_parameter(binding_count),
            &binding_count,
            |bencher, _| {
                bencher.iter(|| {
                    evaluate_relevance(
                        black_box(&event),
                        black_box(&registry),
                        &ApiInventory::default(),
                        black_box(&graph),
                        &[],
                    )
                });
            },
        );
    }
    group.finish();
}

fn coverage_fixture(binding_count: usize) -> GraphData {
    let operation =
        synaptic_api::ApiOperationAnchor::new("vendor", "https", "POST", "/v1/resources");
    let mut links = Vec::with_capacity(binding_count * 2);
    for index in 0..binding_count {
        let source = format!("caller:{index}");
        let source_file = format!("src/client_{}.ts", index % 1_000);
        let source_location = format!("L{}", index + 1);
        links.push(json!({
            "source":source, "target":format!("sdk-call:{index}"), "relation":"calls_sdk",
            "confidence":"EXTRACTED", "source_file":source_file,
            "source_location":source_location, "sdk_package":"npm:vendor-sdk",
            "sdk_member_chain":"resources.create"
        }));
        links.push(json!({
            "source":source, "target":operation.id, "relation":"uses_api",
            "confidence":"INFERRED", "confidence_score":0.99,
            "source_file":source_file, "source_location":source_location,
            "binding_basis":"sdk_symbol", "sdk_package":"npm:vendor-sdk",
            "sdk_member_chain":"resources.create", "api_vendor":"vendor"
        }));
    }
    serde_json::from_value(json!({
        "directed":true,
        "nodes":[{"id":operation.id,"label":"POST /v1/resources","file_type":"concept","source_file":"","_node_type":"api_operation","vendor":"vendor"}],
        "links":links
    }))
    .unwrap()
}

fn benchmark_coverage(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("api_coverage");
    for binding_count in [1_000, 10_000] {
        let graph = coverage_fixture(binding_count);
        group.bench_with_input(
            BenchmarkId::from_parameter(binding_count),
            &binding_count,
            |bencher, _| {
                bencher.iter(|| analyze_api_coverage(black_box(&graph), black_box(&[]), None));
            },
        );
    }
    group.finish();

    let mut group = criterion.benchmark_group("api_runtime_coverage");
    for observation_count in [1_000, 10_000] {
        let (graph, runtime) = runtime_coverage_fixture(observation_count);
        group.bench_with_input(
            BenchmarkId::from_parameter(observation_count),
            &observation_count,
            |bencher, _| {
                bencher.iter(|| {
                    analyze_api_coverage_with_runtime(
                        black_box(&graph),
                        black_box(&[]),
                        None,
                        black_box(std::slice::from_ref(&runtime)),
                    )
                });
            },
        );
    }
    group.finish();
}

fn runtime_coverage_fixture(count: usize) -> (GraphData, RuntimeEvidenceReport) {
    let nodes = (0..count)
        .map(|index| {
            json!({
                "id":format!("function:{index}"), "label":format!("call_{index}"),
                "file_type":"code", "source_file":format!("src/client_{index}.ts")
            })
        })
        .collect::<Vec<_>>();
    let graph = serde_json::from_value(json!({
        "directed":true, "nodes":nodes, "links":[]
    }))
    .unwrap();
    let observations = (0..count)
        .map(|index| RuntimeSurfaceEvidence {
            id: format!("runtime:{index}"),
            kind: RuntimeSurfaceKind::Http,
            protocol: "https".into(),
            method: "GET".into(),
            authority: Some("api.vendor.test".into()),
            path: Some(format!("/v1/resources/{index}")),
            service: None,
            operation: None,
            source_file: Some(format!("src/client_{index}.ts")),
            source_line: None,
            source_function: None,
            evidence_digest: format!("{index:064x}"),
            occurrences: 2,
        })
        .collect();
    (
        graph,
        RuntimeEvidenceReport {
            version: 1,
            origin: "benchmark://runtime".into(),
            environment: Some("benchmark".into()),
            window_start_unix_nano: Some(1),
            window_end_unix_nano: Some(2),
            complete_window: true,
            spans_scanned: count,
            rejected_spans: 0,
            observations,
        },
    )
}

fn dependency_binding_fixture(
    node_count: usize,
    dependency_count: usize,
) -> (Vec<Node>, VendorRegistry, Vec<Dependency>) {
    let nodes = (0..node_count)
        .map(|index| Node {
            id: NodeId(format!("node:{index}")),
            label: format!("symbol_{index}"),
            file_type: FileType::Code,
            source_file: format!("src/file_{index}.rs"),
            source_location: None,
            community: None,
            repo: None,
            extra: serde_json::Map::new(),
        })
        .collect();
    let package_names = (0..dependency_count)
        .map(|index| format!("\"npm:vendor-sdk-{index}\""))
        .collect::<Vec<_>>()
        .join(",");
    let registry = VendorRegistry::new(
        ApiMaintenanceConfig::parse(&format!(
            "schema=1\n[[vendors]]\nid=\"vendor\"\npackages=[{package_names}]\n"
        ))
        .unwrap(),
    )
    .unwrap();
    let dependencies = (0..dependency_count)
        .map(|index| {
            let mut dependency = Dependency::new(
                PackageCoordinate::new(Ecosystem::Npm, format!("vendor-sdk-{index}")),
                format!("workspace_{index}/package.json"),
                DependencyScope::Runtime,
            );
            dependency.resolved_version = Some("1.0.0".into());
            dependency
        })
        .collect();
    (nodes, registry, dependencies)
}

fn benchmark_dependency_binding(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("api_dependency_binding");
    group.sample_size(20);
    for (node_count, dependency_count) in [(10_000, 100), (40_000, 200)] {
        let (nodes, registry, dependencies) =
            dependency_binding_fixture(node_count, dependency_count);
        group.bench_with_input(
            BenchmarkId::new(format!("{node_count}_nodes"), dependency_count),
            &(node_count, dependency_count),
            |bencher, _| {
                bencher.iter_batched(
                    || (nodes.clone(), Vec::new()),
                    |(mut nodes, mut edges)| {
                        black_box(synaptic_api::bind_sdk_dependencies(
                            &mut nodes,
                            &mut edges,
                            black_box(&registry),
                            black_box(&dependencies),
                        ))
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    benchmark_contract_diff,
    benchmark_relevance,
    benchmark_coverage,
    benchmark_dependency_binding
);
criterion_main!(benches);
