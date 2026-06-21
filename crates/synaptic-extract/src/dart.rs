//! Dart extractor — custom walker (methods are deeply nested:
//! `method_declaration → signature → function_signature → name`).
//!
//! `class`/`mixin`/`extension` → name nodes; `extends`/`implements`/`with` →
//! `inherits`/`implements`/`mixes_in`; methods → `.name()` + param/return refs;
//! fields → `field` type refs; `import 'x.dart'` → `imports_from`; calls →
//! `calls`.

#[cfg(feature = "lang-dart")]
use std::collections::HashSet;
#[cfg(feature = "lang-dart")]
use std::sync::LazyLock;

#[cfg(feature = "lang-dart")]
use regex::Regex;
#[cfg(feature = "lang-dart")]
use synaptic_core::{make_id, FileType, NodeId};
#[cfg(feature = "lang-dart")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-dart")]
use crate::common::Builder;
#[cfg(feature = "lang-dart")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-dart")]
use crate::result::ExtractionResult;

const MAX_DEPTH: usize = 2000;

/// Extract a Dart source file already in memory.
#[cfg(feature = "lang-dart")]
pub fn extract_dart_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_dart::LANGUAGE.into())
        .expect("load tree-sitter-dart");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut ex = Dart {
        src: source,
        b: Builder::new(path),
        file_nid: file_nid.clone(),
        stem: file_stem(path),
        function_bodies: Vec::new(),
        scopes: Vec::new(),
    };
    ex.b.add_node(file_nid, filename, 1);
    ex.walk(tree.root_node(), 0);
    ex.run_call_pass();
    ex.framework_scan(source);
    ex.b.into_result()
}

// Flutter framework patterns (regex-based). Edges attach to the enclosing
// method/class (see `framework_scan`/`enclosing`). These are heuristics: `.add(`
// and `.of<>()` can match a non-Bloc collection (`list.add(Widget())`,
// `List.of<Item>()`); the `context` tag marks the intent.
#[cfg(feature = "lang-dart")]
static NAV_STRING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:go|push|goNamed|pushNamed|replace|replaceNamed)\s*\(\s*(?:context\s*,\s*)?['"]([^'"]+)['"]"#)
        .expect("valid nav-string regex")
});
// A route *object* is instantiated (`push(MaterialPageRoute(...))`,
// `push(ProfileScreen())`); the trailing `\(` requires a constructor call so a
// const-route field access (`go(ProfileRoute.path)`) is left to ROUTE_CONST and
// not double-counted.
#[cfg(feature = "lang-dart")]
static NAV_OBJECT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:push|replace|go)\s*\([^)]*\b([A-Z]\w*(?:Route|Screen|Page))\s*\(")
        .expect("valid nav-object regex")
});
#[cfg(feature = "lang-dart")]
static RIVERPOD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bref\s*\.\s*(?:watch|read|listen)\s*\(\s*([A-Za-z_]\w*)")
        .expect("valid riverpod regex")
});
#[cfg(feature = "lang-dart")]
static BLOC_WIDGET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:BlocBuilder|BlocListener|BlocConsumer|BlocProvider|BlocSelector)\s*<\s*([A-Za-z_]\w*)")
        .expect("valid bloc-widget regex")
});
// Const-route navigation: `context.go(Routes.profile)`, a `Capitalized.field`
// argument (not a string, not an instantiation). String routes are NAV_STRING.
#[cfg(feature = "lang-dart")]
static ROUTE_CONST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:go|push|goNamed|pushNamed|replace|replaceNamed)\s*\(\s*(?:context\s*,\s*)?([A-Z]\w*(?:\.\w+)+)\s*[,)]")
        .expect("valid route-const regex")
});
// Bloc: event registration `on<MyEvent>(...)`.
#[cfg(feature = "lang-dart")]
static BLOC_ON_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bon\s*<\s*(\w+)\s*>\s*\(").expect("valid bloc-on regex"));
// Bloc: state emission `emit(MyState(...))` (PascalCase state).
#[cfg(feature = "lang-dart")]
static BLOC_EMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bemit\s*\(\s*(?:const\s+)?([A-Z]\w*)").expect("valid bloc-emit regex")
});
// Bloc: event dispatch `bloc.add(MyEvent())` / `.add(MyEvent(`.
#[cfg(feature = "lang-dart")]
static BLOC_ADD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\.add\s*\(\s*(?:const\s+)?([A-Z]\w*)\s*\(").expect("valid bloc-add regex")
});
// Bloc / provider type lookup `context.read<MyBloc>()` / `.of<T>()`.
#[cfg(feature = "lang-dart")]
static BLOC_LOOKUP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:read|watch|select|of)\s*<\s*([A-Z]\w*)\s*>\s*\(")
        .expect("valid bloc-lookup regex")
});

