//! `ecmascript` extraction methods on `Extractor` (split from walker.rs).

use super::Extractor;
use super::{is_ts_type_noise, module_stem};
use crate::result::ImportRecord;
use codegraph_core::{make_id, NodeId};
use tree_sitter::Node as TsNode;

impl<'tree> Extractor<'_, '_, 'tree> {
    /// EcmaScript class/interface heritage → `inherits` (extends) and
    /// `implements` edges. Handles TS (`extends_clause`/`implements_clause`,
    /// interface `extends_type_clause`) and JS (base directly under
    /// `class_heritage`).
    pub(crate) fn ecmascript_heritage(
        &mut self,
        decl: TsNode<'tree>,
        class_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        for child in Self::children(decl) {
            match child.kind() {
                "class_heritage" => {
                    for h in Self::children(child) {
                        match h.kind() {
                            "extends_clause" => {
                                for base in self.heritage_bases(h) {
                                    self.link_heritage(class_nid, base, stem, line, "inherits");
                                }
                            }
                            "implements_clause" => {
                                for base in self.heritage_bases(h) {
                                    self.link_heritage(class_nid, base, stem, line, "implements");
                                }
                            }
                            // JS: the superclass is a direct child of class_heritage.
                            "identifier" | "type_identifier" => {
                                let base = self.text(h);
                                self.link_heritage(class_nid, base, stem, line, "inherits");
                            }
                            _ => {}
                        }
                    }
                }
                // TS interface heritage: `interface X extends Y, Z`.
                "extends_type_clause" => {
                    for base in self.heritage_bases(child) {
                        self.link_heritage(class_nid, base, stem, line, "inherits");
                    }
                }
                _ => {}
            }
        }
    }

    /// Head type name of one heritage entry: plain identifiers directly, the tail
    /// of a qualified name, and the container head of a `generic_type`
    /// (`Base<T>` → `Base`). The TS analogue of `java_base_head`.
    pub(crate) fn ts_base_head(&self, t: TsNode<'tree>) -> Option<String> {
        match t.kind() {
            "identifier" | "type_identifier" => Some(self.text(t)),
            "nested_type_identifier" => self.text(t).rsplit('.').next().map(str::to_string),
            "generic_type" => Self::children(t)
                .into_iter()
                .find_map(|c| self.ts_base_head(c)),
            _ => None,
        }
    }

    /// EcmaScript `import … from 'm'`: an `imports_from` edge to a module stub
    /// (labeled by the specifier) plus named-import records (module stem = last
    /// path component) for cross-file symbol resolution.
    pub(crate) fn ecmascript_imports(&mut self, node: TsNode<'tree>, file_nid: &NodeId) {
        let line = Self::line(node);
        let Some(spec) = self.import_specifier(node) else {
            return;
        };
        let stem = module_stem(&spec);
        let tgt = NodeId(make_id(&[spec.as_str()]));
        self.add_external_node(tgt.clone(), spec.clone());
        self.add_edge(file_nid.clone(), tgt, "imports_from", line, Some("import"));
        let edge_idx = self.edges.len() - 1;

        // `import_clause` is a positional child, not a named field.
        let mut imported: Vec<String> = Vec::new();
        for child in Self::children(node) {
            if child.kind() == "import_clause" {
                imported.extend(self.import_records(child, &stem, line));
            }
        }
        self.tag_imported_names(edge_idx, imported);
    }

