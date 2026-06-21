//! PHP extractor — Bucket A (declarative `LanguageConfig`).
//!
//! Classes/interfaces/traits/enums → bare name; methods/functions → `.name()` /
//! `name()`; `use A\B\C` → `imports` to the tail; `extends`/`implements` →
//! `inherits`/`implements`; property/parameter/return types → `references`;
//! `$this->m()` / `foo()` → `calls`.

#[cfg(feature = "lang-php")]
use std::collections::HashSet;

#[cfg(feature = "lang-php")]
use synaptic_core::{make_id, FileType, NodeId};
#[cfg(feature = "lang-php")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-php")]
use crate::common::Builder;
#[cfg(feature = "lang-php")]
use crate::config::{HeritageStyle, ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-php")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-php")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-php")]
use crate::walker::extract_with_config;

/// Common PHP library functions / constructs skipped as in-file call targets.
#[cfg(feature = "lang-php")]
const PHP_BUILTINS: &[&str] = &[
    "echo",
    "print",
    "isset",
    "empty",
    "unset",
    "count",
    "strlen",
    "var_dump",
    "printf",
    "sprintf",
    "implode",
    "explode",
    "in_array",
    "array_map",
    "array_filter",
    "array_keys",
    "array_values",
    "array_merge",
    "str_replace",
    "preg_match",
    "preg_replace",
    "json_encode",
    "json_decode",
    "define",
    "defined",
    "function_exists",
    "class_exists",
    "is_array",
    "is_string",
    "is_null",
    "is_int",
    "gettype",
    "intval",
    "strval",
    "trim",
    "ucfirst",
    "strtolower",
    "strtoupper",
    "number_format",
    "date",
    "time",
];

/// The PHP `LanguageConfig`. Calls come in several node shapes
/// (`function_call_expression` uses `function`; `member_call_expression`/
/// `scoped_call_expression` use `name`) — the walker's `name`-field fallback
/// handles the latter.
#[cfg(feature = "lang-php")]
pub fn php_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_php::LANGUAGE_PHP.into(),
        class_types: &[
            "class_declaration",
            "interface_declaration",
            "trait_declaration",
            "enum_declaration",
        ],
        function_types: &["method_declaration", "function_definition"],
        call_types: &[
            "function_call_expression",
            "member_call_expression",
            "scoped_call_expression",
            "nullsafe_member_call_expression",
        ],
        name_field: "name",
        body_field: "body",
        call_function_field: "function",
        call_accessor_node_types: &[],
        call_accessor_field: "",
        function_boundary_types: &["method_declaration", "function_definition"],
        superclasses_field: None,
        decorated_types: &[],
        builtins: PHP_BUILTINS,
        import_types: &["namespace_use_declaration"],
        import_style: Some(ImportStyle::Php),
        type_ref_style: Some(TypeRefStyle::Php),
        heritage_style: Some(HeritageStyle::Php),
        constructor_call_type: None,
        body_kinds: &[],
    }
}

/// Extract a PHP source file already in memory. The generic config-driven walk
/// produces the structural graph; a second Laravel-framework pass adds the
/// framework-aware edges (`bound_to`, `uses_config`, `listened_by`).
#[cfg(feature = "lang-php")]
pub fn extract_php_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut result = extract_with_config(path, source, &php_config());
    let fw = php_framework_pass(path, source);
    result.nodes.extend(fw.nodes);
    result.edges.extend(fw.edges);
    result
}

/// Laravel container-method allowlist for `bound_to`.
#[cfg(feature = "lang-php")]
const CONTAINER_BIND_METHODS: &[&str] = &["bind", "singleton", "scoped", "instance"];

