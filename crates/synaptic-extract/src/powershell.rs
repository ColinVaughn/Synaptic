//! PowerShell extractor — custom walker.
//!
//! `function_statement` → `name()` function nodes; `Import-Module`/`using module`
//! → `imports_from`; commands invoking an in-file function → `calls` edges.
//! (Commands that don't resolve to a defined function stay unresolved — cmdlets
//! never create spurious edges.)

#[cfg(feature = "lang-powershell")]
use std::collections::HashSet;

#[cfg(feature = "lang-powershell")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-powershell")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-powershell")]
use crate::common::Builder;
#[cfg(feature = "lang-powershell")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-powershell")]
use crate::result::ExtractionResult;

const MAX_DEPTH: usize = 2000;

/// Extract a PowerShell source file already in memory.
#[cfg(feature = "lang-powershell")]
pub fn extract_powershell_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_powershell::LANGUAGE.into())
        .expect("load tree-sitter-powershell");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = Ps {
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

/// Read and extract a PowerShell file from disk.
#[cfg(feature = "lang-powershell")]
pub fn extract_powershell_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_powershell_source(&path_str, &source))
}

#[cfg(feature = "lang-powershell")]
struct Ps<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
}

#[cfg(feature = "lang-powershell")]
impl<'tree> Ps<'_, 'tree> {
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

    /// A command's name (`Import-Module`, `helper`, …), case-folded later by callers.
    fn command_name(&self, cmd: TsNode<'tree>) -> Option<String> {
        let n = cmd.child_by_field_name("command_name").or_else(|| {
            Self::children(cmd)
                .into_iter()
                .find(|c| c.kind() == "command_name")
        })?;
        Some(self.text(n).trim().to_string())
    }

    /// A command's first bareword argument (the module name for `Import-Module`).
    fn first_arg(&self, cmd: TsNode<'tree>) -> Option<String> {
        let elems = cmd.child_by_field_name("command_elements").or_else(|| {
            Self::children(cmd)
                .into_iter()
                .find(|c| c.kind() == "command_elements")
        })?;
        Self::children(elems)
            .into_iter()
            .find(|c| {
                matches!(
                    c.kind(),
                    "generic_token"
                        | "command_argument"
                        | "expandable_string_literal"
                        | "verbatim_string_literal"
                )
            })
            .map(|c| {
                self.text(c)
                    .trim_matches(|ch| ch == '"' || ch == '\'')
                    .to_string()
            })
    }

    fn walk(&mut self, node: TsNode<'tree>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "function_statement" => {
                if let Some(name_node) = Self::children(node)
                    .into_iter()
                    .find(|c| c.kind() == "function_name")
                {
                    let name = self.text(name_node);
                    let line = Self::line(node);
                    let nid = NodeId(make_id(&[&self.stem, &name]));
                    self.b.add_node(nid.clone(), format!("{name}()"), line);
                    self.b
                        .add_edge(self.file_nid.clone(), nid.clone(), "contains", line, None);
                    if let Some(block) = Self::children(node)
                        .into_iter()
                        .find(|c| c.kind() == "script_block")
                    {
                        self.function_bodies.push((nid, block));
                    }
                }
            }
            "command" => {
                if let Some(name) = self.command_name(node) {
                    if name.eq_ignore_ascii_case("import-module")
                        || name.eq_ignore_ascii_case("using")
                    {
                        if let Some(arg) = self.first_arg(node) {
                            self.emit_import(&arg, Self::line(node));
                        }
                    }
                }
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

    fn emit_import(&mut self, raw: &str, line: usize) {
        let base = raw.rsplit(['/', '\\', '.']).next().unwrap_or(raw);
        if base.is_empty() {
            return;
        }
        let tgt = NodeId(make_id(&["ps", "mod", base]));
        self.b.add_external_node(tgt.clone(), base.to_string());
        self.b.add_edge(
            self.file_nid.clone(),
            tgt,
            "imports_from",
            line,
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
        if node.kind() == "function_statement" {
            return;
        }
        if node.kind() == "command" {
            if let Some(name) = self.command_name(node) {
                if !name.is_empty() && !name.eq_ignore_ascii_case("import-module") {
                    self.b
                        .resolve_call(caller, &name, false, Self::line(node), index, seen, true);
                }
            }
        }
        for c in Self::children(node) {
            self.walk_calls(c, caller, index, seen, depth + 1);
        }
    }
}

#[cfg(all(test, feature = "lang-powershell"))]
mod tests {
    use super::extract_powershell_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_powershell_source(
            "scripts/app.ps1",
            b"Import-Module Foo\n\nfunction Get-Thing {\n  param($x)\n  Write-Host hi\n  helper\n}\n\nfunction helper {\n  Write-Host helping\n}\n",
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
    fn function_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"Get-Thing()".to_string()), "{ls:?}");
        assert!(ls.contains(&"helper()".to_string()));
    }

    #[test]
    fn import_module_becomes_import() {
        let imps = rels(&extract(), "imports_from");
        assert!(imps.iter().any(|(_, t)| t == "Foo"), "{imps:?}");
    }

    #[test]
    fn command_call_resolves_to_function() {
        let calls = rels(&extract(), "calls");
        // Get-Thing calls helper; Write-Host (a cmdlet) resolves to nothing.
        assert!(
            calls.contains(&("Get-Thing()".to_string(), "helper()".to_string())),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|(_, t)| t.starts_with("Write-Host")));
    }
}
