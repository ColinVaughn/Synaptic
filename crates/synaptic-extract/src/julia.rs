//! Julia extractor — custom walker.
//!
//! `module` → module node; `struct`/`abstract type` → type nodes; `function`
//! → `.name()` / `name()`; `using`/`import` → `imports_from`; in-file function
//! calls → `calls`.

#[cfg(feature = "lang-julia")]
use std::collections::HashSet;

#[cfg(feature = "lang-julia")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-julia")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-julia")]
use crate::common::Builder;
#[cfg(feature = "lang-julia")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-julia")]
use crate::result::ExtractionResult;

/// Base functions never treated as in-file call targets.
#[cfg(feature = "lang-julia")]
const JULIA_BUILTINS: &[&str] = &[
    "println", "print", "length", "push!", "pop!", "error", "throw", "typeof", "isa", "convert",
    "string", "parse", "map", "filter", "reduce", "collect", "zeros", "ones", "size", "sum",
    "minimum", "maximum", "abs", "sqrt", "show", "display", "get", "haskey", "keys", "values",
];

const MAX_DEPTH: usize = 2000;

/// Extract a Julia source file already in memory.
#[cfg(feature = "lang-julia")]
pub fn extract_julia_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_julia::LANGUAGE.into())
        .expect("load tree-sitter-julia");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = Julia {
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

/// Read and extract a Julia file from disk.
#[cfg(feature = "lang-julia")]
pub fn extract_julia_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_julia_source(&path_str, &source))
}

#[cfg(feature = "lang-julia")]
struct Julia<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
}

#[cfg(feature = "lang-julia")]
impl<'tree> Julia<'_, 'tree> {
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

    /// First `identifier` anywhere under `node` (BFS).
    fn first_identifier(&self, node: TsNode<'tree>) -> Option<String> {
        let mut q = std::collections::VecDeque::from([node]);
        while let Some(n) = q.pop_front() {
            if n.kind() == "identifier" {
                return Some(self.text(n));
            }
            for c in Self::children(n) {
                q.push_back(c);
            }
        }
        None
    }

    fn walk(&mut self, node: TsNode<'tree>, scope: Option<NodeId>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "module_definition" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n))
                    .unwrap_or_default();
                if !name.is_empty() {
                    let line = Self::line(node);
                    let nid = NodeId(make_id(&[&self.stem, &name]));
                    self.b.add_node(nid.clone(), name, line);
                    self.b
                        .add_edge(self.file_nid.clone(), nid.clone(), "contains", line, None);
                    for c in Self::children(node) {
                        self.walk(c, Some(nid.clone()), depth + 1);
                    }
                }
            }
            "using_statement" | "import_statement" => {
                for id in Self::children(node)
                    .into_iter()
                    .filter(|c| c.kind() == "identifier")
                {
                    let name = self.text(id);
                    if !name.is_empty() {
                        let tgt = NodeId(make_id(&["julia", "mod", &name]));
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
            }
            "struct_definition" | "abstract_definition" => {
                if let Some(name) = Self::children(node)
                    .into_iter()
                    .find(|c| c.kind() == "type_head")
                    .and_then(|h| self.first_identifier(h))
                {
                    let line = Self::line(node);
                    let nid = NodeId(make_id(&[&self.stem, &name]));
                    self.b.add_node(nid.clone(), name, line);
                    let parent = scope.clone().unwrap_or_else(|| self.file_nid.clone());
                    self.b.add_edge(parent, nid, "contains", line, None);
                }
            }
            "function_definition" | "short_function_definition" => {
                let name = Self::children(node)
                    .into_iter()
                    .find(|c| c.kind() == "signature")
                    .and_then(|s| self.first_identifier(s));
                if let Some(name) = name.filter(|n| !n.is_empty()) {
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
                    // Body = every non-signature child (the signature's own
                    // call_expression is the declaration, not a call).
                    for child in Self::children(node) {
                        if child.kind() != "signature" {
                            self.function_bodies.push((nid.clone(), child));
                        }
                    }
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
        if matches!(
            node.kind(),
            "function_definition" | "short_function_definition"
        ) {
            return;
        }
        if node.kind() == "call_expression" {
            if let Some(callee) = Self::children(node)
                .into_iter()
                .find(|c| c.kind() == "identifier")
                .map(|c| self.text(c))
            {
                if !callee.is_empty() && !JULIA_BUILTINS.contains(&callee.as_str()) {
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

#[cfg(all(test, feature = "lang-julia"))]
mod tests {
    use super::extract_julia_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_julia_source(
            "src/Dog.jl",
            b"module Dog\nusing Pkg\nstruct Animal end\nfunction bark(d)\n  sound(d)\nend\nfunction sound(d)\n  d\nend\nend\n",
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
    fn module_struct_function_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"Dog".to_string()), "{ls:?}");
        assert!(ls.contains(&"Animal".to_string()));
        assert!(ls.contains(&".bark()".to_string()));
        assert!(ls.contains(&".sound()".to_string()));
    }

    #[test]
    fn using_becomes_import() {
        assert!(rels(&extract(), "imports_from")
            .iter()
            .any(|(_, t)| t == "Pkg"));
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
