//! Zig extractor — custom walker.
//!
//! `fn` → `name()` / `.name()`; `const X = struct {…}` (or enum/union) → a type
//! node with its functions as methods; `@import("m")` → `imports_from`; in-file
//! function calls → `calls`.

#[cfg(feature = "lang-zig")]
use std::collections::HashSet;

#[cfg(feature = "lang-zig")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-zig")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-zig")]
use crate::common::Builder;
#[cfg(feature = "lang-zig")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-zig")]
use crate::result::ExtractionResult;

const MAX_DEPTH: usize = 2000;

/// Extract a Zig source file already in memory.
#[cfg(feature = "lang-zig")]
pub fn extract_zig_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_zig::LANGUAGE.into())
        .expect("load tree-sitter-zig");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = Zig {
        src: source,
        b: Builder::new(path),
        file_nid: file_nid.clone(),
        stem: file_stem(path),
        function_bodies: Vec::new(),
    };
    ex.b.add_node(file_nid, filename, 1);
    ex.walk(tree.root_node(), None, 0);
    ex.run_call_pass();
    ex.b.into_result()
}

/// Read and extract a Zig file from disk.
#[cfg(feature = "lang-zig")]
pub fn extract_zig_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_zig_source(&path_str, &source))
}

#[cfg(feature = "lang-zig")]
struct Zig<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
}

#[cfg(feature = "lang-zig")]
impl<'tree> Zig<'_, 'tree> {
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

    fn walk(&mut self, node: TsNode<'tree>, scope: Option<NodeId>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "variable_declaration" => self.handle_var(node, scope, depth),
            "function_declaration" => self.handle_function(node, &scope),
            _ => {
                for c in Self::children(node) {
                    self.walk(c, scope.clone(), depth + 1);
                }
            }
        }
    }

    fn handle_var(&mut self, node: TsNode<'tree>, scope: Option<NodeId>, depth: usize) {
        let name = Self::children(node)
            .into_iter()
            .find(|c| c.kind() == "identifier")
            .map(|c| self.text(c));
        let value = Self::children(node).into_iter().rfind(|c| c.is_named());
        let (Some(name), Some(value)) = (name, value) else {
            return;
        };
        match value.kind() {
            "builtin_function" => {
                // `const x = @import("m")` imports the module's base name.
                let bid = Self::children(value)
                    .into_iter()
                    .find(|c| c.kind() == "builtin_identifier")
                    .map(|c| self.text(c))
                    .unwrap_or_default();
                if bid == "@import" {
                    if let Some(s) = self.first_string(value) {
                        let last = s.rsplit(['/', '\\']).next().unwrap_or(&s);
                        let base = last.strip_suffix(".zig").unwrap_or(last);
                        if !base.is_empty() {
                            let tgt = NodeId(make_id(&["zig", "mod", base]));
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
            }
            "struct_declaration" | "enum_declaration" | "union_declaration"
            | "opaque_declaration" => {
                // `const Name = struct {...}` is a type node; its functions are methods.
                let line = Self::line(node);
                let nid = NodeId(make_id(&[&self.stem, &name]));
                self.b.add_node(nid.clone(), name, line);
                let parent = scope.unwrap_or_else(|| self.file_nid.clone());
                self.b.add_edge(parent, nid.clone(), "contains", line, None);
                for c in Self::children(value) {
                    self.walk(c, Some(nid.clone()), depth + 1);
                }
            }
            _ => {
                for c in Self::children(value) {
                    self.walk(c, scope.clone(), depth + 1);
                }
            }
        }
    }

    fn handle_function(&mut self, node: TsNode<'tree>, scope: &Option<NodeId>) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let line = Self::line(node);
        let nid = if let Some(ty) = scope {
            let m = NodeId(make_id(&[ty.as_str(), &name]));
            self.b.add_node(m.clone(), format!(".{name}()"), line);
            self.b.add_edge(ty.clone(), m.clone(), "method", line, None);
            m
        } else {
            let f = NodeId(make_id(&[&self.stem, &name]));
            self.b.add_node(f.clone(), format!("{name}()"), line);
            self.b
                .add_edge(self.file_nid.clone(), f.clone(), "contains", line, None);
            f
        };
        // Parameter type references.
        if let Some(params) = Self::children(node)
            .into_iter()
            .find(|c| c.kind() == "parameters")
        {
            for p in Self::children(params)
                .into_iter()
                .filter(|c| c.kind() == "parameter")
            {
                if let Some(ty) = p.child_by_field_name("type") {
                    for t in self.type_names(ty) {
                        let tgt = self.b.ensure_named_node(&t, &self.stem, line);
                        if tgt != nid {
                            self.b.add_edge(
                                nid.clone(),
                                tgt,
                                "references",
                                line,
                                Some("parameter_type"),
                            );
                        }
                    }
                }
            }
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.function_bodies.push((nid, body));
        }
    }

    fn type_names(&self, node: TsNode<'tree>) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "identifier" {
                let t = self.text(n);
                // Skip Zig primitive types (u8, i32, void, bool, …).
                let prim = t.starts_with(['u', 'i']) && t[1..].chars().all(|c| c.is_ascii_digit())
                    || matches!(
                        t.as_str(),
                        "void"
                            | "bool"
                            | "type"
                            | "anytype"
                            | "usize"
                            | "isize"
                            | "f32"
                            | "f64"
                            | "comptime_int"
                    );
                if !t.is_empty() && !prim {
                    out.push(t);
                }
                continue;
            }
            for c in Self::children(n) {
                stack.push(c);
            }
        }
        out
    }

    fn first_string(&self, node: TsNode<'tree>) -> Option<String> {
        let mut q = std::collections::VecDeque::from([node]);
        while let Some(n) = q.pop_front() {
            if n.kind() == "string" {
                return Some(self.text(n).trim_matches('"').to_string());
            }
            for c in Self::children(n) {
                q.push_back(c);
            }
        }
        None
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
        if node.kind() == "function_declaration" {
            return;
        }
        if node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                if func.kind() == "identifier" {
                    let callee = self.text(func);
                    if !callee.is_empty() {
                        self.b.resolve_call(
                            caller,
                            &callee,
                            false,
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

#[cfg(all(test, feature = "lang-zig"))]
mod tests {
    use super::extract_zig_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_zig_source(
            "src/dog.zig",
            b"const std = @import(\"std\");\n\nconst Dog = struct {\n  fn bark(self: Dog) void {\n    sound();\n  }\n  fn sound() void {}\n};\n",
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
    fn struct_and_method_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"Dog".to_string()), "{ls:?}");
        assert!(ls.contains(&".bark()".to_string()));
        assert!(ls.contains(&".sound()".to_string()));
    }

    #[test]
    fn import_becomes_import() {
        assert!(rels(&extract(), "imports_from")
            .iter()
            .any(|(_, t)| t == "std"));
    }

    #[test]
    fn calls_resolve() {
        assert!(
            rels(&extract(), "calls").contains(&(".bark()".to_string(), ".sound()".to_string())),
            "{:?}",
            rels(&extract(), "calls")
        );
    }
}
