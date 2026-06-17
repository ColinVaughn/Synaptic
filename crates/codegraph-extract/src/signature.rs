//! Best-effort function-signature capture from a tree-sitter declaration node.
//!
//! Grammars differ in how they expose parameters and return types, so this
//! module favors a universal `raw` header (always populated) plus a structured
//! breakdown that covers the common shapes. Grammars that diverge (Swift's
//! `name`-reuse, C/C++ declarator nesting) still yield a usable `raw`; the
//! hand-written Rust/Go extractors refine the structured fields themselves.
//!
//! Coverage: this runs for the generic config-driven walker (Python, JS/TS,
//! Java, C#, Kotlin, Swift, C++, PHP, Scala, Groovy) and the hand-written
//! Rust/Go extractors. The remaining custom-walker languages (Ruby, Lua, Bash,
//! PowerShell, Dart, Elixir, Julia, Zig, C, Objective-C, Fortran, Verilog, SQL,
//! ...) build nodes via `Builder::add_node` and capture no signature yet; their
//! function nodes simply carry `None`, which every consumer handles gracefully.

use codegraph_core::{Param, Signature};
use tree_sitter::Node as TsNode;

/// Field names that hold a function's parameter-list node, in priority order.
const PARAM_CONTAINER_FIELDS: &[&str] = &["parameters", "function_value_parameters"];
/// Field names that hold a function's return-type node, in priority order.
/// `type` is the return type on Java/C-family `*_declaration` nodes and `result`
/// is Go's; each is only consulted when the earlier ones are absent.
const RETURN_TYPE_FIELDS: &[&str] = &["return_type", "type", "result"];
/// Field names that hold a parameter's type annotation, in priority order.
const PARAM_TYPE_FIELDS: &[&str] = &["type"];
/// Node kinds that look like a function/class body, used to bound the `raw`
/// header when the node has no `body` field.
const BODY_KINDS: &[&str] = &[
    "block",
    "function_body",
    "compound_statement",
    "statement_block",
];
/// Cap on the captured `raw` header to avoid pathological minified inputs.
const MAX_RAW_LEN: usize = 600;

/// Capture a [`Signature`] from a function/method declaration `node`.
///
/// Call this only for function/method nodes. `raw` is always populated;
/// `params`/`return_type` are filled when the grammar exposes them in a
/// recognized shape.
pub fn extract_signature(node: TsNode, src: &[u8]) -> Signature {
    let params = param_container(node)
        .map(|c| collect_params(c, src))
        .unwrap_or_default();
    let return_type = RETURN_TYPE_FIELDS
        .iter()
        .find_map(|f| node.child_by_field_name(f))
        .map(|n| clean_type(&node_text(n, src)))
        .filter(|t| !t.is_empty());
    Signature {
        params,
        return_type,
        raw: raw_header(node, src),
    }
}

fn node_text(node: TsNode, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

/// Normalize a captured type annotation. Some grammars (TypeScript) expose the
/// type as a `type_annotation` node that includes the leading `: `; strip a
/// single leading colon so `name: string` yields `string`, matching the bare
/// type node Python/Java expose.
fn clean_type(raw: &str) -> String {
    let t = raw.trim();
    t.strip_prefix(':').unwrap_or(t).trim().to_string()
}

fn param_container(node: TsNode) -> Option<TsNode> {
    PARAM_CONTAINER_FIELDS
        .iter()
        .find_map(|f| node.child_by_field_name(f))
}

fn collect_params(container: TsNode, src: &[u8]) -> Vec<Param> {
    let mut out = Vec::new();
    let mut cursor = container.walk();
    for child in container.named_children(&mut cursor) {
        // Annotations/comments can appear among parameters; they are not params.
        if matches!(child.kind(), "comment" | "decorator") {
            continue;
        }
        let type_ref = PARAM_TYPE_FIELDS
            .iter()
            .find_map(|f| child.child_by_field_name(f))
            .map(|n| clean_type(&node_text(n, src)))
            .filter(|t| !t.is_empty());
        // One Param per declared name: a single param node can carry several
        // names that share one type (Go `a, b int`).
        for name in param_names(child, src) {
            out.push(Param {
                name,
                type_ref: type_ref.clone(),
            });
        }
    }
    out
}

/// The parameter name(s) declared by one param node. Usually one; Go's
/// `parameter_declaration` repeats the `name` field for `a, b int`.
fn param_names(param: TsNode, src: &[u8]) -> Vec<String> {
    // A bare identifier parameter (Python `x`, JS `x`).
    if is_identifier_kind(param.kind()) {
        return vec![node_text(param, src)];
    }
    // All `name` fields (Go repeats it); then the single `pattern` field (Rust);
    // then the first identifier-ish named child.
    let mut cursor = param.walk();
    let names: Vec<String> = param
        .children_by_field_name("name", &mut cursor)
        .map(|n| node_text(n, src))
        .collect();
    if !names.is_empty() {
        return names;
    }
    if let Some(n) = param.child_by_field_name("pattern") {
        return vec![node_text(n, src)];
    }
    let mut cursor2 = param.walk();
    for child in param.named_children(&mut cursor2) {
        if is_identifier_kind(child.kind()) {
            return vec![node_text(child, src)];
        }
    }
    Vec::new()
}

fn is_identifier_kind(kind: &str) -> bool {
    // `type_identifier` and friends are types, not parameter names.
    if kind.starts_with("type") {
        return false;
    }
    kind == "identifier" || kind.ends_with("_identifier")
}

fn raw_header(node: TsNode, src: &[u8]) -> String {
    let start = node.start_byte();
    let end = body_start(node).unwrap_or_else(|| node.end_byte());
    let end = end.clamp(start, src.len());
    let text = String::from_utf8_lossy(&src[start..end]);
    // Collapse whitespace/newlines so the header reads as a single line.
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(MAX_RAW_LEN).collect()
}

fn body_start(node: TsNode) -> Option<usize> {
    if let Some(b) = node.child_by_field_name("body") {
        return Some(b.start_byte());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if BODY_KINDS.contains(&child.kind()) {
            return Some(child.start_byte());
        }
    }
    None
}
