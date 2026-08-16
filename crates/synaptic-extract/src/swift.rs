//! Swift extractor — Bucket A (declarative `LanguageConfig` + protocol pre-scan).
//! Covers the Swift config, imports, base-type classification, and
//! type-reference collection.
//!
//! Classes/protocols → bare name; functions → `.name()`; `import Foundation`
//! → `imports` edge; `inheritance_specifier` bases give `inherits` (the class
//! base) / `implements` (protocols, via a pre-scan of `protocol_declaration`
//! names); parameter/return types → `references`.

#[cfg(feature = "lang-swift")]
use crate::config::{HeritageStyle, ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-swift")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-swift")]
use crate::walker::extract_with_config;
#[cfg(feature = "lang-swift")]
use synaptic_core::{Confidence, Edge, FileType, Node, NodeId, NodeKind, make_id};

/// The Swift `LanguageConfig`. `class_declaration` also covers struct/enum/actor
/// in this grammar. The callee of a `call_expression` is named positionally, so
/// this relies on the walker's empty-`call_function_field` fallback.
#[cfg(feature = "lang-swift")]
pub fn swift_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_swift::LANGUAGE.into(),
        class_types: &[
            "class_declaration",
            "protocol_declaration",
            "extension_declaration",
        ],
        function_types: &["function_declaration", "protocol_function_declaration"],
        call_types: &["call_expression"],
        name_field: "name",
        body_field: "body",
        call_function_field: "",
        call_accessor_node_types: &[],
        call_accessor_field: "",
        function_boundary_types: &["function_declaration", "protocol_function_declaration"],
        superclasses_field: None,
        decorated_types: &[],
        builtins: &[],
        import_types: &["import_declaration"],
        import_style: Some(ImportStyle::Swift),
        type_ref_style: Some(TypeRefStyle::Swift),
        heritage_style: Some(HeritageStyle::Swift),
        constructor_call_type: None,
        body_kinds: &[],
    }
}

/// Extract a Swift source file already in memory.
#[cfg(feature = "lang-swift")]
pub fn extract_swift_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut result = extract_with_config(path, source, &swift_config());
    recover_swift_declarations(path, source, &mut result);
    result
}

/// Tree-sitter Swift can recover a malformed multiline string by swallowing the
/// rest of an extension. Recover only declarations absent from the AST result.
#[cfg(feature = "lang-swift")]
fn recover_swift_declarations(path: &str, source: &[u8], result: &mut ExtractionResult) {
    use serde_json::Map;

    let stem = crate::paths::file_stem(path);
    let file = crate::paths::file_node_id(path);
    let mut owner: Option<(String, NodeId)> = None;
    for (offset, raw) in String::from_utf8_lossy(source).lines().enumerate() {
        let line = offset + 1;
        let trimmed = raw.trim();
        if raw.len() == trimmed.len() {
            let words: Vec<_> = trimmed.split_whitespace().collect();
            if let Some(i) = words.iter().position(|word| {
                matches!(*word, "class" | "struct" | "enum" | "actor" | "extension")
            }) && let Some(name) = words.get(i + 1).map(|name| {
                name.trim_end_matches(['{', ':'])
                    .split(['<', ':'])
                    .next()
                    .unwrap_or("")
            }) && !name.is_empty()
            {
                let id = NodeId(make_id(&[&stem, name]));
                if !result.nodes.iter().any(|node| node.id == id) {
                    let mut node = Node {
                        id: id.clone(),
                        label: name.to_string(),
                        file_type: FileType::Code,
                        source_file: path.to_string().into(),
                        source_location: Some(format!("L{line}")),
                        origin: Some("ast_recovery".into()),
                        extra: Map::new(),
                        ..Default::default()
                    };
                    node.set_kind(NodeKind::Class);
                    result.nodes.push(node);
                    result
                        .edges
                        .push(swift_edge(file.clone(), id.clone(), "contains", path, line));
                }
                owner = Some((name.to_string(), id));
            }
        }

        let words: Vec<_> = trimmed.split_whitespace().collect();
        let Some(i) = words.iter().position(|word| *word == "func") else {
            continue;
        };
        let Some(name) = words
            .get(i + 1)
            .and_then(|name| name.split(['(', '<']).next())
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        if result.nodes.iter().any(|node| {
            node.source_file == path
                && node.source_location.as_deref() == Some(&format!("L{line}"))
                && node.label.trim_start_matches('.').trim_end_matches("()") == name
        }) {
            continue;
        }
        let (id, label, parent, relation, kind) = match &owner {
            Some((_, owner_id)) if raw.len() > trimmed.len() => (
                NodeId(make_id(&[owner_id.as_str(), name])),
                format!(".{name}()"),
                owner_id.clone(),
                "method",
                NodeKind::Method,
            ),
            _ => (
                NodeId(make_id(&[&stem, name])),
                format!("{name}()"),
                file.clone(),
                "contains",
                NodeKind::Function,
            ),
        };
        let id = if result
            .nodes
            .iter()
            .any(|node| node.id == id && node.label != label)
        {
            NodeId(make_id(&[id.as_str(), &line.to_string()]))
        } else {
            id
        };
        if result.nodes.iter().any(|node| node.id == id) {
            continue;
        }
        let mut node = Node {
            id: id.clone(),
            label,
            file_type: FileType::Code,
            source_file: path.to_string().into(),
            source_location: Some(format!("L{line}")),
            origin: Some("ast_recovery".into()),
            extra: Map::new(),
            ..Default::default()
        };
        node.set_kind(kind);
        result.nodes.push(node);
        result
            .edges
            .push(swift_edge(parent, id, relation, path, line));
    }
}

