//! Cross-file call resolution: turn per-file `raw_calls` into `calls` edges,
//! conservatively.
//!
//! Two passes, run after the graph is built (so the canonical node set is known):
//! 1. **Import-guided** — a `from M import name [as local]` record proves that a
//!    bare `local(...)` call targets `M`'s `name`. If exactly one node matches
//!    `(module_stem, name)`, emit an `EXTRACTED` edge (score 1.0).
//! 2. **Cross-file** — for any remaining unqualified call, if its name maps to
//!    exactly one node across the whole graph, emit an `INFERRED` edge (0.8).
//!
//! Both skip member calls (the raw fact carries no receiver) and ambiguous names.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{Map, json};
use synaptic_core::{Confidence, Edge, ImportRecord, Node, NodeId, RawCall};

use crate::graph::KnowledgeGraph;

/// Source-file extensions whose file-node labels must never be call targets.
const SOURCE_EXTS: &[&str] = &[
    ".py", ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".go", ".rs", ".java",
    ".ql", ".qll",
];

/// Normalize a node label into the lookup key: `foo()`→`foo`, `.bar()`→`bar`,
/// lowercased.
fn normalize_label(label: &str) -> String {
    label
        .trim()
        .trim_matches(|c| c == '(' || c == ')')
        .trim_start_matches('.')
        .to_lowercase()
}

/// A node usable as a deterministic call target: a located code symbol (not a
/// file node, not an external stub).
///
/// We exclude empty-`source_file` nodes so a real call never resolves onto a
/// `{file_type: "code", source_file: ""}` stub emitted for unresolved
/// imports/inheritance bases — a call must never resolve to an unresolved
/// external stub (see the `external_stub_nodes_do_not_absorb_calls` test).
/// Pass 1 is unaffected either way (it already requires a non-empty source stem
/// on both sides).
fn is_resolvable(n: &Node) -> bool {
    if n.file_type != synaptic_core::FileType::Code || n.source_file.is_empty() {
        return false;
    }
    let label = n.label.trim();
    if label.is_empty() || SOURCE_EXTS.iter().any(|e| label.ends_with(e)) {
        return false;
    }
    !normalize_label(label).is_empty()
}

fn member_call_index(kg: &KnowledgeGraph) -> HashMap<(String, String), Vec<NodeId>> {
    let mut index: HashMap<(String, String), Vec<NodeId>> = HashMap::new();
    for edge in kg.edges().filter(|edge| edge.relation == "method") {
        let (Some(owner), Some(method)) = (kg.node(&edge.source), kg.node(&edge.target)) else {
            continue;
        };
        index
            .entry((
                normalize_label(&owner.label),
                normalize_label(&method.label),
            ))
            .or_default()
            .push(method.id.clone());
    }
    index
}

fn owned_member_call_index(kg: &KnowledgeGraph) -> HashMap<(NodeId, String), Vec<NodeId>> {
    let mut index = HashMap::new();
    for edge in kg.edges().filter(|edge| edge.relation == "method") {
        let Some(method) = kg.node(&edge.target) else {
            continue;
        };
        index
            .entry((edge.source.clone(), normalize_label(&method.label)))
            .or_insert_with(Vec::new)
            .push(method.id.clone());
    }
    index
}

fn method_owners(kg: &KnowledgeGraph) -> HashMap<NodeId, String> {
    kg.edges()
        .filter(|edge| edge.relation == "method")
        .filter_map(|edge| {
            Some((
                edge.target.clone(),
                normalize_label(&kg.node(&edge.source)?.label),
            ))
        })
        .collect()
}

fn typed_member_target(
    kg: &KnowledgeGraph,
    call: &RawCall,
    members: &HashMap<(String, String), Vec<NodeId>>,
    owned_members: &HashMap<(NodeId, String), Vec<NodeId>>,
    owners: &HashMap<NodeId, String>,
) -> Option<NodeId> {
    if !call.is_member_call {
        return None;
    }
    let (receiver, member) = call.callee.rsplit_once('.')?;
    let source = call.source_file.to_ascii_lowercase();
    if source.ends_with(".xaml") {
        let owner = kg
            .edges()
            .find(|edge| {
                edge.source == call.caller && edge.context.as_deref() == Some("xaml_code_behind")
            })?
            .target
            .clone();
        let candidates = owned_members.get(&(owner, normalize_label(member)))?;
        return (candidates.len() == 1).then(|| candidates[0].clone());
    }
    if source.ends_with(".rs") {
        let receiver = receiver.rsplit("::").next().unwrap_or(receiver);
        let owner = if matches!(receiver, "self" | "Self") {
            owners.get(&call.caller)?.clone()
        } else {
            normalize_label(receiver)
        };
        let candidates = members.get(&(owner, normalize_label(member)))?;
        return (candidates.len() == 1).then(|| candidates[0].clone());
    }
    if !matches!(
        Path::new(&source).extension().and_then(|ext| ext.to_str()),
        Some(
            "cs" | "java"
                | "swift"
                | "py"
                | "js"
                | "jsx"
                | "mjs"
                | "cjs"
                | "ts"
                | "tsx"
                | "mts"
                | "cts"
        )
    ) {
        return None;
    }
    let receiver = receiver.rsplit('.').next().unwrap_or(receiver);
    let owner = if matches!(receiver, "this" | "self" | "Self") {
        owners.get(&call.caller).cloned()
    } else if receiver.chars().next().is_some_and(char::is_uppercase) {
        Some(normalize_label(receiver))
    } else {
        kg.node(&call.caller)
            .and_then(Node::signature)
            .and_then(|signature| {
                signature
                    .params
                    .into_iter()
                    .find(|param| param.name == receiver)
                    .and_then(|param| param.type_ref)
            })
            .map(|ty| {
                normalize_label(
                    ty.trim_end_matches('?')
                        .rsplit(['.', ':'])
                        .next()
                        .unwrap_or(&ty),
                )
            })
    };
    if let Some(owner) = owner {
        let candidates = members.get(&(owner, normalize_label(member)))?;
        return (candidates.len() == 1).then(|| candidates[0].clone());
    }

    // Java local fields are not part of method signatures. Preserve the old
    // useful fallback only when the receiver is a value (not an explicit type)
    // and exactly one Java method has that name repository-wide.
    if source.ends_with(".java") {
        let member = normalize_label(member);
        let candidates: Vec<_> = members
            .iter()
            .filter(|((_, name), _)| name == &member)
            .flat_map(|(_, ids)| ids)
            .filter(|id| {
                kg.node(id)
                    .is_some_and(|node| node.source_file.to_ascii_lowercase().ends_with(".java"))
            })
            .collect();
        return (candidates.len() == 1).then(|| candidates[0].clone());
    }
    None
}

