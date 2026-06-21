//! Bash extractor — custom walker (no classes).
//!
//! `function_definition` → `name()` function nodes; `source`/`.` commands →
//! `imports_from` to the sourced script (resolved relative to the sourcing file
//! → the target's own file-node id; a variable/glob path falls back to the
//! sourced script's base name); command invocations of an in-file function →
//! `calls` edges. Built-in commands are not call targets.

#[cfg(feature = "lang-bash")]
use std::collections::HashSet;

#[cfg(feature = "lang-bash")]
use synaptic_core::{make_id, NodeId};
#[cfg(feature = "lang-bash")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-bash")]
use crate::common::Builder;
#[cfg(feature = "lang-bash")]
use crate::paths::{file_node_id, file_stem, resolve_relative_path};
#[cfg(feature = "lang-bash")]
use crate::result::ExtractionResult;

/// Common builtins / coreutils never treated as in-file call targets.
#[cfg(feature = "lang-bash")]
const BASH_BUILTINS: &[&str] = &[
    "echo", "printf", "read", "cd", "pwd", "ls", "cat", "grep", "sed", "awk", "test", "[", "[[",
    "return", "local", "export", "set", "unset", "shift", "eval", "exit", "true", "false",
    "source", ".", "exec", "trap", "wait", "kill", "sleep", "mkdir", "rm", "cp", "mv", "touch",
    "chmod", "command", "declare", "readonly", "typeset", "let", "break", "continue", "if", "then",
    "fi", "for", "while", "do", "done", "case", "esac",
];

const MAX_DEPTH: usize = 2000;

/// Extract a Bash source file already in memory.
#[cfg(feature = "lang-bash")]
pub fn extract_bash_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .expect("load tree-sitter-bash");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = Bash {
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

/// Read and extract a Bash file from disk.
#[cfg(feature = "lang-bash")]
pub fn extract_bash_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_bash_source(&path_str, &source))
}

#[cfg(feature = "lang-bash")]
struct Bash<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
}

#[cfg(feature = "lang-bash")]
impl<'tree> Bash<'_, 'tree> {
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

    /// The command's name word (`source`, `helper`, …).
    fn command_name(&self, cmd: TsNode<'tree>) -> Option<String> {
        let name = cmd.child_by_field_name("name")?;
        // `name` is a `command_name` wrapping a `word`/`string`.
        Some(self.text(name).trim().to_string())
    }

    /// The command's first argument word (the sourced path for `source`/`.`).
    fn first_arg(&self, cmd: TsNode<'tree>) -> Option<String> {
        Self::children(cmd)
            .into_iter()
            .find(|c| matches!(c.kind(), "word" | "string" | "concatenation"))
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
            "function_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.text(name_node);
                    let line = Self::line(node);
                    let nid = NodeId(make_id(&[&self.stem, &name]));
                    self.b.add_node(nid.clone(), format!("{name}()"), line);
                    self.b
                        .add_edge(self.file_nid.clone(), nid.clone(), "contains", line, None);
                    if let Some(body) = node.child_by_field_name("body") {
                        self.function_bodies.push((nid, body));
                    }
                }
            }
            "command" => {
                // `source x` / `. x` imports the sourced script's base name.
                if let Some(name) = self.command_name(node) {
                    if matches!(name.as_str(), "source" | ".") {
                        if let Some(arg) = self.first_arg(node) {
                            self.emit_source_import(&arg, Self::line(node));
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

    fn emit_source_import(&mut self, raw: &str, line: usize) {
        let raw = raw.trim();
        if raw.is_empty() {
            return;
        }
        // A path with shell variables (`source "$DIR/x.sh"`) can't be resolved
        // statically; fall back to the sourced script's base name (best-effort,
        // the pre-I-18 behavior).
        if raw.contains('$') {
            let file = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
            let base = file
                .strip_suffix(".sh")
                .or_else(|| file.strip_suffix(".bash"))
                .unwrap_or(file);
            if base.is_empty() {
                return;
            }
            let tgt = NodeId(make_id(&["bash", "src", base]));
            self.b.add_external_node(tgt.clone(), base.to_string());
            self.b.add_edge(
                self.file_nid.clone(),
                tgt,
                "imports_from",
                line,
                Some("import"),
            );
            return;
        }
        // Resolve relative to the sourcing file's own directory (static path
        // resolution) to the target file's
        // own node id. This makes two same-named scripts in different dirs resolve
        // to distinct file nodes, and an in-corpus target connects to its real
        // file node by id (the I-18 fix). [`crate::paths::resolve_relative_path`]
        let resolved = resolve_relative_path(&self.b.path, raw);
        if resolved.is_empty() {
            return;
        }
        let label = std::path::Path::new(&resolved)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| resolved.clone());
        let tgt = file_node_id(&resolved);
        self.b.add_external_node(tgt.clone(), label);
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
        if node.kind() == "function_definition" {
            return; // don't descend into a nested function's body
        }
        if node.kind() == "command" {
            if let Some(name) = self.command_name(node) {
                if !name.is_empty() && !BASH_BUILTINS.contains(&name.as_str()) {
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

#[cfg(all(test, feature = "lang-bash"))]
mod tests {
    use super::extract_bash_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_bash_source(
            "scripts/app.sh",
            b"source ./lib.sh\n\ngreet() {\n  echo hi\n  helper\n}\n\nfunction helper {\n  echo helping\n}\n",
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
        assert!(ls.contains(&"greet()".to_string()), "{ls:?}");
        assert!(ls.contains(&"helper()".to_string()));
    }

    #[test]
    fn source_becomes_import() {
        // `source ./lib.sh` from scripts/app.sh resolves to scripts/lib.sh (the
        // target's own file-node id), not a base-name stub.
        let r = extract();
        let want = crate::paths::file_node_id("scripts/lib.sh");
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "imports_from" && e.target == want),
            "{:?}",
            r.edges
        );
    }

    #[test]
    fn same_name_in_different_dirs_resolve_distinctly() {
        // The I-18 fix: lib.sh sourced from a/ and from b/ must be distinct nodes.
        let a = extract_bash_source("a/app.sh", b"source ./lib.sh\n");
        let b = extract_bash_source("b/app.sh", b"source ./lib.sh\n");
        let target = |r: &ExtractionResult| {
            r.edges
                .iter()
                .find(|e| e.relation == "imports_from")
                .map(|e| e.target.clone())
                .unwrap()
        };
        let ta = target(&a);
        let tb = target(&b);
        assert_ne!(ta, tb);
        assert_eq!(ta, crate::paths::file_node_id("a/lib.sh"));
        assert_eq!(tb, crate::paths::file_node_id("b/lib.sh"));
    }

    #[test]
    fn parent_relative_source_resolves() {
        let r = extract_bash_source("scripts/sub/app.sh", b"source ../common/util.sh\n");
        let want = crate::paths::file_node_id("scripts/common/util.sh");
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "imports_from" && e.target == want),
            "{:?}",
            r.edges
        );
    }

    #[test]
    fn variable_source_path_falls_back_to_base_name() {
        let r = extract_bash_source("scripts/app.sh", b"source \"$LIB_DIR/helpers.sh\"\n");
        let imps = rels(&r, "imports_from");
        assert!(imps.iter().any(|(_, t)| t == "helpers"), "{imps:?}");
    }

    #[test]
    fn function_call_resolves_and_builtins_skipped() {
        let calls = rels(&extract(), "calls");
        // greet() calls helper(); `echo` is a builtin, not a call.
        assert!(
            calls.contains(&("greet()".to_string(), "helper()".to_string())),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|(_, t)| t == "echo" || t == "echo()"));
    }
}
