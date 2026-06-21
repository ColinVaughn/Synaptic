//! `python` extraction methods on `Extractor` (split from walker.rs).

use super::resolve_relative_import;
use super::Extractor;
use crate::python::is_suppressed_type;
use crate::result::ImportRecord;
use synaptic_core::{make_id, NodeId};
use tree_sitter::Node as TsNode;

impl<'tree> Extractor<'_, '_, 'tree> {
    /// Python import handling: emit `imports` /
    /// `imports_from` edges and capture `from M import …` records for B3.
    ///
    /// `import X` and absolute `from M import …` create an external stub node for
    /// the module so the edge survives build's dangling-edge drop. Relative
    /// imports (`.`/`..`) resolve to a file-path id and are left unstubbed so they
    /// bind to the real in-corpus file node (dropped if outside the corpus).
    pub(crate) fn python_imports(&mut self, node: TsNode<'tree>, file_nid: &NodeId) {
        let line = Self::line(node);
        match node.kind() {
            "import_statement" => {
                for child in Self::children(node) {
                    if !matches!(child.kind(), "dotted_name" | "aliased_import") {
                        continue;
                    }
                    let raw = self.text(child);
                    // `numpy as np` -> `numpy`; strip any leading relative dots.
                    let module = raw
                        .split(" as ")
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_start_matches('.');
                    if module.is_empty() {
                        continue;
                    }
                    let tgt = NodeId(make_id(&[module]));
                    self.add_external_node(tgt.clone(), module.to_string());
                    self.add_edge(file_nid.clone(), tgt, "imports", line, Some("import"));
                }
            }
            "import_from_statement" => {
                let Some(module_node) = self.field(node, "module_name") else {
                    return;
                };
                let raw = self.text(module_node);
                let (tgt, is_relative) = if raw.starts_with('.') {
                    let path = resolve_relative_import(&self.path, &raw);
                    (NodeId(make_id(&[&path])), true)
                } else {
                    (NodeId(make_id(&[raw.as_str()])), false)
                };
                if !is_relative {
                    self.add_external_node(tgt.clone(), raw.clone());
                }
                self.add_edge(file_nid.clone(), tgt, "imports_from", line, Some("import"));

                // Records: `from M import name [as local]`, keyed by module stem.
                let stem = raw.trim_matches('.').rsplit('.').next().unwrap_or("");
                if stem.is_empty() {
                    return;
                }
                let names: Vec<TsNode<'tree>> = {
                    let mut cur = node.walk();
                    node.children_by_field_name("name", &mut cur).collect()
                };
                for name_node in names {
                    let (imported, local) = if name_node.kind() == "aliased_import" {
                        let imported = self
                            .field(name_node, "name")
                            .map(|n| self.text(n))
                            .unwrap_or_default();
                        let local = self
                            .field(name_node, "alias")
                            .map(|n| self.text(n))
                            .unwrap_or_else(|| imported.clone());
                        (imported, local)
                    } else {
                        let t = self.text(name_node);
                        (t.clone(), t)
                    };
                    if imported.is_empty() || imported == "*" {
                        continue;
                    }
                    self.imports.push(ImportRecord {
                        local_name: local,
                        imported_name: imported,
                        module_stem: stem.to_string(),
                        source_file: self.path.clone(),
                        source_location: Some(format!("L{line}")),
                    });
                }
            }
            _ => {}
        }
    }

    /// Emit `references` edges for the parameter/return type annotations of a
    /// Python function. Containers/noise are filtered;
    /// nested generic args are tagged `generic_arg`. Targets resolve to the
    /// in-file definition if present, else a freshly-created global stub.
    pub(crate) fn python_type_refs(
        &mut self,
        func_node: TsNode<'tree>,
        func_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        let mut refs: Vec<(String, &'static str)> = Vec::new();
        if let Some(params) = func_node.child_by_field_name("parameters") {
            for child in Self::children(params) {
                if !matches!(child.kind(), "typed_parameter" | "typed_default_parameter") {
                    continue;
                }
                if let Some(ty) = child.child_by_field_name("type") {
                    let mut out = Vec::new();
                    self.collect_type_refs(ty, false, &mut out);
                    for (name, generic) in out {
                        let ctx = if generic {
                            "generic_arg"
                        } else {
                            "parameter_type"
                        };
                        refs.push((name, ctx));
                    }
                }
            }
        }
        if let Some(ret) = func_node.child_by_field_name("return_type") {
            let mut out = Vec::new();
            self.collect_type_refs(ret, false, &mut out);
            for (name, generic) in out {
                let ctx = if generic {
                    "generic_arg"
                } else {
                    "return_type"
                };
                refs.push((name, ctx));
            }
        }
        for (name, ctx) in refs {
            let tgt = self.ensure_named_node(&name, stem, line);
            if &tgt != func_nid {
                self.add_edge(func_nid.clone(), tgt, "references", line, Some(ctx));
            }
        }
    }

    /// Walk a Python type-annotation subtree, appending `(name, is_generic_arg)`.
    fn collect_type_refs(&self, node: TsNode<'tree>, generic: bool, out: &mut Vec<(String, bool)>) {
        match node.kind() {
            "type" => {
                for c in Self::children(node) {
                    if c.is_named() {
                        self.collect_type_refs(c, generic, out);
                    }
                }
            }
            "identifier" => {
                let name = self.text(node);
                if !name.is_empty() && !is_suppressed_type(&name) {
                    out.push((name, generic));
                }
            }
            "attribute" => {
                let full = self.text(node);
                let tail = full.rsplit('.').next().unwrap_or("").to_string();
                if !tail.is_empty() && !is_suppressed_type(&tail) {
                    out.push((tail, generic));
                }
            }
            "generic_type" => {
                for c in Self::children(node) {
                    if c.kind() == "identifier" {
                        let container = self.text(c);
                        if !container.is_empty() && !is_suppressed_type(&container) {
                            out.push((container, generic));
                        }
                    } else if c.kind() == "type_parameter" {
                        for sub in Self::children(c) {
                            if sub.is_named() {
                                self.collect_type_refs(sub, true, out);
                            }
                        }
                    }
                }
            }
            "subscript" => {
                let value = node.child_by_field_name("value");
                if let Some(value) = value {
                    self.collect_type_refs(value, generic, out);
                }
                for c in Self::children(node) {
                    if Some(c.id()) == value.map(|v| v.id()) || !c.is_named() {
                        continue;
                    }
                    self.collect_type_refs(c, true, out);
                }
            }
            _ => {
                if node.is_named() {
                    for c in Self::children(node) {
                        if c.is_named() {
                            self.collect_type_refs(c, generic, out);
                        }
                    }
                }
            }
        }
    }
}
