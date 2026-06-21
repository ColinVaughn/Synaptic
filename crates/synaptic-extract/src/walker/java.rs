//! `java` extraction methods on `Extractor` (split from walker.rs).

use super::Extractor;
use synaptic_core::NodeId;
use tree_sitter::Node as TsNode;

impl<'tree> Extractor<'_, '_, 'tree> {
    /// Java class/interface heritage: `superclass` → `inherits`, `super_interfaces`
    /// → `implements`, interface `extends_interfaces` → `inherits`. Generic args on
    /// a base (`implements Comparable<Dog>`) are ignored — only the base name.
    pub(crate) fn java_heritage(
        &mut self,
        decl: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        for child in Self::children(decl) {
            let relation = match child.kind() {
                "superclass" | "extends_interfaces" => "inherits",
                "super_interfaces" => "implements",
                _ => continue,
            };
            for base in self.java_base_names(child) {
                self.link_heritage(class_nid, base, stem, line, relation);
            }
        }
    }

    /// Base type names directly under a heritage clause (`superclass` holds the
    /// type directly; `super_interfaces`/`extends_interfaces` wrap a `type_list`).
    /// A generic base contributes only its head name, not its type arguments.
    fn java_base_names(&self, clause: TsNode<'tree>) -> Vec<String> {
        let mut bases = Vec::new();
        for c in Self::children(clause) {
            if c.kind() == "type_list" {
                for t in Self::children(c) {
                    if let Some(name) = self.java_base_head(t) {
                        bases.push(name);
                    }
                }
            } else if let Some(name) = self.java_base_head(c) {
                bases.push(name);
            }
        }
        bases
    }

    /// Head name of a base type node: `type_identifier` verbatim,
    /// `scoped_type_identifier` → tail, `generic_type` → its container's head.
    fn java_base_head(&self, t: TsNode<'tree>) -> Option<String> {
        match t.kind() {
            "type_identifier" => Some(self.text(t)),
            "scoped_type_identifier" => self.text(t).rsplit('.').next().map(str::to_string),
            "generic_type" => Self::children(t)
                .into_iter()
                .find_map(|c| self.java_base_head(c)),
            _ => None,
        }
    }

    /// Java method parameter/return type references.
    /// Generic args are tagged `generic_arg`; primitives are skipped.
    pub(crate) fn java_type_refs(
        &mut self,
        func_node: TsNode<'tree>,
        func_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        let mut refs: Vec<(String, &'static str)> = Vec::new();
        if let Some(params) = func_node.child_by_field_name("parameters") {
            for p in Self::children(params) {
                if !matches!(p.kind(), "formal_parameter" | "spread_parameter") {
                    continue;
                }
                if let Some(ty) = p.child_by_field_name("type") {
                    let mut out = Vec::new();
                    self.collect_java_type_refs(ty, false, &mut out);
                    for (n, g) in out {
                        refs.push((n, if g { "generic_arg" } else { "parameter_type" }));
                    }
                }
            }
        }
        // `method_declaration`'s `type` field is the return type (constructors
        // have none).
        if let Some(ret) = func_node.child_by_field_name("type") {
            let mut out = Vec::new();
            self.collect_java_type_refs(ret, false, &mut out);
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

    /// Java method annotation names from the `modifiers` child (`@Override`,
    /// `@Test`). Qualified names keep their tail.
    pub(crate) fn java_annotation_names(&self, method_node: TsNode<'tree>) -> Vec<String> {
        let mut names = Vec::new();
        let Some(modifiers) = Self::children(method_node)
            .into_iter()
            .find(|c| c.kind() == "modifiers")
        else {
            return names;
        };
        for anno in Self::children(modifiers) {
            if !matches!(anno.kind(), "marker_annotation" | "annotation") {
                continue;
            }
            let name_node = anno.child_by_field_name("name").or_else(|| {
                Self::children(anno).into_iter().find(|s| {
                    matches!(
                        s.kind(),
                        "identifier" | "scoped_identifier" | "type_identifier"
                    )
                })
            });
            if let Some(nn) = name_node {
                let text = self.text(nn);
                let head = text.rsplit('.').next().unwrap_or(&text);
                if !head.is_empty() {
                    names.push(head.to_string());
                }
            }
        }
        names
    }

    /// Walk a Java type subtree, appending `(name, is_generic_arg)`. Generic args
    /// (`type_arguments`) recurse with `generic=true`; the primitive node types
    /// (`integral_type`/`floating_point_type`/`boolean_type`/`void_type`) are
    /// skipped.
    fn collect_java_type_refs(
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
            "scoped_type_identifier" => {
                if let Some(tail) = self.text(node).rsplit('.').next() {
                    if !tail.is_empty() {
                        out.push((tail.to_string(), generic));
                    }
                }
            }
            "generic_type" => {
                for c in Self::children(node) {
                    if c.kind() == "type_arguments" {
                        for a in Self::children(c) {
                            if a.is_named() {
                                self.collect_java_type_refs(a, true, out);
                            }
                        }
                    } else if c.is_named() {
                        self.collect_java_type_refs(c, generic, out);
                    }
                }
            }
            "integral_type" | "floating_point_type" | "boolean_type" | "void_type" => {}
            _ => {
                if node.is_named() {
                    for c in Self::children(node) {
                        if c.is_named() {
                            self.collect_java_type_refs(c, generic, out);
                        }
                    }
                }
            }
        }
    }

    /// Java fields: `field_declaration`'s `type` → `references`.
    pub(crate) fn java_class_members(
        &mut self,
        body: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
    ) {
        for child in Self::children(body) {
            if child.kind() != "field_declaration" {
                continue;
            }
            if let Some(ty) = child.child_by_field_name("type") {
                let mut out = Vec::new();
                self.collect_java_type_refs(ty, false, &mut out);
                self.emit_field_refs(out, class_nid, stem, Self::line(child));
            }
        }
    }
}
