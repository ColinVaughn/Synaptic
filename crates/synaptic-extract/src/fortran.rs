//! Fortran extractor — custom walker.
//!
//! `module`/`program`/`submodule` → container nodes; `subroutine`/`function` →
//! `.name()` / `name()`; `use` → `imports_from`; `call X` / function references
//! → `calls`.

#[cfg(feature = "lang-fortran")]
use std::collections::HashSet;

#[cfg(feature = "lang-fortran")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-fortran")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-fortran")]
use crate::common::Builder;
#[cfg(feature = "lang-fortran")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-fortran")]
use crate::result::ExtractionResult;

/// Intrinsics never treated as in-file call targets.
#[cfg(feature = "lang-fortran")]
const FORTRAN_BUILTINS: &[&str] = &[
    "print",
    "write",
    "read",
    "allocate",
    "deallocate",
    "open",
    "close",
    "size",
    "len",
    "trim",
    "abs",
    "sqrt",
    "exp",
    "log",
    "sin",
    "cos",
    "max",
    "min",
    "sum",
    "mod",
    "real",
    "int",
    "present",
    "associated",
    "allocated",
];

const MAX_DEPTH: usize = 2000;

/// Extract a Fortran source file already in memory.
#[cfg(feature = "lang-fortran")]
pub fn extract_fortran_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_fortran::LANGUAGE.into())
        .expect("load tree-sitter-fortran");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = Fortran {
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

/// Read and extract a Fortran file from disk.
#[cfg(feature = "lang-fortran")]
pub fn extract_fortran_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_fortran_source(&path_str, &source))
}

#[cfg(feature = "lang-fortran")]
struct Fortran<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
}

#[cfg(feature = "lang-fortran")]
impl<'tree> Fortran<'_, 'tree> {
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

    /// The `name` of a container/procedure (its leading `*_statement`'s `name`,
    /// which is a field on `subroutine_statement` but positional on
    /// `module_statement`).
    fn decl_name(&self, node: TsNode<'tree>, stmt_kind: &str) -> Option<String> {
        let stmt = Self::children(node)
            .into_iter()
            .find(|c| c.kind() == stmt_kind)?;
        let name = stmt.child_by_field_name("name").or_else(|| {
            Self::children(stmt)
                .into_iter()
                .find(|c| c.kind() == "name")
        })?;
        Some(self.text(name))
    }

    fn walk(&mut self, node: TsNode<'tree>, scope: Option<NodeId>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "module" | "program" | "submodule" => {
                let stmt = match node.kind() {
                    "module" => "module_statement",
                    "program" => "program_statement",
                    _ => "submodule_statement",
                };
                if let Some(name) = self.decl_name(node, stmt).filter(|n| !n.is_empty()) {
                    let line = Self::line(node);
                    let nid = NodeId(make_id(&[&self.stem, &name]));
                    self.b.add_node(nid.clone(), name, line);
                    self.b
                        .add_edge(self.file_nid.clone(), nid.clone(), "contains", line, None);
                    for c in Self::children(node) {
                        self.walk(c, Some(nid.clone()), depth + 1);
                    }
                } else {
                    for c in Self::children(node) {
                        self.walk(c, scope.clone(), depth + 1);
                    }
                }
            }
            "subroutine" | "function" => {
                let stmt = if node.kind() == "subroutine" {
                    "subroutine_statement"
                } else {
                    "function_statement"
                };
                if let Some(name) = self.decl_name(node, stmt).filter(|n| !n.is_empty()) {
                    let line = Self::line(node);
                    let nid = if let Some(m) = &scope {
                        let f = NodeId(make_id(&[m.as_str(), &name]));
                        self.b.add_node(f.clone(), format!(".{name}()"), line);
                        self.b.add_edge(m.clone(), f.clone(), "method", line, None);
                        f
                    } else {
                        let f = NodeId(make_id(&[&self.stem, &name]));
                        self.b.add_node(f.clone(), format!("{name}()"), line);
                        self.b
                            .add_edge(self.file_nid.clone(), f.clone(), "contains", line, None);
                        f
                    };
                    for child in Self::children(node) {
                        if !child.kind().ends_with("_statement") {
                            self.function_bodies.push((nid.clone(), child));
                        }
                    }
                }
            }
            "use_statement" => {
                if let Some(name) = Self::children(node)
                    .into_iter()
                    .find(|c| matches!(c.kind(), "name" | "identifier"))
                    .map(|c| self.text(c))
                    .filter(|n| !n.is_empty())
                {
                    let tgt = NodeId(make_id(&["fortran", "mod", &name]));
                    self.b.add_external_node(tgt.clone(), name);
                    self.b.add_edge(
                        self.file_nid.clone(),
                        tgt,
                        "imports_from",
                        Self::line(node),
                        Some("import"),
                    );
                }
            }
            _ => {
                for c in Self::children(node) {
                    self.walk(c, scope.clone(), depth + 1);
                }
            }
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
        if matches!(node.kind(), "subroutine" | "function") {
            return;
        }
        let callee = match node.kind() {
            "subroutine_call" => node.child_by_field_name("subroutine").map(|s| self.text(s)),
            "call_expression" => Self::children(node)
                .into_iter()
                .find(|c| c.kind() == "identifier")
                .map(|c| self.text(c)),
            _ => None,
        };
        if let Some(callee) = callee {
            if !callee.is_empty() && !FORTRAN_BUILTINS.contains(&callee.to_lowercase().as_str()) {
                self.b
                    .resolve_call(caller, &callee, false, Self::line(node), index, seen, true);
            }
        }
        for c in Self::children(node) {
            self.walk_calls(c, caller, index, seen, depth + 1);
        }
    }
}

#[cfg(all(test, feature = "lang-fortran"))]
mod tests {
    use super::extract_fortran_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_fortran_source(
            "src/m.f90",
            b"module m\ncontains\nsubroutine bark(x)\n  call sound(x)\nend subroutine\nsubroutine sound(x)\nend subroutine\nend module\n",
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
    fn module_and_procedure_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"m".to_string()), "{ls:?}");
        assert!(ls.contains(&".bark()".to_string()));
        assert!(ls.contains(&".sound()".to_string()));
    }

    #[test]
    fn call_resolves() {
        // bark calls sound
        assert!(
            rels(&extract(), "calls").contains(&(".bark()".to_string(), ".sound()".to_string())),
            "{:?}",
            rels(&extract(), "calls")
        );
    }
}
