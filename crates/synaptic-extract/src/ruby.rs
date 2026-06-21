//! Ruby extractor — custom walker.
//!
//! `class`/`module` → name nodes; `class C < Base` → `inherits`; `include`/
//! `extend`/`prepend M` → `mixes_in`; `def`/`def self.` → `.name()` methods (or
//! `name()` at top level); `require`/`require_relative`/`load` → `imports_from`;
//! in-file method calls → `calls`. Ruby is dynamically typed (no type refs).

#[cfg(feature = "lang-ruby")]
use std::collections::HashSet;

#[cfg(feature = "lang-ruby")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-ruby")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-ruby")]
use crate::common::Builder;
#[cfg(feature = "lang-ruby")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-ruby")]
use crate::result::ExtractionResult;

/// Kernel/common methods never treated as in-file call targets.
#[cfg(feature = "lang-ruby")]
const RUBY_BUILTINS: &[&str] = &[
    "puts",
    "print",
    "p",
    "pp",
    "require",
    "require_relative",
    "load",
    "include",
    "extend",
    "prepend",
    "attr_accessor",
    "attr_reader",
    "attr_writer",
    "raise",
    "loop",
    "lambda",
    "proc",
    "new",
    "freeze",
    "dup",
    "clone",
    "send",
    "respond_to?",
    "is_a?",
    "kind_of?",
    "instance_of?",
    "to_s",
    "to_i",
    "to_a",
    "to_h",
    "nil?",
    "empty?",
    "each",
    "map",
    "select",
    "reject",
    "yield",
    "super",
    "block_given?",
    "format",
    "sprintf",
    "catch",
    "throw",
];

/// Call methods that bring a module's methods into the class.
#[cfg(feature = "lang-ruby")]
const MIXIN_CALLS: &[&str] = &["include", "extend", "prepend"];

/// Call methods that load another file.
#[cfg(feature = "lang-ruby")]
const REQUIRE_CALLS: &[&str] = &["require", "require_relative", "load"];

const MAX_DEPTH: usize = 2000;

/// Extract a Ruby source file already in memory.
#[cfg(feature = "lang-ruby")]
pub fn extract_ruby_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_ruby::LANGUAGE.into())
        .expect("load tree-sitter-ruby");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = Ruby {
        src: source,
        b: Builder::new(path),
        file_nid: file_nid.clone(),
        stem: file_stem(path),
        function_bodies: Vec::new(),
    };
    ex.b.add_node(file_nid, filename, 1);
    let root = tree.root_node();
    ex.walk(root, None, 0);
    ex.run_call_pass();
    ex.b.into_result()
}

/// Read and extract a Ruby file from disk.
#[cfg(feature = "lang-ruby")]
pub fn extract_ruby_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_ruby_source(&path_str, &source))
}

#[cfg(feature = "lang-ruby")]
struct Ruby<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
}