/// A second tree-sitter pass over PHP that emits Laravel/PHP framework edges:
/// - `config('x.y')` → caller `--uses_config-->` config-key concept
/// - `$app->bind(A::class, B::class)` → A `--bound_to-->` B
/// - `protected $listen = [E::class => [L::class]]` → E `--listened_by-->` L
/// - `Foo::$bar` → caller `--uses_static_prop-->` Foo
/// - `Foo::BAR` → caller `--references_constant-->` Foo
///
/// Class references resolve to the in-file class node (`make_id(stem, Name)`,
/// matching the generic walker) when defined locally, else to an external stub.
/// Covers config, container bind, static-property uses, constant references, and
/// event-listener registration.
#[cfg(feature = "lang-php")]
fn php_framework_pass(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
        .is_err()
    {
        return ExtractionResult::default();
    }
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let mut fw = PhpFw {
        src: source,
        stem: file_stem(path),
        file_nid: file_node_id(path),
        in_file_classes: HashSet::new(),
        b: Builder::new(path),
    };
    fw.collect_classes(tree.root_node(), 0);
    fw.walk(tree.root_node(), None, None, 0);
    fw.b.into_result()
}

#[cfg(feature = "lang-php")]
struct PhpFw<'a> {
    src: &'a [u8],
    stem: String,
    file_nid: NodeId,
    in_file_classes: HashSet<String>,
    b: Builder,
}

#[cfg(feature = "lang-php")]
impl<'tree> PhpFw<'_> {
    fn text(&self, n: TsNode<'tree>) -> String {
        n.utf8_text(self.src).unwrap_or("").to_string()
    }

