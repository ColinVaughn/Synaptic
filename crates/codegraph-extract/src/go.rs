//! Go extractor (custom walk — Go method receivers and package-scoped types
//! don't fit the generic `LanguageConfig`).
//!
//! Functions → `name()`; methods → `Receiver.method()` under a package-scoped
//! type node (so methods on the same type across files in a package share one
//! node); type declarations (struct/interface) → type nodes with `references`/
//! `embeds`; imports → `imports_from`; intra-file calls → `calls`/`raw_calls`.

use std::collections::HashSet;
use std::path::Path;

use codegraph_core::{make_id, NodeId};
use tree_sitter::{Node as TsNode, Parser};

use crate::common::Builder;
use crate::paths::{file_node_id, file_stem};
use crate::result::ExtractionResult;

/// Go predeclared type names, never emitted as type references.
const GO_PREDECLARED_TYPES: &[&str] = &[
    "bool",
    "byte",
    "complex64",
    "complex128",
    "error",
    "float32",
    "float64",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "rune",
    "string",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "any",
    "comparable",
];

/// Go predeclared functions skipped as call targets.
const GO_BUILTINS: &[&str] = &[
    "make", "len", "cap", "new", "append", "copy", "delete", "panic", "recover", "close", "print",
    "println", "complex", "real", "imag",
];

/// Recursion-depth cap mirroring the generic walker (`walker.rs`): a
/// pathologically nested AST returns early rather than overflowing the stack and
/// aborting the whole run.
const MAX_DEPTH: usize = 2000;

/// Extract a Go source file already in memory.
pub fn extract_go_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .expect("load tree-sitter-go");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };

    let stem = file_stem(path);
    let pkg_scope = Path::new(path)
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| stem.clone());

    let file_nid = file_node_id(path);
    let file_label = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());

    let mut ex = GoExtractor {
        src: source,
        b: Builder::new(path),
        file_nid: file_nid.clone(),
        stem,
        pkg_scope,
        imported_pkgs: HashSet::new(),
        function_bodies: Vec::new(),
        #[cfg(feature = "cross-language")]
        cgo: false,
    };
    ex.b.add_node(file_nid, file_label, 1);
    ex.walk(tree.root_node(), 0);
    ex.run_call_pass();
    ex.b.into_result()
}

struct GoExtractor<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    pkg_scope: String,
    imported_pkgs: HashSet<String>,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
    /// True once `import "C"` (the cgo pseudo-import) is seen, so `C.fn()` calls
    /// are treated as native bindings rather than unresolved package calls. Only
    /// present with the `cross-language` feature (the binding-edge consumer).
    #[cfg(feature = "cross-language")]
    cgo: bool,
}

