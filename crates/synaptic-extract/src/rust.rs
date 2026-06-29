//! Rust extractor (custom walk — `impl`/trait scoping doesn't fit the generic
//! `LanguageConfig`).
//!
//! Functions → `name()` (or `.name()` scoped under an `impl`); struct/enum/trait
//! items → type nodes with `references` (fields) / `inherits` (supertraits);
//! `impl Trait for T` → `implements`; `use` → `imports_from`; intra-file calls →
//! `calls`/`raw_calls` (scoped `Type::method` calls match in-file only).

use std::collections::{HashMap, HashSet};

use synaptic_core::{make_id, NodeId};
use tree_sitter::{Node as TsNode, Parser};

use crate::common::Builder;
use crate::paths::{file_node_id, file_stem};
use crate::result::ExtractionResult;

/// Enum-variant / smart-pointer "calls" skipped as call targets.
const RUST_BUILTINS: &[&str] = &["Some", "None", "Ok", "Err", "drop"];

/// Ubiquitous trait/stdlib method names: resolving them cross-file produces
/// spurious INFERRED edges across crate boundaries, so they never enter the
/// unresolved-call queue (compared lowercase).
const RUST_TRAIT_METHOD_BLOCKLIST: &[&str] = &[
    "new",
    "default",
    "parse",
    "from_str",
    "now",
    "clone",
    "into",
    "from",
    "to_string",
    "to_owned",
    "len",
    "is_empty",
    "iter",
    "next",
    "build",
    "start",
    "run",
    "init",
    "app",
    "get",
    "set",
    "push",
    "pop",
    "insert",
    "remove",
    "contains",
    "collect",
    "map",
    "filter",
    "unwrap",
    "expect",
    "ok",
    "err",
    "some",
    "none",
    "send",
    "recv",
    "lock",
    "read",
    "write",
];

/// Recursion-depth cap mirroring the generic walker (`walker.rs`): a
/// pathologically nested AST returns early rather than overflowing the stack and
/// aborting the whole run.
const MAX_DEPTH: usize = 2000;

/// Extract a Rust source file already in memory.
pub fn extract_rust_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("load tree-sitter-rust");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };

    let file_nid = file_node_id(path);
    let file_label = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());

    let mut ex = RustExtractor {
        src: source,
        b: Builder::new(path),
        file_nid: file_nid.clone(),
        stem: file_stem(path),
        function_bodies: Vec::new(),
    };
    ex.b.add_node(file_nid, file_label, 1);
    ex.walk(tree.root_node(), None, 0);
    ex.run_call_pass();
    ex.b.into_result()
}

struct RustExtractor<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
}