    fn children(n: TsNode<'tree>) -> Vec<TsNode<'tree>> {
        let mut c = n.walk();
        n.children(&mut c).collect()
    }

    fn name_of(&self, n: TsNode<'tree>) -> Option<String> {
        n.child_by_field_name("name")
            .map(|x| self.text(x).trim_start_matches('$').to_string())
    }

    /// First pass: every locally-declared class/interface/trait/enum name.
    fn collect_classes(&mut self, node: TsNode<'tree>, depth: usize) {
        if depth >= 4000 {
            return;
        }
        if matches!(
            node.kind(),
            "class_declaration"
                | "interface_declaration"
                | "trait_declaration"
                | "enum_declaration"
        ) {
            if let Some(name) = self.name_of(node) {
                self.in_file_classes.insert(name);
            }
        }
        for c in Self::children(node) {
            self.collect_classes(c, depth + 1);
        }
    }

    /// Simple class name from a `X::class` / `\Ns\X::class` constant access.
    fn class_ref_name(&self, n: TsNode<'tree>) -> Option<String> {
        if n.kind() != "class_constant_access_expression" {
            return None;
        }
        let t = self.text(n);
        let (lhs, rhs) = t.rsplit_once("::")?;
        if !rhs.trim().eq_ignore_ascii_case("class") {
            return None; // `Foo::CONST`, not `Foo::class`
        }
        let tail = lhs.rsplit(['\\']).next().unwrap_or(lhs).trim();
        if tail.is_empty() {
            None
        } else {
            Some(tail.to_string())
        }
    }

    /// Resolve a class name to the in-file node id, else an external stub id
    /// (creating the stub) — mirroring the generic walker's resolution.
    fn class_node(&mut self, name: &str) -> NodeId {
        if self.in_file_classes.contains(name) {
            return NodeId(make_id(&[&self.stem, name]));
        }
        let id = NodeId(make_id(&[name]));
        self.b.add_external_node(id.clone(), name.to_string());
        id
    }

    fn walk(
        &mut self,
        node: TsNode<'tree>,
        class_id: Option<NodeId>,
        func_id: Option<NodeId>,
        depth: usize,
    ) {
        if depth >= 4000 {
            return;
        }
        match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration" => {
                let cid = self
                    .name_of(node)
                    .map(|n| NodeId(make_id(&[&self.stem, &n])));
                for c in Self::children(node) {
                    self.walk(c, cid.clone(), None, depth + 1);
                }
                return;
            }
            "method_declaration" => {
                let fid = self.name_of(node).map(|n| {
                    let base = class_id.as_ref().map(NodeId::as_str).unwrap_or(&self.stem);
                    NodeId(make_id(&[base, &n]))
                });
                for c in Self::children(node) {
                    self.walk(c, class_id.clone(), fid.clone(), depth + 1);
                }
                return;
            }
            "function_definition" => {
                let fid = self
                    .name_of(node)
                    .map(|n| NodeId(make_id(&[&self.stem, &n])));
                for c in Self::children(node) {
                    self.walk(c, class_id.clone(), fid.clone(), depth + 1);
                }
                return;
            }
            "function_call_expression" => self.handle_config(node, func_id.as_ref()),
            "member_call_expression" => self.handle_bind(node),
            "property_declaration" => self.handle_listen(node),
            // `Foo::$bar` -> uses_static_prop; `Foo::BAR` -> references_constant
            // (`Foo::class` is skipped, handled by bind/listen).
            "scoped_property_access_expression" => {
                self.handle_member_ref(node, func_id.as_ref(), "uses_static_prop", "static_prop")
            }
            "class_constant_access_expression" => {
                let is_class_kw = self
                    .text(node)
                    .rsplit_once("::")
                    .is_some_and(|(_, rhs)| rhs.trim().eq_ignore_ascii_case("class"));
                if !is_class_kw {
                    self.handle_member_ref(
                        node,
                        func_id.as_ref(),
                        "references_constant",
                        "class_constant",
                    );
                }
            }
            _ => {}
        }
        for c in Self::children(node) {
            self.walk(c, class_id.clone(), func_id.clone(), depth + 1);
        }
    }

    /// The simple class name on the left of a `::` access (`\Ns\Foo::x` → `Foo`),
    /// skipping dynamic (`$obj::`) and `self`/`static`/`parent`.
    fn scope_class_name(&self, n: TsNode<'tree>) -> Option<String> {
        let t = self.text(n);
        let (lhs, _) = t.split_once("::")?;
        let lhs = lhs.trim();
        if lhs.starts_with('$') {
            return None;
        }
        let tail = lhs.rsplit(['\\']).next().unwrap_or(lhs).trim();
        if tail.is_empty() || matches!(tail, "self" | "static" | "parent") {
            None
        } else {
            Some(tail.to_string())
        }
    }

    /// `caller --relation--> Class` for a static-prop / class-constant access.
    fn handle_member_ref(
        &mut self,
        node: TsNode<'tree>,
        func_id: Option<&NodeId>,
        relation: &str,
        context: &str,
    ) {
        let Some(name) = self.scope_class_name(node) else {
            return;
        };
        let line = node.start_position().row + 1;
        let source = func_id.cloned().unwrap_or_else(|| self.file_nid.clone());
        let tgt = self.class_node(&name);
        if source != tgt {
            self.b.add_edge(source, tgt, relation, line, Some(context));
        }
    }

    /// `config('app.name')` → caller `--uses_config-->` config-key **concept**
    /// `app`. We emit a concept node rather than resolving the segment to the
    /// actual `config/app.php` file node by label (which would need a cross-file
    /// label pass); the concept node groups all `config('app.*')` usages without
    /// that pass and is what the test pins. (See roadmap I-28 notes.)
    fn handle_config(&mut self, node: TsNode<'tree>, func_id: Option<&NodeId>) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        if !self.text(func).trim().eq_ignore_ascii_case("config") {
            return;
        }
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        let Some(first) = Self::children(args)
            .into_iter()
            .find(|c| matches!(c.kind(), "argument"))
            .and_then(|a| Self::children(a).into_iter().find(TsNode::is_named))
        else {
            return;
        };
        if !matches!(first.kind(), "string" | "encapsed_string") {
            return;
        }
        let raw = self.text(first);
        let key = raw.trim_matches(|c| c == '"' || c == '\'');
        let segment = key.split('.').next().unwrap_or(key).trim();
        if segment.is_empty() {
            return;
        }
        let line = node.start_position().row + 1;
        let cfg = NodeId(make_id(&["config", segment]));
        self.b
            .add_node_typed(cfg.clone(), segment.to_string(), FileType::Concept, line);
        let source = func_id.cloned().unwrap_or_else(|| self.file_nid.clone());
        self.b
            .add_edge(source, cfg, "uses_config", line, Some("config"));
    }

    /// `$this->app->bind(Contract::class, Impl::class)` → Contract `bound_to` Impl.
    fn handle_bind(&mut self, node: TsNode<'tree>) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        if !CONTAINER_BIND_METHODS.contains(&name.trim()) {
            return;
        }
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        let class_args: Vec<String> = Self::children(args)
            .into_iter()
            .filter(|c| c.kind() == "argument")
            .filter_map(|a| {
                Self::children(a)
                    .into_iter()
                    .find_map(|c| self.class_ref_name(c))
            })
            .collect();
        if class_args.len() != 2 {
            return; // requires exactly two ::class args
        }
        let line = node.start_position().row + 1;
        let contract = self.class_node(&class_args[0]);
        let implementation = self.class_node(&class_args[1]);
        if contract != implementation {
            self.b.add_edge(
                contract,
                implementation,
                "bound_to",
                line,
                Some("container_binding"),
            );
        }
    }

    /// `protected $listen = [Event::class => [Listener::class, ...]]` →
    /// Event `--listened_by-->` Listener (one edge per listener).
    fn handle_listen(&mut self, node: TsNode<'tree>) {
        // Property name (any `property_element`'s variable_name) must be a
        // listener map.
        let is_listen_map = Self::descendants(node).into_iter().any(|d| {
            d.kind() == "variable_name"
                && matches!(self.text(d).trim_start_matches('$'), "listen" | "subscribe")
        });
        if !is_listen_map {
            return;
        }
        let Some(map) = Self::descendants(node)
            .into_iter()
            .find(|d| d.kind() == "array_creation_expression")
        else {
            return;
        };
        let line = node.start_position().row + 1;
        for elem in Self::children(map)
            .into_iter()
            .filter(|c| matches!(c.kind(), "array_element_initializer"))
        {
            let parts: Vec<TsNode> = Self::children(elem)
                .into_iter()
                .filter(TsNode::is_named)
                .collect();
            // `Event::class => [ Listener::class, ... ]`
            if parts.len() != 2 {
                continue;
            }
            let Some(event) = self.class_ref_name(parts[0]) else {
                continue;
            };
            let listeners: Vec<String> = if parts[1].kind() == "array_creation_expression" {
                Self::children(parts[1])
                    .into_iter()
                    .filter(|c| c.kind() == "array_element_initializer")
                    .filter_map(|e| {
                        Self::children(e)
                            .into_iter()
                            .find_map(|c| self.class_ref_name(c))
                    })
                    .collect()
            } else {
                self.class_ref_name(parts[1]).into_iter().collect()
            };
            let ev = self.class_node(&event);
            for l in listeners {
                let ln = self.class_node(&l);
                if ev != ln {
                    self.b
                        .add_edge(ev.clone(), ln, "listened_by", line, Some("event_listener"));
                }
            }
        }
    }

    fn descendants(node: TsNode<'tree>) -> Vec<TsNode<'tree>> {
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            out.push(n);
            for c in Self::children(n) {
                stack.push(c);
            }
        }
        out
    }
}

