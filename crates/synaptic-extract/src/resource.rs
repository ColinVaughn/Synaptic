//! Resource extractor: turns structured data/resource files (data JSON, `.mcmeta`)
//! that the config-only JSON extractor drops into a single graph node each, plus
//! (later passes) the string-keyed references between them and to code.
//!
//! Universal, not framework-specific: a Minecraft `ResourceLocation` is just one
//! instance of a path-derived resource id, and `assets/`/`data/` are just two
//! conventional content roots. One node per *file* (never per key), so the
//! orphan-key explosion that makes the config extractor skip data JSON does not
//! apply here.

#[cfg(feature = "lang-json")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "lang-json")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "lang-json")]
use serde_json::{Map, Value};
#[cfg(feature = "lang-json")]
use synaptic_core::{make_id, Confidence, Edge, FileType, Node, NodeId, NodeKind};
#[cfg(feature = "lang-json")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-json")]
use crate::paths::file_node_id;
#[cfg(feature = "lang-json")]
use crate::result::ExtractionResult;

/// Whether extraction indexes data/resource files as graph nodes. On by default
/// (opt out with `--no-resources`); the pipeline flips it before extraction. Uses
/// the same global-toggle idiom as `set_emit_sql_columns`.
#[cfg(feature = "lang-json")]
static EMIT_RESOURCES: AtomicBool = AtomicBool::new(true);

/// Read the resource-indexing toggle.
#[cfg(feature = "lang-json")]
pub fn emit_resources() -> bool {
    EMIT_RESOURCES.load(Ordering::Relaxed)
}

/// Set the resource-indexing toggle (pipeline entry point for `--no-resources`).
#[cfg(feature = "lang-json")]
pub fn set_emit_resources(on: bool) {
    EMIT_RESOURCES.store(on, Ordering::Relaxed);
}

/// Serializes every test that flips [`set_emit_resources`] so they don't race
/// each other across modules within the single lib test binary. Poison-tolerant
/// at the lock sites so one failing test doesn't cascade.
#[cfg(test)]
pub(crate) static RESOURCE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Content-root markers whose *next* segment is a namespace and the one after a
/// category. Generic (also matches non-MC `assets/`, `data/` layouts).
#[cfg(feature = "lang-json")]
const CONTENT_ROOTS: &[&str] = &["assets", "data"];

/// Max distinct reference strings materialized per file (bounds the resolve pass
/// on a pathological giant data file). Unresolved stubs are dropped downstream.
#[cfg(feature = "lang-json")]
const MAX_REFS_PER_FILE: usize = 500;

/// Coarse, path-derived resource category label (`models`, `loot_tables`, ...),
/// or `"data"` when the path has no recognizable `<root>/<namespace>/<category>/…`
/// shape. Cosmetic only — never gates behavior.
#[cfg(feature = "lang-json")]
fn resource_kind(path: &str) -> String {
    let p = path.replace('\\', "/");
    let segs: Vec<&str> = p.split('/').collect();
    for i in (0..segs.len()).rev() {
        if CONTENT_ROOTS.contains(&segs[i]) && i + 2 < segs.len() {
            return segs[i + 2].to_string();
        }
    }
    "data".to_string()
}