    /// Record the specific symbol names an `imports_from`/`re_exports` edge brings
    /// in, under the edge's `imported` extra key. This is what lets forecast-time
    /// impact resolve whether a module importer actually references a given
    /// exported symbol (the edge itself only points at a module stub).
    fn tag_imported_names(&mut self, edge_idx: usize, mut imported: Vec<String>) {
        if imported.is_empty() {
            return;
        }
        imported.sort();
        imported.dedup();
        if let Some(e) = self.edges.get_mut(edge_idx) {
            e.extra.insert(
                "imported".to_string(),
                serde_json::Value::Array(
                    imported
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
    }

    /// EcmaScript dynamic imports: `import('m')`, `require('m')`, and
    /// `System.import('m')` with a **string-literal** specifier → an `imports_from`
    /// edge to a module stub (same shape as a static import). Computed / template /
    /// non-`System` member calls are skipped (no concrete module, or false-positive
    /// risk).
    pub(crate) fn ecmascript_dynamic_import(&mut self, node: TsNode<'tree>, file_nid: &NodeId) {
        let Some(func) = self.field(node, "function") else {
            return;
        };
        let is_import = match func.kind() {
            "import" => true, // native dynamic import: import("m")
            "identifier" => self.text(func) == "require",
            "member_expression" => {
                self.field(func, "object").map(|o| self.text(o)).as_deref() == Some("System")
                    && self
                        .field(func, "property")
                        .map(|p| self.text(p))
                        .as_deref()
                        == Some("import")
            }
            _ => false,
        };
        if !is_import {
            return;
        }
        let Some(args) = self.field(node, "arguments") else {
            return;
        };
        let Some(arg) = Self::children(args).into_iter().find(|c| c.is_named()) else {
            return;
        };
        if arg.kind() != "string" {
            return; // computed/template specifier: no concrete module
        }
        let raw = self.text(arg);
        let spec = raw.trim_matches(|c| c == '\'' || c == '"' || c == '`');
        if spec.is_empty() || spec.contains("${") {
            return;
        }
        let line = Self::line(node);
        let tgt = NodeId(make_id(&[spec]));
        self.add_external_node(tgt.clone(), spec.to_string());
        self.add_edge(file_nid.clone(), tgt, "imports_from", line, Some("import"));
    }

    /// EcmaScript `export { x } from 'm'` re-export: a `re_exports` edge + records.
    /// No-op for a plain `export class X` (no `source`).
    pub(crate) fn ecmascript_reexport(&mut self, node: TsNode<'tree>, file_nid: &NodeId) {
        let line = Self::line(node);
        let Some(spec) = self.import_specifier(node) else {
            return;
        };
        let stem = module_stem(&spec);
        let tgt = NodeId(make_id(&[spec.as_str()]));
        self.add_external_node(tgt.clone(), spec.clone());
        self.add_edge(file_nid.clone(), tgt, "re_exports", line, Some("import"));
        let edge_idx = self.edges.len() - 1;
        let mut imported: Vec<String> = Vec::new();
        for child in Self::children(node) {
            if child.kind() == "export_clause" {
                imported.extend(self.import_records(child, &stem, line));
            }
        }
        self.tag_imported_names(edge_idx, imported);
    }

    /// The module specifier string of an import/export statement (`source`
    /// field), with the surrounding quotes stripped.
    fn import_specifier(&self, node: TsNode<'tree>) -> Option<String> {
        let s = self.field(node, "source")?;
        let raw = self.text(s);
        let spec = raw.trim_matches(|c| c == '\'' || c == '"' || c == '`');
        if spec.is_empty() {
            None
        } else {
            Some(spec.to_string())
        }
    }

    /// Named-import records from an `import_clause`/`export_clause`: each
    /// `import_specifier`/`export_specifier` → `{local, imported, module_stem}`.
    /// Returns the original imported symbol names so the caller can tag the import
    /// edge with what it brought in.
    fn import_records(&mut self, clause: TsNode<'tree>, stem: &str, line: usize) -> Vec<String> {
        let mut specs: Vec<TsNode<'tree>> = Vec::new();
        let mut stack = vec![clause];
        while let Some(n) = stack.pop() {
            if matches!(n.kind(), "import_specifier" | "export_specifier") {
                specs.push(n);
            } else {
                stack.extend(Self::children(n));
            }
        }
        let mut names = Vec::new();
        for spec in specs {
            let Some(name_node) = self.field(spec, "name") else {
                continue;
            };
            let imported = self.text(name_node);
            let local = self
                .field(spec, "alias")
                .map(|a| self.text(a))
                .unwrap_or_else(|| imported.clone());
            if imported.is_empty() {
                continue;
            }
            names.push(imported.clone());
            self.imports.push(ImportRecord {
                local_name: local,
                imported_name: imported,
                module_stem: stem.to_string(),
                source_file: self.path.clone(),
                source_location: Some(format!("L{line}")),
            });
        }
        names
    }

    /// Emit `references` edges for the parameter/return `type_annotation`s of a
    /// TS function/method. Each named `type_identifier` (primitives are
    /// `predefined_type` and skipped) → a `references` edge to the in-file
    /// definition or a global stub.
    pub(crate) fn ecmascript_type_refs(
        &mut self,
        func_node: TsNode<'tree>,
        func_nid: &NodeId,
        stem: &str,
        line: usize,
    ) {
        let mut refs: Vec<(String, &'static str)> = Vec::new();
        if let Some(params) = func_node.child_by_field_name("parameters") {
            for p in Self::children(params) {
                if let Some(ty) = p.child_by_field_name("type") {
                    for name in self.collect_type_identifiers(ty) {
                        refs.push((name, "parameter_type"));
                    }
                }
            }
        }
        if let Some(ret) = func_node.child_by_field_name("return_type") {
            for name in self.collect_type_identifiers(ret) {
                refs.push((name, "return_type"));
            }
        }
        for (name, ctx) in refs {
            let tgt = self.ensure_named_node(&name, stem, line);
            if &tgt != func_nid {
                self.add_edge(func_nid.clone(), tgt, "references", line, Some(ctx));
            }
        }
    }

    /// All `type_identifier` names under a TS type subtree, skipping well-known
    /// built-in type containers (`Array`, `Promise`, …) to limit noise.
    fn collect_type_identifiers(&self, node: TsNode<'tree>) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "type_identifier" {
                let name = self.text(n);
                if !name.is_empty() && !is_ts_type_noise(&name) {
                    out.push(name);
                }
            } else {
                stack.extend(Self::children(n));
            }
        }
        out
    }
}
