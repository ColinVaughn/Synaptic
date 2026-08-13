//! CodeQL QL extractor (`.ql` queries and `.qll` libraries).
//!
//! QL is a declarative, object-oriented logic language, so its useful code graph
//! is made of modules, classes/newtypes, predicates, imports, type relationships,
//! and predicate calls. The maintained tree-sitter grammar provides the AST for
//! those constructs. A small recovery pass covers `signature
//! class|module|predicate`, which the published grammar does not currently model.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::json;
use synaptic_core::{NodeId, NodeKind, Param, RawCall, Signature, Visibility, make_id};
use tree_sitter::{Node as TsNode, Parser};

use crate::common::Builder;
use crate::paths::file_node_id;
use crate::result::{ExtractionResult, ImportRecord};

const MAX_DEPTH: usize = 2000;

static SIGNATURE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^[ \t]*(?:(?:private|cached|external|extensible|final|abstract|deprecated|library|override|query)\s+)*signature\s+(class|module|predicate)\s+([A-Za-z_][A-Za-z0-9_]*)([^\r\n;{]*)",
    )
    .expect("valid QL signature declaration regex")
});

#[derive(Clone)]
struct Scope {
    key: String,
    owner: NodeId,
    class_name: Option<String>,
}

struct CallableSite<'tree> {
    id: NodeId,
    node: TsNode<'tree>,
    scope: Scope,
}

struct TypeRefFact {
    source: NodeId,
    name: String,
    scope_key: String,
    relation: &'static str,
    context: &'static str,
    line: usize,
}

#[derive(Debug)]
enum CallQualifier {
    Unqualified,
    Module(String),
    Receiver(String),
}

struct CallFact {
    caller: NodeId,
    name: String,
    arity: usize,
    qualifier: CallQualifier,
    scope: Scope,
    line: usize,
}

/// Extract a QL query/library already in memory.
pub fn extract_ql_source(path: &str, source: &[u8]) -> ExtractionResult {
    let (parser_source, overlays) = mask_overlay_annotations(source);
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_ql::LANGUAGE.into())
        .expect("load tree-sitter-ql");
    let Some(tree) = parser.parse(parser_source.as_ref(), None) else {
        return ExtractionResult::default();
    };

    // The complete root-relative path, not only the file stem, is the QL symbol
    // namespace. CodeQL packs repeat common filenames (Utils.qll, DataFlow.qll,
    // ...) thousands of times, so a short stem would collapse unrelated symbols.
    let file_scope = ql_file_scope(path);
    let file_nid = file_node_id(path);
    let file_label = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());

    let mut ex = QlExtractor {
        source,
        file_scope: file_scope.clone(),
        file_nid: file_nid.clone(),
        b: Builder::new(path),
        callables: Vec::new(),
        type_refs: Vec::new(),
        predicates: HashMap::new(),
        methods: HashMap::new(),
        types: HashMap::new(),
    };
    ex.b.add_node(file_nid.clone(), file_label, 1);
    if let Some(file) = ex.b.nodes.iter_mut().find(|n| n.id == file_nid) {
        file.extra.insert("_language".into(), json!("ql"));
        file.extra.insert(
            "ql_parse_errors".into(),
            json!(tree.root_node().has_error()),
        );
        if !overlays.is_empty() {
            let mut modes = overlays.clone();
            modes.sort();
            modes.dedup();
            file.extra
                .insert("ql_overlay_directives".into(), json!(overlays.len()));
            file.extra.insert("ql_overlay_modes".into(), json!(modes));
        }
    }

    let root_scope = Scope {
        key: file_scope,
        owner: file_nid,
        class_name: None,
    };
    ex.walk_declarations(tree.root_node(), &root_scope, 0);
    let signature_recoveries = ex.recover_signature_declarations();
    if signature_recoveries > 0
        && let Some(file) = ex.b.nodes.iter_mut().find(|n| n.id == ex.file_nid)
    {
        file.extra.insert(
            "ql_signature_recoveries".into(),
            json!(signature_recoveries),
        );
    }
    ex.resolve_type_refs();
    ex.resolve_calls();
    ex.b.into_result()
}

/// Read and extract a QL source file from disk.
pub fn extract_ql_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    Ok(extract_ql_source(&path.to_string_lossy(), &source))
}

fn ql_file_scope(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let without_ext = normalized
        .strip_suffix(".qll")
        .or_else(|| normalized.strip_suffix(".ql"))
        .unwrap_or(&normalized);
    without_ext.replace('/', "::")
}

struct QlExtractor<'src, 'tree> {
    source: &'src [u8],
    file_scope: String,
    file_nid: NodeId,
    b: Builder,
    callables: Vec<CallableSite<'tree>>,
    type_refs: Vec<TypeRefFact>,
    /// (lexical scope, predicate name, arity) -> declarations.
    predicates: HashMap<(String, String, usize), Vec<NodeId>>,
    /// (class bare name, predicate name, arity) -> member declarations.
    methods: HashMap<(String, String, usize), Vec<NodeId>>,
    /// (lexical scope, bare type name) -> declaration.
    types: HashMap<(String, String), NodeId>,
}