/// Extract one resource file into a single tagged `Document` node plus a
/// `references`/`resource_ref` edge to a stub for each reference-like string value.
/// Stubs are bound to real nodes (or dropped) by [`resolve_resource_refs`] once the
/// whole corpus is known.
#[cfg(feature = "lang-json")]
pub fn extract_resource_source(path: &str, source: &[u8]) -> ExtractionResult {
    let file_id = file_node_id(path);
    let mut file_extra = Map::new();
    file_extra.insert("_origin".to_string(), Value::String("resource".to_string()));
    file_extra.insert(
        "_node_type".to_string(),
        Value::String("resource".to_string()),
    );
    file_extra.insert(
        "resource_kind".to_string(),
        Value::String(resource_kind(path)),
    );
    let mut nodes = vec![Node {
        id: file_id.clone(),
        label: resource_label(path),
        file_type: FileType::Document,
        source_file: path.to_string(),
        source_location: Some("L1".to_string()),
        community: None,
        repo: None,
        extra: file_extra,
    }];
    let mut edges = Vec::new();

    let mut seen_refs: HashSet<String> = HashSet::new();
    let mut stub_ids: HashSet<NodeId> = HashSet::new();
    for (s, line) in scan_ref_strings(source) {
        if seen_refs.len() >= MAX_REFS_PER_FILE {
            break;
        }
        if !looks_like_ref(&s) || !seen_refs.insert(s.clone()) {
            continue;
        }
        let stub_id = NodeId(make_id(&["resref", &s]));
        if stub_ids.insert(stub_id.clone()) {
            let mut stub_extra = Map::new();
            stub_extra.insert("_origin".to_string(), Value::String("resource".to_string()));
            nodes.push(Node {
                id: stub_id.clone(),
                label: s.clone(),
                file_type: FileType::Code,
                source_file: String::new(),
                source_location: None,
                community: None,
                repo: None,
                extra: stub_extra,
            });
        }
        edges.push(Edge {
            source: file_id.clone(),
            target: stub_id,
            relation: "references".to_string(),
            confidence: Confidence::Extracted,
            source_file: path.to_string(),
            source_location: Some(format!("L{line}")),
            confidence_score: None,
            weight: 1.0,
            context: Some("resource_ref".to_string()),
            cross_repo: false,
            extra: Map::new(),
        });
    }

    ExtractionResult {
        nodes,
        edges,
        raw_calls: Vec::new(),
        imports: Vec::new(),
    }
}

/// Parse JSON-syntax `source` and collect every string *value* (skipping object
/// keys) with its 1-based line. Returns empty on a parse failure.
#[cfg(feature = "lang-json")]
fn scan_ref_strings(source: &[u8]) -> Vec<(String, usize)> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_json::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_values(tree.root_node(), source, &mut out);
    out
}

/// Recurse structure, collecting string *values* (object values + array elements),
/// never object keys.
#[cfg(feature = "lang-json")]
fn walk_values(node: TsNode, src: &[u8], out: &mut Vec<(String, usize)>) {
    match node.kind() {
        "document" => {
            for c in children(node) {
                walk_values(c, src, out);
            }
        }
        "object" => {
            for pair in children(node).into_iter().filter(|c| c.kind() == "pair") {
                if let Some(v) = pair.child_by_field_name("value") {
                    walk_values(v, src, out);
                }
            }
        }
        "array" => {
            for el in children(node) {
                walk_values(el, src, out);
            }
        }
        "string" => {
            if let Some(s) = string_value(node, src) {
                out.push((s, node.start_position().row + 1));
            }
        }
        _ => {}
    }
}

#[cfg(feature = "lang-json")]
fn children(node: TsNode) -> Vec<TsNode> {
    let mut c = node.walk();
    node.children(&mut c).collect()
}

/// Unquoted text of a JSON `string` node (via its `string_content` child).
#[cfg(feature = "lang-json")]
fn string_value(node: TsNode, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let inner = children(node)
        .into_iter()
        .find(|c| c.kind() == "string_content");
    Some(match inner {
        Some(c) => c.utf8_text(src).unwrap_or("").to_string(),
        None => String::new(),
    })
}

/// Conservative pre-filter: a string worth emitting as a reference stub. Keeps only
/// *qualified* strings — a path (`a/b`), a namespaced id (`ns:x`), or a
/// fully-qualified name (`com.foo.Bar`); drops prose (whitespace), bare single
/// tokens, empties, and over-long strings. A bare token like a lang value `Rocket`
/// is prose, not a reference — requiring a separator stops it binding to a
/// same-named class. Final precision is enforced by the resolve pass, which drops
/// anything that doesn't bind.
#[cfg(feature = "lang-json")]
fn looks_like_ref(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 200 || s.chars().any(char::is_whitespace) {
        return false;
    }
    s.contains('/') || s.contains(':') || s.contains('.')
}

