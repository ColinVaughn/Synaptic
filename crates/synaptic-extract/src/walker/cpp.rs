//! `cpp` extraction methods on `Extractor` (split from walker.rs).

use super::Extractor;
use synaptic_core::{make_id, NodeId};
use tree_sitter::Node as TsNode;

impl<'tree> Extractor<'_, '_, 'tree> {
    /// C++ class body: `field_declaration` with a `function_declarator` →
    /// a method-prototype node (+ its param/return refs); a data-member
    /// `field_declaration` → its `type` as `references` (ctx `field`).
    pub(crate) fn cpp_class_members(
        &mut self,
        body: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
    ) {
        for child in Self::children(body) {
            if child.kind() != "field_declaration" {
                continue;
            }
            let line = Self::line(child);
            if self.c_function_declarator(child).is_some() {
                let Some(name_node) = self.function_name_node(child) else {
                    continue;
                };
                let name = self.text(name_node);
                let nid = NodeId(make_id(&[class_nid.as_str(), &name]));
                self.add_node(nid.clone(), format!(".{name}()"), line);
                self.add_edge(class_nid.clone(), nid.clone(), "method", line, None);
                self.cpp_type_refs(child, &nid, stem, line);
            } else if let Some(ty) = child.child_by_field_name("type") {
                let mut out = Vec::new();
                self.collect_cpp_type_refs(ty, false, &mut out);
                self.emit_field_refs(out, class_nid, stem, line);
            }
        }
    }

    // C / C++
    /// `#include "x.h"` / `#include <x.h>` → an `imports_from` edge to the
    /// header's base name (path + extension stripped) as an external stub.
    pub(crate) fn c_include(&mut self, node: TsNode<'tree>, file_nid: &NodeId) {
        let line = Self::line(node);
        let Some(path_node) = node.child_by_field_name("path") else {
            return;
        };
        let raw = self.text(path_node);
        let inner = raw
            .trim()
            .trim_matches(|c| c == '<' || c == '>' || c == '"');
        if inner.is_empty() {
            return;
        }
        let file = inner.rsplit(['/', '\\']).next().unwrap_or(inner);
        let base = file
            .strip_suffix(".hpp")
            .or_else(|| file.strip_suffix(".hh"))
            .or_else(|| file.strip_suffix(".h"))
            .unwrap_or(file);
        if base.is_empty() {
            return;
        }
        // Namespaced so a header base name can't collide with a same-named symbol.
        let tgt = NodeId(make_id(&["cinclude", base]));
        self.add_external_node(tgt.clone(), base.to_string());
        self.add_edge(file_nid.clone(), tgt, "imports_from", line, Some("import"));
    }

    /// C++ `base_class_clause` bases → `inherits` (C++ has no interfaces).
    pub(crate) fn cpp_heritage(
        &mut self,
        decl: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        for child in Self::children(decl) {
            if child.kind() != "base_class_clause" {
                continue;
            }
            for base in Self::children(child) {
                if let Some(name) = self.cpp_type_head(base) {
                    self.link_heritage(class_nid, name, stem, line, "inherits");
                }
            }
        }
    }

    /// Head name of a C++ base/type node: `type_identifier` verbatim,
    /// `qualified_identifier` → tail, `template_type` → its head.
    fn cpp_type_head(&self, t: TsNode<'tree>) -> Option<String> {
        match t.kind() {
            "type_identifier" => Some(self.text(t)),
            "qualified_identifier" => self.text(t).rsplit("::").next().map(str::to_string),
            "template_type" => Self::children(t)
                .into_iter()
                .find_map(|c| self.cpp_type_head(c)),
            _ => None,
        }
    }

    /// C/C++ parameter (`function_declarator` → `parameter_list`) + return
    /// (`function_definition.type`) type references.
    pub(crate) fn cpp_type_refs(
        &mut self,
        func_node: TsNode<'tree>,
        func_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        let mut refs: Vec<(String, &'static str)> = Vec::new();
        if let Some(fd) = self.c_function_declarator(func_node) {
            if let Some(params) = fd.child_by_field_name("parameters") {
                for p in Self::children(params) {
                    if p.kind() != "parameter_declaration" {
                        continue;
                    }
                    if let Some(ty) = p.child_by_field_name("type") {
                        let mut out = Vec::new();
                        self.collect_cpp_type_refs(ty, false, &mut out);
                        for (n, g) in out {
                            refs.push((n, if g { "generic_arg" } else { "parameter_type" }));
                        }
                    }
                }
            }
        }
        if let Some(ret) = func_node.child_by_field_name("type") {
            let mut out = Vec::new();
            self.collect_cpp_type_refs(ret, false, &mut out);
            for (n, g) in out {
                refs.push((n, if g { "generic_arg" } else { "return_type" }));
            }
        }
        for (name, ctx) in refs {
            let tgt = self.ensure_named_node(&name, stem, line);
            if &tgt != func_nid {
                self.add_edge(func_nid.clone(), tgt, "references", line, Some(ctx));
            }
        }
    }

    /// Walk a C/C++ type subtree. `template_type` args → `generic=true`; a
    /// `struct`/`union`/`enum` specifier contributes its `name`; primitive node
    /// types (`primitive_type`, `sized_type_specifier`, `auto`,
    /// `placeholder_type_specifier`) are skipped.
    fn collect_cpp_type_refs(
        &self,
        node: TsNode<'tree>,
        generic: bool,
        out: &mut Vec<(String, bool)>,
    ) {
        match node.kind() {
            "type_identifier" => {
                let t = self.text(node);
                if !t.is_empty() {
                    out.push((t, generic));
                }
            }
            "qualified_identifier" => {
                if let Some(tail) = self.text(node).rsplit("::").next() {
                    if !tail.is_empty() {
                        out.push((tail.to_string(), generic));
                    }
                }
            }
            "struct_specifier" | "union_specifier" | "enum_specifier" | "class_specifier" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let t = self.text(name);
                    if !t.is_empty() {
                        out.push((t, generic));
                    }
                }
            }
            "template_type" => {
                for c in Self::children(node) {
                    if c.kind() == "template_argument_list" {
                        for a in Self::children(c) {
                            if a.is_named() {
                                self.collect_cpp_type_refs(a, true, out);
                            }
                        }
                    } else if c.is_named() {
                        self.collect_cpp_type_refs(c, generic, out);
                    }
                }
            }
            "primitive_type" | "sized_type_specifier" | "auto" | "placeholder_type_specifier" => {}
            _ => {
                if node.is_named() {
                    for c in Self::children(node) {
                        if c.is_named() {
                            self.collect_cpp_type_refs(c, generic, out);
                        }
                    }
                }
            }
        }
    }
}
