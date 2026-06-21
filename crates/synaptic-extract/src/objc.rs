//! Objective-C extractor — custom walker.
//!
//! `@interface`/`@implementation` → a class node (unified by name, like Go's
//! package-scoped types); `: Super` → `inherits`; method declarations/definitions
//! → `.name()` methods; `#import "x.h"` → `imports_from`; `[recv sound]` message
//! sends to in-class methods → `calls`.

#[cfg(feature = "lang-objc")]
use std::collections::HashSet;

#[cfg(feature = "lang-objc")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-objc")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-objc")]
use crate::common::Builder;
#[cfg(feature = "lang-objc")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-objc")]
use crate::result::ExtractionResult;

const MAX_DEPTH: usize = 2000;

/// Extract an Objective-C source file already in memory.
#[cfg(feature = "lang-objc")]
pub fn extract_objc_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_objc::LANGUAGE.into())
        .expect("load tree-sitter-objc");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = ObjC {
        src: source,
        b: Builder::new(path),
        file_nid: file_nid.clone(),
        stem: file_stem(path),
        function_bodies: Vec::new(),
    };
    ex.b.add_node(file_nid, filename, 1);
    ex.walk(tree.root_node(), 0);
    ex.run_call_pass();
    ex.b.into_result()
}

/// Read and extract an Objective-C file from disk.
#[cfg(feature = "lang-objc")]
pub fn extract_objc_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_objc_source(&path_str, &source))
}

#[cfg(feature = "lang-objc")]
struct ObjC<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
}

