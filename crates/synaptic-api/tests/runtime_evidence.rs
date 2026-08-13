use serde_json::{Map, json};
use synaptic_api::{
    CoverageState, Dependency, RuntimeSurfaceKind, analyze_api_coverage_with_runtime,
    import_runtime_evidence,
};
use synaptic_core::{DynamicKind, DynamicSite, FileType, GraphData, Node, NodeId};

#[test]
fn otlp_import_is_sanitized_bounded_and_protocol_neutral() {
    let payload = json!({
      "resourceSpans":[{
        "resource":{"attributes":[
          {"key":"deployment.environment.name","value":{"stringValue":"staging"}}
        ]},
        "scopeSpans":[{"spans":[
          {
            "startTimeUnixNano":"100", "endTimeUnixNano":"200",
            "attributes":[
              {"key":"http.request.method","value":{"stringValue":"GET"}},
              {"key":"server.address","value":{"stringValue":"api.acme.test"}},
              {"key":"url.path","value":{"stringValue":"/users/12345?token=secret"}},
              {"key":"http.request.header.authorization","value":{"stringValue":"Bearer secret"}}
            ]
          },
          {
            "startTimeUnixNano":"110", "endTimeUnixNano":"210",
            "attributes":[
              {"key":"rpc.system","value":{"stringValue":"grpc"}},
              {"key":"rpc.service","value":{"stringValue":"acme.v1.WidgetService"}},
              {"key":"rpc.method","value":{"stringValue":"GetWidget"}}
            ]
          }
        ]}]
      }]
    });

    let report =
        import_runtime_evidence("staging.otlp.json", &serde_json::to_vec(&payload).unwrap())
            .unwrap();
    assert!(report.complete_window);
    assert_eq!(report.environment.as_deref(), Some("staging"));
    assert_eq!(report.observations.len(), 2);
    assert_eq!(report.observations[0].path.as_deref(), Some("/users/:id"));
    assert_eq!(report.observations[0].kind, RuntimeSurfaceKind::Http);
    assert_eq!(report.observations[1].kind, RuntimeSurfaceKind::Rpc);
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(!encoded.contains("secret"));
    assert!(!encoded.contains("authorization"));
}

#[test]
fn runtime_targets_promote_dynamic_unknowns_without_becoming_impact_bindings() {
    let node = Node {
        id: NodeId("dynamic:1".into()),
        label: "dynamic call".into(),
        file_type: FileType::Code,
        source_file: "client.py".into(),
        source_location: Some("4".into()),
        community: None,
        repo: None,
        extra: {
            let mut extra = Map::new();
            extra.insert(
                "dynamic_sites".into(),
                serde_json::to_value(vec![DynamicSite {
                    kind: DynamicKind::Reflection,
                    line: 4,
                    snippet: "client[method]()".into(),
                    key: None,
                }])
                .unwrap(),
            );
            extra
        },
        ..Default::default()
    };
    let graph = GraphData {
        nodes: vec![node],
        links: vec![],
        ..GraphData::default()
    };
    let runtime = import_runtime_evidence(
        "test.otlp.json",
        &serde_json::to_vec(&json!({"resourceSpans":[{"scopeSpans":[{"spans":[{
          "startTimeUnixNano":"100", "endTimeUnixNano":"200",
          "attributes":[
            {"key":"http.request.method","value":{"stringValue":"POST"}},
            {"key":"server.address","value":{"stringValue":"api.acme.test"}},
            {"key":"http.route","value":{"stringValue":"/v1/widgets/{id}"}},
            {"key":"code.file.path","value":{"stringValue":"client.py"}},
            {"key":"code.function.name","value":{"stringValue":"dynamic call"}},
            {"key":"code.line.number","value":{"intValue":"4"}}
          ]
        },{
          "startTimeUnixNano":"101", "endTimeUnixNano":"201",
          "attributes":[
            {"key":"http.request.method","value":{"stringValue":"POST"}},
            {"key":"server.address","value":{"stringValue":"api.acme.test"}},
            {"key":"http.route","value":{"stringValue":"/v1/widgets/{id}"}},
            {"key":"code.file.path","value":{"stringValue":"client.py"}},
            {"key":"code.function.name","value":{"stringValue":"dynamic call"}},
            {"key":"code.line.number","value":{"intValue":"4"}}
          ]
        }]}]}]}))
        .unwrap(),
    )
    .unwrap();
    let report =
        analyze_api_coverage_with_runtime(&graph, &Vec::<Dependency>::new(), None, &[runtime]);
    assert!(
        report
            .observations
            .iter()
            .any(|observation| observation.state == CoverageState::Identified
                && observation.authority.as_deref() == Some("api.acme.test")
                && observation.source_node_id.as_deref() == Some("dynamic:1")
                && observation.source_location.as_deref() == Some("4"))
    );
    assert_eq!(report.evidence_windows.len(), 2);
    assert!(graph.links.iter().all(|edge| edge.relation != "uses_api"));
}
