//! Elixir extractor — custom walker. Elixir is homoiconic, so `defmodule`/`def`/
//! `defp`/`alias` are all `call` nodes.
//!
//! `defmodule` → module node; `def`/`defp` → `.name()` functions under it;
//! `alias`/`import`/`require`/`use` → `imports_from`; in-module function calls →
//! `calls`. Elixir is dynamically typed (no type refs).

#[cfg(feature = "lang-elixir")]
use std::collections::HashSet;

#[cfg(feature = "lang-elixir")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-elixir")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-elixir")]
use crate::common::Builder;
#[cfg(feature = "lang-elixir")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-elixir")]
use crate::result::ExtractionResult;

/// `call` targets that load/alias another module.
#[cfg(feature = "lang-elixir")]
const IMPORT_TARGETS: &[&str] = &["alias", "import", "require", "use"];

/// Kernel macros/functions never treated as in-module call targets.
#[cfg(feature = "lang-elixir")]
const ELIXIR_BUILTINS: &[&str] = &[
    "def",
    "defp",
    "defmodule",
    "defmacro",
    "defstruct",
    "defprotocol",
    "defimpl",
    "alias",
    "import",
    "require",
    "use",
    "if",
    "unless",
    "case",
    "cond",
    "for",
    "with",
    "raise",
    "throw",
    "spawn",
    "send",
    "receive",
    "is_nil",
    "is_atom",
    "is_binary",
    "is_list",
    "is_map",
    "to_string",
    "inspect",
    "length",
    "hd",
    "tl",
    "elem",
    "put_elem",
    "Enum",
    "Map",
    "List",
    "String",
    "IO",
];

const MAX_DEPTH: usize = 2000;

/// Extract an Elixir source file already in memory.
#[cfg(feature = "lang-elixir")]
pub fn extract_elixir_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_elixir::LANGUAGE.into())
        .expect("load tree-sitter-elixir");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = Elixir {
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

/// Read and extract an Elixir file from disk.
#[cfg(feature = "lang-elixir")]
pub fn extract_elixir_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_elixir_source(&path_str, &source))
}

#[cfg(feature = "lang-elixir")]
struct Elixir<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
}

