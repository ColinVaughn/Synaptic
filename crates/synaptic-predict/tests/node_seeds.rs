use synaptic_core::{GraphData, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_predict::{forecast_nodes, ForecastOptions};

#[test]
fn arbitrary_operation_seed_forecasts_wrappers_and_tests() {
    let data: GraphData = serde_json::from_value(serde_json::json!({
        "directed": true,
        "nodes": [
            {"id":"api_operation:acme:create", "label":"POST /v1/widgets", "file_type":"concept", "source_file":"", "_node_type":"api_operation"},
            {"id":"fn:create_widget", "label":"create_widget", "file_type":"code", "source_file":"src/client.ts", "kind":"function"},
            {"id":"test:create_widget", "label":"create_widget test", "file_type":"code", "source_file":"test/client.test.ts", "kind":"function", "_is_test":true}
        ],
        "links": [
            {"source":"fn:create_widget", "target":"api_operation:acme:create", "relation":"uses_api", "confidence":"EXTRACTED", "source_file":"src/client.ts"},
            {"source":"test:create_widget", "target":"fn:create_widget", "relation":"calls", "confidence":"EXTRACTED", "source_file":"test/client.test.ts"}
        ]
    }))
    .unwrap();
    let graph = KnowledgeGraph::from_graph_data(data);
    let forecast = forecast_nodes(
        &graph,
        &[NodeId("api_operation:acme:create".into())],
        &ForecastOptions::default(),
    );

    assert_eq!(forecast.changed_nodes[0].id, "api_operation:acme:create");
    assert_eq!(forecast.blast_radius_total, 2);
    assert_eq!(forecast.at_risk_tests[0].file, "test/client.test.ts");
}