/// Label for a resource node: its full posix path. Filenames collide across
/// resource trees (`assets/.../x.json` and a generated copy), and the build's
/// entity-dedup merges same-labeled non-code nodes — so a unique, locatable
/// label per file is required (mirrors the asset-node convention in `resolve.rs`).
#[cfg(feature = "lang-json")]
fn resource_label(path: &str) -> String {
    path.replace('\\', "/")
}

/// Read and extract a resource file from disk.
#[cfg(feature = "lang-json")]
pub fn extract_resource_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    Ok(extract_resource_source(&path.to_string_lossy(), &source))
}

/// Per-run outcome counts for the resource-reference resolver.
#[cfg(feature = "lang-json")]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResourceResolveStats {
    /// `resource_ref` stubs bound to a real node.
    pub bound: usize,
    /// `resource_ref` edges dropped for not resolving.
    pub dropped: usize,
    /// `shadows` edges emitted (generated resource duplicates a source resource).
    pub shadows: usize,
}

/// Cross-file pass: bind each `resource_ref` stub edge to a real node, or drop it.
/// Run after per-file extraction once the full corpus is known (mirrors
/// [`crate::resolve::resolve_imports`]). Conservative: an edge survives only if its
/// reference string resolves to a concrete node via, in order, an existing file
/// path, a resource's path-derived logical id, or a unique code symbol.
#[cfg(feature = "lang-json")]
pub fn resolve_resource_refs(nodes: &mut Vec<Node>, edges: &mut Vec<Edge>) -> ResourceResolveStats {
    let mut stats = ResourceResolveStats::default();

    let label_of: HashMap<NodeId, String> = nodes
        .iter()
        .map(|n| (n.id.clone(), n.label.clone()))
        .collect();

    // (a) real file/resource/code-file nodes by normalized posix path.
    let mut file_by_path: HashMap<String, NodeId> = HashMap::new();
    for n in nodes.iter() {
        if !n.source_file.is_empty() && file_node_id(&n.source_file) == n.id {
            file_by_path
                .entry(n.source_file.replace('\\', "/"))
                .or_insert_with(|| n.id.clone());
        }
    }

    // (b) resource logical ids (full + category-stripped), unique-only.
    let mut logical: HashMap<String, Option<NodeId>> = HashMap::new();
    for n in nodes.iter() {
        if n.extra.get("_node_type").and_then(|v| v.as_str()) != Some("resource") {
            continue;
        }
        for form in logical_ids(&n.source_file) {
            insert_unique(&mut logical, form, &n.id);
        }
    }

    // (c) unique referenceable code symbols by bare label.
    let mut symbol: HashMap<String, Option<NodeId>> = HashMap::new();
    for n in nodes.iter() {
        if n.source_file.is_empty() || !is_referenceable_symbol(n) {
            continue;
        }
        let key = bare_symbol_label(&n.label);
        if !key.is_empty() {
            insert_unique(&mut symbol, key, &n.id);
        }
    }

    let mut keep = Vec::with_capacity(edges.len());
    for e in edges.iter_mut() {
        if e.context.as_deref() != Some("resource_ref") {
            keep.push(true);
            continue;
        }
        let resolved = label_of.get(&e.target).and_then(|s| {
            let dir = e.source_file.replace('\\', "/");
            let dir = dir.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            resolve_one(s, dir, &file_by_path, &logical, &symbol)
        });
        match resolved {
            Some(id) if id != e.source => {
                e.target = id;
                stats.bound += 1;
                keep.push(true);
            }
            _ => {
                stats.dropped += 1;
                keep.push(false);
            }
        }
    }
    let mut i = 0;
    edges.retain(|_| {
        let k = keep[i];
        i += 1;
        k
    });

    // Drop resref stubs no longer referenced by a surviving edge (all of them, since
    // a bound edge now points at the real node and an unbound edge was removed).
    let referenced: HashSet<&NodeId> = edges.iter().flat_map(|e| [&e.source, &e.target]).collect();
    nodes.retain(|n| {
        let is_stub = n.source_file.is_empty()
            && n.extra.get("_origin").and_then(|v| v.as_str()) == Some("resource");
        !is_stub || referenced.contains(&n.id)
    });

    let shadows = detect_generated_shadows(nodes);
    stats.shadows = shadows.len();
    edges.extend(shadows);

    stats
}

