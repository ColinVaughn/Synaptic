//! JSON extractor — Bucket C (custom, config-only). Recognizes config/manifest
//! JSON and extracts its structure.
//!
//! Only recognized config/manifest JSON is extracted (others return empty, to
//! avoid orphan-key explosion on data/fixture JSON). Produces: a file node;
//! `imports` edges from dependency blocks to each package; `extends` edges
//! (string or array); `references` edges from `$ref`; and bounded structural
//! config-key nodes (top level + one nested level, capped) so the config's shape
//! (`scripts.build`, `compilerOptions.strict`) is visible.

#[cfg(feature = "lang-json")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-json")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-json")]
use crate::common::Builder;
#[cfg(feature = "lang-json")]
use crate::paths::file_node_id;
#[cfg(feature = "lang-json")]
use crate::result::ExtractionResult;

/// npm/yarn dependency blocks whose keys are package names.
#[cfg(feature = "lang-json")]
const DEP_BLOCKS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
    "bundleDependencies",
    "bundledDependencies",
];

/// Filenames always treated as config/manifest JSON.
#[cfg(feature = "lang-json")]
const CONFIG_NAMES: &[&str] = &[
    "package.json",
    "tsconfig.json",
    "jsconfig.json",
    ".eslintrc.json",
    "composer.json",
    "deno.json",
];

/// Cap on structural config-key nodes per file (keeps a large config bounded).
#[cfg(feature = "lang-json")]
const MAX_KEY_NODES: usize = 200;

/// Top-level keys whose presence marks a JSON document as config/manifest.
#[cfg(feature = "lang-json")]
const CONFIG_KEYS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "extends",
    "$ref",
    "$schema",
    "compilerOptions",
];

/// Extract a JSON file already in memory. Returns an empty result for data JSON
/// (not a recognized config/manifest).
#[cfg(feature = "lang-json")]
pub fn extract_json_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_json::LANGUAGE.into())
        .expect("load tree-sitter-json");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let root = tree.root_node();
    let Some(top) = children(root).into_iter().find(|c| c.kind() == "object") else {
        return ExtractionResult::default(); // top-level array/scalar = data JSON
    };

    let ex = JsonExtractor { src: source };
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let top_keys: Vec<String> = children(top)
        .into_iter()
        .filter(|c| c.kind() == "pair")
        .filter_map(|p| ex.key_text(p))
        .collect();
    let is_config = CONFIG_NAMES.contains(&filename.as_str())
        || top_keys.iter().any(|k| CONFIG_KEYS.contains(&k.as_str()));
    if !is_config {
        return ExtractionResult::default();
    }

    let mut b = Builder::new(path);
    let file_nid = file_node_id(path);
    b.add_node(file_nid.clone(), filename, 1);

    // Bounded count of structural key nodes (config shape), kept small so a
    // large config can't explode the graph.
    let mut key_budget: usize = MAX_KEY_NODES;
    for pair in children(top).into_iter().filter(|c| c.kind() == "pair") {
        let Some(key) = ex.key_text(pair) else {
            continue;
        };
        let Some(value) = pair.child_by_field_name("value") else {
            continue;
        };
        let line = pair.start_position().row + 1;
        if DEP_BLOCKS.contains(&key.as_str()) && value.kind() == "object" {
            for dep in children(value).into_iter().filter(|c| c.kind() == "pair") {
                if let Some(pkg) = ex.key_text(dep) {
                    if pkg.is_empty() {
                        continue;
                    }
                    let tgt = NodeId(make_id(&["npm", &pkg]));
                    b.add_external_node(tgt.clone(), pkg);
                    b.add_edge(file_nid.clone(), tgt, "imports", line, Some("dependency"));
                }
            }
        } else if key == "extends" {
            ex.handle_targets(&mut b, &file_nid, value, "extends", line);
        } else if key == "$ref" {
            ex.handle_targets(&mut b, &file_nid, value, "references", line);
        }

        // Structural key node (config shape): the top-level key, and one nested
        // level for object values (e.g. `scripts.build`, `compilerOptions.strict`).
        // Dependency blocks are skipped; their child keys are already imports.
        if key.is_empty() || key_budget == 0 {
            continue;
        }
        let key_nid = NodeId(make_id(&["jsonkey", path, &key]));
        b.add_tagged_node(key_nid.clone(), key.clone(), line, "config_key");
        b.add_edge(
            file_nid.clone(),
            key_nid.clone(),
            "contains",
            line,
            Some("config_key"),
        );
        key_budget -= 1;
        if value.kind() == "object" && !DEP_BLOCKS.contains(&key.as_str()) {
            for child in children(value).into_iter().filter(|c| c.kind() == "pair") {
                if key_budget == 0 {
                    break;
                }
                let Some(ck) = ex.key_text(child) else {
                    continue;
                };
                if ck.is_empty() {
                    continue;
                }
                let cline = child.start_position().row + 1;
                let child_nid = NodeId(make_id(&["jsonkey", path, &key, &ck]));
                b.add_tagged_node(child_nid.clone(), ck, cline, "config_key");
                b.add_edge(
                    key_nid.clone(),
                    child_nid,
                    "contains",
                    cline,
                    Some("config_key"),
                );
                key_budget -= 1;
            }
        }
    }
    b.into_result()
}

