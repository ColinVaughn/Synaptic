use synaptic_api::{
    candidate_profile_toml, discover_contracts, normalize_contract, ApiMaintenanceConfig,
    ParseCompleteness, SurfaceFormat,
};

fn operation_protocol(source: &str) -> (String, SurfaceFormat, String) {
    let contract = normalize_contract("acme", source.as_bytes()).unwrap();
    let operation = contract.operations.values().next().expect("one operation");
    (
        operation.anchor.protocol.clone(),
        contract.format,
        operation.key.clone(),
    )
}

#[test]
fn auto_detects_asyncapi_and_openrpc() {
    let asyncapi = r#"{
      "asyncapi":"2.6.0",
      "channels":{"orders/created":{"publish":{"operationId":"publishOrder","message":{"payload":{"type":"object","required":["id"],"properties":{"id":{"type":"string"}}}}}}}
    }"#;
    let contract = normalize_contract("acme", asyncapi.as_bytes()).unwrap();
    assert_eq!(contract.format, SurfaceFormat::AsyncApi);
    assert_eq!(contract.completeness, ParseCompleteness::Partial);
    assert!(contract
        .losses
        .iter()
        .any(|loss| loss.reason.contains("format-specific policy")));
    let operation = &contract.operations["publishOrder"];
    assert_eq!(operation.anchor.protocol, "asyncapi");
    assert!(operation.request_fields["id"].required);

    let asyncapi_v3 = r##"{
      "asyncapi":"3.1.0",
      "channels":{"orders":{"address":"orders/{tenant}","messages":{"created":{"payload":{"type":"object","properties":{"id":{"type":"string"}}}}}}},
      "operations":{"receiveOrder":{"action":"receive","channel":{"$ref":"#/channels/orders"},"messages":[{"$ref":"#/channels/orders/messages/created"}]}}
    }"##;
    let contract = normalize_contract("acme", asyncapi_v3.as_bytes()).unwrap();
    assert_eq!(contract.operations.len(), 1);
    let operation = &contract.operations["receiveOrder"];
    assert_eq!(operation.anchor.method, "RECEIVE");
    assert_eq!(operation.anchor.canonical_path, "/orders/{tenant}");
    assert_eq!(operation.response_fields["id"].field_type, "string");

    let openrpc = r#"{
      "openrpc":"1.3.2",
      "methods":[{"name":"subtract","params":[{"name":"minuend","required":true,"schema":{"type":"number"}}],"result":{"name":"result","schema":{"type":"number"}}}]
    }"#;
    let contract = normalize_contract("acme", openrpc.as_bytes()).unwrap();
    assert_eq!(contract.format, SurfaceFormat::OpenRpc);
    assert_eq!(contract.completeness, ParseCompleteness::Partial);
    assert_eq!(contract.operations["subtract"].anchor.protocol, "jsonrpc");
    assert!(contract.operations["subtract"].request_fields["minuend"].required);
}

#[test]
fn auto_detects_graphql_sdl_and_introspection() {
    let sdl = r#"
      schema { query: Query }
      type Query { widget(id: ID!): Widget! }
      type Widget { id: ID! name: String }
    "#;
    let contract = normalize_contract("acme", sdl.as_bytes()).unwrap();
    assert_eq!(contract.format, SurfaceFormat::GraphQl);
    assert_eq!(contract.completeness, ParseCompleteness::Partial);
    let operation = &contract.operations["Query.widget"];
    assert_eq!(operation.anchor.protocol, "graphql");
    assert!(operation.request_fields["id"].required);
    assert_eq!(operation.response_fields["result"].field_type, "Widget!");

    let introspection = r#"{"data":{"__schema":{"queryType":{"name":"Query"},"types":[{"kind":"OBJECT","name":"Query","fields":[{"name":"health","args":[],"type":{"kind":"SCALAR","name":"String","ofType":null}}]}]}}}"#;
    let contract = normalize_contract("acme", introspection.as_bytes()).unwrap();
    assert_eq!(contract.format, SurfaceFormat::GraphQl);
    assert!(contract.operations.contains_key("Query.health"));
}

#[test]
fn graphql_sdl_honors_explicit_root_type_names() {
    let sdl = r#"
      schema {
        query: Root
        mutation: Commands
      }
      type Root {
        widget(id: ID!): Widget!
      }
      type Commands {
        renameWidget(id: ID!, name: String!): Widget
      }
      type Widget { id: ID! name: String }
    "#;

    let contract = normalize_contract("acme", sdl.as_bytes()).unwrap();

    assert!(contract.operations.contains_key("Root.widget"));
    assert!(contract.operations.contains_key("Commands.renameWidget"));
    assert_eq!(contract.operations["Root.widget"].anchor.method, "QUERY");
    assert_eq!(
        contract.operations["Commands.renameWidget"].anchor.method,
        "MUTATION"
    );
}

