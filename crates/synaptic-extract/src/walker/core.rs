//! `core` extraction methods on `Extractor` (split from walker.rs).

use super::Extractor;
use super::{first_docstring, COMMENT_TOKENS, MAX_DEPTH, RATIONALE_MARKERS};
use crate::config::{HeritageStyle, ImportStyle, TypeRefStyle};
use crate::paths::file_node_id;
use crate::result::RawCall;
use serde_json::Map;
use std::collections::{HashMap, HashSet};
use synaptic_core::{make_id, Confidence, Edge, FileType, Node, NodeId};
use tree_sitter::Node as TsNode;

impl<'tree> Extractor<'_, '_, 'tree> {
    pub(crate) fn text(&self, node: TsNode<'tree>) -> String {
        node.utf8_text(self.source).unwrap_or("").to_string()
    }

    pub(crate) fn line(node: TsNode<'tree>) -> usize {
        node.start_position().row + 1
    }

    /// The node's full source range (1-based lines and columns).
    pub(crate) fn span(node: TsNode<'tree>) -> synaptic_core::Span {
        let s = node.start_position();
        let e = node.end_position();
        synaptic_core::Span {
            start_line: s.row as u32 + 1,
            start_col: s.column as u32 + 1,
            end_line: e.row as u32 + 1,
            end_col: e.column as u32 + 1,
        }
    }

    /// `extra` with the AST-provenance tag, so the build-stage ghost remap can
    /// distinguish AST nodes from (future) semantic nodes.
    fn ast_origin() -> Map<String, serde_json::Value> {
        let mut m = Map::new();
        m.insert(
            "_origin".to_string(),
            serde_json::Value::String("ast".to_string()),
        );
        m
    }

    pub(crate) fn add_node(&mut self, id: NodeId, label: String, line: usize) {
        if self.seen.insert(id.clone()) {
            self.nodes.push(Node {
                id,
                label,
                file_type: FileType::Code,
                source_file: self.path.clone(),
                source_location: Some(format!("L{line}")),
                community: None,
                repo: None,
                extra: Self::ast_origin(),
            });
        }
    }

