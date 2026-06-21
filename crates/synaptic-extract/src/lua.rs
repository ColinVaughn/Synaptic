//! Lua extractor — custom walker (table-based, no classes).
//!
//! `function_declaration` → `name()` free functions, or `.field()` methods under
//! a `Table` node when the name is a `Table.field` / `Table:field` index;
//! `require('m')` → `imports_from`; in-file function calls → `calls` edges.

#[cfg(feature = "lang-lua")]
use std::collections::HashSet;

#[cfg(feature = "lang-lua")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-lua")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-lua")]
use crate::common::Builder;
#[cfg(feature = "lang-lua")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-lua")]
use crate::result::ExtractionResult;

/// Standard-library globals never treated as in-file call targets.
#[cfg(feature = "lang-lua")]
const LUA_BUILTINS: &[&str] = &[
    "print",
    "pairs",
    "ipairs",
    "type",
    "tostring",
    "tonumber",
    "require",
    "pcall",
    "xpcall",
    "error",
    "assert",
    "setmetatable",
    "getmetatable",
    "next",
    "select",
    "rawget",
    "rawset",
    "rawequal",
    "rawlen",
    "table",
    "string",
    "math",
    "io",
    "os",
    "coroutine",
    "unpack",
    "collectgarbage",
    "load",
    "loadstring",
    "dofile",
    "loadfile",
];

const MAX_DEPTH: usize = 2000;

/// Extract a Lua source file already in memory.
#[cfg(feature = "lang-lua")]
pub fn extract_lua_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_lua::LANGUAGE.into())
        .expect("load tree-sitter-lua");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = Lua {
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

/// Read and extract a Lua file from disk.
#[cfg(feature = "lang-lua")]
pub fn extract_lua_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_lua_source(&path_str, &source))
}

#[cfg(feature = "lang-lua")]
struct Lua<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
}

#[cfg(feature = "lang-lua")]
impl<'tree> Lua<'_, 'tree> {
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

    fn walk(&mut self, node: TsNode<'tree>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "function_declaration" => self.function_decl(node),
            "function_call" => {
                self.maybe_require(node);
                for c in Self::children(node) {
                    self.walk(c, depth + 1);
                }
            }
            _ => {
                for c in Self::children(node) {
                    self.walk(c, depth + 1);
                }
            }
        }
    }

    fn function_decl(&mut self, node: TsNode<'tree>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let line = Self::line(node);
        let nid = match name.kind() {
            "identifier" => {
                let fname = self.text(name);
                let nid = NodeId(make_id(&[&self.stem, &fname]));
                self.b.add_node(nid.clone(), format!("{fname}()"), line);
                self.b
                    .add_edge(self.file_nid.clone(), nid.clone(), "contains", line, None);
                nid
            }
            // `function Tbl.method()` / `function Tbl:method()` is a method under Tbl.
            "dot_index_expression" | "method_index_expression" => {
                let table = name
                    .child_by_field_name("table")
                    .map(|t| self.text(t))
                    .unwrap_or_default();
                let field = name
                    .child_by_field_name("field")
                    .or_else(|| name.child_by_field_name("method"))
                    .map(|f| self.text(f))
                    .unwrap_or_default();
                if table.is_empty() || field.is_empty() {
                    return;
                }
                let tbl_nid = NodeId(make_id(&[&self.stem, &table]));
                self.b.add_node(tbl_nid.clone(), table, line);
                self.b.add_edge(
                    self.file_nid.clone(),
                    tbl_nid.clone(),
                    "contains",
                    line,
                    None,
                );
                let m = NodeId(make_id(&[tbl_nid.as_str(), &field]));
                self.b.add_node(m.clone(), format!(".{field}()"), line);
                self.b.add_edge(tbl_nid, m.clone(), "method", line, None);
                m
            }
            _ => return,
        };
        if let Some(body) = node.child_by_field_name("body") {
            self.function_bodies.push((nid, body));
        }
    }

    /// `require('m')` / `require "m"` → `imports_from` to the module's last path
    /// component.
    fn maybe_require(&mut self, call: TsNode<'tree>) {
        let Some(name) = call.child_by_field_name("name") else {
            return;
        };
        if name.kind() != "identifier" || self.text(name) != "require" {
            return;
        }
        let Some(args) = call.child_by_field_name("arguments") else {
            return;
        };
        let module = Self::children(args)
            .into_iter()
            .find(|c| c.kind() == "string")
            .map(|s| self.text(s));
        let Some(module) = module else { return };
        let module = module.trim_matches(|c| c == '"' || c == '\'' || c == '[' || c == ']');
        let base = module.rsplit(['.', '/']).next().unwrap_or(module);
        if base.is_empty() {
            return;
        }
        let tgt = NodeId(make_id(&["lua", "mod", base]));
        self.b.add_external_node(tgt.clone(), base.to_string());
        self.b.add_edge(
            self.file_nid.clone(),
            tgt,
            "imports_from",
            Self::line(call),
            Some("import"),
        );
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
        if node.kind() == "function_call" {
            if let Some((callee, is_member)) = self.callee(node) {
                if !callee.is_empty() && !LUA_BUILTINS.contains(&callee.as_str()) {
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
        for c in Self::children(node) {
            self.walk_calls(c, caller, index, seen, depth + 1);
        }
    }

    /// `(callee_name, is_member)` for a `function_call`.
    fn callee(&self, call: TsNode<'tree>) -> Option<(String, bool)> {
        let name = call.child_by_field_name("name")?;
        match name.kind() {
            "identifier" => Some((self.text(name), false)),
            "dot_index_expression" | "method_index_expression" => {
                let f = name
                    .child_by_field_name("field")
                    .or_else(|| name.child_by_field_name("method"))?;
                Some((self.text(f), true))
            }
            _ => None,
        }
    }
}

#[cfg(all(test, feature = "lang-lua"))]
mod tests {
    use super::extract_lua_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_lua_source(
            "src/dog.lua",
            b"local util = require('pkg.util')\n\nlocal function sound()\n  return 1\nend\n\nfunction Dog.bark(self)\n  return sound()\nend\n",
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
    fn free_function_and_table_method_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"sound()".to_string()), "{ls:?}");
        assert!(ls.contains(&"Dog".to_string()));
        assert!(ls.contains(&".bark()".to_string()));
    }

    #[test]
    fn require_becomes_import() {
        let imps = rels(&extract(), "imports_from");
        assert!(imps.iter().any(|(_, t)| t == "util"), "{imps:?}");
    }

    #[test]
    fn method_calls_free_function() {
        let calls = rels(&extract(), "calls");
        // Dog.bark() calls sound()
        assert!(
            calls.contains(&(".bark()".to_string(), "sound()".to_string())),
            "{calls:?}"
        );
    }
}