/// Emit a `shadows` edge (generated -> source) for every resource that exists under
/// both a generated and a source content root at the same logical path. Universal
/// "a generated artifact duplicates a hand-authored one" — no framework schema.
#[cfg(feature = "lang-json")]
fn detect_generated_shadows(nodes: &[Node]) -> Vec<Edge> {
    let mut generated: HashMap<String, Vec<&Node>> = HashMap::new();
    let mut source: HashMap<String, Vec<&Node>> = HashMap::new();
    for n in nodes {
        if n.extra.get("_node_type").and_then(|v| v.as_str()) != Some("resource") {
            continue;
        }
        let Some(lp) = logical_path(&n.source_file) else {
            continue;
        };
        if is_generated(&n.source_file) {
            generated.entry(lp).or_default().push(n);
        } else {
            source.entry(lp).or_default().push(n);
        }
    }
    let mut edges = Vec::new();
    for (lp, gens) in &generated {
        if let Some(srcs) = source.get(lp) {
            for g in gens {
                for s in srcs {
                    edges.push(shadow_edge(g, s));
                }
            }
        }
    }
    edges.sort_by(|a, b| {
        a.source
            .0
            .cmp(&b.source.0)
            .then(a.target.0.cmp(&b.target.0))
    });
    edges
}

/// Content-root-relative path (from the first `assets`/`data` segment onward), the
/// key shared by a generated copy and a source copy of the same resource. `None`
/// when the path has no content-root shape.
#[cfg(feature = "lang-json")]
fn logical_path(path: &str) -> Option<String> {
    let p = path.replace('\\', "/");
    let segs: Vec<&str> = p.split('/').collect();
    segs.iter()
        .position(|s| CONTENT_ROOTS.contains(s))
        .map(|i| segs[i..].join("/"))
}

/// A path that lives under a generated/build output root.
#[cfg(feature = "lang-json")]
fn is_generated(path: &str) -> bool {
    let p = path.replace('\\', "/").to_ascii_lowercase();
    p.contains("/generated/") || p.starts_with("generated/") || p.contains("/build/")
}

#[cfg(feature = "lang-json")]
fn shadow_edge(generated: &Node, source: &Node) -> Edge {
    Edge {
        source: generated.id.clone(),
        target: source.id.clone(),
        relation: "shadows".to_string(),
        confidence: Confidence::Extracted,
        source_file: generated.source_file.clone(),
        source_location: None,
        confidence_score: None,
        weight: 1.0,
        context: Some("generated".to_string()),
        cross_repo: false,
        extra: Map::new(),
    }
}

/// Insert `id` for `key`, collapsing to ambiguous (`None`) if a different id is
/// already present.
#[cfg(feature = "lang-json")]
fn insert_unique(map: &mut HashMap<String, Option<NodeId>>, key: String, id: &NodeId) {
    map.entry(key)
        .and_modify(|e| {
            if e.as_ref() != Some(id) {
                *e = None;
            }
        })
        .or_insert_with(|| Some(id.clone()));
}

/// Resolve one reference string in precedence order: path, logical id, symbol.
#[cfg(feature = "lang-json")]
fn resolve_one(
    s: &str,
    dir: &str,
    file_by_path: &HashMap<String, NodeId>,
    logical: &HashMap<String, Option<NodeId>>,
    symbol: &HashMap<String, Option<NodeId>>,
) -> Option<NodeId> {
    if let Some(id) = resolve_path(s, dir, file_by_path) {
        return Some(id);
    }
    if let Some(Some(id)) = logical.get(s) {
        return Some(id.clone());
    }
    resolve_symbol(s, symbol)
}