    /// Add a located code node enriched with kind, optional visibility, and the
    /// full source span (derived from `node`). Deduped by id like [`add_node`].
    pub(crate) fn add_code_node(
        &mut self,
        id: NodeId,
        label: String,
        node: TsNode<'tree>,
        kind: synaptic_core::NodeKind,
        visibility: Option<synaptic_core::Visibility>,
        signature: Option<synaptic_core::Signature>,
    ) {
        if self.seen.insert(id.clone()) {
            let mut n = Node {
                id,
                label,
                file_type: FileType::Code,
                source_file: self.path.clone(),
                source_location: Some(format!("L{}", node.start_position().row + 1)),
                community: None,
                repo: None,
                extra: Self::ast_origin(),
            };
            n.set_kind(kind);
            n.set_span(Self::span(node));
            if let Some(v) = visibility {
                n.set_visibility(v);
            }
            if let Some(s) = signature {
                n.set_signature(s);
            }
            self.nodes.push(n);
        } else if let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) {
            // Enrich a plain stub created earlier (e.g. a name referenced before its
            // declaration), without overwriting an already-enriched node.
            if n.kind().is_none() {
                n.set_kind(kind);
                n.set_span(Self::span(node));
                if let Some(v) = visibility {
                    n.set_visibility(v);
                }
                if let Some(s) = signature {
                    n.set_signature(s);
                }
            }
        }
    }

    /// Map a class-family grammar node kind to a [`NodeKind`].
    pub(crate) fn class_kind(ts_kind: &str) -> synaptic_core::NodeKind {
        use synaptic_core::NodeKind::*;
        let k = ts_kind.to_ascii_lowercase();
        if k.contains("interface") {
            Interface
        } else if k.contains("trait") {
            Trait
        } else if k.contains("enum") {
            Enum
        } else if k.contains("struct") {
            Struct
        } else if k.contains("protocol") {
            Protocol
        } else if k.contains("object") {
            Object
        } else {
            Class
        }
    }

    /// Best-effort declared visibility from a declaration node: scans an immediate
    /// `modifiers`/`modifier`/`visibility` child (Java/C#/Kotlin/Swift/TS/Rust) or a
    /// bare `public`/`private`/`protected`/`internal` keyword child. None = unknown.
    pub(crate) fn visibility_of(&self, node: TsNode<'tree>) -> Option<synaptic_core::Visibility> {
        use synaptic_core::Visibility::*;
        let kw = |w: &str| match w {
            "public" => Some(Public),
            "protected" => Some(Protected),
            "private" => Some(Private),
            "internal" => Some(Internal),
            _ => None,
        };
        let mut cur = node.walk();
        for child in node.children(&mut cur) {
            let k = child.kind();
            // A bare keyword child is unambiguous.
            if let Some(v) = kw(k) {
                return Some(v);
            }
            if k == "modifiers"
                || k == "modifier"
                || k == "visibility_modifier"
                || k == "visibility"
            {
                // Tokenize so an annotation whose NAME contains a keyword substring
                // (e.g. `@PublicApi private`) can't masquerade as a modifier: skip
                // any `@...` token and match keywords as whole words, in order.
                for tok in self.text(child).split_whitespace() {
                    if tok.starts_with('@') {
                        continue;
                    }
                    if let Some(v) = kw(&tok.to_ascii_lowercase()) {
                        return Some(v);
                    }
                }
            }
        }
        None
    }

    /// Python has no AST modifiers: a leading underscore is the private convention.
    pub(crate) fn python_visibility(name: &str) -> Option<synaptic_core::Visibility> {
        name.starts_with('_')
            .then_some(synaptic_core::Visibility::Private)
    }

    /// Visibility for a declaration named `name`: the Python underscore convention
    /// for Python configs, else the AST-modifier scan.
    fn decl_visibility(
        &self,
        node: TsNode<'tree>,
        name: &str,
    ) -> Option<synaptic_core::Visibility> {
        if matches!(self.cfg.type_ref_style, Some(TypeRefStyle::Python)) {
            Self::python_visibility(name)
        } else {
            self.visibility_of(node)
        }
    }

    /// Append a `FileType::Rationale` node (deduped by id) + a `rationale_for`
    /// edge to `target`. The label is collapsed to one line and capped at 80 chars.
    pub(crate) fn add_rationale(&mut self, label: String, line: usize, target: NodeId, stem: &str) {
        let rid = NodeId(make_id(&[stem, "rationale", &line.to_string()]));
        if self.seen.insert(rid.clone()) {
            let label: String = label
                .chars()
                .take(80)
                .collect::<String>()
                .replace(['\r', '\n'], " ")
                .trim()
                .to_string();
            self.nodes.push(Node {
                id: rid.clone(),
                label,
                file_type: FileType::Rationale,
                source_file: self.path.clone(),
                source_location: Some(format!("L{line}")),
                community: None,
                repo: None,
                extra: Self::ast_origin(),
            });
        }
        self.add_edge(rid, target, "rationale_for", line, None);
    }

    /// Line scan for rationale comment markers (`# NOTE:`, `// HACK:`, `-- TODO:`,
    /// …), each linked to the file node. Language-agnostic comment pass.
    pub(crate) fn scan_rationale_comments(&mut self, file_nid: &NodeId, stem: &str) {
        // Collect first (borrowing the source via `Cow`, no full-file clone), then
        // mutate. The block scopes the borrow so `add_rationale` can take `&mut self`.
        let hits: Vec<(usize, String)> = {
            let text = String::from_utf8_lossy(self.source);
            text.lines()
                .enumerate()
                .filter_map(|(i, raw)| {
                    let s = raw.trim_start();
                    let tok = COMMENT_TOKENS.iter().find(|t| s.starts_with(**t))?;
                    let rest = s[tok.len()..].trim_start();
                    let is_marker = RATIONALE_MARKERS
                        .iter()
                        .any(|m| rest.strip_prefix(m).is_some_and(|r| r.starts_with(':')));
                    is_marker.then(|| (i + 1, s.to_string()))
                })
                .collect()
        };
        for (line, label) in hits {
            self.add_rationale(label, line, file_nid.clone(), stem);
        }
    }

    pub(crate) fn add_external_node(&mut self, id: NodeId, label: String) {
        if self.seen.insert(id.clone()) {
            self.nodes.push(Node {
                id,
                label,
                file_type: FileType::Code,
                source_file: String::new(),
                source_location: None,
                community: None,
                repo: None,
                extra: Self::ast_origin(),
            });
        }
    }

    pub(crate) fn add_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        relation: &str,
        line: usize,
        context: Option<&str>,
    ) {
        self.edges.push(Edge {
            source,
            target,
            relation: relation.to_string(),
            confidence: Confidence::Extracted,
            source_file: self.path.clone(),
            source_location: Some(format!("L{line}")),
            confidence_score: None,
            weight: 1.0,
            context: context.map(str::to_string),
            cross_repo: false,
            extra: Map::new(),
        });
    }

    pub(crate) fn field(&self, node: TsNode<'tree>, name: &str) -> Option<TsNode<'tree>> {
        node.child_by_field_name(name)
    }

    pub(crate) fn children(node: TsNode<'tree>) -> Vec<TsNode<'tree>> {
        let mut cur = node.walk();
        node.children(&mut cur).collect()
    }

    /// The function name node: the named `name_field` if present, else (C/C++,
    /// where the name is buried in a declarator chain) the identifier reached by
    /// unwrapping `function_definition.declarator` through pointer/reference/
    /// function declarators. No-op for grammars that expose a `name` field.
    pub(crate) fn function_name_node(&self, node: TsNode<'tree>) -> Option<TsNode<'tree>> {
        if let Some(n) = self.field(node, self.cfg.name_field) {
            return Some(n);
        }
        let mut d = node.child_by_field_name("declarator")?;
        for _ in 0..MAX_DEPTH {
            match d.kind() {
                "identifier"
                | "field_identifier"
                | "type_identifier"
                | "qualified_identifier"
                | "destructor_name"
                | "operator_name" => return Some(d),
                _ => d = d.child_by_field_name("declarator")?,
            }
        }
        None
    }

    /// The `function_declarator` inside a C/C++ `function_definition`'s declarator
    /// chain (holds the `parameters`), or `None`.
    pub(crate) fn c_function_declarator(&self, node: TsNode<'tree>) -> Option<TsNode<'tree>> {
        let mut d = node.child_by_field_name("declarator")?;
        for _ in 0..MAX_DEPTH {
            if d.kind() == "function_declarator" {
                return Some(d);
            }
            d = d.child_by_field_name("declarator")?;
        }
        None
    }

    /// The class/function body: the named `body_field` if present, else (for
    /// grammars that attach the body positionally, e.g. Kotlin) the first child
    /// whose kind is in `body_kinds`.
    pub(crate) fn body_of(&self, node: TsNode<'tree>) -> Option<TsNode<'tree>> {
        self.field(node, self.cfg.body_field).or_else(|| {
            if self.cfg.body_kinds.is_empty() {
                None
            } else {
                Self::children(node)
                    .into_iter()
                    .find(|c| self.cfg.body_kinds.contains(&c.kind()))
            }
        })
    }

    pub(crate) fn walk(
        &mut self,
        node: TsNode<'tree>,
        file_nid: &NodeId,
        parent_class: Option<&NodeId>,
        stem: &str,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return; // guard against stack overflow on pathologically nested input
        }
        let t = node.kind();

        // Transparent wrappers (e.g. `decorated_definition`): recurse preserving
        // the parent-class scope so decorated methods stay methods (not functions).
        if self.cfg.decorated_types.contains(&t) {
            // Iterate the cursor directly: no per-node `Vec` allocation.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                self.walk(child, file_nid, parent_class, stem, depth + 1);
            }
            return;
        }

        // Import statements: emit `imports`/`imports_from` edges + records.
        if self.cfg.import_types.contains(&t) {
            match self.cfg.import_style {
                Some(ImportStyle::Python) => self.python_imports(node, file_nid),
                Some(ImportStyle::EcmaScript) => self.ecmascript_imports(node, file_nid),
                Some(ImportStyle::Java) => {
                    self.dotted_import(node, file_nid, &["scoped_identifier", "identifier"])
                }
                Some(ImportStyle::CSharp) => {
                    self.dotted_import(node, file_nid, &["qualified_name", "identifier"])
                }
                Some(ImportStyle::Kotlin) => {
                    self.dotted_import(node, file_nid, &["qualified_identifier"])
                }
                Some(ImportStyle::Swift) => self.dotted_import(node, file_nid, &["identifier"]),
                Some(ImportStyle::CInclude) => self.c_include(node, file_nid),
                Some(ImportStyle::Php) => self.php_imports(node, file_nid),
                Some(ImportStyle::Scala) => self.scala_imports(node, file_nid),
                None => {}
            }
            return;
        }

        // EcmaScript `export { x } from 'm'` re-exports. Handled here (not via
        // `import_types`) without an early return, so an inline `export class X`
        // declaration is still extracted by the structural recursion below.
        if matches!(self.cfg.import_style, Some(ImportStyle::EcmaScript)) && t == "export_statement"
        {
            self.ecmascript_reexport(node, file_nid);
        }

        // EcmaScript dynamic imports (`import()`/`require()`/`System.import()`).
        // Non-early-return so the normal call-edge extraction still runs.
        if matches!(self.cfg.import_style, Some(ImportStyle::EcmaScript)) && t == "call_expression"
        {
            self.ecmascript_dynamic_import(node, file_nid);
        }

        if self.cfg.class_types.contains(&t) {
            let Some(name_node) = self.field(node, self.cfg.name_field) else {
                return;
            };
            let class_name = self.text(name_node);
            let class_nid = NodeId(make_id(&[stem, &class_name]));
            let line = Self::line(node);
            let kind = Self::class_kind(t);
            let vis = self.decl_visibility(node, &class_name);
            self.add_code_node(class_nid.clone(), class_name, node, kind, vis, None);
            self.add_edge(file_nid.clone(), class_nid.clone(), "contains", line, None);

            if let Some(field) = self.cfg.superclasses_field {
                if let Some(args) = self.field(node, field) {
                    for arg in Self::children(args) {
                        if arg.kind() == "identifier" {
                            let base = self.text(arg);
                            let local = NodeId(make_id(&[stem, &base]));
                            let base_nid = if self.seen.contains(&local) {
                                local
                            } else {
                                let global = NodeId(make_id(&[base.as_str()]));
                                self.add_external_node(global.clone(), base.clone());
                                global
                            };
                            self.add_edge(class_nid.clone(), base_nid, "inherits", line, None);
                        }
                    }
                }
            }

            // Grammar-specific `extends`/`implements` heritage (a different shape
            // than Python's `superclasses` field).
            match self.cfg.heritage_style {
                Some(HeritageStyle::EcmaScript) => {
                    self.ecmascript_heritage(node, &class_nid, stem, line)
                }
                Some(HeritageStyle::Java) => self.java_heritage(node, &class_nid, stem, line),
                Some(HeritageStyle::CSharp) => self.csharp_heritage(node, &class_nid, stem, line),
                Some(HeritageStyle::Kotlin) => self.kotlin_heritage(node, &class_nid, stem, line),
                Some(HeritageStyle::Swift) => self.swift_heritage(node, &class_nid, stem, line),
                Some(HeritageStyle::Cpp) => self.cpp_heritage(node, &class_nid, stem, line),
                Some(HeritageStyle::Php) => self.php_heritage(node, &class_nid, stem, line),
                Some(HeritageStyle::Scala) => self.scala_heritage(node, &class_nid, stem, line),
                None => {}
            }

            // Non-method members: fields/properties become type `references`
            // (ctx `field`); C++ turns method prototypes into method nodes; Swift
            // captures init/deinit/subscript. Takes the declaration (it reads the
            // body and, for Kotlin, the primary constructor) so it runs even for a
            // body-less class like `class Dog(val x: Foo)`.
            self.class_members(node, &class_nid, stem);
            if let Some(body) = self.body_of(node) {
                // Python class docstring becomes rationale (reuses this parse/walk).
                if matches!(self.cfg.type_ref_style, Some(TypeRefStyle::Python)) {
                    if let Some((doc, dline)) = first_docstring(body, self.source) {
                        self.add_rationale(doc, dline, class_nid.clone(), stem);
                    }
                }
                for child in Self::children(body) {
                    self.walk(child, file_nid, Some(&class_nid), stem, depth + 1);
                }
            }
            return;
        }

        if self.cfg.function_types.contains(&t) {
            let Some(name_node) = self.function_name_node(node) else {
                return;
            };
            let func_name = self.text(name_node);
            let line = Self::line(node);
            let vis = self.decl_visibility(node, &func_name);
            let sig = crate::signature::extract_signature(node, self.source);
            let func_nid = if let Some(class_nid) = parent_class {
                let nid = NodeId(make_id(&[class_nid.as_str(), &func_name]));
                self.add_code_node(
                    nid.clone(),
                    format!(".{func_name}()"),
                    node,
                    synaptic_core::NodeKind::Method,
                    vis,
                    Some(sig),
                );
                self.add_edge(class_nid.clone(), nid.clone(), "method", line, None);
                nid
            } else {
                let nid = NodeId(make_id(&[stem, &func_name]));
                self.add_code_node(
                    nid.clone(),
                    format!("{func_name}()"),
                    node,
                    synaptic_core::NodeKind::Function,
                    vis,
                    Some(sig),
                );
                self.add_edge(file_nid.clone(), nid.clone(), "contains", line, None);
                nid
            };
            // Type-reference edges from parameter/return annotations.
            match self.cfg.type_ref_style {
                Some(TypeRefStyle::Python) => self.python_type_refs(node, &func_nid, stem, line),
                Some(TypeRefStyle::EcmaScript) => {
                    self.ecmascript_type_refs(node, &func_nid, stem, line)
                }
                Some(TypeRefStyle::Java) => self.java_type_refs(node, &func_nid, stem, line),
                Some(TypeRefStyle::CSharp) => self.csharp_type_refs(node, &func_nid, stem, line),
                Some(TypeRefStyle::Kotlin) => self.kotlin_type_refs(node, &func_nid, stem, line),
                Some(TypeRefStyle::Swift) => self.swift_type_refs(node, &func_nid, stem, line),
                Some(TypeRefStyle::Cpp) => self.cpp_type_refs(node, &func_nid, stem, line),
                Some(TypeRefStyle::Php) => self.php_type_refs(node, &func_nid, stem, line),
                Some(TypeRefStyle::Scala) => self.scala_type_refs(node, &func_nid, stem, line),
                None => {}
            }
            // Annotation/attribute references (Java `@Anno`, C# `[Attr]`) become
            // `references` edges with context "attribute".
            let anno_names = match self.cfg.type_ref_style {
                Some(TypeRefStyle::Java) => self.java_annotation_names(node),
                Some(TypeRefStyle::CSharp) => self.csharp_attribute_names(node),
                _ => Vec::new(),
            };
            for name in anno_names {
                let tgt = self.ensure_named_node(&name, stem, line);
                if tgt != func_nid {
                    self.add_edge(func_nid.clone(), tgt, "references", line, Some("attribute"));
                }
            }
            if let Some(body) = self.body_of(node) {
                // Python function docstring becomes rationale (reuses this parse/walk).
                if matches!(self.cfg.type_ref_style, Some(TypeRefStyle::Python)) {
                    if let Some((doc, dline)) = first_docstring(body, self.source) {
                        self.add_rationale(doc, dline, func_nid.clone(), stem);
                    }
                }
                self.function_bodies.push((func_nid, body));
            }
            return;
        }

        // Default: recurse, resetting class scope.
        // Iterate the cursor directly: no per-node `Vec` allocation.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, file_nid, None, stem, depth + 1);
        }
    }

    /// Base type names inside a heritage clause, including generic bases
    /// (`extends Base<T>` → `Base`) via [`ts_base_head`](Self::ts_base_head).
    pub(crate) fn heritage_bases(&self, clause: TsNode<'tree>) -> Vec<String> {
        Self::children(clause)
            .into_iter()
            .filter_map(|c| self.ts_base_head(c))
            .collect()
    }

    /// Link `class_nid` to a base/interface by name, creating an external stub
    /// when the base is not defined in this file (so the edge survives build's
    /// dangling-edge drop). Used by the `superclasses_field` path.
    pub(crate) fn link_heritage(
        &mut self,
        class_nid: &NodeId,
        base: String,
        stem: &str,
        line: usize,
        relation: &str,
    ) {
        let local = NodeId(make_id(&[stem, &base]));
        let base_nid = if self.seen.contains(&local) {
            local
        } else {
            let global = NodeId(make_id(&[base.as_str()]));
            self.add_external_node(global.clone(), base.clone());
            global
        };
        self.add_edge(class_nid.clone(), base_nid, relation, line, None);
    }

    // Java / C# (dotted-name imports)
    /// An `imports` edge to the tail of a dotted import name (Java `import
    /// a.b.C;`, C# `using A.B.C;`) as an external stub. `name_kinds` are the node
    /// kinds that carry the dotted name; for a wildcard/package the tail is the
    /// last segment.
    fn dotted_import(&mut self, node: TsNode<'tree>, file_nid: &NodeId, name_kinds: &[&str]) {
        let line = Self::line(node);
        let name_node = Self::children(node)
            .into_iter()
            .find(|c| name_kinds.contains(&c.kind()));
        let Some(nn) = name_node else { return };
        let full = self.text(nn);
        let tail = full.rsplit('.').next().unwrap_or(&full).trim();
        if tail.is_empty() {
            return;
        }
        let tgt = NodeId(make_id(&[tail]));
        self.add_external_node(tgt.clone(), tail.to_string());
        self.add_edge(file_nid.clone(), tgt, "imports", line, Some("import"));
    }

    /// First identifier under the first `user_type` in `node` (the base type name
    /// of a Kotlin delegation specifier / constructor invocation).
    pub(crate) fn user_type_head(&self, node: TsNode<'tree>) -> Option<String> {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "user_type" {
                return Self::children(n)
                    .into_iter()
                    .find(|c| matches!(c.kind(), "identifier" | "type_identifier"))
                    .map(|c| self.text(c));
            }
            for c in Self::children(n) {
                stack.push(c);
            }
        }
        None
    }

    /// All children of `node` attached under field `name` (Swift reuses `name`
    /// for both the identifier and the type).
    pub(crate) fn named_field_nodes(&self, node: TsNode<'tree>, field: &str) -> Vec<TsNode<'tree>> {
        let mut cur = node.walk();
        node.children_by_field_name(field, &mut cur).collect()
    }

    // class members (fields/properties/prototypes)
    /// Per-language class-body member handling, run before the method walk:
    /// fields/properties become type `references` (ctx `field`); C++ method
    /// prototypes and Swift init/deinit/subscript become method nodes.
    fn class_members(&mut self, decl: TsNode<'tree>, class_nid: &NodeId, stem: &str) {
        match self.cfg.type_ref_style {
            Some(TypeRefStyle::Java) => {
                if let Some(body) = self.body_of(decl) {
                    self.java_class_members(body, class_nid, stem);
                }
            }
            Some(TypeRefStyle::CSharp) => {
                if let Some(body) = self.body_of(decl) {
                    self.csharp_class_members(body, class_nid, stem);
                }
            }
            Some(TypeRefStyle::Kotlin) => self.kotlin_class_members(decl, class_nid, stem),
            Some(TypeRefStyle::Swift) => {
                if let Some(body) = self.body_of(decl) {
                    self.swift_class_members(body, class_nid, stem);
                }
            }
            Some(TypeRefStyle::Cpp) => {
                if let Some(body) = self.body_of(decl) {
                    self.cpp_class_members(body, class_nid, stem);
                }
            }
            Some(TypeRefStyle::Php) => {
                if let Some(body) = self.body_of(decl) {
                    self.php_class_members(body, class_nid, stem);
                }
            }
            _ => {}
        }
    }

    /// Emit `references` (ctx `field`, or `generic_arg`) from `owner` to each
    /// collected member type.
    pub(crate) fn emit_field_refs(
        &mut self,
        refs: Vec<(String, bool)>,
        owner: &NodeId,
        stem: &str,
        line: usize,
    ) {
        for (name, generic) in refs {
            let ctx = if generic { "generic_arg" } else { "field" };
            let tgt = self.ensure_named_node(&name, stem, line);
            if &tgt != owner {
                self.add_edge(owner.clone(), tgt, "references", line, Some(ctx));
            }
        }
    }

    // pre-scan
    /// Collect in-file interface/protocol names (by node kind, per the language's
    /// heritage style) so heritage classification can tell interfaces from base
    /// classes. No-op for languages that don't need it.
    pub(crate) fn pre_scan(&mut self, root: TsNode<'tree>) {
        let kinds: &[&str] = match self.cfg.heritage_style {
            Some(HeritageStyle::CSharp) => &["interface_declaration"],
            Some(HeritageStyle::Swift) => &["protocol_declaration"],
            _ => return,
        };
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if kinds.contains(&n.kind()) {
                if let Some(name) = self.field(n, self.cfg.name_field) {
                    self.interface_names.insert(self.text(name));
                }
            }
            for c in Self::children(n) {
                stack.push(c);
            }
        }
    }

    pub(crate) fn run_call_pass(&mut self) {
        // Map normalized label -> node id: "run_analysis()" -> "run_analysis",
        // ".forward()" -> "forward" (reference: raw.strip("()").lstrip(".")).
        let mut label_to_nid: HashMap<String, NodeId> = HashMap::new();
        for n in &self.nodes {
            let key = n
                .label
                .trim_matches(|c| c == '(' || c == ')')
                .trim_start_matches('.')
                .to_string();
            label_to_nid.insert(key, n.id.clone());
        }

        let bodies = std::mem::take(&mut self.function_bodies);
        let mut seen_pairs: HashSet<(NodeId, NodeId)> = HashSet::new();
        for (caller, body) in bodies {
            self.walk_calls(body, &caller, &label_to_nid, &mut seen_pairs, 0);
        }
    }

    fn walk_calls(
        &mut self,
        node: TsNode<'tree>,
        caller: &NodeId,
        label_to_nid: &HashMap<String, NodeId>,
        seen_pairs: &mut HashSet<(NodeId, NodeId)>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        if self.cfg.function_boundary_types.contains(&node.kind()) {
            return;
        }

        if self.cfg.call_types.contains(&node.kind()) {
            if let Some((callee, is_member)) = self.callee_name(node) {
                let line = Self::line(node);
                self.record_call(
                    caller,
                    callee,
                    is_member,
                    line,
                    Some(Self::span(node)),
                    label_to_nid,
                    seen_pairs,
                );
            }
        }
        // `new X(...)` constructor call: the callee is the constructed type.
        if Some(node.kind()) == self.cfg.constructor_call_type {
            if let Some(ctor) = self.field(node, "constructor") {
                if matches!(ctor.kind(), "identifier" | "type_identifier") {
                    let callee = self.text(ctor);
                    let line = Self::line(node);
                    self.record_call(
                        caller,
                        callee,
                        false,
                        line,
                        Some(Self::span(node)),
                        label_to_nid,
                        seen_pairs,
                    );
                }
            }
        }

        // EcmaScript dynamic imports inside function/method bodies (`walk()` covers
        // module-scope ones). Emits a file -> module-stub `imports_from` edge.
        if matches!(self.cfg.import_style, Some(ImportStyle::EcmaScript))
            && self.cfg.call_types.contains(&node.kind())
        {
            let file_nid = file_node_id(&self.path);
            self.ecmascript_dynamic_import(node, &file_nid);
        }

        for child in Self::children(node) {
            self.walk_calls(child, caller, label_to_nid, seen_pairs, depth + 1);
        }
    }

    /// Resolve a discovered callee to a `calls` edge (in-file target) or a
    /// `RawCall` (unresolved, for cross-file resolution). Builtins are skipped.
    #[allow(clippy::too_many_arguments)]
    fn record_call(
        &mut self,
        caller: &NodeId,
        callee: String,
        is_member: bool,
        line: usize,
        span: Option<synaptic_core::Span>,
        label_to_nid: &HashMap<String, NodeId>,
        seen_pairs: &mut HashSet<(NodeId, NodeId)>,
    ) {
        if self.cfg.builtins.contains(&callee.as_str()) {
            return;
        }
        match label_to_nid.get(&callee) {
            Some(tgt) if tgt != caller => {
                let pair = (caller.clone(), tgt.clone());
                if seen_pairs.insert(pair) {
                    self.add_edge(caller.clone(), tgt.clone(), "calls", line, Some("call"));
                }
            }
            Some(_) => {} // self-call, ignore
            None => {
                self.raw_calls.push(RawCall {
                    caller: caller.clone(),
                    callee,
                    is_member_call: is_member,
                    source_file: self.path.clone(),
                    source_location: Some(format!("L{line}")),
                    span,
                });
            }
        }
    }

    /// Returns `(callee_name, is_member_call)` for a call node, or `None`. When
    /// `call_function_field` is empty the callee is the first named child (for
    /// grammars whose call node names the callee positionally, e.g. Kotlin/Swift
    /// `call_expression`).
    fn callee_name(&self, call: TsNode<'tree>) -> Option<(String, bool)> {
        let func = if self.cfg.call_function_field.is_empty() {
            Self::children(call).into_iter().find(|c| c.is_named())?
        } else {
            // Fall back to a `name` field for grammars with separate call-node
            // types (PHP `member_call_expression`/`scoped_call_expression` carry
            // the callee in `name`, not the `function` field of a
            // `function_call_expression`).
            match self.field(call, self.cfg.call_function_field) {
                Some(f) => f,
                None => self.field(call, "name")?,
            }
        };
        if matches!(func.kind(), "identifier" | "simple_identifier") {
            Some((self.text(func), false))
        } else if self.cfg.call_accessor_node_types.contains(&func.kind()) {
            let attr = self.field(func, self.cfg.call_accessor_field)?;
            Some((self.text(attr), true))
        } else if func.kind() == "navigation_expression" {
            // Kotlin/Swift member call `recv.method()`: the member is the last
            // identifier (directly, or inside the last `navigation_suffix`).
            self.navigation_member(func).map(|m| (m, true))
        } else {
            Some((self.text(func), false))
        }
    }

    /// The trailing member name of a `navigation_expression` (`a.b.method` → `method`).
    fn navigation_member(&self, nav: TsNode<'tree>) -> Option<String> {
        for c in Self::children(nav).into_iter().rev() {
            match c.kind() {
                "identifier" | "simple_identifier" => return Some(self.text(c)),
                "navigation_suffix" => {
                    if let Some(id) = Self::children(c)
                        .into_iter()
                        .rev()
                        .find(|x| matches!(x.kind(), "identifier" | "simple_identifier"))
                    {
                        return Some(self.text(id));
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Resolve a name to an existing in-file node id, else a global id (creating a
    /// stub node when unseen).
    pub(crate) fn ensure_named_node(&mut self, name: &str, stem: &str, line: usize) -> NodeId {
        let local = NodeId(make_id(&[stem, name]));
        if self.seen.contains(&local) {
            return local;
        }
        let global = NodeId(make_id(&[name]));
        if !self.seen.contains(&global) {
            self.add_node(global.clone(), name.to_string(), line);
        }
        global
    }
}