#[cfg(feature = "lang-objc")]
impl<'tree> ObjC<'_, 'tree> {
    fn text(&self, n: TsNode<'tree>) -> String {
        n.utf8_text(self.src).unwrap_or("").to_string()
    }

    fn line(n: TsNode<'tree>) -> usize {
        n.start_position().row + 1
    }

    fn children(n: TsNode<'tree>) -> Vec<TsNode<'tree>> {
        let mut c = n.walk();
        n.children(&mut c).collect()
    }

    /// First direct `identifier` child (the class or method selector name).
    fn first_identifier(&self, node: TsNode<'tree>) -> Option<String> {
        Self::children(node)
            .into_iter()
            .find(|c| c.kind() == "identifier")
            .map(|c| self.text(c))
    }

    fn walk(&mut self, node: TsNode<'tree>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "preproc_include" => {
                if let Some(p) = node.child_by_field_name("path") {
                    let raw = self.text(p);
                    let inner = raw.trim_matches(|c| c == '<' || c == '>' || c == '"');
                    let file = inner.rsplit(['/', '\\']).next().unwrap_or(inner);
                    let base = file
                        .strip_suffix(".h")
                        .or_else(|| file.strip_suffix(".m"))
                        .unwrap_or(file);
                    if !base.is_empty() {
                        let tgt = NodeId(make_id(&["objc", "hdr", base]));
                        self.b.add_external_node(tgt.clone(), base.to_string());
                        self.b.add_edge(
                            self.file_nid.clone(),
                            tgt,
                            "imports_from",
                            Self::line(node),
                            Some("import"),
                        );
                    }
                }
            }
            "class_interface" => {
                let Some(class_nid) = self.class_node(node) else {
                    return;
                };
                if let Some(sc) = node.child_by_field_name("superclass") {
                    let base = self.text(sc);
                    self.link(&class_nid, &base, Self::line(node), "inherits");
                }
                for m in Self::children(node)
                    .into_iter()
                    .filter(|c| c.kind() == "method_declaration")
                {
                    self.method(m, &class_nid, false);
                }
            }
            "class_implementation" => {
                let Some(class_nid) = self.class_node(node) else {
                    return;
                };
                let mut stack = Self::children(node);
                while let Some(n) = stack.pop() {
                    if n.kind() == "method_definition" {
                        self.method(n, &class_nid, true);
                    } else {
                        stack.extend(Self::children(n));
                    }
                }
            }
            _ => {
                for c in Self::children(node) {
                    self.walk(c, depth + 1);
                }
            }
        }
    }

    /// The class node for an `@interface`/`@implementation` (unified by name).
    fn class_node(&mut self, node: TsNode<'tree>) -> Option<NodeId> {
        let name = self.first_identifier(node)?;
        if name.is_empty() {
            return None;
        }
        let line = Self::line(node);
        let nid = NodeId(make_id(&[&self.stem, &name]));
        self.b.add_node(nid.clone(), name, line);
        self.b
            .add_edge(self.file_nid.clone(), nid.clone(), "contains", line, None);
        Some(nid)
    }

    fn method(&mut self, node: TsNode<'tree>, class_nid: &NodeId, with_body: bool) {
        let Some(name) = self.first_identifier(node) else {
            return;
        };
        if name.is_empty() {
            return;
        }
        let line = Self::line(node);
        let m = NodeId(make_id(&[class_nid.as_str(), &name]));
        self.b.add_node(m.clone(), format!(".{name}()"), line);
        self.b
            .add_edge(class_nid.clone(), m.clone(), "method", line, None);
        if with_body {
            if let Some(body) = Self::children(node)
                .into_iter()
                .find(|c| c.kind() == "compound_statement")
            {
                self.function_bodies.push((m, body));
            }
        }
    }

    fn link(&mut self, owner: &NodeId, name: &str, line: usize, relation: &str) {
        if name.is_empty() {
            return;
        }
        let local = NodeId(make_id(&[&self.stem, name]));
        let tgt = if self.b.seen.contains(&local) {
            local
        } else {
            let global = NodeId(make_id(&[name]));
            self.b.add_external_node(global.clone(), name.to_string());
            global
        };
        self.b.add_edge(owner.clone(), tgt, relation, line, None);
    }

    fn run_call_pass(&mut self) {
        let index = self.b.label_index();
        let bodies = std::mem::take(&mut self.function_bodies);
        let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();
        for (caller, body) in bodies {
            self.walk_calls(body, &caller, &index, &mut seen, 0);
        }
    }

    fn walk_calls(
        &mut self,
        node: TsNode<'tree>,
        caller: &NodeId,
        index: &std::collections::HashMap<String, NodeId>,
        seen: &mut HashSet<(NodeId, NodeId)>,
        depth: usize,
    ) {
        if depth >= MAX_DEPTH {
            return;
        }
        if matches!(node.kind(), "method_definition" | "method_declaration") {
            return;
        }
        if node.kind() == "message_expression" {
            if let Some(method) = node.child_by_field_name("method") {
                let callee = self.text(method);
                if !callee.is_empty() {
                    self.b
                        .resolve_call(caller, &callee, true, Self::line(node), index, seen, true);
                }
            }
        }
        for c in Self::children(node) {
            self.walk_calls(c, caller, index, seen, depth + 1);
        }
    }
}

#[cfg(all(test, feature = "lang-objc"))]
mod tests {
    use super::extract_objc_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_objc_source(
            "src/Dog.m",
            b"#import \"animal.h\"\n@interface Dog : Animal\n- (NSString *)bark;\n@end\n@implementation Dog\n- (NSString *)bark { return [self sound]; }\n- (NSString *)sound { return @\"woof\"; }\n@end\n",
        )
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
    fn class_and_method_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"Dog".to_string()), "{ls:?}");
        assert!(ls.contains(&".bark()".to_string()));
        assert!(ls.contains(&".sound()".to_string()));
    }

    #[test]
    fn superclass_and_import() {
        let r = extract();
        assert!(rels(&r, "inherits").contains(&("Dog".to_string(), "Animal".to_string())));
        assert!(rels(&r, "imports_from").iter().any(|(_, t)| t == "animal"));
    }

    #[test]
    fn message_send_resolves_to_method() {
        // [self sound] from bark calls .sound()
        assert!(
            rels(&extract(), "calls").contains(&(".bark()".to_string(), ".sound()".to_string())),
            "{:?}",
            rels(&extract(), "calls")
        );
    }
}