#[cfg(feature = "lang-elixir")]
impl<'tree> Elixir<'_, 'tree> {
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

    /// The `target` identifier text of a `call` (`def`, `defmodule`, `sound`, …).
    fn call_target(&self, call: TsNode<'tree>) -> Option<String> {
        let t = call.child_by_field_name("target")?;
        (t.kind() == "identifier").then(|| self.text(t))
    }

    /// The `arguments` node of a call (a positional child, not a field).
    fn arguments(&self, call: TsNode<'tree>) -> Option<TsNode<'tree>> {
        Self::children(call)
            .into_iter()
            .find(|c| c.kind() == "arguments")
    }

    fn first_arg(&self, call: TsNode<'tree>) -> Option<TsNode<'tree>> {
        let args = self.arguments(call)?;
        Self::children(args).into_iter().find(|c| c.is_named())
    }

    /// The `do:` body of a call — a `do_block` child or the `do` keyword value.
    fn do_body(&self, call: TsNode<'tree>) -> Option<TsNode<'tree>> {
        if let Some(b) = Self::children(call)
            .into_iter()
            .find(|c| c.kind() == "do_block")
        {
            return Some(b);
        }
        // keywords -> pair(key: keyword "do") -> value
        let args = self.arguments(call)?;
        for kw in Self::children(args)
            .into_iter()
            .filter(|c| c.kind() == "keywords")
        {
            for pair in Self::children(kw)
                .into_iter()
                .filter(|c| c.kind() == "pair")
            {
                let key = pair.child_by_field_name("key").map(|k| self.text(k));
                if key.as_deref() == Some("do:") || key.as_deref() == Some("do") {
                    return pair.child_by_field_name("value");
                }
            }
        }
        None
    }

    fn walk(&mut self, node: TsNode<'tree>, scope: Option<NodeId>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        if node.kind() == "call" {
            let target = self.call_target(node);
            match target.as_deref() {
                Some("defmodule") => {
                    let name = self
                        .first_arg(node)
                        .map(|a| self.text(a))
                        .unwrap_or_default();
                    if !name.is_empty() {
                        let line = Self::line(node);
                        let nid = NodeId(make_id(&[&self.stem, &name]));
                        self.b.add_node(nid.clone(), name, line);
                        self.b
                            .add_edge(self.file_nid.clone(), nid.clone(), "contains", line, None);
                        if let Some(body) = self.do_body(node) {
                            for c in Self::children(body) {
                                self.walk(c, Some(nid.clone()), depth + 1);
                            }
                        }
                    }
                    return;
                }
                Some("def") | Some("defp") => {
                    let fname = self.first_arg(node).and_then(|a| match a.kind() {
                        "call" => a.child_by_field_name("target").map(|t| self.text(t)),
                        "identifier" => Some(self.text(a)),
                        _ => None,
                    });
                    if let Some(fname) = fname.filter(|n| !n.is_empty()) {
                        let line = Self::line(node);
                        let nid = if let Some(m) = &scope {
                            let f = NodeId(make_id(&[m.as_str(), &fname]));
                            self.b.add_node(f.clone(), format!(".{fname}()"), line);
                            self.b.add_edge(m.clone(), f.clone(), "method", line, None);
                            f
                        } else {
                            let f = NodeId(make_id(&[&self.stem, &fname]));
                            self.b.add_node(f.clone(), format!("{fname}()"), line);
                            self.b.add_edge(
                                self.file_nid.clone(),
                                f.clone(),
                                "contains",
                                line,
                                None,
                            );
                            f
                        };
                        if let Some(body) = self.do_body(node) {
                            self.function_bodies.push((nid, body));
                        }
                    }
                    return;
                }
                Some(t) if IMPORT_TARGETS.contains(&t) => {
                    if let Some(arg) = self.first_arg(node) {
                        let full = self.text(arg);
                        let tail = full.rsplit('.').next().unwrap_or(&full).trim();
                        if !tail.is_empty() {
                            let tgt = NodeId(make_id(&["elixir", "mod", tail]));
                            self.b.add_external_node(tgt.clone(), tail.to_string());
                            self.b.add_edge(
                                self.file_nid.clone(),
                                tgt,
                                "imports_from",
                                Self::line(node),
                                Some("import"),
                            );
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
        for c in Self::children(node) {
            self.walk(c, scope.clone(), depth + 1);
        }
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
        if node.kind() == "call" {
            if let Some(callee) = self.call_target(node) {
                if !callee.is_empty() && !ELIXIR_BUILTINS.contains(&callee.as_str()) {
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
        for c in Self::children(node) {
            self.walk_calls(c, caller, index, seen, depth + 1);
        }
    }
}

#[cfg(all(test, feature = "lang-elixir"))]
mod tests {
    use super::extract_elixir_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_elixir_source(
            "lib/dog.ex",
            b"defmodule Dog do\n  alias My.Animal\n  def bark(food) do\n    sound(food)\n  end\n  defp sound(f) do\n    f\n  end\nend\n",
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
    fn module_and_function_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"Dog".to_string()), "{ls:?}");
        assert!(ls.contains(&".bark()".to_string()));
        assert!(ls.contains(&".sound()".to_string()));
    }

    #[test]
    fn alias_becomes_import() {
        assert!(rels(&extract(), "imports_from")
            .iter()
            .any(|(_, t)| t == "Animal"));
    }

    #[test]
    fn calls_resolve() {
        // bark calls sound
        assert!(
            rels(&extract(), "calls").contains(&(".bark()".to_string(), ".sound()".to_string())),
            "{:?}",
            rels(&extract(), "calls")
        );
    }
}