fn source_family(path: &str) -> Option<String> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(
        match extension.as_str() {
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => "ecmascript",
            "rs" => "rust",
            "c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => "native",
            "java" | "kt" | "kts" | "groovy" | "scala" => "jvm",
            _ => extension.as_str(),
        }
        .to_string(),
    )
}

fn call_candidates(kg: &KnowledgeGraph, call: &RawCall, ids: &[NodeId]) -> Vec<NodeId> {
    let family = source_family(&call.source_file);
    let candidates: Vec<_> = ids
        .iter()
        .filter_map(|id| {
            let node = kg.node(id)?;
            if family.is_some() && source_family(&node.source_file) != family {
                return None;
            }
            if matches!(family.as_deref(), Some("ecmascript" | "rust" | "native"))
                && !matches!(
                    node.kind(),
                    Some(
                        synaptic_core::NodeKind::Function
                            | synaptic_core::NodeKind::Method
                            | synaptic_core::NodeKind::Constructor
                    )
                )
            {
                return None;
            }
            Some(id.clone())
        })
        .collect();
    let implementations: Vec<_> = candidates
        .iter()
        .filter(|id| {
            kg.node(id).is_some_and(|node| {
                node.extra
                    .get("_declaration_only")
                    .and_then(|value| value.as_bool())
                    != Some(true)
            })
        })
        .cloned()
        .collect();
    if implementations.is_empty() {
        candidates
    } else {
        implementations
    }
}

fn source_stem(source_file: &str) -> String {
    Path::new(source_file)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `normalized_label -> node ids` for conservative cross-file resolution.
fn build_label_index(kg: &KnowledgeGraph) -> HashMap<String, Vec<NodeId>> {
    let mut idx: HashMap<String, Vec<NodeId>> = HashMap::new();
    for n in kg.nodes() {
        if !is_resolvable(n) {
            continue;
        }
        idx.entry(normalize_label(&n.label))
            .or_default()
            .push(n.id.clone());
    }
    idx
}

/// `(module_stem, normalized_label) -> node ids` — stricter than the label index;
/// import evidence resolves calls that global label uniqueness alone cannot.
fn build_symbol_index(kg: &KnowledgeGraph) -> HashMap<(String, String), Vec<NodeId>> {
    let mut idx: HashMap<(String, String), Vec<NodeId>> = HashMap::new();
    for n in kg.nodes() {
        if !is_resolvable(n) {
            continue;
        }
        let stem = source_stem(&n.source_file);
        if stem.is_empty() {
            continue;
        }
        idx.entry((stem, normalize_label(&n.label)))
            .or_default()
            .push(n.id.clone());
    }
    idx
}

#[allow(clippy::too_many_arguments)]
fn calls_edge(
    caller: NodeId,
    target: NodeId,
    confidence: Confidence,
    score: f32,
    context: &str,
    source_file: String,
    source_location: Option<String>,
) -> Edge {
    Edge {
        source: caller,
        target,
        relation: "calls".to_string(),
        confidence,
        confidence_score: Some(score),
        source_file,
        source_location,
        weight: 1.0,
        context: Some(context.to_string()),
        cross_repo: false,
        extra: Map::new(),
    }
}

fn is_bash_file(path: &str) -> bool {
    path.ends_with(".sh") || path.ends_with(".bash")
}

/// Bash-specific resolution: a call in a file
/// that `source`d another file resolves to a function defined in a **sourced**
/// file, scoped to that sourced set. This resolves calls the generic passes miss
/// (a name that's globally ambiguous but unique among the sourced files) and at
/// EXTRACTED confidence (the source relationship proves the target). Emits an
/// edge only when the callee matches exactly one function across all sourced
/// files.
fn resolve_bash_sources(
    kg: &KnowledgeGraph,
    raw_calls: &[RawCall],
    sourced: &HashMap<NodeId, HashSet<NodeId>>,
    known: &mut HashSet<(NodeId, NodeId, String)>,
) -> Vec<Edge> {
    // No bash `source` edges, nothing to do (the common non-bash case pays only
    // the cheap `is_bash_file` check folded into the caller's edge loop, plus this
    // early return, no extra graph scans).
    if sourced.is_empty() {
        return Vec::new();
    }
    // The file-node id for a path, collapses any slash style, so it equals both
    // the `imports_from` target id and a function's owning-file id.
    let file_id = |path: &str| NodeId(synaptic_core::make_id(&[path]));

    // functions_by_file[file_id][normalized label] = bash function node ids.
    let mut functions_by_file: HashMap<NodeId, HashMap<String, Vec<NodeId>>> = HashMap::new();
    for n in kg.nodes() {
        if !is_bash_file(&n.source_file) || !n.label.trim().ends_with("()") {
            continue;
        }
        let key = normalize_label(&n.label);
        if key.is_empty() {
            continue;
        }
        functions_by_file
            .entry(file_id(&n.source_file))
            .or_default()
            .entry(key)
            .or_default()
            .push(n.id.clone());
    }

    let mut out = Vec::new();
    for rc in raw_calls {
        if rc.is_member_call || !is_bash_file(&rc.source_file) {
            continue;
        }
        let callee = normalize_label(&rc.callee);
        if callee.is_empty() {
            continue;
        }
        let Some(srcset) = sourced.get(&file_id(&rc.source_file)) else {
            continue;
        };
        let matches: Vec<NodeId> = srcset
            .iter()
            .filter_map(|sf| functions_by_file.get(sf))
            .filter_map(|byname| byname.get(&callee))
            .flatten()
            .cloned()
            .collect();
        if matches.len() != 1 || rc.caller == matches[0] {
            continue;
        }
        let target = matches
            .into_iter()
            .next()
            .expect("exactly one match (len checked above)");
        if !known.insert((rc.caller.clone(), target.clone(), "calls".to_string())) {
            continue;
        }
        let mut edge = calls_edge(
            rc.caller.clone(),
            target,
            Confidence::Extracted,
            1.0,
            "bash_source_call",
            rc.source_file.clone(),
            rc.source_location.clone(),
        );
        edge.extra
            .insert("metadata".to_string(), json!({ "resolver": "bash_source" }));
        out.push(edge);
    }
    out
}

fn parse_ql_call(callee: &str) -> Option<(Option<&str>, &str, usize)> {
    let encoded = callee.strip_prefix("ql:")?;
    let (name_with_qualifier, arity) = encoded.rsplit_once('/')?;
    let arity = arity.parse().ok()?;
    let (qualifier, name) = match name_with_qualifier.rsplit_once("::") {
        Some((qualifier, name)) => (Some(qualifier), name),
        None => (None, name_with_qualifier),
    };
    (!name.is_empty()).then_some((qualifier, name, arity))
}

fn ql_import_suffix(spec: &str) -> String {
    let compact: String = spec.chars().filter(|c| !c.is_whitespace()).collect();
    let base = compact.split("::").next().unwrap_or(&compact);
    let base = base.split('<').next().unwrap_or(base);
    base.replace('.', "/")
}

fn ql_common_path_prefix_len(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(left, right)| left.eq_ignore_ascii_case(right))
        .count()
}