/// (a) A path-shaped string that (repo-relative, or relative to the referrer's
/// dir, optionally + a resource extension) hits a real file/resource node.
#[cfg(feature = "lang-json")]
fn resolve_path(s: &str, dir: &str, file_by_path: &HashMap<String, NodeId>) -> Option<NodeId> {
    if !s.contains('/') && !s.contains('.') {
        return None; // not path-like; leave to logical/symbol
    }
    let s = s.trim_start_matches("./").replace('\\', "/");
    let mut cands = vec![s.clone()];
    if !dir.is_empty() {
        if let Some(joined) = join_norm(dir, &s) {
            cands.push(joined);
        }
    }
    for c in cands {
        if let Some(id) = file_by_path.get(&c) {
            return Some(id.clone());
        }
        for ext in RESOURCE_EXTS {
            if let Some(id) = file_by_path.get(&format!("{c}.{ext}")) {
                return Some(id.clone());
            }
        }
    }
    None
}

/// (c) A unique referenceable symbol whose bare label equals the string or the
/// string's last `.`/`:`/`/`-delimited segment (fully-qualified names).
#[cfg(feature = "lang-json")]
fn resolve_symbol(s: &str, symbol: &HashMap<String, Option<NodeId>>) -> Option<NodeId> {
    if let Some(Some(id)) = symbol.get(s) {
        return Some(id.clone());
    }
    let seg = s.rsplit(['.', ':', '/']).next().unwrap_or(s);
    if seg != s {
        if let Some(Some(id)) = symbol.get(seg) {
            return Some(id.clone());
        }
    }
    None
}

/// Resource file extensions used for stripping/appending in id + path resolution.
#[cfg(feature = "lang-json")]
const RESOURCE_EXTS: &[&str] = &["json", "mcmeta", "json5", "jsonc", "yaml", "yml"];

/// Path-derived logical id forms for a resource, from its `<root>/<ns>/<rest>`
/// shape: full (`ns:rest`) and category-stripped (`ns:rest-without-first-seg`).
/// Empty when the path has no content-root shape.
#[cfg(feature = "lang-json")]
fn logical_ids(source_file: &str) -> Vec<String> {
    let p = source_file.replace('\\', "/");
    let segs: Vec<&str> = p.split('/').collect();
    let mut out = Vec::new();
    for i in (0..segs.len()).rev() {
        if CONTENT_ROOTS.contains(&segs[i]) && i + 2 < segs.len() {
            let ns = segs[i + 1];
            let rest = strip_known_ext(&segs[i + 2..].join("/"));
            out.push(format!("{ns}:{rest}"));
            if let Some((_, tail)) = rest.split_once('/') {
                out.push(format!("{ns}:{tail}"));
            }
            break;
        }
    }
    out
}

#[cfg(feature = "lang-json")]
fn strip_known_ext(s: &str) -> String {
    for ext in RESOURCE_EXTS {
        if let Some(base) = s.strip_suffix(&format!(".{ext}")) {
            return base.to_string();
        }
    }
    s.to_string()
}

/// Symbol kinds a resource string may name: TYPES only (a class/interface/enum/…
/// used as a mixin, serializer, or entity type). Excludes functions/methods —
/// matching a dotted key's last segment to a getter like `description()` is noise —
/// and fields/variables/config keys, to keep (c) precise.
#[cfg(feature = "lang-json")]
fn is_referenceable_symbol(n: &Node) -> bool {
    matches!(
        n.kind(),
        Some(
            NodeKind::Class
                | NodeKind::Interface
                | NodeKind::Trait
                | NodeKind::Struct
                | NodeKind::Enum
                | NodeKind::Protocol
                | NodeKind::Object
                | NodeKind::TypeAlias
        )
    )
}

/// Bare symbol name: strip a leading `.` and a trailing `()`/param list.
#[cfg(feature = "lang-json")]
fn bare_symbol_label(label: &str) -> String {
    label
        .trim_start_matches('.')
        .split('(')
        .next()
        .unwrap_or(label)
        .to_string()
}