/// Read and extract a PHP file from disk.
#[cfg(feature = "lang-php")]
pub fn extract_php_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_php_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-php"))]
mod tests {
    use super::extract_php_source;
    use crate::result::ExtractionResult;
    use synaptic_core::Confidence;

    const SAMPLE: &[u8] = b"<?php\nnamespace App;\nuse Lib\\Base;\n\nclass Dog extends Animal implements Greeter {\n  private Leash $leash;\n  public function bark(Food $f): string { return $this->sound(); }\n  public function sound(): string { return \"woof\"; }\n}\n";

    fn extract() -> ExtractionResult {
        extract_php_source("src/Dog.php", SAMPLE)
    }

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let lbl = |id: &synaptic_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| (lbl(&e.source), lbl(&e.target)))
            .collect()
    }

    fn refs_with_ctx(r: &ExtractionResult) -> Vec<(String, String)> {
        r.edges
            .iter()
            .filter(|e| e.relation == "references")
            .map(|e| {
                let tgt = r
                    .nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| e.target.0.clone());
                (tgt, e.context.clone().unwrap_or_default())
            })
            .collect()
    }

    #[test]
    fn class_and_method_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"Dog".to_string()), "{ls:?}");
        assert!(ls.contains(&".bark()".to_string()));
        assert!(ls.contains(&".sound()".to_string()));
    }

    #[test]
    fn use_import_tail() {
        let imps = rels(&extract(), "imports");
        assert!(imps.iter().any(|(_, t)| t == "Base"), "{imps:?}");
    }

    #[test]
    fn extends_and_implements() {
        let r = extract();
        assert!(rels(&r, "inherits").contains(&("Dog".to_string(), "Animal".to_string())));
        assert!(rels(&r, "implements").contains(&("Dog".to_string(), "Greeter".to_string())));
    }

    #[test]
    fn property_and_param_type_references() {
        let refs = refs_with_ctx(&extract());
        assert!(
            refs.contains(&("Leash".to_string(), "field".to_string())),
            "{refs:?}"
        );
        assert!(refs.contains(&("Food".to_string(), "parameter_type".to_string())));
        // `string` is a primitive_type, not referenced.
        assert!(!refs.iter().any(|(t, _)| t == "string"));
    }

    #[test]
    fn member_call_resolves() {
        // bark() calls $this->sound()
        let calls = rels(&extract(), "calls");
        assert!(
            calls.contains(&(".bark()".to_string(), ".sound()".to_string())),
            "{calls:?}"
        );
    }

    #[test]
    fn structural_edges_extracted_confidence() {
        let r = extract();
        for e in &r.edges {
            if matches!(e.relation.as_str(), "contains" | "method" | "inherits") {
                assert_eq!(e.confidence, Confidence::Extracted, "edge {e:?}");
            }
        }
    }

    fn tgt_labels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let lbl = |id: &synaptic_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| (lbl(&e.source), lbl(&e.target)))
            .collect()
    }

    #[test]
    fn laravel_container_bind_is_bound_to() {
        let src = b"<?php\nclass AppServiceProvider {\n  public function register() {\n    $this->app->bind(PaymentContract::class, StripePayment::class);\n  }\n}\n";
        let r = extract_php_source("app/Providers/AppServiceProvider.php", src);
        let bound = tgt_labels(&r, "bound_to");
        assert!(
            bound.contains(&("PaymentContract".to_string(), "StripePayment".to_string())),
            "{bound:?}"
        );
    }

    #[test]
    fn laravel_config_helper_is_uses_config() {
        let src = b"<?php\nclass Mailer {\n  public function send() {\n    $host = config('mail.host');\n  }\n}\n";
        let r = extract_php_source("app/Mailer.php", src);
        let uses = tgt_labels(&r, "uses_config");
        // caller .send() uses the `mail` config namespace.
        assert!(uses.iter().any(|(_, t)| t == "mail"), "{uses:?}");
        assert!(uses.iter().any(|(s, _)| s == ".send()"), "{uses:?}");
        // the config key is a concept node.
        assert_eq!(
            r.nodes
                .iter()
                .find(|n| n.label == "mail")
                .map(|n| n.file_type),
            Some(synaptic_core::FileType::Concept)
        );
    }

    #[test]
    fn static_prop_and_class_constant_refs() {
        let src = b"<?php\nclass Svc {\n  public function run() {\n    $x = Config::$cache;\n    $y = Status::ACTIVE;\n    $z = Other::class;\n  }\n}\n";
        let r = extract_php_source("app/Svc.php", src);
        let stat = tgt_labels(&r, "uses_static_prop");
        assert!(stat.iter().any(|(_, t)| t == "Config"), "{stat:?}");
        let consts = tgt_labels(&r, "references_constant");
        assert!(consts.iter().any(|(_, t)| t == "Status"), "{consts:?}");
        // `Other::class` must NOT become a references_constant (it's a ::class ref).
        assert!(!consts.iter().any(|(_, t)| t == "Other"), "{consts:?}");
    }

    #[test]
    fn laravel_event_listener_map_is_listened_by() {
        let src = b"<?php\nclass EventServiceProvider {\n  protected $listen = [\n    OrderShipped::class => [\n      SendShipmentNotification::class,\n    ],\n  ];\n}\n";
        let r = extract_php_source("app/Providers/EventServiceProvider.php", src);
        let listened = tgt_labels(&r, "listened_by");
        assert!(
            listened.contains(&(
                "OrderShipped".to_string(),
                "SendShipmentNotification".to_string()
            )),
            "{listened:?}"
        );
    }
}