type QlModuleSourceIndex = HashMap<String, Vec<String>>;

fn ql_path_suffixes(path: &str) -> Vec<String> {
    let normalized = path.replace('\\', "/");
    let without_ext = normalized
        .strip_suffix(".qll")
        .or_else(|| normalized.strip_suffix(".ql"))
        .unwrap_or(&normalized);
    let parts: Vec<&str> = without_ext.split('/').collect();
    (0..parts.len())
        .map(|start| parts[start..].join("/").to_ascii_lowercase())
        .collect()
}

fn build_ql_module_source_index(source_files: &HashSet<String>) -> QlModuleSourceIndex {
    let mut index = HashMap::new();
    for source in source_files {
        for suffix in ql_path_suffixes(source) {
            index
                .entry(suffix)
                .or_insert_with(Vec::new)
                .push(source.clone());
        }
    }
    index
}

fn ql_module_sources<'a>(
    spec: &str,
    importer: &str,
    source_index: &'a QlModuleSourceIndex,
) -> Vec<&'a str> {
    let suffix = ql_import_suffix(spec).to_ascii_lowercase();
    if suffix.is_empty() {
        return Vec::new();
    }
    let mut candidates: Vec<&str> = source_index
        .get(&suffix)
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
    if candidates.len() <= 1 {
        return candidates;
    }

    let importer = importer.replace('\\', "/");
    let best = candidates
        .iter()
        .map(|path| ql_common_path_prefix_len(&importer, path))
        .max()
        .unwrap_or(0);
    candidates.retain(|path| ql_common_path_prefix_len(&importer, path) == best);
    candidates
}