#[cfg(feature = "lang-dart")]
fn line_at(src: &str, byte: usize) -> usize {
    src.get(..byte)
        .map(|s| s.bytes().filter(|&b| b == b'\n').count() + 1)
        .unwrap_or(1)
}

/// Read and extract a Dart file from disk.
#[cfg(feature = "lang-dart")]
pub fn extract_dart_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_dart_source(&path_str, &source))
}

#[cfg(feature = "lang-dart")]
struct Dart<'a, 'tree> {
    src: &'a [u8],
    b: Builder,
    file_nid: NodeId,
    stem: String,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
    /// `(start_byte, end_byte, node)` for each class + method, so a framework
    /// regex match can attach to its *enclosing* method/class (innermost wins),
    /// not just the file — per-function/class scanning.
    scopes: Vec<(usize, usize, NodeId)>,
}

#[cfg(feature = "lang-dart")]
impl<'tree> Dart<'_, 'tree> {
    fn text(&self, n: TsNode<'tree>) -> String {
        n.utf8_text(self.src).unwrap_or("").to_string()
    }

    fn line(n: TsNode<'tree>) -> usize {
        n.start_position().row + 1
    }

    fn children(n: TsNode<'tree>) -> Vec<TsNode<'tree>> {
        let mut c = n.walk();
        n.children(&mut c).collect()
    }

    /// All `type_identifier` names under a `type`/`return_type` subtree.
    fn type_names(&self, node: TsNode<'tree>) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "type_identifier" {
                let t = self.text(n);
                if !t.is_empty() {
                    out.push(t);
                }
            }
            for c in Self::children(n) {
                stack.push(c);
            }
        }
        out
    }

    fn walk(&mut self, node: TsNode<'tree>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        match node.kind() {
            "import_or_export" => self.handle_import(node),
            "class_declaration" | "mixin_declaration" | "extension_declaration" => {
                self.handle_class(node)
            }
            _ => {
                for c in Self::children(node) {
                    self.walk(c, depth + 1);
                }
            }
        }
    }

    fn handle_import(&mut self, node: TsNode<'tree>) {
        // import_or_export descends to a string_literal (the uri).
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "string_literal" {
                let raw = self.text(n);
                let uri = raw.trim_matches(|c| c == '"' || c == '\'');
                let last = uri.rsplit(['/', ':']).next().unwrap_or(uri);
                let base = last.strip_suffix(".dart").unwrap_or(last);
                if !base.is_empty() {
                    let tgt = NodeId(make_id(&["dart", "pkg", base]));
                    self.b.add_external_node(tgt.clone(), base.to_string());
                    self.b.add_edge(
                        self.file_nid.clone(),
                        tgt,
                        "imports_from",
                        Self::line(node),
                        Some("import"),
                    );
                }
                return;
            }
            for c in Self::children(n) {
                stack.push(c);
            }
        }
    }

    fn handle_class(&mut self, node: TsNode<'tree>) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let line = Self::line(node);
        let class_nid = NodeId(make_id(&[&self.stem, &name]));
        self.b.add_node(class_nid.clone(), name, line);
        self.b.add_edge(
            self.file_nid.clone(),
            class_nid.clone(),
            "contains",
            line,
            None,
        );
        self.scopes
            .push((node.start_byte(), node.end_byte(), class_nid.clone()));

        for child in Self::children(node) {
            match child.kind() {
                "superclass" => {
                    for n in self.type_names(child) {
                        self.link(&class_nid, &n, line, "inherits");
                    }
                }
                "interfaces" => {
                    for n in self.type_names(child) {
                        self.link(&class_nid, &n, line, "implements");
                    }
                }
                "mixins" => {
                    for n in self.type_names(child) {
                        self.link(&class_nid, &n, line, "mixes_in");
                    }
                }
                _ => {}
            }
        }
        if let Some(body) = node.child_by_field_name("body") {
            for member in Self::children(body) {
                self.handle_member(member, &class_nid);
            }
        }
    }

    fn handle_member(&mut self, member: TsNode<'tree>, class_nid: &NodeId) {
        // class_member wraps a declaration/method_declaration.
        for inner in Self::children(member) {
            match inner.kind() {
                "method_declaration" => self.handle_method(inner, class_nid),
                "declaration" => {
                    // a field: `Type name;` gives a field type reference.
                    if let Some(ty) = Self::children(inner)
                        .into_iter()
                        .find(|c| c.kind() == "type")
                    {
                        let line = Self::line(inner);
                        for n in self.type_names(ty) {
                            let tgt = self.b.ensure_named_node(&n, &self.stem, line);
                            if &tgt != class_nid {
                                self.b.add_edge(
                                    class_nid.clone(),
                                    tgt,
                                    "references",
                                    line,
                                    Some("field"),
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_method(&mut self, method: TsNode<'tree>, class_nid: &NodeId) {
        let Some(sig) = method.child_by_field_name("signature") else {
            return;
        };
        let Some(fsig) = Self::children(sig)
            .into_iter()
            .find(|c| c.kind() == "function_signature")
        else {
            return;
        };
        let Some(name_node) = fsig.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let line = Self::line(method);
        let m = NodeId(make_id(&[class_nid.as_str(), &name]));
        self.b.add_node(m.clone(), format!(".{name}()"), line);
        self.b
            .add_edge(class_nid.clone(), m.clone(), "method", line, None);
        self.scopes
            .push((method.start_byte(), method.end_byte(), m.clone()));

        // Parameter + return type references.
        if let Some(params) = fsig.child_by_field_name("parameters") {
            for p in Self::children(params)
                .into_iter()
                .filter(|c| c.kind() == "formal_parameter")
            {
                if let Some(ty) = Self::children(p).into_iter().find(|c| c.kind() == "type") {
                    for n in self.type_names(ty) {
                        self.ref_type(&m, &n, "parameter_type", line);
                    }
                }
            }
        }
        if let Some(ret) = fsig.child_by_field_name("return_type") {
            for n in self.type_names(ret) {
                self.ref_type(&m, &n, "return_type", line);
            }
        }
        if let Some(body) = method.child_by_field_name("body") {
            self.function_bodies.push((m, body));
        }
    }

    fn ref_type(&mut self, owner: &NodeId, name: &str, ctx: &str, line: usize) {
        let tgt = self.b.ensure_named_node(name, &self.stem, line);
        if &tgt != owner {
            self.b
                .add_edge(owner.clone(), tgt, "references", line, Some(ctx));
        }
    }

    fn link(&mut self, owner: &NodeId, name: &str, line: usize, relation: &str) {
        if name.is_empty() {
            return;
        }
        let tgt = self.resolve_ref(name);
        self.b.add_edge(owner.clone(), tgt, relation, line, None);
    }

    /// Resolve a referenced symbol name to its in-file node id, else an external
    /// stub (mirroring `link`/`ensure_named_node`).
    fn resolve_ref(&mut self, name: &str) -> NodeId {
        let local = NodeId(make_id(&[&self.stem, name]));
        if self.b.seen.contains(&local) {
            return local;
        }
        let global = NodeId(make_id(&[name]));
        self.b.add_external_node(global.clone(), name.to_string());
        global
    }

    /// The innermost enclosing method/class node for a byte offset, else the file
    /// node — so a framework edge originates at the function/class that contains
    /// the call, not just the file.
    fn enclosing(&self, offset: usize) -> NodeId {
        self.scopes
            .iter()
            .filter(|(s, e, _)| *s <= offset && offset < *e)
            .min_by_key(|(s, e, _)| e - s)
            .map(|(_, _, id)| id.clone())
            .unwrap_or_else(|| self.file_nid.clone())
    }

    /// Emit `relation` edges from the enclosing scope to `resolve_ref(capture[1])`
    /// for every match of `re` (skipping self-edges).
    fn fw_ref_edge(&mut self, src: &str, re: &Regex, relation: &str, context: &str) {
        for cap in re.captures_iter(src) {
            let start = cap.get(0).expect("regex group 0 is the full match").start();
            let name = cap[1].to_string();
            if name.is_empty() {
                continue;
            }
            let owner = self.enclosing(start);
            let tgt = self.resolve_ref(&name);
            if tgt != owner {
                self.b
                    .add_edge(owner, tgt, relation, line_at(src, start), Some(context));
            }
        }
    }

    /// Emit `navigates` edges to a `route` concept node for every match of `re`.
    fn fw_route_edge(&mut self, src: &str, re: &Regex, context: &str) {
        for cap in re.captures_iter(src) {
            let start = cap.get(0).expect("regex group 0 is the full match").start();
            let route = cap[1].to_string();
            if route.is_empty() {
                continue;
            }
            let owner = self.enclosing(start);
            let line = line_at(src, start);
            let id = NodeId(make_id(&["route", &route]));
            self.b
                .add_node_typed(id.clone(), route, FileType::Concept, line);
            self.b.add_edge(owner, id, "navigates", line, Some(context));
        }
    }

    /// Flutter framework edges (regex, attached to the enclosing method/class):
    /// navigation (`navigates`), Riverpod/Bloc
    /// references (`references`), and Bloc event/state flow (`calls`).
    fn framework_scan(&mut self, source: &[u8]) {
        // Regex offsets index `src`; `scopes` use tree-sitter offsets into the
        // original bytes. These agree exactly for valid UTF-8 (Dart is required to
        // be UTF-8); invalid bytes only shift attribution, never panic.
        let src = String::from_utf8_lossy(source).into_owned();
        self.fw_route_edge(&src, &NAV_STRING_RE, "route_path");
        self.fw_route_edge(&src, &ROUTE_CONST_RE, "route_const");
        self.fw_ref_edge(&src, &NAV_OBJECT_RE, "navigates", "route_object");
        self.fw_ref_edge(&src, &RIVERPOD_RE, "references", "riverpod_reference");
        self.fw_ref_edge(&src, &BLOC_WIDGET_RE, "references", "bloc_widget_binding");
        self.fw_ref_edge(&src, &BLOC_ON_RE, "calls", "bloc_event");
        self.fw_ref_edge(&src, &BLOC_EMIT_RE, "calls", "emit_state");
        self.fw_ref_edge(&src, &BLOC_ADD_RE, "calls", "bloc_add_event");
        self.fw_ref_edge(&src, &BLOC_LOOKUP_RE, "references", "bloc_lookup");
    }

    fn run_call_pass(&mut self) {
        let index = self.b.label_index();
        let bodies = std::mem::take(&mut self.function_bodies);
        let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();
        for (caller, body) in bodies {
            self.walk_calls(body, &caller, &index, &mut seen, 0);
        }
    }

    fn walk_calls(
        &mut self,
        node: TsNode<'tree>,
        caller: &NodeId,
        index: &std::collections::HashMap<String, NodeId>,
        seen: &mut HashSet<(NodeId, NodeId)>,
        depth: usize,
    ) {
        if depth >= MAX_DEPTH {
            return;
        }
        if node.kind() == "method_declaration" {
            return;
        }
        if node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                let callee = match func.kind() {
                    "identifier" => Some((self.text(func), false)),
                    "selector" | "unconditional_assignable_selector" => Self::children(func)
                        .into_iter()
                        .find(|c| c.kind() == "identifier")
                        .map(|c| (self.text(c), true)),
                    _ => None,
                };
                if let Some((callee, is_member)) = callee {
                    if !callee.is_empty() {
                        self.b.resolve_call(
                            caller,
                            &callee,
                            is_member,
                            Self::line(node),
                            index,
                            seen,
                            true,
                        );
                    }
                }
            }
        }
        for c in Self::children(node) {
            self.walk_calls(c, caller, index, seen, depth + 1);
        }
    }
}

#[cfg(all(test, feature = "lang-dart"))]
mod tests {
    use super::extract_dart_source;
    use crate::result::ExtractionResult;

    fn extract() -> ExtractionResult {
        extract_dart_source(
            "lib/dog.dart",
            b"import 'animal.dart';\n\nclass Dog extends Animal implements Greeter {\n  Leash leash;\n  String bark(Food f) => sound();\n  String sound() => 'woof';\n}\n",
        )
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

    #[test]
    fn class_and_method_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"Dog".to_string()), "{ls:?}");
        assert!(ls.contains(&".bark()".to_string()));
        assert!(ls.contains(&".sound()".to_string()));
    }

    #[test]
    fn extends_implements_and_import() {
        let r = extract();
        assert!(rels(&r, "inherits").contains(&("Dog".to_string(), "Animal".to_string())));
        assert!(rels(&r, "implements").contains(&("Dog".to_string(), "Greeter".to_string())));
        assert!(rels(&r, "imports_from").iter().any(|(_, t)| t == "animal"));
    }

    #[test]
    fn field_and_param_type_references() {
        let r = extract();
        let refs: Vec<(String, String)> = r
            .edges
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
            .collect();
        assert!(
            refs.contains(&("Leash".to_string(), "field".to_string())),
            "{refs:?}"
        );
        assert!(refs.contains(&("Food".to_string(), "parameter_type".to_string())));
    }

    #[test]
    fn calls_resolve() {
        assert!(
            rels(&extract(), "calls").contains(&(".bark()".to_string(), ".sound()".to_string())),
            "{:?}",
            rels(&extract(), "calls")
        );
    }

    fn ctx_targets(r: &ExtractionResult, relation: &str, context: &str) -> Vec<String> {
        r.edges
            .iter()
            .filter(|e| e.relation == relation && e.context.as_deref() == Some(context))
            .map(|e| {
                r.nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| e.target.0.clone())
            })
            .collect()
    }

    #[test]
    fn flutter_navigation_string_and_object() {
        let r = extract_dart_source(
            "lib/nav.dart",
            b"void go() {\n  context.pushNamed('/profile');\n  Navigator.push(context, ProfileScreen());\n}\n",
        );
        assert!(
            ctx_targets(&r, "navigates", "route_path").contains(&"/profile".to_string()),
            "{:?}",
            r.edges
        );
        assert!(
            ctx_targets(&r, "navigates", "route_object").contains(&"ProfileScreen".to_string()),
            "{:?}",
            r.edges
        );
        // A top-level function isn't a node, so these attach to the file node.
        let file = crate::paths::file_node_id("lib/nav.dart");
        assert!(
            r.edges
                .iter()
                .filter(|e| e.relation == "navigates")
                .all(|e| e.source == file),
            "top-level navigates should originate at the file node"
        );
    }

    #[test]
    fn framework_edges_attach_to_enclosing_method() {
        // Inside a class method, the framework edge originates at the *method*
        // node, not the file (not file-level).
        let r = extract_dart_source(
            "lib/page.dart",
            b"class HomePage {\n  void open() {\n    context.go('/profile');\n  }\n}\n",
        );
        let method_id = r
            .nodes
            .iter()
            .find(|n| n.label == ".open()")
            .expect("method node")
            .id
            .clone();
        let nav = r
            .edges
            .iter()
            .find(|e| e.relation == "navigates")
            .expect("a navigates edge");
        assert_eq!(
            nav.source, method_id,
            "should originate at .open(): {:?}",
            r.edges
        );
    }

    #[test]
    fn flutter_bloc_event_state_flow() {
        let r = extract_dart_source(
            "lib/counter_bloc.dart",
            b"class CounterBloc {\n  CounterBloc() {\n    on<Increment>((e, emit) {\n      emit(CounterLoaded());\n    });\n  }\n  void fire(bloc) { bloc.add(Increment()); }\n}\n",
        );
        assert!(
            ctx_targets(&r, "calls", "bloc_event").contains(&"Increment".to_string()),
            "{:?}",
            r.edges
        );
        assert!(
            ctx_targets(&r, "calls", "emit_state").contains(&"CounterLoaded".to_string()),
            "{:?}",
            r.edges
        );
        assert!(
            ctx_targets(&r, "calls", "bloc_add_event").contains(&"Increment".to_string()),
            "{:?}",
            r.edges
        );
    }

    #[test]
    fn flutter_route_const_and_bloc_lookup() {
        let r = extract_dart_source(
            "lib/nav2.dart",
            b"void go() {\n  context.go(Routes.profile);\n  final b = context.read<AuthBloc>();\n}\n",
        );
        assert!(
            ctx_targets(&r, "navigates", "route_const").contains(&"Routes.profile".to_string()),
            "{:?}",
            r.edges
        );
        assert!(
            ctx_targets(&r, "references", "bloc_lookup").contains(&"AuthBloc".to_string()),
            "{:?}",
            r.edges
        );
    }

    #[test]
    fn flutter_riverpod_and_bloc_references() {
        let r = extract_dart_source(
            "lib/widget.dart",
            b"Widget build(ref) {\n  final user = ref.watch(userProvider);\n  return BlocBuilder<AuthBloc, AuthState>();\n}\n",
        );
        assert!(
            ctx_targets(&r, "references", "riverpod_reference")
                .contains(&"userProvider".to_string()),
            "{:?}",
            r.edges
        );
        assert!(
            ctx_targets(&r, "references", "bloc_widget_binding").contains(&"AuthBloc".to_string()),
            "{:?}",
            r.edges
        );
    }
}