impl<'tree> QlExtractor<'_, 'tree> {
    fn text(&self, node: TsNode<'tree>) -> String {
        node.utf8_text(self.source).unwrap_or("").to_string()
    }

    fn line(node: TsNode<'tree>) -> usize {
        node.start_position().row + 1
    }

    fn named_children(node: TsNode<'tree>) -> Vec<TsNode<'tree>> {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).collect()
    }

    fn visibility(&self, node: TsNode<'tree>) -> Option<Visibility> {
        let Some(parent) = node.parent() else {
            return Some(Visibility::Public);
        };
        let is_private = Self::named_children(parent)
            .into_iter()
            .take_while(|child| child.id() != node.id())
            .filter(|child| child.kind() == "annotation")
            .any(|annotation| {
                annotation
                    .child_by_field_name("name")
                    .is_some_and(|name| self.text(name).trim() == "private")
            });
        Some(if is_private {
            Visibility::Private
        } else {
            Visibility::Public
        })
    }

    fn tag_ql_node(&mut self, id: &NodeId, arity: Option<usize>, declaration: &str) {
        if let Some(node) = self.b.nodes.iter_mut().find(|n| &n.id == id) {
            node.extra.insert("_language".into(), json!("ql"));
            node.extra
                .insert("ql_declaration".into(), json!(declaration));
            if let Some(arity) = arity {
                node.extra.insert("ql_arity".into(), json!(arity));
            }
        }
    }

    fn walk_declarations(&mut self, node: TsNode<'tree>, scope: &Scope, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "module" => self.add_module(node, scope, depth),
            "dataclass" => self.add_class(node, scope, depth),
            "datatype" => self.add_newtype(node, scope),
            "classlessPredicate" => self.add_predicate(node, scope, false, "predicate"),
            "memberPredicate" => self.add_predicate(node, scope, true, "predicate"),
            "charpred" => self.add_characteristic_predicate(node, scope),
            "field" => self.add_field(node, scope),
            "importDirective" => self.add_import(node, scope),
            "select" => self.callables.push(CallableSite {
                id: scope.owner.clone(),
                node,
                scope: scope.clone(),
            }),
            _ => {
                for child in Self::named_children(node) {
                    self.walk_declarations(child, scope, depth + 1);
                }
            }
        }
    }

    fn add_module(&mut self, node: TsNode<'tree>, scope: &Scope, depth: usize) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let line = Self::line(node);
        let key = format!("{}::{name}", scope.key);
        let id = NodeId(make_id(&[&key]));
        self.b.add_code_node(
            id.clone(),
            name.clone(),
            node,
            NodeKind::Module,
            self.visibility(node),
            None,
        );
        self.tag_ql_node(&id, None, "module");
        self.b.add_edge(
            scope.owner.clone(),
            id.clone(),
            "contains",
            line,
            Some("module"),
        );
        self.types.insert((scope.key.clone(), name), id.clone());

        // Parameterized modules implement signatures; retain those dependencies.
        let mut cursor = node.walk();
        for implemented in node.children_by_field_name("implements", &mut cursor) {
            if let Some(name) = self.type_name(implemented) {
                self.type_refs.push(TypeRefFact {
                    source: id.clone(),
                    name,
                    scope_key: scope.key.clone(),
                    relation: "implements",
                    context: "module_signature",
                    line,
                });
            }
        }

        let nested = Scope {
            key,
            owner: id,
            class_name: None,
        };
        for child in Self::named_children(node)
            .into_iter()
            .filter(|child| child.kind() == "moduleMember")
        {
            self.walk_declarations(child, &nested, depth + 1);
        }
    }

    fn add_class(&mut self, node: TsNode<'tree>, scope: &Scope, depth: usize) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let line = Self::line(node);
        let key = format!("{}::{name}", scope.key);
        let id = NodeId(make_id(&[&key]));
        let alias = Self::named_children(node)
            .iter()
            .any(|child| matches!(child.kind(), "typeAliasBody" | "typeUnionBody"));
        self.b.add_code_node(
            id.clone(),
            name.clone(),
            node,
            if alias {
                NodeKind::TypeAlias
            } else {
                NodeKind::Class
            },
            self.visibility(node),
            None,
        );
        self.tag_ql_node(&id, None, if alias { "type_alias" } else { "class" });
        self.b.add_edge(
            scope.owner.clone(),
            id.clone(),
            "contains",
            line,
            Some(if alias { "type_alias" } else { "class" }),
        );
        self.types
            .insert((scope.key.clone(), name.clone()), id.clone());

        let mut cursor = node.walk();
        for base in node.children_by_field_name("extends", &mut cursor) {
            if base.kind() == "typeExpr"
                && let Some(name) = self.type_name(base)
            {
                self.type_refs.push(TypeRefFact {
                    source: id.clone(),
                    name,
                    scope_key: scope.key.clone(),
                    relation: "inherits",
                    context: "extends",
                    line,
                });
            }
        }
        let mut cursor = node.walk();
        for constraint in node.children_by_field_name("instanceof", &mut cursor) {
            if constraint.kind() == "typeExpr"
                && let Some(name) = self.type_name(constraint)
            {
                self.type_refs.push(TypeRefFact {
                    source: id.clone(),
                    name,
                    scope_key: scope.key.clone(),
                    relation: "references",
                    context: "instanceof",
                    line,
                });
            }
        }

        let nested = Scope {
            key,
            owner: id,
            class_name: Some(name),
        };
        for child in Self::named_children(node)
            .into_iter()
            .filter(|child| child.kind() == "classMember")
        {
            self.walk_declarations(child, &nested, depth + 1);
        }
    }

    fn add_newtype(&mut self, node: TsNode<'tree>, scope: &Scope) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node).trim().to_string();
        let line = Self::line(node);
        let key = format!("{}::{name}", scope.key);
        let id = NodeId(make_id(&[&key]));
        self.b.add_code_node(
            id.clone(),
            name.clone(),
            node,
            NodeKind::Enum,
            self.visibility(node),
            None,
        );
        self.tag_ql_node(&id, None, "newtype");
        self.b.add_edge(
            scope.owner.clone(),
            id.clone(),
            "contains",
            line,
            Some("newtype"),
        );
        self.types
            .insert((scope.key.clone(), name.clone()), id.clone());

        let Some(branches) = Self::named_children(node)
            .into_iter()
            .find(|child| child.kind() == "datatypeBranches")
        else {
            return;
        };
        for branch in Self::named_children(branches)
            .into_iter()
            .filter(|child| child.kind() == "datatypeBranch")
        {
            let Some(branch_name_node) = branch.child_by_field_name("name") else {
                continue;
            };
            let branch_name = self.text(branch_name_node).trim().to_string();
            let arity = Self::named_children(branch)
                .iter()
                .filter(|child| child.kind() == "varDecl")
                .count();
            let branch_id = NodeId(make_id(&[
                &key,
                &branch_name.to_ascii_lowercase(),
                &arity.to_string(),
            ]));
            self.b.add_code_node(
                branch_id.clone(),
                format!("{branch_name}()"),
                branch,
                NodeKind::Constructor,
                self.visibility(branch),
                Some(self.signature(branch, &branch_name, None)),
            );
            self.tag_ql_node(&branch_id, Some(arity), "newtype_branch");
            self.b.add_edge(
                id.clone(),
                branch_id.clone(),
                "contains",
                Self::line(branch),
                Some("newtype_branch"),
            );
            self.predicates
                .entry((scope.key.clone(), branch_name.to_ascii_lowercase(), arity))
                .or_default()
                .push(branch_id);
        }
    }

    fn add_predicate(
        &mut self,
        node: TsNode<'tree>,
        scope: &Scope,
        member: bool,
        declaration: &str,
    ) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let params = Self::named_children(node)
            .into_iter()
            .filter(|child| child.kind() == "varDecl")
            .count();
        let id = NodeId(make_id(&[
            &scope.key,
            &name.to_ascii_lowercase(),
            &params.to_string(),
        ]));
        let return_type = node
            .child_by_field_name("returnType")
            .map(|n| self.text(n).trim().to_string());
        self.b.add_code_node(
            id.clone(),
            if member {
                format!(".{name}()")
            } else {
                format!("{name}()")
            },
            node,
            if member {
                NodeKind::Method
            } else {
                NodeKind::Function
            },
            self.visibility(node),
            Some(self.signature(node, &name, return_type.as_deref())),
        );
        self.tag_ql_node(&id, Some(params), declaration);
        self.b.add_edge(
            scope.owner.clone(),
            id.clone(),
            if member { "method" } else { "contains" },
            Self::line(node),
            Some(declaration),
        );
        self.predicates
            .entry((scope.key.clone(), name.to_ascii_lowercase(), params))
            .or_default()
            .push(id.clone());
        if let Some(class_name) = &scope.class_name {
            self.methods
                .entry((
                    class_name.to_ascii_lowercase(),
                    name.to_ascii_lowercase(),
                    params,
                ))
                .or_default()
                .push(id.clone());
        }

        if let Some(return_node) = node.child_by_field_name("returnType") {
            self.queue_type_ref(
                &id,
                return_node,
                &scope.key,
                "return_type",
                Self::line(node),
            );
        }
        for parameter in Self::named_children(node)
            .into_iter()
            .filter(|child| child.kind() == "varDecl")
        {
            if let Some(type_node) = Self::named_children(parameter)
                .into_iter()
                .find(|child| child.kind() == "typeExpr")
            {
                self.queue_type_ref(
                    &id,
                    type_node,
                    &scope.key,
                    "parameter_type",
                    Self::line(parameter),
                );
            }
        }

        self.callables.push(CallableSite {
            id,
            node,
            scope: scope.clone(),
        });
    }

    fn add_characteristic_predicate(&mut self, node: TsNode<'tree>, scope: &Scope) {
        let Some(name_node) = Self::named_children(node)
            .into_iter()
            .find(|child| child.kind() == "className")
        else {
            return;
        };
        let name = self.text(name_node).trim().to_string();
        let id = NodeId(make_id(&[
            &scope.key,
            &name.to_ascii_lowercase(),
            "characteristic",
        ]));
        self.b.add_code_node(
            id.clone(),
            format!(".{name}()"),
            node,
            NodeKind::Constructor,
            self.visibility(node),
            Some(Signature {
                params: vec![],
                return_type: None,
                raw: format!("{name}()"),
            }),
        );
        self.tag_ql_node(&id, Some(0), "characteristic_predicate");
        self.b.add_edge(
            scope.owner.clone(),
            id.clone(),
            "method",
            Self::line(node),
            Some("characteristic_predicate"),
        );
        if let Some(class_name) = &scope.class_name {
            self.methods
                .entry((
                    class_name.to_ascii_lowercase(),
                    name.to_ascii_lowercase(),
                    0,
                ))
                .or_default()
                .push(id.clone());
        }
        self.callables.push(CallableSite {
            id,
            node,
            scope: scope.clone(),
        });
    }

    fn add_field(&mut self, node: TsNode<'tree>, scope: &Scope) {
        let Some(decl) = Self::named_children(node)
            .into_iter()
            .find(|child| child.kind() == "varDecl")
        else {
            return;
        };
        let Some(name_node) = Self::named_children(decl)
            .iter()
            .find(|child| child.kind() == "varName")
            .copied()
        else {
            return;
        };
        let name = self.text(name_node).trim().to_string();
        let id = NodeId(make_id(&[&scope.key, &name.to_ascii_lowercase(), "field"]));
        self.b.add_code_node(
            id.clone(),
            name,
            node,
            NodeKind::Field,
            self.visibility(node),
            None,
        );
        self.tag_ql_node(&id, None, "field");
        self.b.add_edge(
            scope.owner.clone(),
            id.clone(),
            "contains",
            Self::line(node),
            Some("field"),
        );
        if let Some(type_node) = Self::named_children(decl)
            .into_iter()
            .find(|child| child.kind() == "typeExpr")
        {
            self.queue_type_ref(&id, type_node, &scope.key, "field", Self::line(node));
        }
    }

    fn add_import(&mut self, node: TsNode<'tree>, scope: &Scope) {
        let children = Self::named_children(node);
        let Some(import_expr) = children
            .iter()
            .find(|child| child.kind() == "importModuleExpr")
            .copied()
        else {
            return;
        };
        let spec = compact_ql_name(&self.text(import_expr));
        if spec.is_empty() {
            return;
        }
        let alias = children
            .iter()
            .rfind(|child| child.kind() == "moduleName")
            .map(|child| self.text(*child).trim().to_string())
            .filter(|s| !s.is_empty());
        let local_name = alias.unwrap_or_else(|| ql_module_leaf(&spec));
        let target = NodeId(make_id(&["ql_import", &spec]));
        self.b.add_external_node(target.clone(), spec.clone());
        self.b.add_edge(
            scope.owner.clone(),
            target,
            "imports_from",
            Self::line(node),
            Some("ql_import"),
        );
        // `imported_name == "*"` is a module import marker consumed by the
        // QL-aware symbol resolver. Existing Python/JS named-import behavior is
        // unchanged.
        self.b.imports.push(ImportRecord {
            local_name,
            imported_name: "*".into(),
            module_stem: spec,
            source_file: self.b.path.clone(),
            source_location: Some(format!("L{}", Self::line(node))),
        });
    }

    fn signature(&self, node: TsNode<'tree>, name: &str, return_type: Option<&str>) -> Signature {
        let mut params = Vec::new();
        for decl in Self::named_children(node)
            .into_iter()
            .filter(|child| child.kind() == "varDecl")
        {
            let children = Self::named_children(decl);
            let param_name = children
                .iter()
                .find(|child| child.kind() == "varName")
                .map(|n| self.text(*n).trim().to_string())
                .unwrap_or_else(|| "_".into());
            let type_ref = children
                .iter()
                .find(|child| child.kind() == "typeExpr")
                .map(|n| compact_ql_name(&self.text(*n)))
                .filter(|s| !s.is_empty());
            params.push(Param {
                name: param_name,
                type_ref,
            });
        }
        let params_raw = params
            .iter()
            .map(|p| match &p.type_ref {
                Some(ty) => format!("{ty} {}", p.name),
                None => p.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let return_type = return_type
            .map(compact_ql_name)
            .filter(|ty| ty != "predicate" && !ty.is_empty());
        let raw = match &return_type {
            Some(ty) => format!("{ty} {name}({params_raw})"),
            None => format!("predicate {name}({params_raw})"),
        };
        Signature {
            params,
            return_type,
            raw,
        }
    }

    fn type_name(&self, node: TsNode<'tree>) -> Option<String> {
        if node.kind() == "signatureExpr" {
            return Self::named_children(node)
                .into_iter()
                .find_map(|child| self.type_name(child));
        }
        if node.kind() != "typeExpr" {
            return None;
        }
        node.child_by_field_name("name")
            .map(|name| {
                let qualifier = node
                    .child_by_field_name("qualifier")
                    .map(|q| compact_ql_name(&self.text(q)));
                let bare = self.text(name).trim().to_string();
                match qualifier.filter(|q| !q.is_empty()) {
                    Some(q) => format!("{q}::{bare}"),
                    None => bare,
                }
            })
            .filter(|name| !name.is_empty())
    }

    fn queue_type_ref(
        &mut self,
        source: &NodeId,
        type_node: TsNode<'tree>,
        scope_key: &str,
        context: &'static str,
        line: usize,
    ) {
        let Some(name) = self.type_name(type_node) else {
            return;
        };
        self.type_refs.push(TypeRefFact {
            source: source.clone(),
            name,
            scope_key: scope_key.to_string(),
            relation: "references",
            context,
            line,
        });
    }

    fn resolve_type_refs(&mut self) {
        let facts = std::mem::take(&mut self.type_refs);
        for fact in facts {
            let bare = fact.name.rsplit("::").next().unwrap_or(&fact.name);
            if is_ql_primitive(bare) {
                continue;
            }
            let target = self
                .find_type(&fact.scope_key, &fact.name)
                .unwrap_or_else(|| self.b.ensure_named_node(bare, &fact.scope_key, fact.line));
            if target != fact.source {
                self.b.add_edge(
                    fact.source,
                    target,
                    fact.relation,
                    fact.line,
                    Some(fact.context),
                );
            }
        }
    }

    fn find_type(&self, scope_key: &str, name: &str) -> Option<NodeId> {
        let bare = name
            .rsplit("::")
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase();
        if let Some((qualifier, _)) = name.rsplit_once("::") {
            for candidate_scope in lexical_scopes(scope_key) {
                let qualified_scope = format!("{candidate_scope}::{qualifier}");
                if let Some(id) = self
                    .types
                    .iter()
                    .find(|((scope, n), _)| {
                        scope == &qualified_scope && n.eq_ignore_ascii_case(&bare)
                    })
                    .map(|(_, id)| id.clone())
                {
                    return Some(id);
                }
            }
        }
        for candidate_scope in lexical_scopes(scope_key) {
            if let Some(id) = self
                .types
                .iter()
                .find(|((scope, n), _)| scope == &candidate_scope && n.eq_ignore_ascii_case(&bare))
                .map(|(_, id)| id.clone())
            {
                return Some(id);
            }
        }
        None
    }

    fn resolve_calls(&mut self) {
        let sites = std::mem::take(&mut self.callables);
        let mut calls = Vec::new();
        for site in sites {
            let variable_types = self.variable_types(site.node);
            self.collect_calls(
                site.node,
                &site.id,
                &site.scope,
                &variable_types,
                0,
                &mut calls,
            );
        }

        let mut seen = HashSet::new();
        for call in calls {
            let target = match &call.qualifier {
                CallQualifier::Unqualified => {
                    self.find_lexical_predicate(&call.scope.key, &call.name, call.arity)
                }
                CallQualifier::Module(module) => {
                    self.find_module_predicate(&call.scope.key, module, &call.name, call.arity)
                }
                CallQualifier::Receiver(receiver) => {
                    self.find_member_predicate(&call.scope, receiver, &call.name, call.arity)
                }
            };
            if let Some(target) = target {
                if target != call.caller && seen.insert((call.caller.clone(), target.clone())) {
                    self.b.add_edge(
                        call.caller,
                        target,
                        "calls",
                        call.line,
                        Some("ql_predicate_call"),
                    );
                }
                continue;
            }

            let (callee, is_member_call) = match call.qualifier {
                CallQualifier::Unqualified => (format!("ql:{}/{}", call.name, call.arity), false),
                CallQualifier::Module(module) => {
                    (format!("ql:{module}::{}/{}", call.name, call.arity), false)
                }
                CallQualifier::Receiver(_) => (format!("ql:{}/{}", call.name, call.arity), true),
            };
            self.b.raw_calls.push(RawCall {
                caller: call.caller,
                callee,
                is_member_call,
                source_file: self.b.path.clone(),
                source_location: Some(format!("L{}", call.line)),
                span: None,
            });
        }
    }

    fn variable_types(&self, node: TsNode<'tree>) -> HashMap<String, String> {
        let mut vars = HashMap::new();
        let mut stack = vec![(node, 0usize)];
        while let Some((current, depth)) = stack.pop() {
            if depth >= MAX_DEPTH {
                continue;
            }
            if current.kind() == "varDecl" {
                let children = Self::named_children(current);
                let name = children
                    .iter()
                    .find(|child| child.kind() == "varName")
                    .map(|n| self.text(*n).trim().to_string());
                let ty = children
                    .iter()
                    .find(|child| child.kind() == "typeExpr")
                    .and_then(|n| self.type_name(*n));
                if let (Some(name), Some(ty)) = (name, ty) {
                    vars.insert(name, ty);
                }
            }
            for child in Self::named_children(current).into_iter().rev() {
                if child.id() != node.id()
                    && matches!(
                        child.kind(),
                        "classlessPredicate"
                            | "memberPredicate"
                            | "charpred"
                            | "module"
                            | "dataclass"
                    )
                {
                    continue;
                }
                stack.push((child, depth + 1));
            }
        }
        vars
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_calls(
        &self,
        node: TsNode<'tree>,
        caller: &NodeId,
        scope: &Scope,
        variable_types: &HashMap<String, String>,
        depth: usize,
        out: &mut Vec<CallFact>,
    ) {
        if depth >= MAX_DEPTH {
            return;
        }
        if node.kind() == "call_or_unqual_agg_expr" {
            let children = Self::named_children(node);
            if let Some(pred) = children
                .iter()
                .find(|child| child.kind() == "aritylessPredicateExpr")
            {
                let Some(name_node) = pred.child_by_field_name("name") else {
                    return;
                };
                let name = self.text(name_node).trim().to_string();
                let qualifier = pred
                    .child_by_field_name("qualifier")
                    .map(|q| compact_ql_name(&self.text(q)))
                    .filter(|q| !q.is_empty())
                    .map(CallQualifier::Module)
                    .unwrap_or(CallQualifier::Unqualified);
                let arity = children
                    .iter()
                    .find(|child| child.kind() == "call_body")
                    .map(|body| body.named_child_count())
                    .unwrap_or(0);
                out.push(CallFact {
                    caller: caller.clone(),
                    name,
                    arity,
                    qualifier,
                    scope: scope.clone(),
                    line: Self::line(node),
                });
            }
            // Recurse into arguments, but not into the callee node (which would
            // otherwise be counted again as an arityless zero-argument call).
            for child in children {
                if child.kind() != "aritylessPredicateExpr" {
                    self.collect_calls(child, caller, scope, variable_types, depth + 1, out);
                }
            }
            return;
        }
        if node.kind() == "qualifiedRhs"
            && let Some(name_node) = node.child_by_field_name("name")
        {
            let name = self.text(name_node).trim().to_string();
            let arity = Self::named_children(node)
                .iter()
                .filter(|child| !matches!(child.kind(), "predicateName" | "closure" | "typeExpr"))
                .count();
            let receiver = node
                .parent()
                .and_then(|parent| {
                    Self::named_children(parent)
                        .into_iter()
                        .take_while(|child| child.id() != node.id())
                        .last()
                })
                .map(|recv| compact_ql_name(&self.text(recv)))
                .unwrap_or_default();
            let receiver = receiver.split('.').next().unwrap_or(&receiver).to_string();
            let typed_receiver = variable_types.get(&receiver).cloned().unwrap_or(receiver);
            out.push(CallFact {
                caller: caller.clone(),
                name,
                arity,
                qualifier: CallQualifier::Receiver(typed_receiver),
                scope: scope.clone(),
                line: Self::line(node),
            });
        }
        for child in Self::named_children(node) {
            if child.id() != node.id()
                && matches!(
                    child.kind(),
                    "classlessPredicate" | "memberPredicate" | "charpred" | "module" | "dataclass"
                )
            {
                continue;
            }
            self.collect_calls(child, caller, scope, variable_types, depth + 1, out);
        }
    }

    fn find_lexical_predicate(&self, scope: &str, name: &str, arity: usize) -> Option<NodeId> {
        for candidate_scope in lexical_scopes(scope) {
            let key = (candidate_scope, name.to_ascii_lowercase(), arity);
            if let Some(matches) = self.predicates.get(&key) {
                if matches.len() == 1 {
                    return matches.first().cloned();
                }
                return None;
            }
        }
        None
    }

    fn find_module_predicate(
        &self,
        scope: &str,
        module: &str,
        name: &str,
        arity: usize,
    ) -> Option<NodeId> {
        for candidate_scope in lexical_scopes(scope) {
            let module_scope = format!("{candidate_scope}::{module}");
            let key = (module_scope, name.to_ascii_lowercase(), arity);
            if let Some(matches) = self.predicates.get(&key) {
                if matches.len() == 1 {
                    return matches.first().cloned();
                }
                return None;
            }
        }
        None
    }

    fn find_member_predicate(
        &self,
        scope: &Scope,
        receiver: &str,
        name: &str,
        arity: usize,
    ) -> Option<NodeId> {
        let class_name = if receiver == "this" || receiver == "super" {
            scope.class_name.as_deref()?
        } else {
            receiver.rsplit("::").next().unwrap_or(receiver)
        };
        let key = (
            class_name.to_ascii_lowercase(),
            name.to_ascii_lowercase(),
            arity,
        );
        self.methods
            .get(&key)
            .filter(|matches| matches.len() == 1)
            .and_then(|matches| matches.first().cloned())
    }

    fn recover_signature_declarations(&mut self) -> usize {
        let masked = mask_ql_comments(self.source);
        let text = String::from_utf8_lossy(&masked);
        let newlines: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
        let mut recovered = 0usize;
        for cap in SIGNATURE_RE.captures_iter(&text) {
            let kind = &cap[1];
            let name = &cap[2];
            let tail = cap.get(3).map(|m| m.as_str()).unwrap_or("");
            let arity = if kind == "predicate" {
                parameter_arity(tail)
            } else {
                None
            };
            let start = cap.get(0).expect("signature regex full match").start();
            let line = newlines.partition_point(|&nl| nl < start) + 1;
            let id = NodeId(make_id(&[
                &self.file_scope,
                "signature",
                kind,
                &name.to_ascii_lowercase(),
                &arity.unwrap_or(0).to_string(),
                &line.to_string(),
            ]));
            if self.b.seen.contains(&id) {
                continue;
            }
            self.b.add_node(
                id.clone(),
                if kind == "predicate" {
                    format!("{name}()")
                } else {
                    name.to_string()
                },
                line,
            );
            if let Some(node) = self.b.nodes.iter_mut().find(|node| node.id == id) {
                node.set_kind(match kind {
                    "class" => NodeKind::Interface,
                    "module" => NodeKind::Module,
                    "predicate" => NodeKind::Function,
                    _ => NodeKind::Other,
                });
                node.set_visibility(
                    if cap
                        .get(0)
                        .is_some_and(|m| m.as_str().split_whitespace().any(|w| w == "private"))
                    {
                        Visibility::Private
                    } else {
                        Visibility::Public
                    },
                );
                node.extra.insert("_language".into(), json!("ql"));
                node.extra
                    .insert("ql_declaration".into(), json!("signature"));
                if let Some(arity) = arity {
                    node.extra.insert("ql_arity".into(), json!(arity));
                }
            }
            self.b.add_edge(
                self.file_nid.clone(),
                id,
                "contains",
                line,
                Some("signature"),
            );
            recovered += 1;
        }
        recovered
    }
}

fn compact_ql_name(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_whitespace()).collect()
}

fn ql_module_leaf(spec: &str) -> String {
    let before_args = spec.split('<').next().unwrap_or(spec);
    before_args
        .rsplit(['.', ':'])
        .find(|part| !part.is_empty())
        .unwrap_or(before_args)
        .to_string()
}

fn lexical_scopes(scope: &str) -> Vec<String> {
    let mut scopes = Vec::new();
    let mut current = scope;
    loop {
        scopes.push(current.to_string());
        if let Some((parent, _)) = current.rsplit_once("::") {
            current = parent;
        } else {
            break;
        }
    }
    scopes
}

fn is_ql_primitive(name: &str) -> bool {
    matches!(
        name,
        "boolean" | "date" | "float" | "int" | "string" | "predicate"
    ) || name.starts_with('@')
}

/// The published QL grammar predates CodeQL's `overlay[mode]` annotation. It is
/// written on its own line immediately before the declaration it decorates.
/// Mask only those complete directive lines, preserving byte offsets and
/// newlines so every tree-sitter span still addresses the original source. The
/// modes are retained on the file node instead of being silently discarded.
fn mask_overlay_annotations(source: &[u8]) -> (Cow<'_, [u8]>, Vec<String>) {
    if !source
        .windows(b"overlay[".len())
        .any(|window| window == b"overlay[")
    {
        return (Cow::Borrowed(source), Vec::new());
    }

    // A block comment can contain a line that looks exactly like a directive.
    // Search the comment-masked view, then apply byte-for-byte masking to a
    // clone of the original source only when a live annotation is found.
    let active_source = mask_ql_comments(source);
    let mut masked: Option<Vec<u8>> = None;
    let mut modes = Vec::new();
    let mut start = 0usize;

    while start < active_source.len() {
        let line_end = active_source[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| start + offset)
            .unwrap_or(active_source.len());
        let content_end = if line_end > start && active_source[line_end - 1] == b'\r' {
            line_end - 1
        } else {
            line_end
        };
        let line = String::from_utf8_lossy(&active_source[start..content_end]);
        let trimmed = line.trim();
        if let Some(mode) = trimmed
            .strip_prefix("overlay[")
            .and_then(|rest| rest.strip_suffix(']'))
            .filter(|mode| {
                !mode.is_empty()
                    && mode
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'?'))
            })
        {
            modes.push(mode.to_string());
            let output = masked.get_or_insert_with(|| source.to_vec());
            output[start..content_end].fill(b' ');
        }

        if line_end == active_source.len() {
            break;
        }
        start = line_end + 1;
    }

    match masked {
        Some(masked) => (Cow::Owned(masked), modes),
        None => (Cow::Borrowed(source), modes),
    }
}