/// Resolve QL predicate calls with arity and module-import evidence. QL permits
/// overloads, so the generic label-only pass is insufficient: `flow/2` and
/// `flow/3` intentionally share the display label `flow()`.
fn resolve_ql_calls(
    kg: &KnowledgeGraph,
    raw_calls: &[RawCall],
    imports: &[ImportRecord],
    known: &mut HashSet<(NodeId, NodeId, String)>,
) -> Vec<Edge> {
    let mut by_source: HashMap<(String, String, usize), Vec<NodeId>> = HashMap::new();
    let mut global: HashMap<(String, usize), Vec<NodeId>> = HashMap::new();
    let mut source_files = HashSet::new();
    for node in kg.nodes() {
        if node.extra.get("_language").and_then(|v| v.as_str()) != Some("ql") {
            continue;
        }
        let Some(arity) = node.extra.get("ql_arity").and_then(|v| v.as_u64()) else {
            continue;
        };
        if !matches!(
            node.kind(),
            Some(synaptic_core::NodeKind::Function | synaptic_core::NodeKind::Constructor)
        ) {
            continue;
        }
        let name = normalize_label(&node.label);
        if name.is_empty() || node.source_file.is_empty() {
            continue;
        }
        let source = node.source_file.replace('\\', "/");
        source_files.insert(source.clone());
        by_source
            .entry((source, name.clone(), arity as usize))
            .or_default()
            .push(node.id.clone());
        global
            .entry((name, arity as usize))
            .or_default()
            .push(node.id.clone());
    }
    if by_source.is_empty() {
        return Vec::new();
    }
    let module_source_index = build_ql_module_source_index(&source_files);

    let mut module_imports: HashMap<&str, Vec<&ImportRecord>> = HashMap::new();
    for import in imports.iter().filter(|import| import.imported_name == "*") {
        module_imports
            .entry(import.source_file.as_str())
            .or_default()
            .push(import);
    }

    let mut out = Vec::new();
    for call in raw_calls {
        if call.is_member_call {
            continue;
        }
        let Some((qualifier, name, arity)) = parse_ql_call(&call.callee) else {
            continue;
        };
        let normalized_name = name.to_ascii_lowercase();
        let imports = module_imports
            .get(call.source_file.as_str())
            .cloned()
            .unwrap_or_default();
        let selected: Vec<&ImportRecord> = match qualifier {
            Some(qualifier) => {
                let root = qualifier.split("::").next().unwrap_or(qualifier);
                imports
                    .into_iter()
                    .filter(|import| import.local_name.eq_ignore_ascii_case(root))
                    .collect()
            }
            None => imports,
        };

        let mut candidates = Vec::new();
        for import in selected {
            for source in
                ql_module_sources(&import.module_stem, &call.source_file, &module_source_index)
            {
                if let Some(matches) =
                    by_source.get(&(source.to_string(), normalized_name.clone(), arity))
                {
                    candidates.extend(matches.iter().cloned());
                }
            }
        }
        candidates.sort();
        candidates.dedup();

        let (target, confidence, score, context) = if candidates.len() == 1 {
            (
                candidates.remove(0),
                Confidence::Extracted,
                1.0,
                "ql_import_call",
            )
        } else if candidates.is_empty() {
            let Some(matches) = global.get(&(normalized_name, arity)) else {
                continue;
            };
            if matches.len() != 1 {
                continue;
            }
            (
                matches[0].clone(),
                Confidence::Inferred,
                0.8,
                "ql_unique_call",
            )
        } else {
            continue;
        };
        if target == call.caller
            || !known.insert((call.caller.clone(), target.clone(), "calls".to_string()))
        {
            continue;
        }
        let mut edge = calls_edge(
            call.caller.clone(),
            target,
            confidence,
            score,
            context,
            call.source_file.clone(),
            call.source_location.clone(),
        );
        edge.extra.insert(
            "metadata".to_string(),
            json!({
                "resolver": context,
                "ql_name": name,
                "ql_arity": arity,
                "ql_qualifier": qualifier,
            }),
        );
        out.push(edge);
    }
    out
}