#[cfg(feature = "lang-ruby")]
impl<'tree> Ruby<'_, 'tree> {
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

    /// `scope` is the enclosing class/module node id (`None` at top level).
    fn walk(&mut self, node: TsNode<'tree>, scope: Option<NodeId>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "class" | "module" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(name_node);
                let line = Self::line(node);
                let nid = NodeId(make_id(&[&self.stem, &name]));
                self.b.add_node(nid.clone(), name, line);
                let parent = scope.clone().unwrap_or_else(|| self.file_nid.clone());
                self.b.add_edge(parent, nid.clone(), "contains", line, None);

                if node.kind() == "class" {
                    if let Some(sc) = node.child_by_field_name("superclass") {
                        for base in Self::children(sc)
                            .into_iter()
                            .filter(|c| c.kind() == "constant")
                        {
                            self.link(&nid, &self.text(base), line, "inherits");
                        }
                    }
                }
                if let Some(body) = node.child_by_field_name("body") {
                    for c in Self::children(body) {
                        self.walk(c, Some(nid.clone()), depth + 1);
                    }
                }
            }
            "method" | "singleton_method" => {
                let Some(name_node) = node.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(name_node);
                let line = Self::line(node);
                let nid = if let Some(cls) = &scope {
                    let m = NodeId(make_id(&[cls.as_str(), &name]));
                    self.b.add_node(m.clone(), format!(".{name}()"), line);
                    self.b
                        .add_edge(cls.clone(), m.clone(), "method", line, None);
                    m
                } else {
                    let f = NodeId(make_id(&[&self.stem, &name]));
                    self.b.add_node(f.clone(), format!("{name}()"), line);
                    self.b
                        .add_edge(self.file_nid.clone(), f.clone(), "contains", line, None);
                    f
                };
                if let Some(body) = node.child_by_field_name("body") {
                    self.function_bodies.push((nid, body));
                }
            }
            "call" => {
                self.handle_special_call(node, &scope);
                // Recurse (a class may be defined inside a block, etc.).
                for c in Self::children(node) {
                    self.walk(c, scope.clone(), depth + 1);
                }
            }
            _ => {
                for c in Self::children(node) {
                    self.walk(c, scope.clone(), depth + 1);
                }
            }
        }
    }

    /// `require`/`load` → import; `include`/`extend`/`prepend` → `mixes_in` from
    /// the enclosing class.
    fn handle_special_call(&mut self, call: TsNode<'tree>, scope: &Option<NodeId>) {
        let Some(method) = call.child_by_field_name("method") else {
            return;
        };
        if method.kind() != "identifier" {
            return;
        }
        let name = self.text(method);
        let line = Self::line(call);
        let args = call.child_by_field_name("arguments");
        if REQUIRE_CALLS.contains(&name.as_str()) {
            if let Some(args) = args {
                if let Some(s) = Self::children(args)
                    .into_iter()
                    .find(|c| c.kind() == "string")
                {
                    let raw = self.text(s);
                    let module = raw.trim_matches(|c| c == '"' || c == '\'');
                    let base = module.rsplit(['/', '\\']).next().unwrap_or(module);
                    if !base.is_empty() {
                        let tgt = NodeId(make_id(&["ruby", "lib", base]));
                        self.b.add_external_node(tgt.clone(), base.to_string());
                        self.b.add_edge(
                            self.file_nid.clone(),
                            tgt,
                            "imports_from",
                            line,
                            Some("import"),
                        );
                    }
                }
            }
        } else if MIXIN_CALLS.contains(&name.as_str()) {
            if let Some(cls) = scope {
                if let Some(args) = args {
                    for c in Self::children(args)
                        .into_iter()
                        .filter(|c| c.kind() == "constant")
                    {
                        self.link(&cls.clone(), &self.text(c), line, "mixes_in");
                    }
                }
            }
        }
    }

    /// Link `owner` to a base/module by name (external stub if not in-file).
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
        if matches!(node.kind(), "method" | "singleton_method") {
            return;
        }
        if node.kind() == "call" {
            if let Some(method) = node.child_by_field_name("method") {
                if matches!(method.kind(), "identifier" | "constant") {
                    let callee = self.text(method);
                    if !callee.is_empty() && !RUBY_BUILTINS.contains(&callee.as_str()) {
                        let is_member = node.child_by_field_name("receiver").is_some();
                        self.b.resolve_call(
                            caller,
                            &callee,
                            is_member,
                            Self::line(node),
                            index,
                            seen,
                            true,
                        );
                    }
                }
            }
        }
        for c in Self::children(node) {
            self.walk_calls(c, caller, index, seen, depth + 1);
        }
    }
}

#[cfg(all(test, feature = "lang-ruby"))]
mod tests {
    use super::extract_ruby_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_ruby_source(
            "lib/dog.rb",
            b"require 'set'\n\nclass Dog < Animal\n  include Walkable\n\n  def bark(food)\n    sound(food)\n  end\n\n  def sound(food)\n    food\n  end\nend\n",
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
    fn superclass_inherits_and_include_mixes_in() {
        let r = extract();
        assert!(
            rels(&r, "inherits").contains(&("Dog".to_string(), "Animal".to_string())),
            "{:?}",
            rels(&r, "inherits")
        );
        assert!(
            rels(&r, "mixes_in").contains(&("Dog".to_string(), "Walkable".to_string())),
            "{:?}",
            rels(&r, "mixes_in")
        );
    }

    #[test]
    fn require_imports_and_calls_resolve() {
        let r = extract();
        assert!(rels(&r, "imports_from").iter().any(|(_, t)| t == "set"));
        // bark() calls sound()
        assert!(
            rels(&r, "calls").contains(&(".bark()".to_string(), ".sound()".to_string())),
            "{:?}",
            rels(&r, "calls")
        );
    }
}