fn mask_ql_comments(source: &[u8]) -> Vec<u8> {
    let mut out = source.to_vec();
    let mut i = 0usize;
    let mut block = false;
    while i < out.len() {
        if block {
            if i + 1 < out.len() && out[i] == b'*' && out[i + 1] == b'/' {
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                block = false;
            } else {
                if out[i] != b'\n' && out[i] != b'\r' {
                    out[i] = b' ';
                }
                i += 1;
            }
        } else if i + 1 < out.len() && out[i] == b'/' && out[i + 1] == b'*' {
            out[i] = b' ';
            out[i + 1] = b' ';
            i += 2;
            block = true;
        } else if i + 1 < out.len() && out[i] == b'/' && out[i + 1] == b'/' {
            out[i] = b' ';
            out[i + 1] = b' ';
            i += 2;
            while i < out.len() && out[i] != b'\n' && out[i] != b'\r' {
                out[i] = b' ';
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    out
}

fn parameter_arity(tail: &str) -> Option<usize> {
    let open = tail.find('(')?;
    let close = tail[open + 1..].find(')')? + open + 1;
    let params = tail[open + 1..close].trim();
    if params.is_empty() {
        return Some(0);
    }
    let mut depth = 0usize;
    let mut commas = 0usize;
    for ch in params.chars() {
        match ch {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => commas += 1,
            _ => {}
        }
    }
    Some(commas + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = br#"
import semmle.code.java.DataFlow as DF

module Helpers {
  predicate helper(int x) { x > 0 }
}

class Source extends DataFlow::Node instanceof Expr {
  string label;
  string getName() { result = helper(1) }
  predicate isSource() { this.getName() != "" }
}

newtype Direction = Incoming(int n) or Outgoing(int n)

int top(int x) {
  result = Helpers::helper(x) and
  exists(Source s | s.isSource())
}

from Source s
where top(1) = 1 and DF::flow(s, s)
select s, s.getName()
"#;

    fn extract() -> ExtractionResult {
        extract_ql_source("java/ql/lib/example/Test.qll", SAMPLE)
    }

    fn node<'a>(r: &'a ExtractionResult, label: &str) -> &'a synaptic_core::Node {
        r.nodes
            .iter()
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("missing {label:?}; nodes={:?}", r.nodes))
    }

    fn has_edge(r: &ExtractionResult, from: &str, relation: &str, to: &str) -> bool {
        let ids = |label: &str| {
            r.nodes
                .iter()
                .filter(|n| n.label == label)
                .map(|n| n.id.clone())
                .collect::<HashSet<_>>()
        };
        let from_ids = ids(from);
        let to_ids = ids(to);
        r.edges.iter().any(|edge| {
            edge.relation == relation
                && from_ids.contains(&edge.source)
                && to_ids.contains(&edge.target)
        })
    }

    #[test]
    fn extracts_ql_structure_and_enrichment() {
        let r = extract();
        assert_eq!(node(&r, "Helpers").kind(), Some(NodeKind::Module));
        assert_eq!(node(&r, "Source").kind(), Some(NodeKind::Class));
        assert_eq!(node(&r, "Direction").kind(), Some(NodeKind::Enum));
        assert_eq!(node(&r, "label").kind(), Some(NodeKind::Field));
        assert_eq!(node(&r, ".getName()").kind(), Some(NodeKind::Method));
        let top = node(&r, "top()");
        assert_eq!(top.kind(), Some(NodeKind::Function));
        assert_eq!(top.signature().expect("QL signature").arity(), 1);
        assert_eq!(top.extra.get("ql_arity"), Some(&json!(1)));
    }

    #[test]
    fn extracts_import_heritage_types_and_calls() {
        let r = extract();
        assert!(r.edges.iter().any(|e| {
            e.relation == "imports_from"
                && e.context.as_deref() == Some("ql_import")
                && r.nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .is_some_and(|n| n.label == "semmle.code.java.DataFlow")
        }));
        assert!(has_edge(&r, "Source", "inherits", "Node"));
        assert!(has_edge(&r, "top()", "calls", "helper()"));
        assert!(has_edge(&r, "top()", "calls", ".isSource()"));
        assert!(has_edge(&r, "Test.qll", "calls", "top()"));
        assert!(
            r.raw_calls
                .iter()
                .any(|call| call.callee == "ql:DF::flow/2"),
            "{:?}",
            r.raw_calls
        );
    }

    #[test]
    fn overloads_have_distinct_ids_and_arity_aware_calls() {
        let r = extract_ql_source(
            "Overloads.qll",
            b"predicate p() { any() }\npredicate p(int x) { x > 0 }\npredicate q() { p() }\n",
        );
        let p: Vec<_> = r.nodes.iter().filter(|n| n.label == "p()").collect();
        assert_eq!(p.len(), 2);
        assert_ne!(p[0].id, p[1].id);
        let q = node(&r, "q()");
        let called: Vec<_> = r
            .edges
            .iter()
            .filter(|e| e.source == q.id && e.relation == "calls")
            .filter_map(|e| r.nodes.iter().find(|n| n.id == e.target))
            .collect();
        assert_eq!(called.len(), 1);
        assert_eq!(called[0].extra.get("ql_arity"), Some(&json!(0)));
    }

    #[test]
    fn recovers_signature_declarations_ignored_by_grammar() {
        let r = extract_ql_source(
            "Signatures.qll",
            b"signature module InputSig<T T> {\n  signature class NodeSig;\n}\nprivate signature predicate relevantNodeSig(NodeSig n);\n",
        );
        let recovered: Vec<_> = r
            .nodes
            .iter()
            .filter(|n| n.extra.get("ql_declaration") == Some(&json!("signature")))
            .collect();
        assert_eq!(recovered.len(), 3, "{:?}", r.nodes);
        let pred = node(&r, "relevantNodeSig()");
        assert_eq!(pred.visibility(), Some(Visibility::Private));
        assert_eq!(pred.extra.get("ql_arity"), Some(&json!(1)));
    }

    #[test]
    fn comments_do_not_create_signature_nodes() {
        let r = extract_ql_source(
            "NoSignatures.qll",
            b"// signature class Fake;\n/*\nsignature predicate nope(int x);\n*/\npredicate real() { any() }\n",
        );
        assert!(!r.nodes.iter().any(|n| n.label == "Fake"));
        assert!(!r.nodes.iter().any(|n| n.label == "nope()"));
        assert!(r.nodes.iter().any(|n| n.label == "real()"));
    }

    #[test]
    fn overlay_annotations_are_masked_but_retained_as_file_metadata() {
        let source =
            b"overlay[local?]\nmodule;\n\n/*\noverlay[commented]\n*/\noverlay[global]\nprivate predicate active() { 1 = 1 }\n";
        let result = extract_ql_source("javascript/ql/lib/Overlay.qll", source);
        let file = result
            .nodes
            .iter()
            .find(|node| node.id == file_node_id("javascript/ql/lib/Overlay.qll"))
            .expect("file node");
        assert_eq!(file.extra.get("ql_parse_errors"), Some(&json!(false)));
        assert_eq!(file.extra.get("ql_overlay_directives"), Some(&json!(2)));
        assert_eq!(
            file.extra.get("ql_overlay_modes"),
            Some(&json!(["global", "local?"]))
        );
        assert!(
            result
                .nodes
                .iter()
                .any(|node| node.label == "active()"
                    && node.visibility() == Some(Visibility::Private))
        );
    }
}
