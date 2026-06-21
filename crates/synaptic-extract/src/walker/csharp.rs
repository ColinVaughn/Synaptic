//! `csharp` extraction methods on `Extractor` (split from walker.rs).

use super::is_csharp_interface_name;
use super::Extractor;
use synaptic_core::NodeId;
use tree_sitter::Node as TsNode;

impl<'tree> Extractor<'_, '_, 'tree> {
    /// C# attribute names from `attribute_list` children (`[Serializable]`,
    /// `[Route(...)]`). Qualified names keep their tail.
    pub(crate) fn csharp_attribute_names(&self, method_node: TsNode<'tree>) -> Vec<String> {
        let mut names = Vec::new();
        for child in Self::children(method_node) {
            if child.kind() != "attribute_list" {
                continue;
            }
            for attr in Self::children(child) {
                if attr.kind() != "attribute" {
                    continue;
                }
                let name_node = attr.child_by_field_name("name").or_else(|| {
                    Self::children(attr)
                        .into_iter()
                        .find(|s| matches!(s.kind(), "identifier" | "qualified_name"))
                });
                if let Some(nn) = name_node {
                    let text = self.text(nn);
                    let head = text.rsplit('.').next().unwrap_or(&text);
                    if !head.is_empty() {
                        names.push(head.to_string());
                    }
                }
            }
        }
        names
    }

    // C#
    /// C# `base_list` bases → `inherits`/`implements`, classified by the
    /// pre-scanned interface set plus the `I`-prefix convention.
    pub(crate) fn csharp_heritage(
        &mut self,
        decl: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        for child in Self::children(decl) {
            if child.kind() != "base_list" {
                continue;
            }
            for base in Self::children(child) {
                let Some(name) = self.csharp_base_head(base) else {
                    continue;
                };
                let relation =
                    if self.interface_names.contains(&name) || is_csharp_interface_name(&name) {
                        "implements"
                    } else {
                        "inherits"
                    };
                self.link_heritage(class_nid, name, stem, line, relation);
            }
        }
    }

    /// Head name of a C# base: `identifier` verbatim, `qualified_name` → tail,
    /// `generic_name` → its identifier head.
    fn csharp_base_head(&self, t: TsNode<'tree>) -> Option<String> {
        match t.kind() {
            "identifier" => Some(self.text(t)),
            "qualified_name" => self.text(t).rsplit('.').next().map(str::to_string),
            "generic_name" => Self::children(t)
                .into_iter()
                .find(|c| c.kind() == "identifier")
                .map(|c| self.text(c)),
            _ => None,
        }
    }

    /// C# method parameter/return type references.
    /// The return type field is `returns` (older grammars: `type`).
    pub(crate) fn csharp_type_refs(
        &mut self,
        func_node: TsNode<'tree>,
        func_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        let mut refs: Vec<(String, &'static str)> = Vec::new();
        if let Some(params) = func_node.child_by_field_name("parameters") {
            for p in Self::children(params) {
                if p.kind() != "parameter" {
                    continue;
                }
                if let Some(ty) = p.child_by_field_name("type") {
                    let mut out = Vec::new();
                    self.collect_csharp_type_refs(ty, false, &mut out);
                    for (n, g) in out {
                        refs.push((n, if g { "generic_arg" } else { "parameter_type" }));
                    }
                }
            }
        }
        let ret = func_node
            .child_by_field_name("returns")
            .or_else(|| func_node.child_by_field_name("type"));
        if let Some(ret) = ret {
            let mut out = Vec::new();
            self.collect_csharp_type_refs(ret, false, &mut out);
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

    /// Walk a C# type subtree. Generic args (`type_argument_list`) recurse with
    /// `generic=true`; `predefined_type` (`int`, `string`, …) is skipped.
    fn collect_csharp_type_refs(
        &self,
        node: TsNode<'tree>,
        generic: bool,
        out: &mut Vec<(String, bool)>,
    ) {
        match node.kind() {
            "identifier" => {
                let t = self.text(node);
                if !t.is_empty() {
                    out.push((t, generic));
                }
            }
            "qualified_name" => {
                if let Some(tail) = self.text(node).rsplit('.').next() {
                    if !tail.is_empty() {
                        out.push((tail.to_string(), generic));
                    }
                }
            }
            "generic_name" => {
                for c in Self::children(node) {
                    if c.kind() == "type_argument_list" {
                        for a in Self::children(c) {
                            if a.is_named() {
                                self.collect_csharp_type_refs(a, true, out);
                            }
                        }
                    } else if c.is_named() {
                        self.collect_csharp_type_refs(c, generic, out);
                    }
                }
            }
            "predefined_type" => {}
            _ => {
                if node.is_named() {
                    for c in Self::children(node) {
                        if c.is_named() {
                            self.collect_csharp_type_refs(c, generic, out);
                        }
                    }
                }
            }
        }
    }

    /// C# fields (`field_declaration` → `variable_declaration.type`) and auto-
    /// properties (`property_declaration.type`) → `references`.
    pub(crate) fn csharp_class_members(
        &mut self,
        body: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
    ) {
        for child in Self::children(body) {
            let ty = match child.kind() {
                "field_declaration" => Self::children(child)
                    .into_iter()
                    .find(|c| c.kind() == "variable_declaration")
                    .and_then(|vd| vd.child_by_field_name("type")),
                "property_declaration" => child.child_by_field_name("type"),
                _ => None,
            };
            if let Some(ty) = ty {
                let mut out = Vec::new();
                self.collect_csharp_type_refs(ty, false, &mut out);
                self.emit_field_refs(out, class_nid, stem, Self::line(child));
            }
        }
    }
}
