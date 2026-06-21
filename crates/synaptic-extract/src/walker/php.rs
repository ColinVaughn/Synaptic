//! `php` extraction methods on `Extractor` (split from walker.rs).

use super::Extractor;
use synaptic_core::{make_id, NodeId};
use tree_sitter::Node as TsNode;

impl<'tree> Extractor<'_, '_, 'tree> {
    // PHP
    /// PHP `use A\B\C;` → an `imports` edge to each clause's `\`-tail.
    pub(crate) fn php_imports(&mut self, node: TsNode<'tree>, file_nid: &NodeId) {
        let line = Self::line(node);
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "namespace_use_clause" {
                if let Some(name) = Self::children(n)
                    .into_iter()
                    .find(|c| matches!(c.kind(), "qualified_name" | "name"))
                {
                    let tail = self.php_tail(name);
                    if !tail.is_empty() {
                        let tgt = NodeId(make_id(&[&tail]));
                        self.add_external_node(tgt.clone(), tail);
                        self.add_edge(file_nid.clone(), tgt, "imports", line, Some("import"));
                    }
                }
                continue;
            }
            for c in Self::children(n) {
                stack.push(c);
            }
        }
    }

    /// PHP heritage: `base_clause` (`extends`) → `inherits`, `class_interface_clause`
    /// (`implements`) → `implements`.
    pub(crate) fn php_heritage(
        &mut self,
        decl: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        for child in Self::children(decl) {
            let relation = match child.kind() {
                "base_clause" => "inherits",
                "class_interface_clause" => "implements",
                _ => continue,
            };
            for base in Self::children(child)
                .into_iter()
                .filter(|c| matches!(c.kind(), "name" | "qualified_name"))
            {
                let name = self.php_tail(base);
                self.link_heritage(class_nid, name, stem, line, relation);
            }
        }
    }

    /// The `\`-tail of a PHP `name` / `qualified_name` (`Lib\Base` → `Base`).
    fn php_tail(&self, node: TsNode<'tree>) -> String {
        let t = self.text(node);
        t.rsplit('\\').next().unwrap_or(&t).trim().to_string()
    }

    /// PHP method parameter (`simple_parameter.type`) + return (`return_type`)
    /// type references.
    pub(crate) fn php_type_refs(
        &mut self,
        func_node: TsNode<'tree>,
        func_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        let mut refs: Vec<(String, &'static str)> = Vec::new();
        if let Some(params) = func_node.child_by_field_name("parameters") {
            for p in Self::children(params) {
                if !matches!(
                    p.kind(),
                    "simple_parameter" | "property_promotion_parameter" | "variadic_parameter"
                ) {
                    continue;
                }
                if let Some(ty) = p.child_by_field_name("type") {
                    let mut out = Vec::new();
                    self.collect_php_type_refs(ty, &mut out);
                    for n in out {
                        refs.push((n, "parameter_type"));
                    }
                }
            }
        }
        if let Some(ret) = func_node.child_by_field_name("return_type") {
            let mut out = Vec::new();
            self.collect_php_type_refs(ret, &mut out);
            for n in out {
                refs.push((n, "return_type"));
            }
        }
        for (name, ctx) in refs {
            let tgt = self.ensure_named_node(&name, stem, line);
            if &tgt != func_nid {
                self.add_edge(func_nid.clone(), tgt, "references", line, Some(ctx));
            }
        }
    }

    /// Walk a PHP type subtree; `named_type` → its name, `primitive_type` skipped.
    fn collect_php_type_refs(&self, node: TsNode<'tree>, out: &mut Vec<String>) {
        match node.kind() {
            "name" => {
                let t = self.text(node);
                if !t.is_empty() {
                    out.push(t);
                }
            }
            "qualified_name" => {
                let t = self.php_tail(node);
                if !t.is_empty() {
                    out.push(t);
                }
            }
            "primitive_type" => {}
            _ => {
                // named_type/optional_type/union_type/intersection_type: recurse.
                for c in Self::children(node) {
                    if c.is_named() {
                        self.collect_php_type_refs(c, out);
                    }
                }
            }
        }
    }

    /// PHP `property_declaration.type` → `references` (ctx `field`).
    pub(crate) fn php_class_members(
        &mut self,
        body: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
    ) {
        for child in Self::children(body) {
            if child.kind() != "property_declaration" {
                continue;
            }
            if let Some(ty) = child.child_by_field_name("type") {
                let mut out = Vec::new();
                self.collect_php_type_refs(ty, &mut out);
                let refs: Vec<(String, bool)> = out.into_iter().map(|n| (n, false)).collect();
                self.emit_field_refs(refs, class_nid, stem, Self::line(child));
            }
        }
    }
}
