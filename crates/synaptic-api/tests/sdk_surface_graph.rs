use serde_json::Map;
use synaptic_api::extract_sdk_surface_from_graph;
use synaptic_core::{FileType, GraphData, Node, NodeId, Param, Signature, Visibility};

fn symbol(id: &str, label: &str, file: &str, visibility: Visibility, raw: &str) -> Node {
    let mut node = Node {
        id: NodeId(id.into()),
        label: label.into(),
        file_type: FileType::Code,
        source_file: file.into(),
        source_location: Some("1".into()),
        community: None,
        repo: None,
        extra: Map::new(),
        ..Default::default()
    };
    node.set_visibility(visibility);
    node.set_signature(Signature {
        params: vec![Param {
            name: "request".into(),
            type_ref: None,
        }],
        return_type: None,
        raw: raw.into(),
    });
    node
}

#[test]
fn graph_surface_extraction_is_language_neutral_and_fail_closed() {
    let graph = GraphData {
        nodes: vec![
            symbol(
                "ts:function:widgets.create",
                "widgets.create",
                "src/client.ts",
                Visibility::Public,
                "(request: Widget): Promise<Widget>",
            ),
            symbol(
                "rust:function:delete_widget",
                "delete_widget",
                "src/lib.rs",
                Visibility::Public,
                "(id: &str) -> Result<()> ",
            ),
            symbol(
                "python:function:_secret",
                "_secret",
                "client.py",
                Visibility::Private,
                "(token)",
            ),
        ],
        ..GraphData::default()
    };
    let surface =
        extract_sdk_surface_from_graph("acme", "pkg:npm/%40acme/sdk@2.0.0", "2.0.0", None, &graph)
            .unwrap();
    assert_eq!(surface.exports.len(), 2);
    assert_eq!(
        surface.exports["widgets.create"],
        "(request: Widget): Promise<Widget>"
    );
    assert!(surface.exports.contains_key("delete_widget"));
    assert!(!surface.exports.contains_key("_secret"));
    assert!(surface.complete);
}

#[test]
fn duplicate_or_signatureless_public_symbols_are_retained_as_losses() {
    let mut first = symbol(
        "go:function:Open",
        "Open",
        "a/client.go",
        Visibility::Public,
        "(id string)",
    );
    // `signature` is a typed Node field: clearing it via `extra` would be a
    // silent no-op.
    first.signature = None;
    let second = symbol(
        "java:method:Open",
        "Open",
        "b/Client.java",
        Visibility::Public,
        "(String id)",
    );
    let graph = GraphData {
        nodes: vec![first, second],
        ..GraphData::default()
    };
    let surface =
        extract_sdk_surface_from_graph("acme", "generic:acme-sdk", "1", None, &graph).unwrap();
    assert!(!surface.complete);
    assert_eq!(surface.losses.len(), 2);
    assert!(surface.exports.keys().all(|key| key.contains("::Open")));
}