impl<'tree> RustExtractor<'_, 'tree> {
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

    /// Rust visibility: a `visibility_modifier` child (`pub`, `pub(crate)`, ...)
    /// is public; its absence means module-private.
    fn rust_vis(node: TsNode<'tree>) -> Option<synaptic_core::Visibility> {
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            if ch.kind() == "visibility_modifier" {
                return Some(synaptic_core::Visibility::Public);
            }
        }
        Some(synaptic_core::Visibility::Private)
    }

    /// True when `node` is test code the path heuristic cannot see: it carries a
    /// test attribute (`#[test]`, `#[tokio::test]`, `#[rstest]`, ...) directly, or
    /// sits inside a `#[cfg(test)]` module. Catches the inline-unit-test forms
    /// that live in ordinary `src/` files.
    fn in_test_scope(&self, node: TsNode<'tree>) -> bool {
        if self.has_test_attr(node) {
            return true;
        }
        let mut p = node.parent();
        while let Some(pp) = p {
            if pp.kind() == "mod_item" && self.has_test_attr(pp) {
                return true;
            }
            p = pp.parent();
        }
        false
    }

    /// True if `node`'s immediately-preceding attribute siblings include a test
    /// attribute or a `cfg(test)` gate.
    fn has_test_attr(&self, node: TsNode<'tree>) -> bool {
        let mut sib = node.prev_sibling();
        while let Some(s) = sib {
            match s.kind() {
                "attribute_item" => {
                    if Self::attr_is_test(&self.text(s)) {
                        return true;
                    }
                }
                // Doc/line comments may sit between an attribute and its item.
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            sib = s.prev_sibling();
        }
        false
    }

    /// Whether an attribute's raw text (`#[...]`) is a test marker: a `test`
    /// attribute path (`test`, `tokio::test`, `rstest`, ...) or a `cfg`/`cfg_attr`
    /// gated on the `test` predicate. Avoids matching incidental "test" text such
    /// as `#[serde(rename = "test")]`.
    fn attr_is_test(raw: &str) -> bool {
        let inner: String = raw
            .trim()
            .trim_start_matches('#')
            .trim_start_matches('[')
            .trim_end_matches(']')
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let path = inner.split('(').next().unwrap_or("");
        if path == "test" || path.ends_with("::test") || path == "rstest" || path == "test_case" {
            return true;
        }
        if path == "cfg" || path == "cfg_attr" {
            return inner
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|t| t == "test");
        }
        false
    }

    fn walk(&mut self, node: TsNode<'tree>, parent_impl: Option<&NodeId>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "function_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let func_name = self.text(name);
                    let line = Self::line(node);
                    let vis = Self::rust_vis(node);
                    let sig = crate::signature::extract_signature(node, self.src);
                    let is_test = self.in_test_scope(node);
                    let func_nid = if let Some(impl_nid) = parent_impl {
                        let nid = NodeId(make_id(&[impl_nid.as_str(), &func_name]));
                        self.b.add_code_node(
                            nid.clone(),
                            format!(".{func_name}()"),
                            node,
                            synaptic_core::NodeKind::Method,
                            vis,
                            Some(sig),
                        );
                        self.b
                            .add_edge(impl_nid.clone(), nid.clone(), "method", line, None);
                        nid
                    } else {
                        let nid = NodeId(make_id(&[&self.stem, &func_name]));
                        self.b.add_code_node(
                            nid.clone(),
                            format!("{func_name}()"),
                            node,
                            synaptic_core::NodeKind::Function,
                            vis,
                            Some(sig),
                        );
                        self.b
                            .add_edge(self.file_nid.clone(), nid.clone(), "contains", line, None);
                        nid
                    };
                    if is_test {
                        self.b.mark_test(&func_nid);
                    }
                    self.emit_refs(node, &func_nid, line);
                    if let Some(body) = node.child_by_field_name("body") {
                        self.function_bodies.push((func_nid, body));
                    }
                }
            }
            "struct_item" | "enum_item" | "trait_item" => self.walk_type_item(node),
            "impl_item" => self.walk_impl(node, depth),
            "use_declaration" => self.walk_use(node),
            _ => {
                for child in Self::children(node) {
                    self.walk(child, None, depth + 1);
                }
            }
        }
    }

    fn walk_type_item(&mut self, node: TsNode<'tree>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let item_name = self.text(name);
        let line = Self::line(node);
        let item_nid = NodeId(make_id(&[&self.stem, &item_name]));
        let kind = match node.kind() {
            "struct_item" => synaptic_core::NodeKind::Struct,
            "enum_item" => synaptic_core::NodeKind::Enum,
            "trait_item" => synaptic_core::NodeKind::Trait,
            _ => synaptic_core::NodeKind::Other,
        };
        self.b.add_code_node(
            item_nid.clone(),
            item_name,
            node,
            kind,
            Self::rust_vis(node),
            None,
        );
        self.b.add_edge(
            self.file_nid.clone(),
            item_nid.clone(),
            "contains",
            line,
            None,
        );

        if node.kind() == "trait_item" {
            for bounds in Self::children(node)
                .into_iter()
                .filter(|c| c.kind() == "trait_bounds")
            {
                for sub in Self::children(bounds).into_iter().filter(|c| c.is_named()) {
                    let mut refs = Vec::new();
                    self.collect_type_refs(sub, false, &mut refs);
                    for (idx, (ref_name, _generic)) in refs.into_iter().enumerate() {
                        let tgt = self.b.ensure_named_node(&ref_name, &self.stem, line);
                        if tgt == item_nid {
                            continue;
                        }
                        if idx == 0 {
                            self.b
                                .add_edge(item_nid.clone(), tgt, "inherits", line, None);
                        } else {
                            self.b.add_edge(
                                item_nid.clone(),
                                tgt,
                                "references",
                                line,
                                Some("generic_arg"),
                            );
                        }
                    }
                }
            }
        }
        if node.kind() == "struct_item" {
            for fdl in Self::children(node)
                .into_iter()
                .filter(|c| c.kind() == "field_declaration_list")
            {
                for field in Self::children(fdl)
                    .into_iter()
                    .filter(|c| c.kind() == "field_declaration")
                {
                    let fline = Self::line(field);
                    let type_node = field.child_by_field_name("type").or_else(|| {
                        Self::children(field).into_iter().find(|c| {
                            matches!(
                                c.kind(),
                                "type_identifier"
                                    | "generic_type"
                                    | "scoped_type_identifier"
                                    | "reference_type"
                                    | "primitive_type"
                            )
                        })
                    });
                    let mut refs = Vec::new();
                    if let Some(tn) = type_node {
                        self.collect_type_refs(tn, false, &mut refs);
                    }
                    for (ref_name, generic) in refs {
                        let ctx = if generic { "generic_arg" } else { "field" };
                        let tgt = self.b.ensure_named_node(&ref_name, &self.stem, fline);
                        if tgt != item_nid {
                            self.b
                                .add_edge(item_nid.clone(), tgt, "references", fline, Some(ctx));
                        }
                    }
                }
            }
        }
    }

    fn walk_impl(&mut self, node: TsNode<'tree>, depth: usize) {
        let line = Self::line(node);
        let impl_nid = node.child_by_field_name("type").map(|tn| {
            let type_name = self.text(tn).trim().to_string();
            let nid = NodeId(make_id(&[&self.stem, &type_name]));
            self.b.add_node(nid.clone(), type_name, line);
            nid
        });
        if let (Some(trait_node), Some(impl_nid)) = (node.child_by_field_name("trait"), &impl_nid) {
            let mut refs = Vec::new();
            self.collect_type_refs(trait_node, false, &mut refs);
            for (idx, (ref_name, _generic)) in refs.into_iter().enumerate() {
                let tgt = self.b.ensure_named_node(&ref_name, &self.stem, line);
                if &tgt == impl_nid {
                    continue;
                }
                if idx == 0 {
                    self.b
                        .add_edge(impl_nid.clone(), tgt, "implements", line, None);
                } else {
                    self.b.add_edge(
                        impl_nid.clone(),
                        tgt,
                        "references",
                        line,
                        Some("generic_arg"),
                    );
                }
            }
        }
        if let Some(body) = node.child_by_field_name("body") {
            for child in Self::children(body) {
                self.walk(child, impl_nid.as_ref(), depth + 1);
            }
        }
    }

    fn walk_use(&mut self, node: TsNode<'tree>) {
        let Some(arg) = node.child_by_field_name("argument") else {
            return;
        };
        let raw = self.text(arg);
        let clean = raw
            .split('{')
            .next()
            .unwrap_or("")
            .trim_end_matches(':')
            .trim_end_matches('*')
            .trim_end_matches(':');
        let module_name = clean.rsplit("::").next().unwrap_or("").trim();
        if module_name.is_empty() {
            return;
        }
        let tgt = NodeId(make_id(&[module_name]));
        self.b
            .add_external_node(tgt.clone(), module_name.to_string());
        self.b.add_edge(
            self.file_nid.clone(),
            tgt,
            "imports_from",
            Self::line(node),
            Some("import"),
        );
    }

    /// Parameter + return type references.
    fn emit_refs(&mut self, func_node: TsNode<'tree>, func_nid: &NodeId, line: usize) {
        let mut refs: Vec<(String, &'static str)> = Vec::new();
        if let Some(params) = func_node.child_by_field_name("parameters") {
            for p in Self::children(params)
                .into_iter()
                .filter(|c| c.kind() == "parameter")
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
        if let Some(rt) = func_node.child_by_field_name("return_type") {
            let mut out = Vec::new();
            self.collect_type_refs(rt, false, &mut out);
            for (n, g) in out {
                refs.push((n, if g { "generic_arg" } else { "return_type" }));
            }
        }
        for (name, ctx) in refs {
            let tgt = self.b.ensure_named_node(&name, &self.stem, line);
            if &tgt != func_nid {
                self.b
                    .add_edge(func_nid.clone(), tgt, "references", line, Some(ctx));
            }
        }
    }

    /// Recursively collect type references from a Rust type node.
    fn collect_type_refs(&self, node: TsNode<'tree>, generic: bool, out: &mut Vec<(String, bool)>) {
        match node.kind() {
            "primitive_type" => {}
            "type_identifier" => {
                let t = self.text(node);
                if !t.is_empty() {
                    out.push((t, generic));
                }
            }
            "scoped_type_identifier" => {
                let full = self.text(node);
                let tail = full.rsplit("::").next().unwrap_or("").to_string();
                if !tail.is_empty() {
                    out.push((tail, generic));
                }
            }
            "generic_type" => {
                let name_node = node.child_by_field_name("type").or_else(|| {
                    Self::children(node)
                        .into_iter()
                        .find(|c| matches!(c.kind(), "type_identifier" | "scoped_type_identifier"))
                });
                if let Some(nn) = name_node {
                    let full = self.text(nn);
                    let tail = full.rsplit("::").next().unwrap_or("").to_string();
                    if !tail.is_empty() {
                        out.push((tail, generic));
                    }
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
            "reference_type" | "pointer_type" | "array_type" | "tuple_type" | "slice_type" => {
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
        index: &HashMap<String, NodeId>,
        seen_pairs: &mut HashSet<(NodeId, NodeId)>,
        depth: usize,
    ) {
        if depth >= MAX_DEPTH {
            return;
        }
        if node.kind() == "function_item" {
            return;
        }
        if node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                let (callee, is_member, is_scoped) = match func.kind() {
                    "identifier" => (Some(self.text(func)), false, false),
                    "field_expression" => (
                        func.child_by_field_name("field").map(|f| self.text(f)),
                        true,
                        false,
                    ),
                    "scoped_identifier" => (
                        func.child_by_field_name("name").map(|n| self.text(n)),
                        false,
                        true,
                    ),
                    _ => (None, false, false),
                };
                if let Some(callee) = callee {
                    if !callee.is_empty() && !RUST_BUILTINS.contains(&callee.as_str()) {
                        // Scoped (`Type::method`) and blocklisted trait-method names
                        // may still match in-file, but never enqueue a raw call.
                        let enqueue_raw = !is_scoped
                            && !RUST_TRAIT_METHOD_BLOCKLIST
                                .contains(&callee.to_lowercase().as_str());
                        let line = Self::line(node);
                        self.b.resolve_call(
                            caller,
                            &callee,
                            is_member,
                            line,
                            index,
                            seen_pairs,
                            enqueue_raw,
                        );
                    }
                }
            }
        }
        for child in Self::children(node) {
            self.walk_calls(child, caller, index, seen_pairs, depth + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::extract_rust_source;

    fn extract(src: &[u8]) -> (Vec<String>, std::collections::HashSet<(String, String)>) {
        let r = extract_rust_source("src/lib.rs", src);
        let labels = r.nodes.iter().map(|n| n.label.clone()).collect();
        let lbl = |id: &synaptic_core::NodeId| {
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
    fn impl_methods_scoped_to_type_and_call_free_fn() {
        let src = b"struct Engine { size: usize }\n\nimpl Engine {\n    fn run(&self) { helper(); }\n}\n\nfn helper() {}\n";
        let (labels, calls) = extract(src);
        assert!(labels.contains(&"Engine".to_string()), "{labels:?}");
        assert!(labels.contains(&".run()".to_string()));
        assert!(labels.contains(&"helper()".to_string()));
        assert!(
            calls.contains(&(".run()".into(), "helper()".into())),
            "{calls:?}"
        );
    }

    #[test]
    fn impl_trait_emits_implements_edge() {
        let src = b"struct S;\ntrait Greet { fn hi(&self); }\nimpl Greet for S {\n    fn hi(&self) {}\n}\n";
        let r = extract_rust_source("src/lib.rs", src);
        let rels: Vec<&str> = r.edges.iter().map(|e| e.relation.as_str()).collect();
        assert!(
            rels.contains(&"implements"),
            "S implements Greet; rels {rels:?}"
        );
        assert!(rels.contains(&"method")); // impl S -> .hi()
    }

    #[test]
    fn trait_supertrait_is_inherits() {
        let src = b"trait Base {}\ntrait Derived: Base {}\n";
        let r = extract_rust_source("src/lib.rs", src);
        assert!(
            r.edges.iter().any(|e| e.relation == "inherits"),
            "Derived: Base → inherits; edges {:?}",
            r.edges
                .iter()
                .map(|e| (&e.relation, &e.target))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn use_declaration_becomes_import_edge() {
        let r = extract_rust_source("src/lib.rs", b"use std::collections::HashMap;\nfn f() {}\n");
        let hashmap = synaptic_core::make_id(&["HashMap"]);
        assert!(r
            .edges
            .iter()
            .any(|e| e.relation == "imports_from" && e.target.0 == hashmap));
    }

    #[test]
    fn inline_cfg_test_functions_are_marked_as_tests() {
        // A src/ file (not a test path), so only the extraction flag can mark
        // these inline unit tests -- the case the path heuristic misses.
        let src = b"pub fn production() {}\n\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n    #[test]\n    fn it_works() {}\n}\n";
        let r = extract_rust_source("src/lib.rs", src);
        let marked = |label: &str| {
            r.nodes
                .iter()
                .find(|n| n.label == label)
                .unwrap_or_else(|| panic!("node {label} missing"))
                .is_test()
        };
        assert!(!marked("production()"), "production code is not a test");
        assert!(marked("it_works()"), "#[test] inside cfg(test) is a test");
        assert!(
            marked("helper()"),
            "any fn in a #[cfg(test)] mod is test code"
        );
    }

    #[test]
    fn direct_test_attribute_is_marked_outside_a_test_module() {
        let r = extract_rust_source("src/lib.rs", b"#[tokio::test]\nasync fn runs() {}\n");
        let n = r
            .nodes
            .iter()
            .find(|n| n.label == "runs()")
            .expect("fn present");
        assert!(n.is_test(), "#[tokio::test] marks a test");
    }

    #[test]
    fn production_attribute_is_not_mistaken_for_a_test() {
        // An attribute whose text incidentally contains "test" must not flag the
        // function (no false positives from string-literal arguments).
        let r = extract_rust_source(
            "src/lib.rs",
            b"#[doc = \"a test of the system\"]\npub fn real() {}\n",
        );
        let n = r
            .nodes
            .iter()
            .find(|n| n.label == "real()")
            .expect("fn present");
        assert!(!n.is_test(), "#[doc=...] is not a test attribute");
    }

    #[test]
    fn scoped_call_does_not_enqueue_raw() {
        // `Config::load()` is scoped, so no raw_call (avoids cross-crate INFERRED
        // noise), and `new` is blocklisted.
        let src = b"fn run() {\n    let c = Config::load();\n    let v = Vec::new();\n}\n";
        let r = extract_rust_source("src/lib.rs", src);
        assert!(r.raw_calls.iter().all(|c| c.callee != "load"));
        assert!(r.raw_calls.iter().all(|c| c.callee != "new"));
    }
}