#[test]
fn auto_detects_protobuf_wsdl_and_smithy() {
    let proto = r#"
      syntax = "proto3";
      package acme.v1;
      message GetWidgetRequest { string id = 1; }
      message Widget { string id = 1; string name = 2; }
      service WidgetService { rpc GetWidget(GetWidgetRequest) returns (Widget); }
    "#;
    assert_eq!(
        operation_protocol(proto),
        (
            "grpc".into(),
            SurfaceFormat::Protobuf,
            "acme.v1.WidgetService.GetWidget".into()
        )
    );

    let wsdl = r#"<?xml version="1.0"?>
      <definitions xmlns="http://schemas.xmlsoap.org/wsdl/" targetNamespace="urn:acme">
        <portType name="WidgetPort"><operation name="GetWidget"><input message="tns:GetWidgetRequest"/><output message="tns:Widget"/></operation></portType>
      </definitions>"#;
    assert_eq!(
        operation_protocol(wsdl),
        (
            "soap".into(),
            SurfaceFormat::Wsdl,
            "WidgetPort.GetWidget".into()
        )
    );

    let smithy = r#"
      $version: "2"
      namespace acme.widgets
      service WidgetService { version: "2026-01-01", operations: [GetWidget] }
      operation GetWidget { input := { @required id: String }, output := { id: String } }
    "#;
    assert_eq!(
        operation_protocol(smithy),
        (
            "smithy".into(),
            SurfaceFormat::Smithy,
            "acme.widgets.GetWidget".into()
        )
    );
}

#[test]
fn readers_reject_unknown_and_oversized_documents() {
    assert!(normalize_contract("acme", b"definitely not a contract").is_err());
    let oversized = vec![b' '; 10 * 1024 * 1024 + 1];
    assert!(normalize_contract("acme", &oversized).is_err());
}

#[test]
fn unresolved_or_remote_references_make_normalization_explicitly_partial() {
    let contract = normalize_contract(
        "acme",
        br##"{"openapi":"3.1.0","paths":{"/x":{"post":{"requestBody":{"content":{"application/json":{"schema":{"$ref":"https://example.test/schema.json"}}}},"responses":{"200":{"description":"ok"}}}}}}"##,
    )
    .unwrap();
    assert_eq!(
        contract.completeness,
        synaptic_api::ParseCompleteness::Partial
    );
    assert_eq!(contract.losses.len(), 1);
    assert!(contract.losses[0].reason.contains("remote reference"));

    let unresolved = normalize_contract(
        "acme",
        br##"{"openapi":"3.1.0","paths":{"/x":{"get":{"responses":{"200":{"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Missing"}}}}}}}}}"##,
    )
    .unwrap();
    assert_eq!(
        unresolved.completeness,
        synaptic_api::ParseCompleteness::Partial
    );
    assert!(unresolved.losses[0].reason.contains("unresolved"));
}

#[test]
fn local_discovery_is_recursive_and_keeps_rejected_candidates_visible() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("contracts/nested")).unwrap();
    std::fs::write(
        repo.path().join("contracts/nested/service.proto"),
        r#"syntax = "proto3"; service Health { rpc Check(Req) returns (Res); }"#,
    )
    .unwrap();
    std::fs::write(repo.path().join("contracts/broken.graphql"), "type Query {").unwrap();
    std::fs::write(
        repo.path().join("package.json"),
        r#"{"name":"not-a-contract"}"#,
    )
    .unwrap();

    let report = discover_contracts(repo.path()).unwrap();
    assert_eq!(report.candidates_scanned, 2);
    assert_eq!(report.contracts.len(), 1);
    assert_eq!(report.contracts[0].format, SurfaceFormat::Protobuf);
    assert_eq!(report.contracts[0].path, "contracts/nested/service.proto");
    assert_eq!(report.rejected.len(), 1);
    assert_eq!(report.rejected[0].path, "contracts/broken.graphql");

    let candidate = candidate_profile_toml(&report).unwrap();
    let config = ApiMaintenanceConfig::parse(&candidate).unwrap();
    assert_eq!(config.vendors.len(), 1);
    assert!(!config.vendors[0].enabled);
    assert_eq!(config.vendors[0].sources.len(), 1);
    assert!(candidate.contains("review-only candidate"));
}
