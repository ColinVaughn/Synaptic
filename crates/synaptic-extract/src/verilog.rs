//! Verilog extractor — custom walker (HDL).
//!
//! `module`/`interface`/`program` → container nodes; `function`/`task` →
//! `.name()` procedures under them. (Verilog has no general call graph; the
//! structural map is modules + their procedures. Module instantiations are a
//! documented future addition.)

#[cfg(feature = "lang-verilog")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-verilog")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-verilog")]
use crate::common::Builder;
#[cfg(feature = "lang-verilog")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-verilog")]
use crate::result::ExtractionResult;

const MAX_DEPTH: usize = 2000;

/// Extract a Verilog source file already in memory.
#[cfg(feature = "lang-verilog")]
pub fn extract_verilog_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_verilog::LANGUAGE.into())
        .expect("load tree-sitter-verilog");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = Verilog {
        src: source,
        b: Builder::new(path),
        file_nid: file_nid.clone(),
        stem: file_stem(path),
    };
    ex.b.add_node(file_nid, filename, 1);
    ex.walk(tree.root_node(), None, 0);
    ex.b.into_result()
}

/// Read and extract a Verilog file from disk.
#[cfg(feature = "lang-verilog")]
pub fn extract_verilog_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_verilog_source(&path_str, &source))
}

#[cfg(feature = "lang-verilog")]
struct Verilog<'a> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
}

#[cfg(feature = "lang-verilog")]
impl Verilog<'_> {
    fn text(&self, n: TsNode) -> String {
        n.utf8_text(self.src).unwrap_or("").to_string()
    }

    fn line(n: TsNode) -> usize {
        n.start_position().row + 1
    }

    fn children<'t>(n: TsNode<'t>) -> Vec<TsNode<'t>> {
        let mut c = n.walk();
        n.children(&mut c).collect()
    }

    /// First `simple_identifier` under the first descendant of kind `holder`
    /// (e.g. `module_header`, `function_identifier`).
    fn name_under(&self, node: TsNode, holder: &str) -> Option<String> {
        let mut q = std::collections::VecDeque::from([node]);
        let mut holder_node = None;
        while let Some(n) = q.pop_front() {
            if n.kind() == holder {
                holder_node = Some(n);
                break;
            }
            for c in Self::children(n) {
                q.push_back(c);
            }
        }
        let holder_node = holder_node?;
        let mut q = std::collections::VecDeque::from([holder_node]);
        while let Some(n) = q.pop_front() {
            if n.kind() == "simple_identifier" {
                return Some(self.text(n));
            }
            for c in Self::children(n) {
                q.push_back(c);
            }
        }
        None
    }

    fn walk(&mut self, node: TsNode, scope: Option<NodeId>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "module_declaration" | "interface_declaration" | "program_declaration" => {
                let holder = match node.kind() {
                    "module_declaration" => "module_header",
                    "interface_declaration" => "interface_header",
                    _ => "program_header",
                };
                if let Some(name) = self.name_under(node, holder).filter(|n| !n.is_empty()) {
                    let line = Self::line(node);
                    let nid = NodeId(make_id(&[&self.stem, &name]));
                    self.b.add_node(nid.clone(), name, line);
                    self.b
                        .add_edge(self.file_nid.clone(), nid.clone(), "contains", line, None);
                    for c in Self::children(node) {
                        self.walk(c, Some(nid.clone()), depth + 1);
                    }
                    return;
                }
            }
            "function_declaration" | "task_declaration" => {
                let holder = if node.kind() == "function_declaration" {
                    "function_identifier"
                } else {
                    "task_identifier"
                };
                if let Some(name) = self.name_under(node, holder).filter(|n| !n.is_empty()) {
                    let line = Self::line(node);
                    let (parent, label) = match &scope {
                        Some(m) => (m.clone(), format!(".{name}()")),
                        None => (self.file_nid.clone(), format!("{name}()")),
                    };
                    let rel = if scope.is_some() {
                        "method"
                    } else {
                        "contains"
                    };
                    let nid = NodeId(make_id(&[parent.as_str(), &name]));
                    self.b.add_node(nid.clone(), label, line);
                    self.b.add_edge(parent, nid, rel, line, None);
                    return;
                }
            }
            _ => {}
        }
        for c in Self::children(node) {
            self.walk(c, scope.clone(), depth + 1);
        }
    }
}

#[cfg(all(test, feature = "lang-verilog"))]
mod tests {
    use super::extract_verilog_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_verilog_source(
            "rtl/counter.v",
            b"module counter(input clk);\n  function integer foo;\n    foo = 1;\n  endfunction\n  task reset;\n  endtask\nendmodule\n",
        )
    }

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

    #[test]
    fn module_function_task_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"counter".to_string()), "{ls:?}");
        assert!(ls.contains(&".foo()".to_string()));
        assert!(ls.contains(&".reset()".to_string()));
    }

    #[test]
    fn procedures_contained_in_module() {
        let r = extract();
        let counter = r
            .nodes
            .iter()
            .find(|n| n.label == "counter")
            .map(|n| n.id.clone())
            .unwrap();
        // module to .foo() / .reset() via `method` edges.
        let methods = r
            .edges
            .iter()
            .filter(|e| e.relation == "method" && e.source == counter)
            .count();
        assert_eq!(methods, 2, "module contains 2 procedures");
    }
}