/// Resolve `raw_calls` against the built graph, returning the new `calls` edges
/// (bash sourced-calls + import-guided EXTRACTED, then single-candidate cross-file
/// INFERRED). Endpoints are canonical node ids; the caller adds them to the graph
/// (which drops any whose endpoints don't exist).
pub fn resolve_symbols(
    kg: &KnowledgeGraph,
    raw_calls: &[RawCall],
    imports: &[ImportRecord],
) -> Vec<Edge> {
    // Single edge pass: seed dedup with existing (source, target, relation)
    // triples (so we never duplicate an intra-file `calls` edge emitted at
    // extraction) AND collect bash `source` edges for Pass 0, no extra graph scan.
    let mut known: HashSet<(NodeId, NodeId, String)> = HashSet::new();
    let mut bash_sourced: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for e in kg.edges() {
        known.insert((e.source.clone(), e.target.clone(), e.relation.clone()));
        if e.relation == "imports_from" && is_bash_file(&e.source_file) {
            bash_sourced
                .entry(e.source.clone())
                .or_default()
                .insert(e.target.clone());
        }
    }
    // Pass 0: bash sourced-function calls (EXTRACTED, sourced-scoped)
    // Runs first so its EXTRACTED edges win and dedup blocks a weaker INFERRED
    // duplicate from the generic cross-file pass.
    let mut out: Vec<Edge> = resolve_bash_sources(kg, raw_calls, &bash_sourced, &mut known);
    out.extend(resolve_ql_calls(kg, raw_calls, imports, &mut known));

    // C# keeps the member receiver (`Type.Method` / `this.Method` / typed
    // parameter calls), so exact owner+method matches can resolve across files
    // without guessing by globally-unique method name.
    let members = member_call_index(kg);
    let owned_members = owned_member_call_index(kg);
    let owners = method_owners(kg);
    for call in raw_calls {
        let Some(target) = typed_member_target(kg, call, &members, &owned_members, &owners) else {
            continue;
        };
        if call.caller == target
            || !known.insert((call.caller.clone(), target.clone(), "calls".to_string()))
        {
            continue;
        }
        out.push(calls_edge(
            call.caller.clone(),
            target,
            Confidence::Extracted,
            1.0,
            if call.source_file.to_ascii_lowercase().ends_with(".xaml") {
                "xaml_event_handler"
            } else if call.source_file.to_ascii_lowercase().ends_with(".rs") {
                "rust_typed_member_call"
            } else {
                "typed_member_call"
            },
            call.source_file.clone(),
            call.source_location.clone(),
        ));
    }

    // Pass 1: import-guided (EXTRACTED, 1.0)
    let symbol_index = build_symbol_index(kg);
    let mut aliases_by_file: HashMap<&str, HashMap<&str, &ImportRecord>> = HashMap::new();
    let mut namespaces_by_file: HashMap<&str, HashMap<&str, &ImportRecord>> = HashMap::new();
    for imp in imports {
        if imp.imported_name == "*" {
            namespaces_by_file
                .entry(imp.source_file.as_str())
                .or_default()
                .insert(imp.local_name.as_str(), imp);
            continue;
        }
        aliases_by_file
            .entry(imp.source_file.as_str())
            .or_default()
            .insert(imp.local_name.as_str(), imp);
    }
    for rc in raw_calls.iter().filter(|call| call.is_member_call) {
        let Some((receiver, member)) = rc.callee.rsplit_once('.') else {
            continue;
        };
        let Some(imported) = namespaces_by_file
            .get(rc.source_file.as_str())
            .and_then(|aliases| aliases.get(receiver))
        else {
            continue;
        };
        let Some(cands) =
            symbol_index.get(&(imported.module_stem.clone(), member.to_ascii_lowercase()))
        else {
            continue;
        };
        let cands = call_candidates(kg, rc, cands);
        if cands.len() != 1 || rc.caller == cands[0] {
            continue;
        }
        let target = cands[0].clone();
        if known.insert((rc.caller.clone(), target.clone(), "calls".to_string())) {
            out.push(calls_edge(
                rc.caller.clone(),
                target,
                Confidence::Extracted,
                1.0,
                "namespace_import_call",
                rc.source_file.clone(),
                rc.source_location.clone(),
            ));
        }
    }
    for rc in raw_calls {
        if rc.is_member_call
            || rc.callee.starts_with("ql:")
            || rc.source_file.to_ascii_lowercase().ends_with(".cs")
        {
            continue;
        }
        let callee = rc.callee.trim();
        if callee.is_empty() {
            continue;
        }
        let Some(aliases) = aliases_by_file.get(rc.source_file.as_str()) else {
            continue;
        };
        let Some(imported) = aliases.get(callee) else {
            continue;
        };
        let key = (
            imported.module_stem.clone(),
            imported.imported_name.to_lowercase(),
        );
        let Some(cands) = symbol_index.get(&key) else {
            continue;
        };
        let cands = call_candidates(kg, rc, cands);
        if cands.len() != 1 {
            continue;
        }
        let target = cands[0].clone();
        if rc.caller == target {
            continue;
        }
        if !known.insert((rc.caller.clone(), target.clone(), "calls".to_string())) {
            continue;
        }
        let mut edge = calls_edge(
            rc.caller.clone(),
            target,
            Confidence::Extracted,
            1.0,
            "import_guided_call",
            rc.source_file.clone(),
            // Empty string is treated as "absent", so fall back to the import
            // site's location.
            rc.source_location
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| imported.source_location.clone()),
        );
        // Provenance block: sanitized `metadata` on the import-guided edge.
        edge.extra.insert(
            "metadata".to_string(),
            json!({
                "resolver": "python_import_guided",
                "local_name": imported.local_name,
                "imported_name": imported.imported_name,
                "module_stem": imported.module_stem,
                "import_source_location": imported.source_location,
            }),
        );
        out.push(edge);
    }

    // Pass 2: cross-file single-candidate (INFERRED, 0.8)
    let label_index = build_label_index(kg);
    for rc in raw_calls {
        if rc.is_member_call
            || rc.callee.starts_with("ql:")
            || rc.source_file.to_ascii_lowercase().ends_with(".cs")
        {
            continue;
        }
        let callee = rc.callee.trim();
        if callee.is_empty() {
            continue;
        }
        if aliases_by_file
            .get(rc.source_file.as_str())
            .is_some_and(|aliases| aliases.contains_key(callee))
        {
            continue;
        }
        let Some(cands) = label_index.get(&callee.to_lowercase()) else {
            continue;
        };
        let cands = call_candidates(kg, rc, cands);
        if cands.len() != 1 {
            continue;
        }
        let target = cands[0].clone();
        if rc.caller == target {
            continue;
        }
        if !known.insert((rc.caller.clone(), target.clone(), "calls".to_string())) {
            continue;
        }
        out.push(calls_edge(
            rc.caller.clone(),
            target,
            Confidence::Inferred,
            0.8,
            "call",
            rc.source_file.clone(),
            rc.source_location.clone(),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaptic_core::{FileType, GraphData};

    fn node(id: &str, label: &str, sf: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: sf.into(),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
            ..Default::default()
        }
    }

    fn function_node(id: &str, label: &str, sf: &str) -> Node {
        let mut n = node(id, label, sf);
        n.set_kind(synaptic_core::NodeKind::Function);
        n
    }

    fn kg(nodes: Vec<Node>, links: Vec<Edge>) -> KnowledgeGraph {
        KnowledgeGraph::from_graph_data(GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links,
            hyperedges: vec![],
            built_at_commit: None,
        })
    }

    fn raw(caller: &str, callee: &str, member: bool, sf: &str) -> RawCall {
        RawCall {
            caller: NodeId(caller.into()),
            callee: callee.into(),
            is_member_call: member,
            source_file: sf.into(),
            source_location: Some("L2".into()),
            span: None,
        }
    }

    fn imp(local: &str, imported: &str, stem: &str, sf: &str) -> ImportRecord {
        ImportRecord {
            local_name: local.into(),
            imported_name: imported.into(),
            module_stem: stem.into(),
            source_file: sf.into(),
            source_location: Some("L1".into()),
        }
    }

    fn ql_predicate(id: &str, label: &str, sf: &str, arity: usize) -> Node {
        let mut n = node(id, label, sf);
        n.set_kind(synaptic_core::NodeKind::Function);
        n.extra.insert("_language".into(), json!("ql"));
        n.extra.insert("ql_arity".into(), json!(arity));
        n
    }

    #[test]
    fn import_guided_resolves_extracted() {
        // a.py: `from helper import transform`; caller calls transform().
        let g = kg(
            vec![
                node("a_caller", "caller()", "a.py"),
                node("helper_transform", "transform()", "helper.py"),
            ],
            vec![],
        );
        let edges = resolve_symbols(
            &g,
            &[raw("a_caller", "transform", false, "a.py")],
            &[imp("transform", "transform", "helper", "a.py")],
        );
        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        assert_eq!(e.source, NodeId("a_caller".into()));
        assert_eq!(e.target, NodeId("helper_transform".into()));
        assert_eq!(e.confidence, Confidence::Extracted);
        assert_eq!(e.confidence_score, Some(1.0));
        assert_eq!(e.context.as_deref(), Some("import_guided_call"));
        // Provenance metadata is carried on the import-guided edge.
        let meta = e.extra.get("metadata").expect("metadata present");
        assert_eq!(meta["resolver"], "python_import_guided");
        assert_eq!(meta["imported_name"], "transform");
        assert_eq!(meta["module_stem"], "helper");
    }

    #[test]
    fn ql_import_alias_and_arity_resolve_exactly() {
        let g = kg(
            vec![
                ql_predicate("query", "query()", "java/ql/src/Query.ql", 0),
                ql_predicate(
                    "java_flow_2",
                    "flow()",
                    "java/ql/lib/semmle/code/java/DataFlow.qll",
                    2,
                ),
                ql_predicate(
                    "java_flow_3",
                    "flow()",
                    "java/ql/lib/semmle/code/java/DataFlow.qll",
                    3,
                ),
                ql_predicate(
                    "cpp_flow_2",
                    "flow()",
                    "cpp/ql/lib/semmle/code/cpp/DataFlow.qll",
                    2,
                ),
            ],
            vec![],
        );
        let edges = resolve_symbols(
            &g,
            &[raw("query", "ql:DF::flow/2", false, "java/ql/src/Query.ql")],
            &[imp(
                "DF",
                "*",
                "semmle.code.java.DataFlow",
                "java/ql/src/Query.ql",
            )],
        );
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].target, NodeId("java_flow_2".into()));
        assert_eq!(edges[0].confidence, Confidence::Extracted);
        assert_eq!(edges[0].context.as_deref(), Some("ql_import_call"));
        assert_eq!(edges[0].extra["metadata"]["ql_arity"], json!(2));
    }

    #[test]
    fn ql_unique_fallback_is_arity_aware() {
        let g = kg(
            vec![
                ql_predicate("query", "query()", "queries/Query.ql", 0),
                ql_predicate("target_1", "helper()", "lib/Helpers.qll", 1),
                ql_predicate("target_2", "helper()", "lib/Helpers.qll", 2),
            ],
            vec![],
        );
        let edges = resolve_symbols(
            &g,
            &[raw("query", "ql:helper/2", false, "queries/Query.ql")],
            &[],
        );
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].target, NodeId("target_2".into()));
        assert_eq!(edges[0].confidence, Confidence::Inferred);
        assert_eq!(edges[0].context.as_deref(), Some("ql_unique_call"));
    }

    fn imports_from_edge(src: &str, tgt: &str, sf: &str) -> Edge {
        Edge {
            source: NodeId(src.into()),
            target: NodeId(tgt.into()),
            relation: "imports_from".into(),
            confidence: Confidence::Extracted,
            confidence_score: Some(1.0),
            source_file: sf.into(),
            source_location: Some("L1".into()),
            weight: 1.0,
            context: Some("import".into()),
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn bash_sourced_call_resolves_extracted_and_scoped() {
        // a/app.sh sources a/lib.sh and calls greet(); greet is defined in lib.sh
        // AND (ambiguously) in an UNsourced b/other.sh. The sourced scope picks
        // lib.sh's greet at EXTRACTED, where the global pass would refuse (the
        // name is globally ambiguous).
        let app = synaptic_core::make_id(&["a/app.sh"]);
        let lib = synaptic_core::make_id(&["a/lib.sh"]);
        let g = kg(
            vec![
                node(&app, "app.sh", "a/app.sh"),
                node(&lib, "lib.sh", "a/lib.sh"), // sourced file's own node (edge target)
                node("app_run", "run()", "a/app.sh"),
                node("lib_greet", "greet()", "a/lib.sh"),
                node("other_greet", "greet()", "b/other.sh"), // not sourced
            ],
            vec![imports_from_edge(&app, &lib, "a/app.sh")],
        );
        let edges = resolve_symbols(&g, &[raw("app_run", "greet", false, "a/app.sh")], &[]);
        let calls: Vec<_> = edges.iter().filter(|e| e.relation == "calls").collect();
        assert_eq!(calls.len(), 1, "{edges:?}");
        assert_eq!(calls[0].target, NodeId("lib_greet".into()));
        assert_eq!(calls[0].confidence, Confidence::Extracted);
        assert_eq!(calls[0].context.as_deref(), Some("bash_source_call"));
    }

    #[test]
    fn cross_file_single_candidate_is_inferred() {
        // No import record, so falls to the global single-candidate pass.
        let g = kg(
            vec![
                node("a_caller", "caller()", "a.py"),
                node("helper_transform", "transform()", "helper.py"),
            ],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("a_caller", "transform", false, "a.py")], &[]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].confidence, Confidence::Inferred);
        assert_eq!(edges[0].confidence_score, Some(0.8));
        assert_eq!(edges[0].context.as_deref(), Some("call"));
    }

    #[test]
    fn imported_external_name_does_not_fall_back_to_unrelated_global_symbol() {
        let g = kg(
            vec![
                function_node("caller", "run()", "route.test.ts"),
                function_node("unrelated", ".test()", "route.ts"),
            ],
            vec![],
        );
        let edges = resolve_symbols(
            &g,
            &[raw("caller", "test", false, "route.test.ts")],
            &[imp("test", "test", "node:test", "route.test.ts")],
        );
        assert!(edges.is_empty(), "{edges:?}");
    }

    #[test]
    fn namespace_import_member_resolves_to_imported_module_symbol() {
        let g = kg(
            vec![
                function_node("caller", "run()", "api.ts"),
                function_node("util_normalize", "normalizeParams()", "util.ts"),
                function_node("other_normalize", "normalizeParams()", "other.ts"),
            ],
            vec![],
        );
        let edges = resolve_symbols(
            &g,
            &[raw("caller", "util.normalizeParams", true, "api.ts")],
            &[imp("util", "*", "util", "api.ts")],
        );
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].target, NodeId("util_normalize".into()));
        assert_eq!(edges[0].context.as_deref(), Some("namespace_import_call"));
        assert_eq!(edges[0].confidence, Confidence::Extracted);
    }

    #[test]
    fn import_guided_call_prefers_overload_implementation() {
        let signature = |id: &str| {
            let mut node = function_node(id, "parse()", "parser.ts");
            node.extra.insert("_declaration_only".into(), json!(true));
            node
        };
        let g = kg(
            vec![
                function_node("caller", "run()", "api.ts"),
                signature("parse_string"),
                signature("parse_number"),
                function_node("parse_impl", "parse()", "parser.ts"),
            ],
            vec![],
        );
        let edges = resolve_symbols(
            &g,
            &[raw("caller", "parse", false, "api.ts")],
            &[imp("parse", "parse", "parser", "api.ts")],
        );
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].target, NodeId("parse_impl".into()));
    }

    #[test]
    fn typescript_this_member_resolves_with_owner_evidence() {
        let mut service = node("service", "Service", "service.ts");
        service.set_kind(synaptic_core::NodeKind::Class);
        let mut run = function_node("run", ".run()", "service.ts");
        run.set_kind(synaptic_core::NodeKind::Method);
        let mut stop = function_node("stop", ".stop()", "service.ts");
        stop.set_kind(synaptic_core::NodeKind::Method);
        let method = |target: &str| Edge {
            source: NodeId("service".into()),
            target: NodeId(target.into()),
            relation: "method".into(),
            confidence: Confidence::Extracted,
            confidence_score: Some(1.0),
            source_file: "service.ts".into(),
            source_location: Some("L1".into()),
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        };
        let g = kg(
            vec![service, run, stop],
            vec![method("run"), method("stop")],
        );
        let edges = resolve_symbols(&g, &[raw("run", "this.stop", true, "service.ts")], &[]);
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].target, NodeId("stop".into()));
        assert_eq!(edges[0].context.as_deref(), Some("typed_member_call"));
    }

    #[test]
    fn rust_scoped_member_resolves_to_named_owner() {
        let g = kg(
            vec![
                function_node("caller", "run()", "runner.rs"),
                node("runner", "Runner", "runner.rs"),
                function_node("runner_new", ".new()", "runner.rs"),
                node("three", "Three", "other.rs"),
                function_node("three_new", ".new()", "other.rs"),
            ],
            vec![
                Edge {
                    source: NodeId("runner".into()),
                    target: NodeId("runner_new".into()),
                    relation: "method".into(),
                    confidence: Confidence::Extracted,
                    confidence_score: Some(1.0),
                    source_file: "runner.rs".into(),
                    source_location: Some("L1".into()),
                    weight: 1.0,
                    context: None,
                    cross_repo: false,
                    extra: Map::new(),
                },
                Edge {
                    source: NodeId("three".into()),
                    target: NodeId("three_new".into()),
                    relation: "method".into(),
                    confidence: Confidence::Extracted,
                    confidence_score: Some(1.0),
                    source_file: "other.rs".into(),
                    source_location: Some("L1".into()),
                    weight: 1.0,
                    context: None,
                    cross_repo: false,
                    extra: Map::new(),
                },
            ],
        );
        let edges = resolve_symbols(&g, &[raw("caller", "Runner.new", true, "runner.rs")], &[]);
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].target, NodeId("runner_new".into()));
    }

    #[test]
    fn c_call_ignores_same_named_symbol_from_another_language() {
        let g = kg(
            vec![
                function_node("caller", "run()", "a.c"),
                function_node("c_target", "deflateParams()", "deflate.c"),
                function_node("pascal_target", "deflateParams()", "pascal.pas"),
            ],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("caller", "deflateParams", false, "a.c")], &[]);
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].target, NodeId("c_target".into()));
    }

    #[test]
    fn ecmascript_call_does_not_resolve_to_a_type() {
        let mut mapper = node("mapper", "Mapper", "index.d.ts");
        mapper.set_kind(synaptic_core::NodeKind::Class);
        let g = kg(
            vec![function_node("caller", "run()", "index.js"), mapper],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("caller", "mapper", false, "index.js")], &[]);
        assert!(edges.is_empty(), "{edges:?}");
    }

    #[test]
    fn python_call_does_not_resolve_to_same_named_ruby_symbol() {
        let g = kg(
            vec![
                function_node("caller", "summary.py", "summary.py"),
                function_node("ruby_run", ".run()", "summary.rb"),
            ],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("caller", "run", false, "summary.py")], &[]);
        assert!(edges.is_empty(), "{edges:?}");
    }

    #[test]
    fn ambiguous_label_is_not_resolved() {
        // Two `transform()` definitions: cross-file refuses to guess.
        let g = kg(
            vec![
                node("a_caller", "caller()", "a.py"),
                node("h1_transform", "transform()", "h1.py"),
                node("h2_transform", "transform()", "h2.py"),
            ],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("a_caller", "transform", false, "a.py")], &[]);
        assert!(
            edges.is_empty(),
            "ambiguous name must not resolve: {edges:?}"
        );
    }

    #[test]
    fn member_calls_are_skipped() {
        let g = kg(
            vec![
                node("a_caller", "caller()", "a.py"),
                node("helper_transform", "transform()", "helper.py"),
            ],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("a_caller", "transform", true, "a.py")], &[]);
        assert!(edges.is_empty());
    }

    #[test]
    fn qualified_csharp_member_call_resolves_to_the_named_type() {
        let g = kg(
            vec![
                node("caller", ".OnStartup()", "App.cs"),
                node(
                    "coordinator",
                    "StandaloneUpdateCoordinator",
                    "Coordinator.cs",
                ),
                node("prepare", ".PrepareForUninstall()", "Coordinator.cs"),
            ],
            vec![Edge {
                source: NodeId("coordinator".into()),
                target: NodeId("prepare".into()),
                relation: "method".into(),
                confidence: Confidence::Extracted,
                confidence_score: Some(1.0),
                source_file: "Coordinator.cs".into(),
                source_location: Some("L2".into()),
                weight: 1.0,
                context: None,
                cross_repo: false,
                extra: Map::new(),
            }],
        );
        let edges = resolve_symbols(
            &g,
            &[raw(
                "caller",
                "StandaloneUpdateCoordinator.PrepareForUninstall",
                true,
                "App.cs",
            )],
            &[],
        );
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].target, NodeId("prepare".into()));
    }

    #[test]
    fn csharp_bare_call_does_not_guess_a_unique_cross_file_method() {
        let g = kg(
            vec![
                node("caller", ".Apply()", "App.cs"),
                node("equals", ".Equals()", "Other.cs"),
            ],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("caller", "Equals", false, "App.cs")], &[]);
        assert!(
            edges.is_empty(),
            "C# bare call guessed across files: {edges:?}"
        );
    }

    #[test]
    fn java_static_call_resolves_only_to_its_named_owner() {
        let g = kg(
            vec![
                node("caller", ".run()", "Test.java"),
                node("json_array", "JsonArray", "JsonArray.java"),
                node("as_list", ".asList()", "JsonArray.java"),
            ],
            vec![Edge {
                source: NodeId("json_array".into()),
                target: NodeId("as_list".into()),
                relation: "method".into(),
                confidence: Confidence::Extracted,
                confidence_score: Some(1.0),
                source_file: "JsonArray.java".into(),
                source_location: Some("L2".into()),
                weight: 1.0,
                context: None,
                cross_repo: false,
                extra: Map::new(),
            }],
        );
        let edges = resolve_symbols(
            &g,
            &[raw("caller", "Arrays.asList", true, "Test.java")],
            &[],
        );
        assert!(edges.is_empty(), "{edges:?}");
    }

    #[test]
    fn xaml_event_resolves_through_its_code_behind_owner() {
        let mut code_behind = imports_from_edge("xaml", "window", "Views/MainWindow.xaml");
        code_behind.relation = "references".into();
        code_behind.context = Some("xaml_code_behind".into());
        let g = kg(
            vec![
                node("xaml", "MainWindow.xaml", "Views/MainWindow.xaml"),
                node("window", "MainWindow", "Views/MainWindow.cs"),
                node("click", ".Connect_Click()", "Views/MainWindow.cs"),
                node("other", "MainWindow", "Other/MainWindow.cs"),
                node("other_click", ".Connect_Click()", "Other/MainWindow.cs"),
            ],
            vec![
                code_behind,
                Edge {
                    source: NodeId("window".into()),
                    target: NodeId("click".into()),
                    relation: "method".into(),
                    confidence: Confidence::Extracted,
                    confidence_score: Some(1.0),
                    source_file: "Views/MainWindow.cs".into(),
                    source_location: Some("L2".into()),
                    weight: 1.0,
                    context: None,
                    cross_repo: false,
                    extra: Map::new(),
                },
                Edge {
                    source: NodeId("other".into()),
                    target: NodeId("other_click".into()),
                    relation: "method".into(),
                    confidence: Confidence::Extracted,
                    confidence_score: Some(1.0),
                    source_file: "Other/MainWindow.cs".into(),
                    source_location: Some("L2".into()),
                    weight: 1.0,
                    context: None,
                    cross_repo: false,
                    extra: Map::new(),
                },
            ],
        );
        let edges = resolve_symbols(
            &g,
            &[raw(
                "xaml",
                "MainWindow.Connect_Click",
                true,
                "Views/MainWindow.xaml",
            )],
            &[],
        );
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0].target, NodeId("click".into()));
        assert_eq!(edges[0].context.as_deref(), Some("xaml_event_handler"));
    }

    #[test]
    fn existing_calls_edge_is_not_duplicated() {
        let mut existing = calls_edge(
            NodeId("a_caller".into()),
            NodeId("helper_transform".into()),
            Confidence::Extracted,
            1.0,
            "call",
            "a.py".into(),
            None,
        );
        existing.confidence_score = None;
        let g = kg(
            vec![
                node("a_caller", "caller()", "a.py"),
                node("helper_transform", "transform()", "helper.py"),
            ],
            vec![existing],
        );
        let edges = resolve_symbols(&g, &[raw("a_caller", "transform", false, "a.py")], &[]);
        assert!(
            edges.is_empty(),
            "should not duplicate the existing calls edge"
        );
    }

    #[test]
    fn external_stub_nodes_do_not_absorb_calls() {
        // An import stub (empty source_file, label "os") must not be a call target.
        let g = kg(
            vec![node("a_caller", "caller()", "a.py"), {
                let mut n = node("os", "os", "");
                n.source_location = None;
                n
            }],
            vec![],
        );
        let edges = resolve_symbols(&g, &[raw("a_caller", "os", false, "a.py")], &[]);
        assert!(edges.is_empty());
    }
}