#[cfg(feature = "lang-swift")]
fn swift_edge(source: NodeId, target: NodeId, relation: &str, path: &str, line: usize) -> Edge {
    Edge {
        source,
        target,
        relation: relation.into(),
        confidence: Confidence::Extracted,
        confidence_score: None,
        source_file: path.into(),
        source_location: Some(format!("L{line}")),
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: serde_json::Map::new(),
    }
}

/// Read and extract a Swift file from disk.
#[cfg(feature = "lang-swift")]
pub fn extract_swift_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_swift_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-swift"))]
mod tests {
    use super::extract_swift_source;
    use crate::result::ExtractionResult;
    use synaptic_core::Confidence;

    #[test]
    fn extension_methods_are_extracted_and_typed_calls_are_deferred() {
        let r = extract_swift_source(
            "Sources/Completion.swift",
            b"extension ToolInfoV0 { var script: String { \"\" } }\nextension CommandInfoV0 { private func valueCompletion() {}\nfunc run() { Platform.exit(0) } }",
        );
        let ls = labels(&r);
        assert!(ls.contains(&".valueCompletion()".to_string()), "{ls:?}");
        assert!(
            r.raw_calls
                .iter()
                .any(|call| call.callee == "Platform.exit" && call.is_member_call),
            "{:?}",
            r.raw_calls
        );
    }

    #[test]
    fn explicit_constructor_receiver_is_not_rebound_to_local_init() {
        let r = extract_swift_source(
            "Sources/Completion.swift",
            b"struct Generator { init() {}\nfunc run() { Platform.shellName.flatMap(CompletionShell.init(rawValue:)) } }",
        );
        assert!(
            r.raw_calls
                .iter()
                .any(|call| call.callee == "CompletionShell.init"),
            "raw calls: {:?}; edges: {:?}",
            r.raw_calls,
            r.edges
        );
        assert!(!r.edges.iter().any(|edge| {
            edge.relation == "calls"
                && r.nodes
                    .iter()
                    .find(|node| node.id == edge.target)
                    .is_some_and(|node| node.label == ".init()")
        }));
    }

    #[test]
    fn leading_underscore_methods_do_not_collide() {
        let r = extract_swift_source(
            "Sources/Serialize.swift",
            b"extension Node {\n  func _serialized() {}\n  func serialized() {}\n}",
        );
        let ls = labels(&r);
        assert!(ls.contains(&"._serialized()".to_string()), "{ls:?}");
        assert!(ls.contains(&".serialized()".to_string()), "{ls:?}");
    }

