//! `scala` extraction methods on `Extractor` (split from walker.rs).

use super::Extractor;
use synaptic_core::{make_id, NodeId};
use tree_sitter::Node as TsNode;

impl<'tree> Extractor<'_, '_, 'tree> {
    // Scala
    /// Scala `import a.b.C` → an `imports` edge to the last path identifier.
    pub(crate) fn scala_imports(&mut self, node: TsNode<'tree>, file_nid: &NodeId) {
        let line = Self::line(node);
        if let Some(last) = Self::children(node)
            .into_iter()
            .rfind(|c| c.kind() == "identifier")
        {
            let name = self.text(last);
            if !name.is_empty() {
                let tgt = NodeId(make_id(&[&name]));
                self.add_external_node(tgt.clone(), name);
                self.add_edge(file_nid.clone(), tgt, "imports", line, Some("import"));
            }
        }
    }

    /// Scala `extends A with B`: the `extends_clause`'s first type → `inherits`,
    /// later types (mixed-in traits) → `mixes_in`.
    pub(crate) fn scala_heritage(
        &mut self,
        decl: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        for child in Self::children(decl) {
            if child.kind() != "extends_clause" {
                continue;
            }
            let types: Vec<String> = Self::children(child)
                .into_iter()
                .filter(|c| c.kind() == "type_identifier")
                .map(|c| self.text(c))
                .collect();
            for (i, name) in types.into_iter().enumerate() {
                let relation = if i == 0 { "inherits" } else { "mixes_in" };
                self.link_heritage(class_nid, name, stem, line, relation);
            }
        }
    }

    /// Scala parameter/return type references.
    pub(crate) fn scala_type_refs(
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
                    self.collect_scala_type_refs(ty, false, &mut out);
                    for (n, g) in out {
                        refs.push((n, if g { "generic_arg" } else { "parameter_type" }));
                    }
                }
            }
        }
        if let Some(ret) = func_node.child_by_field_name("return_type") {
            let mut out = Vec::new();
            self.collect_scala_type_refs(ret, false, &mut out);
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

    /// Walk a Scala type subtree; `generic_type` args → `generic_arg`.
    fn collect_scala_type_refs(
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
            "generic_type" => {
                for c in Self::children(node) {
                    if c.kind() == "type_arguments" {
                        for a in Self::children(c) {
                            if a.is_named() {
                                self.collect_scala_type_refs(a, true, out);
                            }
                        }
                    } else if c.is_named() {
                        self.collect_scala_type_refs(c, generic, out);
                    }
                }
            }
            _ => {
                if node.is_named() {
                    for c in Self::children(node) {
                        if c.is_named() {
                            self.collect_scala_type_refs(c, generic, out);
                        }
                    }
                }
            }
        }
    }
}
