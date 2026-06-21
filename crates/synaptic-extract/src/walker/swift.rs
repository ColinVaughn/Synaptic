//! `swift` extraction methods on `Extractor` (split from walker.rs).

use super::Extractor;
use synaptic_core::{make_id, NodeId};
use tree_sitter::Node as TsNode;

impl<'tree> Extractor<'_, '_, 'tree> {
    // Swift
    /// Swift `inheritance_specifier` bases → `inherits`/`implements`, classified
    /// by the pre-scanned protocol set (protocols → `implements`, else the class
    /// base → `inherits`).
    pub(crate) fn swift_heritage(
        &mut self,
        decl: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        for child in Self::children(decl) {
            if child.kind() != "inheritance_specifier" {
                continue;
            }
            let base = child.child_by_field_name("inherits_from").or_else(|| {
                Self::children(child)
                    .into_iter()
                    .find(|c| c.kind() == "user_type")
            });
            let Some(base) = base else { continue };
            let Some(name) = self.swift_type_head(base) else {
                continue;
            };
            let relation = if self.interface_names.contains(&name) {
                "implements"
            } else {
                "inherits"
            };
            self.link_heritage(class_nid, name, stem, line, relation);
        }
    }

    /// First `type_identifier` under a Swift type subtree.
    fn swift_type_head(&self, node: TsNode<'tree>) -> Option<String> {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "type_identifier" {
                return Some(self.text(n));
            }
            for c in Self::children(n) {
                stack.push(c);
            }
        }
        None
    }

    /// Swift parameter + return type references. The grammar reuses the `name`
    /// field: a function/parameter has a `name` for the identifier and a second
    /// `name` holding the `user_type` (the type) — we collect the `user_type` one.
    pub(crate) fn swift_type_refs(
        &mut self,
        func_node: TsNode<'tree>,
        func_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        let mut refs: Vec<(String, &'static str)> = Vec::new();
        for p in Self::children(func_node)
            .into_iter()
            .filter(|c| c.kind() == "parameter")
        {
            for ty in self
                .named_field_nodes(p, "name")
                .into_iter()
                .filter(|c| c.kind() == "user_type")
            {
                let mut out = Vec::new();
                self.collect_swift_type_refs(ty, false, &mut out);
                for (n, g) in out {
                    refs.push((n, if g { "generic_arg" } else { "parameter_type" }));
                }
            }
        }
        for ty in self
            .named_field_nodes(func_node, "name")
            .into_iter()
            .filter(|c| c.kind() == "user_type")
        {
            let mut out = Vec::new();
            self.collect_swift_type_refs(ty, false, &mut out);
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

    /// Walk a Swift type subtree; `type_arguments` recurse `generic=true`.
    fn collect_swift_type_refs(
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
            "user_type" => {
                for c in Self::children(node) {
                    if c.kind() == "type_arguments" {
                        for a in Self::children(c) {
                            if a.is_named() {
                                self.collect_swift_type_refs(a, true, out);
                            }
                        }
                    } else if c.is_named() {
                        self.collect_swift_type_refs(c, generic, out);
                    }
                }
            }
            _ => {
                if node.is_named() {
                    for c in Self::children(node) {
                        if c.is_named() {
                            self.collect_swift_type_refs(c, generic, out);
                        }
                    }
                }
            }
        }
    }

    /// Swift body members: `property_declaration` (`type_annotation` →
    /// `references`) and `init`/`deinit`/`subscript` declarations → method nodes
    /// with synthetic names (the grammar gives them no `name` field).
    pub(crate) fn swift_class_members(
        &mut self,
        body: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
    ) {
        for child in Self::children(body) {
            match child.kind() {
                "property_declaration" => {
                    let mut out = Vec::new();
                    for ta in Self::children(child)
                        .into_iter()
                        .filter(|c| c.kind() == "type_annotation")
                    {
                        for ut in self.named_field_nodes(ta, "name") {
                            self.collect_swift_type_refs(ut, false, &mut out);
                        }
                    }
                    self.emit_field_refs(out, class_nid, stem, Self::line(child));
                }
                "init_declaration" | "deinit_declaration" | "subscript_declaration" => {
                    let name = match child.kind() {
                        "init_declaration" => "init",
                        "deinit_declaration" => "deinit",
                        _ => "subscript",
                    };
                    let line = Self::line(child);
                    let nid = NodeId(make_id(&[class_nid.as_str(), name]));
                    self.add_node(nid.clone(), format!(".{name}()"), line);
                    self.add_edge(class_nid.clone(), nid.clone(), "method", line, None);
                    self.swift_type_refs(child, &nid, stem, line);
                    if let Some(b) = child.child_by_field_name("body") {
                        self.function_bodies.push((nid, b));
                    }
                }
                _ => {}
            }
        }
    }
}