    const SAMPLE: &[u8] = br#"
import Foundation

protocol Greeter {
    func greet() -> String
}

class Animal {
    func breathe() {}
}

class Dog: Animal, Greeter {
    func greet() -> String { return makeSound() }
    func makeSound() -> String { return "woof" }
    func feed(food: Food) { breathe() }
}
"#;

    fn extract() -> ExtractionResult {
        extract_swift_source("Sources/Sample.swift", SAMPLE)
    }

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let lbl = |id: &synaptic_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| (lbl(&e.source), lbl(&e.target)))
            .collect()
    }

    #[test]
    fn class_and_protocol_nodes() {
        let r = extract();
        let ls = labels(&r);
        assert!(ls.contains(&"Dog".to_string()), "{ls:?}");
        assert!(ls.contains(&"Animal".to_string()));
        assert!(ls.contains(&"Greeter".to_string()));
    }

    #[test]
    fn methods_scoped_under_type() {
        let r = extract();
        let ls = labels(&r);
        assert!(ls.contains(&".makeSound()".to_string()), "{ls:?}");
        assert!(ls.contains(&".breathe()".to_string()));
        assert!(ls.contains(&".greet()".to_string()));
        assert!(ls.contains(&".feed()".to_string()));
    }

    #[test]
    fn import_emits_module() {
        let r = extract();
        let imps = rels(&r, "imports");
        assert!(
            imps.iter().any(|(_, t)| t == "Foundation"),
            "imports: {imps:?}"
        );
    }

    #[test]
    fn inheritance_classified_via_protocol_prescan() {
        let r = extract();
        let inh = rels(&r, "inherits");
        let imp = rels(&r, "implements");
        // Dog: Animal, Greeter -> Animal is a class (inherits), Greeter is an
        // in-file protocol (implements).
        assert!(
            inh.contains(&("Dog".to_string(), "Animal".to_string())),
            "inherits: {inh:?}"
        );
        assert!(
            imp.contains(&("Dog".to_string(), "Greeter".to_string())),
            "implements: {imp:?}"
        );
    }

    #[test]
    fn parameter_and_return_type_references() {
        let r = extract();
        let refs: Vec<(String, String)> = r
            .edges
            .iter()
            .filter(|e| e.relation == "references")
            .map(|e| {
                let tgt = r
                    .nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| e.target.0.clone());
                (tgt, e.context.clone().unwrap_or_default())
            })
            .collect();
        assert!(
            refs.contains(&("Food".to_string(), "parameter_type".to_string())),
            "refs: {refs:?}"
        );
        assert!(refs.contains(&("String".to_string(), "return_type".to_string())));
    }

    #[test]
    fn intra_file_calls_resolve() {
        let r = extract();
        let calls = rels(&r, "calls");
        assert!(
            calls.contains(&(".greet()".to_string(), ".makeSound()".to_string())),
            "calls: {calls:?}"
        );
        assert!(calls.contains(&(".feed()".to_string(), ".breathe()".to_string())));
    }

    #[test]
    fn init_subscript_nodes_and_property_refs() {
        let r = extract_swift_source(
            "F.swift",
            b"class C {\n  var leash: Leash\n  init(tag: Tag) {}\n  subscript(i: Int) -> Slot { return slot }\n}\n",
        );
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        assert!(labels.contains(&".init()".to_string()), "{labels:?}");
        assert!(labels.contains(&".subscript()".to_string()));
        let refs: Vec<(String, String)> = r
            .edges
            .iter()
            .filter(|e| e.relation == "references")
            .map(|e| {
                let tgt = r
                    .nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| e.target.0.clone());
                (tgt, e.context.clone().unwrap_or_default())
            })
            .collect();
        assert!(
            refs.contains(&("Leash".to_string(), "field".to_string())),
            "{refs:?}"
        );
        assert!(refs.contains(&("Tag".to_string(), "parameter_type".to_string())));
        assert!(refs.contains(&("Slot".to_string(), "return_type".to_string())));
    }

    #[test]
    fn structural_edges_are_extracted_confidence() {
        let r = extract();
        for e in &r.edges {
            if matches!(e.relation.as_str(), "contains" | "method" | "inherits") {
                assert_eq!(e.confidence, Confidence::Extracted, "edge {e:?}");
            }
        }
    }
}