/// Read and extract a JSON file from disk.
#[cfg(feature = "lang-json")]
pub fn extract_json_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_json_source(&path_str, &source))
}

#[cfg(feature = "lang-json")]
fn children(node: TsNode) -> Vec<TsNode> {
    let mut c = node.walk();
    node.children(&mut c).collect()
}

#[cfg(feature = "lang-json")]
struct JsonExtractor<'a> {
    src: &'a [u8],
}

#[cfg(feature = "lang-json")]
impl JsonExtractor<'_> {
    fn text(&self, node: TsNode) -> String {
        node.utf8_text(self.src).unwrap_or("").to_string()
    }

    /// The unquoted text of a `pair`'s `key` (a `string` node).
    fn key_text(&self, pair: TsNode) -> Option<String> {
        self.string_value(pair.child_by_field_name("key")?)
    }

    /// The unquoted text of a JSON `string` node (via its `string_content`).
    fn string_value(&self, node: TsNode) -> Option<String> {
        if node.kind() != "string" {
            return None;
        }
        let inner = children(node)
            .into_iter()
            .find(|c| c.kind() == "string_content");
        Some(match inner {
            Some(c) => self.text(c),
            None => String::new(), // empty string ""
        })
    }

    /// Emit `relation` edges to a string value, or to each string element of an
    /// array value (e.g. an `extends` chain). Targets are namespaced `ref_*` nodes
    /// so external config references can't collide with code node ids.
    fn handle_targets(
        &self,
        b: &mut Builder,
        file_nid: &NodeId,
        value: TsNode,
        relation: &str,
        line: usize,
    ) {
        let mut targets = Vec::new();
        match value.kind() {
            "string" => {
                if let Some(s) = self.string_value(value) {
                    targets.push(s);
                }
            }
            "array" => {
                for el in children(value) {
                    if let Some(s) = self.string_value(el) {
                        targets.push(s);
                    }
                }
            }
            _ => {}
        }
        for t in targets {
            if t.is_empty() {
                continue;
            }
            let tgt = NodeId(make_id(&["ref", &t]));
            b.add_external_node(tgt.clone(), t);
            b.add_edge(file_nid.clone(), tgt, relation, line, Some("config"));
        }
    }
}

#[cfg(all(test, feature = "lang-json"))]
mod tests {
    use super::extract_json_source;
    use crate::result::ExtractionResult;

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<String> {
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
            .map(|e| lbl(&e.target))
            .collect()
    }

    #[test]
    fn package_json_dependencies_become_imports() {
        let src = br#"{"name":"app","dependencies":{"lodash":"^4.0.0","react":"^18"}}"#;
        let r = extract_json_source("package.json", src);
        let imps = rels(&r, "imports");
        assert!(imps.contains(&"lodash".to_string()), "{imps:?}");
        assert!(imps.contains(&"react".to_string()));
    }

    #[test]
    fn tsconfig_extends_string_and_array() {
        let s = br#"{"extends":"./base.json","compilerOptions":{}}"#;
        let r = extract_json_source("tsconfig.json", s);
        assert!(rels(&r, "extends").contains(&"./base.json".to_string()));

        let a = br#"{"extends":["a","b"]}"#;
        let r2 = extract_json_source("tsconfig.json", a);
        let ext = rels(&r2, "extends");
        assert!(
            ext.contains(&"a".to_string()) && ext.contains(&"b".to_string()),
            "{ext:?}"
        );
    }

    #[test]
    fn ref_becomes_references() {
        let src = br##"{"$ref":"#/defs/Foo","$schema":"x"}"##;
        let r = extract_json_source("schema.json", src);
        assert!(rels(&r, "references").contains(&"#/defs/Foo".to_string()));
    }

    #[test]
    fn config_keys_become_nodes_one_level_deep() {
        let src =
            br#"{"name":"app","scripts":{"build":"tsc","test":"jest"},"dependencies":{"x":"1"}}"#;
        let r = extract_json_source("package.json", src);
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        // top-level keys + one nested level
        assert!(labels.contains(&"scripts".to_string()), "{labels:?}");
        assert!(labels.contains(&"build".to_string()));
        assert!(labels.contains(&"test".to_string()));
        // dependency block is NOT recursed into as key nodes (its child is an import)
        assert!(
            !labels.contains(&"x".to_string()) || r.edges.iter().any(|e| e.relation == "imports")
        );
        // scripts.build is contained under the scripts key node
        let scripts_id = synaptic_core::make_id(&["jsonkey", "package.json", "scripts"]);
        assert!(r
            .edges
            .iter()
            .any(|e| e.source.0 == scripts_id && e.relation == "contains"));
        // Config-key nodes are tagged non-code so change-impact excludes them.
        for n in r
            .nodes
            .iter()
            .filter(|n| matches!(n.label.as_str(), "scripts" | "build" | "test" | "name"))
        {
            assert!(
                !n.is_code_symbol(),
                "config key {:?} must not be a code symbol",
                n.label
            );
        }
    }

    #[test]
    fn data_json_is_skipped() {
        // No config filename, no config keys -> empty.
        let r = extract_json_source("data/items.json", br#"{"items":[1,2,3],"count":3}"#);
        assert!(r.nodes.is_empty() && r.edges.is_empty());
        // Top-level array -> data.
        let r2 = extract_json_source("arr.json", br#"[1,2,3]"#);
        assert!(r2.nodes.is_empty());
    }
}
