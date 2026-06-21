//! `kotlin` extraction methods on `Extractor` (split from walker.rs).

use super::Extractor;
use synaptic_core::NodeId;
use tree_sitter::Node as TsNode;

impl<'tree> Extractor<'_, '_, 'tree> {
    // Kotlin
    /// Kotlin `delegation_specifiers`: a `constructor_invocation` base →
    /// `inherits` (the superclass), a bare `user_type` base → `implements`.
    pub(crate) fn kotlin_heritage(
        &mut self,
        decl: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        for child in Self::children(decl) {
            if child.kind() != "delegation_specifiers" {
                continue;
            }
            for spec in Self::children(child) {
                if spec.kind() != "delegation_specifier" {
                    continue;
                }
                for inner in Self::children(spec) {
                    let relation = match inner.kind() {
                        "constructor_invocation" => "inherits",
                        "user_type" => "implements",
                        _ => continue,
                    };
                    if let Some(name) = self.user_type_head(inner) {
                        self.link_heritage(class_nid, name, stem, line, relation);
                    }
                }
            }
        }
    }

    /// Kotlin parameter (`function_value_parameters` → `parameter` → `user_type`)
    /// and return (the direct `user_type` child) type references.
    pub(crate) fn kotlin_type_refs(
        &mut self,
        func_node: TsNode<'tree>,
        func_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        let mut refs: Vec<(String, &'static str)> = Vec::new();
        for child in Self::children(func_node) {
            match child.kind() {
                "function_value_parameters" => {
                    for p in Self::children(child) {
                        if p.kind() != "parameter" {
                            continue;
                        }
                        for u in Self::children(p)
                            .into_iter()
                            .filter(|c| c.kind() == "user_type")
                        {
                            let mut out = Vec::new();
                            self.collect_kotlin_type_refs(u, false, &mut out);
                            for (n, g) in out {
                                refs.push((n, if g { "generic_arg" } else { "parameter_type" }));
                            }
                        }
                    }
                }
                "user_type" => {
                    let mut out = Vec::new();
                    self.collect_kotlin_type_refs(child, false, &mut out);
                    for (n, g) in out {
                        refs.push((n, if g { "generic_arg" } else { "return_type" }));
                    }
                }
                _ => {}
            }
        }
        for (name, ctx) in refs {
            let tgt = self.ensure_named_node(&name, stem, line);
            if &tgt != func_nid {
                self.add_edge(func_nid.clone(), tgt, "references", line, Some(ctx));
            }
        }
    }

    /// Walk a Kotlin `user_type` subtree; `type_arguments` recurse `generic=true`.
    fn collect_kotlin_type_refs(
        &self,
        node: TsNode<'tree>,
        generic: bool,
        out: &mut Vec<(String, bool)>,
    ) {
        match node.kind() {
            "identifier" | "type_identifier" => {
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
                                self.collect_kotlin_type_refs(a, true, out);
                            }
                        }
                    } else if c.is_named() {
                        self.collect_kotlin_type_refs(c, generic, out);
                    }
                }
            }
            _ => {
                if node.is_named() {
                    for c in Self::children(node) {
                        if c.is_named() {
                            self.collect_kotlin_type_refs(c, generic, out);
                        }
                    }
                }
            }
        }
    }

    /// Kotlin primary-constructor `val`/`var` parameters and body
    /// `property_declaration`s → `references` (their `user_type`).
    pub(crate) fn kotlin_class_members(
        &mut self,
        decl: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
    ) {
        for pc in Self::children(decl)
            .into_iter()
            .filter(|c| c.kind() == "primary_constructor")
        {
            for cps in Self::children(pc)
                .into_iter()
                .filter(|c| c.kind() == "class_parameters")
            {
                for cp in Self::children(cps)
                    .into_iter()
                    .filter(|c| c.kind() == "class_parameter")
                {
                    let mut out = Vec::new();
                    for ut in Self::children(cp)
                        .into_iter()
                        .filter(|c| c.kind() == "user_type")
                    {
                        self.collect_kotlin_type_refs(ut, false, &mut out);
                    }
                    self.emit_field_refs(out, class_nid, stem, Self::line(cp));
                }
            }
        }
        if let Some(body) = self.body_of(decl) {
            for prop in Self::children(body)
                .into_iter()
                .filter(|c| c.kind() == "property_declaration")
            {
                let vd = Self::children(prop)
                    .into_iter()
                    .find(|c| c.kind() == "variable_declaration");
                let mut out = Vec::new();
                if let Some(vd) = vd {
                    for ut in Self::children(vd)
                        .into_iter()
                        .filter(|c| c.kind() == "user_type")
                    {
                        self.collect_kotlin_type_refs(ut, false, &mut out);
                    }
                }
                self.emit_field_refs(out, class_nid, stem, Self::line(prop));
            }
        }
    }
}