/// Join `spec` onto `dir` (posix), normalizing `.`/`..`. `None` if it climbs above
/// the root.
#[cfg(feature = "lang-json")]
fn join_norm(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for comp in spec.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

#[cfg(all(test, feature = "lang-json"))]
mod tests {
    use super::*;
    use synaptic_core::{FileType, NodeKind};

    fn aggregate(rs: Vec<ExtractionResult>) -> (Vec<Node>, Vec<Edge>) {
        let (mut nodes, mut edges) = (Vec::new(), Vec::new());
        for r in rs {
            nodes.extend(r.nodes);
            edges.extend(r.edges);
        }
        (nodes, edges)
    }

    fn code_node(id: &str, label: &str, kind: NodeKind, file: &str) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: file.into(),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
        };
        n.set_kind(kind);
        n
    }

    fn resref_edges(edges: &[Edge]) -> impl Iterator<Item = &Edge> {
        edges
            .iter()
            .filter(|e| e.relation == "references" && e.context.as_deref() == Some("resource_ref"))
    }

    fn has_live_resref_stub(nodes: &[Node]) -> bool {
        nodes.iter().any(|n| {
            n.source_file.is_empty()
                && n.extra.get("_origin").and_then(|v| v.as_str()) == Some("resource")
        })
    }

    #[test]
    fn binds_reference_to_existing_resource_by_logical_id() {
        let referencing = extract_resource_source(
            "assets/mymod/models/block/x.json",
            br#"{"parent":"mymod:block/base"}"#,
        );
        let target = extract_resource_source("assets/mymod/models/block/base.json", b"{}");
        let (mut nodes, mut edges) = aggregate(vec![referencing, target]);
        let stats = resolve_resource_refs(&mut nodes, &mut edges);
        let base_id = file_node_id("assets/mymod/models/block/base.json");
        assert!(
            resref_edges(&edges).any(|e| e.target == base_id),
            "reference rebound to base.json"
        );
        assert_eq!(stats.bound, 1);
        assert!(!has_live_resref_stub(&nodes), "stub dropped after binding");
    }

    #[test]
    fn binds_reference_to_existing_file_by_path() {
        let referencing =
            extract_resource_source("data/mymod/x.json", br#"{"ref":"data/mymod/other.json"}"#);
        let target = extract_resource_source("data/mymod/other.json", b"{}");
        let (mut nodes, mut edges) = aggregate(vec![referencing, target]);
        resolve_resource_refs(&mut nodes, &mut edges);
        let other_id = file_node_id("data/mymod/other.json");
        assert!(resref_edges(&edges).any(|e| e.target == other_id));
    }

    #[test]
    fn binds_fully_qualified_symbol_by_last_segment() {
        let referencing =
            extract_resource_source("data/mymod/x.json", br#"{"type":"com.mymod.SkeletonBoss"}"#);
        let (mut nodes, mut edges) = aggregate(vec![referencing]);
        nodes.push(code_node(
            "cls_skeleton",
            "SkeletonBoss",
            NodeKind::Class,
            "src/SkeletonBoss.java",
        ));
        resolve_resource_refs(&mut nodes, &mut edges);
        assert!(resref_edges(&edges).any(|e| e.target == NodeId("cls_skeleton".into())));
    }

    #[test]
    fn function_name_coincidence_does_not_bind() {
        // A qualified string whose last segment matches only a free function/getter
        // is not a real reference (an i18n key `a.b.description` vs a `description()`
        // method). Resource strings that name code name TYPES, not functions.
        let referencing =
            extract_resource_source("data/mymod/x.json", br#"{"key":"a.b.description"}"#);
        let (mut nodes, mut edges) = aggregate(vec![referencing]);
        nodes.push(code_node(
            "fn_desc",
            "description()",
            NodeKind::Function,
            "src/Body.java",
        ));
        resolve_resource_refs(&mut nodes, &mut edges);
        assert_eq!(
            resref_edges(&edges).count(),
            0,
            "a free-function name match must not bind"
        );
    }

    #[test]
    fn drops_unresolved_reference_and_stub_but_keeps_file_node() {
        let referencing = extract_resource_source(
            "data/mymod/x.json",
            br#"{"parent":"minecraft:block/nonexistent"}"#,
        );
        let (mut nodes, mut edges) = aggregate(vec![referencing]);
        let stats = resolve_resource_refs(&mut nodes, &mut edges);
        assert_eq!(resref_edges(&edges).count(), 0, "unresolved edge dropped");
        assert!(!has_live_resref_stub(&nodes), "unresolved stub dropped");
        assert!(
            nodes
                .iter()
                .any(|n| n.id == file_node_id("data/mymod/x.json")),
            "the resource file node survives"
        );
        assert_eq!(stats.dropped, 1);
    }

    #[test]
    fn ambiguous_symbol_reference_is_dropped() {
        let referencing =
            extract_resource_source("data/mymod/x.json", br#"{"type":"com.mymod.Widget"}"#);
        let (mut nodes, mut edges) = aggregate(vec![referencing]);
        nodes.push(code_node(
            "w1",
            "Widget",
            NodeKind::Class,
            "src/a/Widget.java",
        ));
        nodes.push(code_node(
            "w2",
            "Widget",
            NodeKind::Class,
            "src/b/Widget.java",
        ));
        resolve_resource_refs(&mut nodes, &mut edges);
        assert_eq!(
            resref_edges(&edges).count(),
            0,
            "ambiguous symbol match does not bind"
        );
    }

    #[test]
    fn emits_shadows_edge_when_generated_duplicates_source() {
        let src = extract_resource_source("src/main/resources/assets/mymod/models/x.json", b"{}");
        let gen = extract_resource_source("src/main/generated/assets/mymod/models/x.json", b"{}");
        let (mut nodes, mut edges) = aggregate(vec![src, gen]);
        let stats = resolve_resource_refs(&mut nodes, &mut edges);
        let src_id = file_node_id("src/main/resources/assets/mymod/models/x.json");
        let gen_id = file_node_id("src/main/generated/assets/mymod/models/x.json");
        assert!(
            edges
                .iter()
                .any(|e| e.relation == "shadows" && e.source == gen_id && e.target == src_id),
            "generated -> source shadows edge"
        );
        assert_eq!(stats.shadows, 1);
    }

    #[test]
    fn no_shadows_edge_when_resource_in_one_root_only() {
        let src = extract_resource_source("src/main/resources/assets/mymod/models/x.json", b"{}");
        let (mut nodes, mut edges) = aggregate(vec![src]);
        resolve_resource_refs(&mut nodes, &mut edges);
        assert!(!edges.iter().any(|e| e.relation == "shadows"));
    }

    #[test]
    fn mints_one_resource_node_for_data_json() {
        let r = extract_resource_source(
            "assets/mymod/models/block/x.json",
            br#"{"parent":"block/cube_all"}"#,
        );
        // Exactly one *resource file* node (ref stubs are separate, untagged nodes).
        let resource_nodes: Vec<_> = r
            .nodes
            .iter()
            .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("resource"))
            .collect();
        assert_eq!(resource_nodes.len(), 1, "one resource file node per file");
        let n = resource_nodes[0];
        assert_eq!(n.id, file_node_id("assets/mymod/models/block/x.json"));
        assert_eq!(n.file_type, FileType::Document);
        assert_eq!(n.source_file, "assets/mymod/models/block/x.json");
        assert_eq!(
            n.extra.get("_node_type").and_then(|v| v.as_str()),
            Some("resource")
        );
        assert_eq!(
            n.extra.get("_origin").and_then(|v| v.as_str()),
            Some("resource")
        );
    }

    #[test]
    fn resource_kind_from_category_segment() {
        let r = extract_resource_source("assets/mymod/models/block/x.json", b"{}");
        assert_eq!(
            r.nodes[0]
                .extra
                .get("resource_kind")
                .and_then(|v| v.as_str()),
            Some("models")
        );
    }

    #[test]
    fn resource_kind_from_nested_source_resources_layout() {
        // The standard `src/main/resources/assets/<ns>/<category>/…` layout must
        // anchor on the `assets` content root, not the outer `resources` dir.
        let r = extract_resource_source(
            "src/main/resources/data/mymod/loot_tables/blocks/x.json",
            b"{}",
        );
        assert_eq!(
            r.nodes[0]
                .extra
                .get("resource_kind")
                .and_then(|v| v.as_str()),
            Some("loot_tables")
        );
    }

    #[test]
    fn resource_kind_falls_back_to_data_without_shape() {
        let r = extract_resource_source("config/whatever.json", b"{}");
        assert_eq!(
            r.nodes[0]
                .extra
                .get("resource_kind")
                .and_then(|v| v.as_str()),
            Some("data")
        );
    }

    #[test]
    fn mints_node_for_top_level_array_resource() {
        // A top-level array is valid resource data (the config extractor drops it);
        // still one node per file.
        let r = extract_resource_source("data/mymod/tags/blocks/x.json", b"[1,2,3]");
        assert!(r
            .nodes
            .iter()
            .any(|n| n.id == file_node_id("data/mymod/tags/blocks/x.json")));
    }

    fn ref_targets(r: &ExtractionResult) -> Vec<String> {
        r.edges
            .iter()
            .filter(|e| e.relation == "references" && e.context.as_deref() == Some("resource_ref"))
            .map(|e| {
                r.nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn emits_ref_stubs_for_pathlike_and_id_values() {
        let r = extract_resource_source(
            "assets/mymod/models/block/x.json",
            br#"{"parent":"minecraft:block/cube_all","textures":{"all":"mymod:block/x"}}"#,
        );
        let t = ref_targets(&r);
        assert!(t.contains(&"minecraft:block/cube_all".to_string()), "{t:?}");
        assert!(t.contains(&"mymod:block/x".to_string()), "{t:?}");
        // every ref edge originates at the resource file node
        assert!(r
            .edges
            .iter()
            .filter(|e| e.context.as_deref() == Some("resource_ref"))
            .all(|e| e.source == file_node_id("assets/mymod/models/block/x.json")));
    }

    #[test]
    fn skips_bare_identifier_value() {
        // A bare (separator-less) word is not a reference. This avoids binding a
        // human-readable value (e.g. a lang string "Rocket") to a same-named class;
        // genuine symbol references are qualified (an FQN or a namespaced id).
        let r = extract_resource_source("data/mymod/x.json", br#"{"type":"SkeletonBoss"}"#);
        assert!(ref_targets(&r).is_empty(), "{:?}", ref_targets(&r));
    }

    #[test]
    fn skips_freetext_value_strings() {
        // A value with whitespace is prose, not a reference; the key is not scanned.
        let r = extract_resource_source(
            "assets/mymod/lang/en_us.json",
            br#"{"a.b.c":"Block of Stone"}"#,
        );
        assert!(ref_targets(&r).is_empty(), "{:?}", ref_targets(&r));
    }

    #[test]
    fn skips_object_keys() {
        // The key looks reference-like (`:`), but keys are structure, not refs.
        let r =
            extract_resource_source("assets/mymod/x.json", br#"{"minecraft:foo":"hello world"}"#);
        assert!(ref_targets(&r).is_empty());
    }

    #[test]
    fn dedups_repeated_reference_within_file() {
        let r = extract_resource_source(
            "data/mymod/x.json",
            br#"{"a":"mymod:foo/bar","b":"mymod:foo/bar"}"#,
        );
        assert_eq!(ref_targets(&r).len(), 1);
    }

    #[test]
    fn caps_references_per_file() {
        let mut obj = String::from("{");
        for i in 0..600 {
            if i > 0 {
                obj.push(',');
            }
            obj.push_str(&format!("\"k{i}\":\"mymod:item/x{i}\""));
        }
        obj.push('}');
        let r = extract_resource_source("data/mymod/big.json", obj.as_bytes());
        assert!(
            ref_targets(&r).len() <= MAX_REFS_PER_FILE,
            "got {}",
            ref_targets(&r).len()
        );
    }
}