impl<'tree> GoExtractor<'_, 'tree> {
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

    /// Go visibility is by name case: an uppercase initial is exported (public).
    fn go_vis(name: &str) -> Option<codegraph_core::Visibility> {
        match name.chars().next() {
            Some(c) if c.is_uppercase() => Some(codegraph_core::Visibility::Public),
            Some(_) => Some(codegraph_core::Visibility::Private),
            None => None,
        }
    }

    fn walk(&mut self, node: TsNode<'tree>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "function_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let func_name = self.text(name);
                    let line = Self::line(node);
                    let func_nid = NodeId(make_id(&[&self.stem, &func_name]));
                    self.b.add_code_node(
                        func_nid.clone(),
                        format!("{func_name}()"),
                        node,
                        codegraph_core::NodeKind::Function,
                        Self::go_vis(&func_name),
                        Some(crate::signature::extract_signature(node, self.src)),
                    );
                    self.b.add_edge(
                        self.file_nid.clone(),
                        func_nid.clone(),
                        "contains",
                        line,
                        None,
                    );
                    self.emit_refs(node, &func_nid, line);
                    if let Some(body) = node.child_by_field_name("body") {
                        self.function_bodies.push((func_nid, body));
                    }
                }
            }
            "method_declaration" => {
                let recv_type = node
                    .child_by_field_name("receiver")
                    .and_then(|r| {
                        Self::children(r)
                            .into_iter()
                            .find(|c| c.kind() == "parameter_declaration")
                    })
                    .and_then(|p| p.child_by_field_name("type"))
                    .map(|t| self.text(t).trim_start_matches('*').trim().to_string())
                    .filter(|s| !s.is_empty());
                let Some(name) = node.child_by_field_name("name") else {
                    return;
                };
                let method_name = self.text(name);
                let line = Self::line(node);
                let sig = crate::signature::extract_signature(node, self.src);
                let method_nid = if let Some(recv) = recv_type {
                    let parent = NodeId(make_id(&[&self.pkg_scope, &recv]));
                    // Receiver-type stub (the type's own decl is enriched in
                    // walk_type_spec); leave it a plain node.
                    self.b.add_node(parent.clone(), recv, line);
                    let m = NodeId(make_id(&[parent.as_str(), &method_name]));
                    self.b.add_code_node(
                        m.clone(),
                        format!(".{method_name}()"),
                        node,
                        codegraph_core::NodeKind::Method,
                        Self::go_vis(&method_name),
                        Some(sig),
                    );
                    self.b.add_edge(parent, m.clone(), "method", line, None);
                    m
                } else {
                    let m = NodeId(make_id(&[&self.stem, &method_name]));
                    self.b.add_code_node(
                        m.clone(),
                        format!("{method_name}()"),
                        node,
                        codegraph_core::NodeKind::Method,
                        Self::go_vis(&method_name),
                        Some(sig),
                    );
                    self.b
                        .add_edge(self.file_nid.clone(), m.clone(), "contains", line, None);
                    m
                };
                self.emit_refs(node, &method_nid, line);
                if let Some(body) = node.child_by_field_name("body") {
                    self.function_bodies.push((method_nid, body));
                }
            }
            "type_declaration" => {
                for spec in Self::children(node)
                    .into_iter()
                    .filter(|c| c.kind() == "type_spec")
                {
                    self.walk_type_spec(spec);
                }
            }
            "import_declaration" => self.walk_imports(node),
            _ => {
                for child in Self::children(node) {
                    self.walk(child, depth + 1);
                }
            }
        }
    }

    fn walk_type_spec(&mut self, spec: TsNode<'tree>) {
        let Some(name) = spec.child_by_field_name("name") else {
            return;
        };
        let type_name = self.text(name);
        let line = Self::line(spec);
        let type_nid = NodeId(make_id(&[&self.pkg_scope, &type_name]));
        let kind = Self::children(spec)
            .into_iter()
            .find_map(|c| match c.kind() {
                "struct_type" => Some(codegraph_core::NodeKind::Struct),
                "interface_type" => Some(codegraph_core::NodeKind::Interface),
                _ => None,
            })
            .unwrap_or(codegraph_core::NodeKind::TypeAlias);
        let vis = Self::go_vis(&type_name);
        self.b
            .add_code_node(type_nid.clone(), type_name, spec, kind, vis, None);
        self.b.add_edge(
            self.file_nid.clone(),
            type_nid.clone(),
            "contains",
            line,
            None,
        );

        let Some(body) = Self::children(spec)
            .into_iter()
            .find(|c| matches!(c.kind(), "struct_type" | "interface_type"))
        else {
            return;
        };
        if body.kind() == "struct_type" {
            for fdl in Self::children(body)
                .into_iter()
                .filter(|c| c.kind() == "field_declaration_list")
            {
                for field in Self::children(fdl)
                    .into_iter()
                    .filter(|c| c.kind() == "field_declaration")
                {
                    let has_name = Self::children(field)
                        .iter()
                        .any(|c| c.kind() == "field_identifier");
                    let fline = Self::line(field);
                    let type_node = field.child_by_field_name("type").or_else(|| {
                        Self::children(field)
                            .into_iter()
                            .find(|c| c.is_named() && c.kind() != "field_identifier")
                    });
                    let mut refs = Vec::new();
                    if let Some(tn) = type_node {
                        self.collect_type_refs(tn, false, &mut refs);
                    }
                    for (ref_name, generic) in refs {
                        let tgt = self.b.ensure_named_node(&ref_name, &self.pkg_scope, fline);
                        if tgt == type_nid {
                            continue;
                        }
                        if !has_name && !generic {
                            self.b
                                .add_edge(type_nid.clone(), tgt, "embeds", fline, None);
                        } else {
                            let ctx = if generic { "generic_arg" } else { "field" };
                            self.b
                                .add_edge(type_nid.clone(), tgt, "references", fline, Some(ctx));
                        }
                    }
                }
            }
        } else {
            // interface_type
            for elem in Self::children(body)
                .into_iter()
                .filter(|c| c.kind() == "type_elem")
            {
                let eline = Self::line(elem);
                let mut refs = Vec::new();
                for sub in Self::children(elem).into_iter().filter(|c| c.is_named()) {
                    self.collect_type_refs(sub, false, &mut refs);
                }
                for (ref_name, generic) in refs {
                    let tgt = self.b.ensure_named_node(&ref_name, &self.pkg_scope, eline);
                    if tgt == type_nid {
                        continue;
                    }
                    if !generic {
                        self.b
                            .add_edge(type_nid.clone(), tgt, "embeds", eline, None);
                    } else {
                        self.b.add_edge(
                            type_nid.clone(),
                            tgt,
                            "references",
                            eline,
                            Some("generic_arg"),
                        );
                    }
                }
            }
        }
    }

    fn walk_imports(&mut self, node: TsNode<'tree>) {
        let mut specs: Vec<TsNode<'tree>> = Vec::new();
        for child in Self::children(node) {
            match child.kind() {
                "import_spec" => specs.push(child),
                "import_spec_list" => specs.extend(
                    Self::children(child)
                        .into_iter()
                        .filter(|c| c.kind() == "import_spec"),
                ),
                _ => {}
            }
        }
        for spec in specs {
            let Some(path_node) = spec.child_by_field_name("path") else {
                continue;
            };
            let raw = self.text(path_node);
            let raw = raw.trim_matches('"');
            if raw.is_empty() {
                continue;
            }
            // `import "C"` is cgo's pseudo-package, not a real import. The
            // pseudo-import is skipped regardless; the cgo flag (which drives the
            // binding edges) is only tracked under the cross-language feature.
            if raw == "C" {
                #[cfg(feature = "cross-language")]
                {
                    self.cgo = true;
                }
                continue;
            }
            // Prefix so stdlib names (e.g. "context") don't collide with local files.
            let tgt = NodeId(make_id(&["go", "pkg", raw]));
            self.b.add_external_node(tgt.clone(), raw.to_string());
            self.b.add_edge(
                self.file_nid.clone(),
                tgt,
                "imports_from",
                Self::line(spec),
                Some("import"),
            );
            let local = spec
                .child_by_field_name("name")
                .map(|n| self.text(n))
                .unwrap_or_else(|| raw.rsplit('/').next().unwrap_or(raw).to_string());
            if !local.is_empty() && local != "_" && local != "." {
                self.imported_pkgs.insert(local);
            }
        }
    }

    /// Parameter + result type references.
    fn emit_refs(&mut self, func_node: TsNode<'tree>, func_nid: &NodeId, line: usize) {
        let mut refs: Vec<(String, &'static str)> = Vec::new();
        if let Some(params) = func_node.child_by_field_name("parameters") {
            for p in Self::children(params)
                .into_iter()
                .filter(|c| c.kind() == "parameter_declaration")
            {
                if let Some(tn) = p.child_by_field_name("type") {
                    let mut out = Vec::new();
                    self.collect_type_refs(tn, false, &mut out);
                    for (n, g) in out {
                        refs.push((n, if g { "generic_arg" } else { "parameter_type" }));
                    }
                }
            }
        }
        if let Some(result) = func_node.child_by_field_name("result") {
            let param_decls: Vec<TsNode<'tree>> = if result.kind() == "parameter_list" {
                Self::children(result)
                    .into_iter()
                    .filter(|c| c.kind() == "parameter_declaration")
                    .collect()
            } else {
                vec![]
            };
            if param_decls.is_empty() {
                let mut out = Vec::new();
                self.collect_type_refs(result, false, &mut out);
                for (n, g) in out {
                    refs.push((n, if g { "generic_arg" } else { "return_type" }));
                }
            } else {
                for p in param_decls {
                    let tn = p
                        .child_by_field_name("type")
                        .or_else(|| Self::children(p).into_iter().find(|c| c.is_named()));
                    if let Some(tn) = tn {
                        let mut out = Vec::new();
                        self.collect_type_refs(tn, false, &mut out);
                        for (n, g) in out {
                            refs.push((n, if g { "generic_arg" } else { "return_type" }));
                        }
                    }
                }
            }
        }
        for (name, ctx) in refs {
            let tgt = self.b.ensure_named_node(&name, &self.pkg_scope, line);
            if &tgt != func_nid {
                self.b
                    .add_edge(func_nid.clone(), tgt, "references", line, Some(ctx));
            }
        }
    }

    /// Recursively collect type references from a Go type node.
    fn collect_type_refs(&self, node: TsNode<'tree>, generic: bool, out: &mut Vec<(String, bool)>) {
        match node.kind() {
            "type_identifier" => {
                let t = self.text(node);
                if !t.is_empty() && !GO_PREDECLARED_TYPES.contains(&t.as_str()) {
                    out.push((t, generic));
                }
            }
            "qualified_type" => {
                let full = self.text(node);
                let tail = full.rsplit('.').next().unwrap_or("").to_string();
                if !tail.is_empty() && !GO_PREDECLARED_TYPES.contains(&tail.as_str()) {
                    out.push((tail, generic));
                }
            }
            "generic_type" => {
                if let Some(tf) = node.child_by_field_name("type") {
                    self.collect_type_refs(tf, generic, out);
                }
                for c in Self::children(node)
                    .into_iter()
                    .filter(|c| c.kind() == "type_arguments")
                {
                    for arg in Self::children(c).into_iter().filter(|a| a.is_named()) {
                        self.collect_type_refs(arg, true, out);
                    }
                }
            }
            "pointer_type" | "slice_type" | "array_type" | "map_type" | "channel_type"
            | "parenthesized_type" => {
                for c in Self::children(node).into_iter().filter(|c| c.is_named()) {
                    self.collect_type_refs(c, generic, out);
                }
            }
            _ => {
                if node.is_named() {
                    for c in Self::children(node).into_iter().filter(|c| c.is_named()) {
                        self.collect_type_refs(c, generic, out);
                    }
                }
            }
        }
    }

    fn run_call_pass(&mut self) {
        let index = self.b.label_index();
        let bodies = std::mem::take(&mut self.function_bodies);
        let mut seen_pairs: HashSet<(NodeId, NodeId)> = HashSet::new();
        for (caller, body) in bodies {
            self.walk_calls(body, &caller, &index, &mut seen_pairs, 0);
        }
    }

    fn walk_calls(
        &mut self,
        node: TsNode<'tree>,
        caller: &NodeId,
        index: &std::collections::HashMap<String, NodeId>,
        seen_pairs: &mut HashSet<(NodeId, NodeId)>,
        depth: usize,
    ) {
        if depth >= MAX_DEPTH {
            return;
        }
        if matches!(node.kind(), "function_declaration" | "method_declaration") {
            return;
        }
        if node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                // cgo `C.fn()` is a native binding, not a Go call (cross-language
                // feature only); when handled it suppresses the normal call.
                #[cfg(feature = "cross-language")]
                let handled = self.try_cgo_call(func, node, caller);
                #[cfg(not(feature = "cross-language"))]
                let handled = false;
                if !handled {
                    let (callee, is_member) = match func.kind() {
                        "identifier" => (Some(self.text(func)), false),
                        "selector_expression" => {
                            let field = func.child_by_field_name("field").map(|f| self.text(f));
                            let operand = func
                                .child_by_field_name("operand")
                                .map(|o| self.text(o))
                                .unwrap_or_default();
                            // Package-qualified call is resolvable; receiver is a member.
                            (field, !self.imported_pkgs.contains(&operand))
                        }
                        _ => (None, false),
                    };
                    if let Some(callee) = callee {
                        if !callee.is_empty() && !GO_BUILTINS.contains(&callee.as_str()) {
                            let line = Self::line(node);
                            self.b.resolve_call(
                                caller, &callee, is_member, line, index, seen_pairs, true,
                            );
                        }
                    }
                }
            }
        }
        for child in Self::children(node) {
            self.walk_calls(child, caller, index, seen_pairs, depth + 1);
        }
    }

    /// cgo: a `C.fn()` selector in a file that imported "C" links the caller to a
    /// native target via `binds_native`. Returns true when it handled the call
    /// (so the normal call resolution is skipped).
    #[cfg(feature = "cross-language")]
    fn try_cgo_call(&mut self, func: TsNode<'tree>, node: TsNode<'tree>, caller: &NodeId) -> bool {
        if !self.cgo || func.kind() != "selector_expression" {
            return false;
        }
        let operand = func
            .child_by_field_name("operand")
            .map(|o| self.text(o))
            .unwrap_or_default();
        if operand != "C" {
            return false;
        }
        if let Some(field) = func.child_by_field_name("field") {
            let fname = self.text(field);
            if !fname.is_empty() {
                let line = Self::line(node);
                let tgt = NodeId(make_id(&["native", "c", &fname]));
                self.b.add_external_node(tgt.clone(), format!("C.{fname}"));
                self.b
                    .add_inferred_edge(caller.clone(), tgt, "binds_native", line, Some("cgo"));
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::extract_go_source;

    fn pairs(src: &[u8]) -> (Vec<String>, std::collections::HashSet<(String, String)>) {
        let r = extract_go_source("pkg/svc.go", src);
        let labels = r.nodes.iter().map(|n| n.label.clone()).collect();
        let lbl = |id: &codegraph_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        let calls = r
            .edges
            .iter()
            .filter(|e| e.relation == "calls")
            .map(|e| (lbl(&e.source), lbl(&e.target)))
            .collect();
        (labels, calls)
    }

    #[test]
    fn method_scoped_under_receiver_type() {
        let src = b"package svc\n\ntype Server struct { name string }\n\nfunc (s *Server) Start() { helper() }\n\nfunc helper() {}\n";
        let (labels, calls) = pairs(src);
        assert!(labels.contains(&"Server".to_string()), "{labels:?}");
        assert!(labels.contains(&".Start()".to_string()));
        assert!(labels.contains(&"helper()".to_string()));
        // method Start() calls module func helper().
        assert!(
            calls.contains(&(".Start()".into(), "helper()".into())),
            "{calls:?}"
        );
    }

    #[test]
    fn type_method_edge_and_function_contains() {
        let r = extract_go_source(
            "pkg/svc.go",
            b"package svc\n\ntype T struct {}\n\nfunc (t T) M() {}\n\nfunc Free() {}\n",
        );
        let rels: Vec<&str> = r.edges.iter().map(|e| e.relation.as_str()).collect();
        assert!(rels.contains(&"method")); // T -> .M()
        assert!(rels.contains(&"contains")); // file -> Free(), file -> T
    }

    #[test]
    fn imports_become_prefixed_stub_edges() {
        let r = extract_go_source(
            "pkg/svc.go",
            b"package svc\n\nimport (\n  \"fmt\"\n  \"context\"\n)\n",
        );
        let fmt_id = codegraph_core::make_id(&["go", "pkg", "fmt"]);
        assert!(r.nodes.iter().any(|n| n.id.0 == fmt_id && n.label == "fmt"));
        assert!(r
            .edges
            .iter()
            .any(|e| e.relation == "imports_from" && e.target.0 == fmt_id));
    }

    #[test]
    fn package_qualified_call_is_not_member() {
        // `fmt.Println` is a package call, not a member call, so it goes to raw_calls
        // (cross-file/external), not dropped as a receiver method.
        let r = extract_go_source(
            "pkg/svc.go",
            b"package svc\n\nimport \"fmt\"\n\nfunc Go() { fmt.Println(\"x\") }\n",
        );
        let rc = r.raw_calls.iter().find(|c| c.callee == "Println");
        assert!(rc.is_some(), "raw_calls: {:?}", r.raw_calls);
        assert!(
            !rc.unwrap().is_member_call,
            "fmt is an imported pkg, not a receiver"
        );
    }

    #[cfg(feature = "cross-language")]
    #[test]
    fn cgo_calls_emit_binds_native_edges() {
        // A cgo file (`import "C"`) calling C.sqrt links to a native target via a
        // `binds_native` edge (INFERRED) rather than an unresolved package call.
        let src = b"package main\n\n// #include <math.h>\nimport \"C\"\n\nfunc Compute() float64 {\n\treturn float64(C.sqrt(4))\n}\n";
        let r = extract_go_source("m.go", src);

        let csqrt = r
            .nodes
            .iter()
            .find(|n| n.label == "C.sqrt")
            .expect("native target node for the C function");
        assert!(
            csqrt.source_file.is_empty(),
            "native target is an external stub"
        );

        let compute = r.nodes.iter().find(|n| n.label == "Compute()").unwrap();
        let edge = r
            .edges
            .iter()
            .find(|e| e.relation == "binds_native")
            .expect("a binds_native edge");
        assert_eq!(edge.source, compute.id);
        assert_eq!(edge.target, csqrt.id);
        assert_eq!(edge.confidence, codegraph_core::Confidence::Inferred);
        assert_eq!(edge.context.as_deref(), Some("cgo"));
    }
}
