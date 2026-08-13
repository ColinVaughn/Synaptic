use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Write;

use synaptic_core::{Edge, Node, NodeId};
use tree_sitter::{Node as TsNode, Parser};

use crate::config::{LanguageConfig, TypeRefStyle};
use crate::paths::{file_node_id, file_stem};
use crate::result::{ExtractionResult, ImportRecord, RawCall};

mod core;
mod cpp;
mod csharp;
mod ecmascript;
mod java;
mod kotlin;
mod php;
mod python;
mod scala;
mod swift;

/// Recursion guard for both passes — bounds stack depth on pathologically nested
/// input.
const MAX_DEPTH: usize = 2000;

/// Comment markers that flag design rationale.
const RATIONALE_MARKERS: &[&str] = &[
    "NOTE",
    "IMPORTANT",
    "HACK",
    "WHY",
    "RATIONALE",
    "TODO",
    "FIXME",
];
/// Line-comment tokens scanned for those markers — covers Python (`#`),
/// C-family (`//`) and SQL/Lua (`--`).
const COMMENT_TOKENS: &[&str] = &["#", "//", "--"];

pub(crate) fn normalize_ecmascript_source(source: &[u8]) -> Cow<'_, [u8]> {
    if !source.contains(&0) {
        return Cow::Borrowed(source);
    }
    Cow::Owned(
        source
            .iter()
            .map(|byte| if *byte == 0 { b' ' } else { *byte })
            .collect(),
    )
}

/// Blank declaration/control macros while preserving byte offsets and lines.
/// Tree-sitter parses unexpanded C-family source, so annotations such as
/// `ZEXPORT`, `FMT_BEGIN_NAMESPACE`, and `TEST(...) {` otherwise hide real
/// declarations or recover as fake functions.
pub(crate) fn normalize_c_family_source(
    source: &[u8],
    blank_all_conditionals: bool,
) -> Cow<'_, [u8]> {
    let mut normalized: Option<Vec<u8>> = None;
    let mut blanked_conditionals = Vec::new();
    let mut i = 0;
    while i < source.len() {
        if source[i] == b'#' && line_prefix_is_whitespace(source, i) {
            let end = skip_preprocessor(source, i);
            let directive = source[i + 1..end]
                .split(|b| b.is_ascii_whitespace())
                .find(|part| !part.is_empty())
                .unwrap_or_default();
            let blank = blank_all_conditionals
                && matches!(
                    directive,
                    b"if" | b"ifdef" | b"ifndef" | b"elif" | b"else" | b"endif"
                )
                || match directive {
                    b"if" | b"ifdef" | b"ifndef" => {
                        let blank = blanked_conditionals.last().copied().unwrap_or(false)
                            || continues_expression(source, i);
                        blanked_conditionals.push(blank);
                        blank
                    }
                    b"elif" | b"else" => blanked_conditionals.last().copied().unwrap_or(false),
                    b"endif" => blanked_conditionals.pop().unwrap_or(false),
                    _ => false,
                };
            if blank {
                blank_preserving_lines(&mut normalized, source, i, end);
            }
            i = end;
            continue;
        }
        if source[i..].starts_with(b"//") {
            i = source[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(source.len(), |n| i + n + 1);
            continue;
        }
        if source[i..].starts_with(b"/*") {
            i = source[i + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(source.len(), |n| i + n + 4);
            continue;
        }
        if matches!(source[i], b'"' | b'\'') {
            i = skip_quoted(source, i, source[i]);
            continue;
        }
        if !is_ident_start(source[i]) {
            i += 1;
            continue;
        }

        let start = i;
        i += 1;
        while i < source.len() && is_ident_continue(source[i]) {
            i += 1;
        }
        let token = &source[start..i];
        if token == b"extern"
            && next_non_whitespace(source, i).is_some_and(|next| {
                source[next..].starts_with(b"template")
                    && source
                        .get(next + b"template".len())
                        .is_none_or(|b| !is_ident_continue(*b))
            })
            && let Some(end) = source[i..].iter().position(|&b| b == b';')
        {
            let end = i + end + 1;
            blank_preserving_lines(&mut normalized, source, start, end);
            i = end;
            continue;
        }
        if !is_macro_identifier(token) {
            continue;
        }

        if token == b"SLATE_BEGIN_ARGS"
            && let Some(end_start) = source[i..]
                .windows(b"SLATE_END_ARGS".len())
                .position(|window| window == b"SLATE_END_ARGS")
                .map(|position| i + position)
        {
            let after = end_start + b"SLATE_END_ARGS".len();
            let end = next_non_whitespace(source, after)
                .filter(|&p| source[p] == b'(')
                .and_then(|p| matching_paren(source, p))
                .map_or(after, |close| close + 1);
            blank_preserving_lines(&mut normalized, source, start, end);
            i = end;
            continue;
        }

        let open = next_non_whitespace(source, i).filter(|&p| source[p] == b'(');
        let close = open.and_then(|p| matching_paren(source, p));
        let wraps_block = close
            .and_then(|p| next_non_whitespace(source, p + 1))
            .is_some_and(|p| source[p] == b'{');
        let structural = is_standalone_line(source, start, i)
            && (token.windows(6).any(|part| part == b"BEGIN_")
                || token.windows(4).any(|part| part == b"END_"));
        if declaration_macro(token) || structural || wraps_block {
            let end = if declaration_macro(token) {
                close.map_or(i, |p| p + 1)
            } else if wraps_block {
                close.map_or(i, |p| p + 1)
            } else {
                i
            };
            blank_preserving_lines(&mut normalized, source, start, end);
            i = end;
        }
    }
    normalized.map_or(Cow::Borrowed(source), Cow::Owned)
}

fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

fn is_macro_identifier(token: &[u8]) -> bool {
    token.iter().any(u8::is_ascii_uppercase)
        && token
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

fn declaration_macro(token: &[u8]) -> bool {
    let name = std::str::from_utf8(token).unwrap_or("");
    matches!(
        name,
        "FAR"
            | "NEAR"
            | "PASCAL"
            | "CALLBACK"
            | "WINAPI"
            | "APIENTRY"
            | "UCLASS"
            | "UENUM"
            | "UFUNCTION"
            | "UINTERFACE"
            | "UMETA"
            | "UPROPERTY"
            | "USTRUCT"
            | "UE_LOG"
    ) || (name.starts_with("GENERATED_") && name.ends_with("BODY"))
        || [
            "API",
            "EXPORT",
            "IMPORT",
            "INTERNAL",
            "NODISCARD",
            "DEPRECATED",
            "MAYBE_UNUSED",
            "CONSTEXPR",
            "CONSTEVAL",
            "INLINE",
            "VISIBILITY",
            "ATTRIBUTE",
            "DECLSPEC",
            "DIAGNOSTIC",
            "WARNING",
            "PRAGMA",
            "LOCK_REQUIRED",
            "LOCK_EXCLUDED",
            "NOMACRO",
        ]
        .iter()
        .any(|marker| name.contains(marker))
}

fn line_prefix_is_whitespace(source: &[u8], at: usize) -> bool {
    let start = source[..at]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |p| p + 1);
    source[start..at].iter().all(u8::is_ascii_whitespace)
}

fn continues_expression(source: &[u8], at: usize) -> bool {
    source[..at]
        .iter()
        .rev()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| {
            matches!(
                b,
                b'=' | b':' | b'?' | b',' | b'(' | b'+' | b'-' | b'*' | b'/' | b'&' | b'|'
            )
        })
}

fn is_standalone_line(source: &[u8], start: usize, end: usize) -> bool {
    if !line_prefix_is_whitespace(source, start) {
        return false;
    }
    let tail_end = source[end..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(source.len(), |p| end + p);
    let tail = &source[end..tail_end];
    tail.iter().all(u8::is_ascii_whitespace)
        || tail
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .is_some_and(|p| tail[p..].starts_with(b"//"))
}

fn skip_preprocessor(source: &[u8], mut i: usize) -> usize {
    loop {
        let end = source[i..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(source.len(), |p| i + p + 1);
        let continued = source[i..end]
            .iter()
            .rev()
            .find(|&&b| !matches!(b, b'\r' | b'\n' | b' ' | b'\t'))
            == Some(&b'\\');
        i = end;
        if !continued || i == source.len() {
            return i;
        }
    }
}

fn next_non_whitespace(source: &[u8], from: usize) -> Option<usize> {
    source[from..]
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|p| from + p)
}

fn skip_quoted(source: &[u8], mut i: usize, quote: u8) -> usize {
    i += 1;
    while i < source.len() {
        if source[i] == b'\\' {
            i = (i + 2).min(source.len());
        } else if source[i] == quote {
            return i + 1;
        } else {
            i += 1;
        }
    }
    i
}

fn matching_paren(source: &[u8], open: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = open + 1;
    while i < source.len() {
        if source[i..].starts_with(b"//") {
            i = source[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(source.len(), |n| i + n + 1);
        } else if source[i..].starts_with(b"/*") {
            i = source[i + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(source.len(), |n| i + n + 4);
        } else if matches!(source[i], b'"' | b'\'') {
            i = skip_quoted(source, i, source[i]);
        } else {
            match source[i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
    None
}

fn blank_preserving_lines(
    normalized: &mut Option<Vec<u8>>,
    source: &[u8],
    start: usize,
    end: usize,
) {
    let out = normalized.get_or_insert_with(|| source.to_vec());
    for b in &mut out[start..end] {
        if !matches!(*b, b'\r' | b'\n') {
            *b = b' ';
        }
    }
}

/// Keep punctuation-bearing C++ names distinct after `make_id` normalization.
pub(crate) fn c_family_function_id_part(name: &str) -> Cow<'_, str> {
    let chars: Vec<_> = name.chars().collect();
    let safe = |index: usize, ch: char| {
        ch.is_alphanumeric()
            || (ch == '_'
                && index > 0
                && index + 1 < chars.len()
                && chars[index - 1] != '_'
                && chars[index + 1] != '_')
    };
    if chars.iter().enumerate().all(|(index, &ch)| safe(index, ch)) {
        return Cow::Borrowed(name);
    }
    let mut encoded = String::with_capacity(name.len() + 8);
    for (index, &ch) in chars.iter().enumerate() {
        if safe(index, ch) {
            encoded.push(ch);
        } else {
            write!(encoded, "_x{:x}_", ch as u32).expect("write to string");
        }
    }
    Cow::Owned(encoded)
}

/// Strip a Python string literal's surrounding quotes + whitespace (the optional
/// `r`/`f`/`b` prefix is left as-is; only the `"`/`'` quote chars are stripped).
fn strip_py_quotes(s: &str) -> String {
    s.trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim()
        .to_string()
}

/// The docstring of `node`'s body: its first child, if an `expression_statement`
/// wrapping a string > 20 chars → `(text, 1-based line)`.
/// Used for module (`node` = root), class, and function bodies.
fn first_docstring(node: TsNode<'_>, source: &[u8]) -> Option<(String, usize)> {
    let mut cursor = node.walk();
    let first = node.children(&mut cursor).next()?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let mut inner = first.walk();
    for sub in first.children(&mut inner) {
        if matches!(sub.kind(), "string" | "concatenated_string") {
            let text = strip_py_quotes(sub.utf8_text(source).unwrap_or(""));
            if text.chars().count() > 20 {
                return Some((text, first.start_position().row + 1));
            }
        }
    }
    None
}

/// True for files whose module docstring is boilerplate, not rationale
/// (codegen/protobuf/Alembic/Django).
fn is_autogenerated_python(source: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&source[..source.len().min(2048)]);
    head.contains("DO NOT EDIT")
        || head.contains("@generated")
        || head.contains("Generated by the protocol buffer")
        || (head.contains("down_revision") && head.contains("def upgrade("))
        || (head.contains("class Migration(migrations.Migration)") && head.contains("operations"))
}

/// Extract one file's content using `cfg`. Two passes: a structural DFS, then an
/// intra-file call pass over collected function bodies.
pub fn extract_with_config(path: &str, source: &[u8], cfg: &LanguageConfig) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&(cfg.language)())
        .expect("load tree-sitter language");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };

    let mut ex = Extractor {
        cfg,
        source,
        path: path.to_string(),
        nodes: Vec::new(),
        edges: Vec::new(),
        raw_calls: Vec::new(),
        imports: Vec::new(),
        seen: HashSet::new(),
        function_bodies: Vec::new(),
        interface_names: HashSet::new(),
        declared_types: HashSet::new(),
        owned_fn_nodes: HashSet::new(),
    };

    let file_nid = file_node_id(path);
    let file_label = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    ex.add_node(file_nid.clone(), file_label, 1);

    let stem = file_stem(path);
    ex.pre_scan(tree.root_node());
    ex.walk(tree.root_node(), &file_nid, None, &stem, 0);
    ex.run_call_pass(tree.root_node());
    ex.scan_rationale_comments(&file_nid, &stem);
    // Module docstring (Python only; class/function docstrings are handled inline
    // during the walk so there is no second parse). Skips auto-generated files.
    if matches!(cfg.type_ref_style, Some(TypeRefStyle::Python))
        && !is_autogenerated_python(source)
        && let Some((doc, line)) = first_docstring(tree.root_node(), source)
    {
        ex.add_rationale(doc, line, file_nid.clone(), &stem);
    }

    ExtractionResult {
        nodes: ex.nodes,
        edges: ex.edges,
        raw_calls: ex.raw_calls,
        imports: ex.imports,
    }
}

struct Extractor<'cfg, 'src, 'tree> {
    cfg: &'cfg LanguageConfig,
    source: &'src [u8],
    path: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    raw_calls: Vec<RawCall>,
    imports: Vec<ImportRecord>,
    seen: HashSet<NodeId>,
    function_bodies: Vec<(NodeId, TsNode<'tree>)>,
    /// In-file interface/protocol names from a pre-scan, so heritage
    /// classification (C#/Swift) can tell interfaces from base classes. Empty
    /// unless the language's heritage style uses it.
    interface_names: HashSet<String>,
    /// All type declarations in this file, collected before the main walk so
    /// references to a type declared later use its scoped id instead of a
    /// global shadow node.
    declared_types: HashSet<String>,
    /// tree-sitter ids of the function/method nodes that got their OWN graph node
    /// (and whose body is walked on its own). The call pass uses this to tell a
    /// named nested function (skip -- walked separately) from an anonymous callback
    /// (recurse -- its calls belong to the enclosing function).
    owned_fn_nodes: HashSet<usize>,
}

/// True for the C# `IFoo` interface-naming convention: a leading `I` followed by
/// an uppercase letter (`IDisposable` yes, `Item` no).
fn is_csharp_interface_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next() == Some('I') && chars.next().is_some_and(|c| c.is_uppercase())
}

/// Well-known TS built-in type containers/utilities skipped as type references.
/// Primitives are `predefined_type` nodes and never reach here.
fn is_ts_type_noise(name: &str) -> bool {
    matches!(
        name,
        "Array"
            | "ReadonlyArray"
            | "Promise"
            | "Map"
            | "Set"
            | "WeakMap"
            | "WeakSet"
            | "Record"
            | "Readonly"
            | "Partial"
            | "Required"
            | "Pick"
            | "Omit"
            | "Exclude"
            | "Extract"
            | "NonNullable"
            | "Parameters"
            | "ReturnType"
            | "Awaited"
            | "Date"
            | "RegExp"
            | "Error"
    )
}

/// Module stem = last path component of a JS/TS import specifier, with any known
/// JS/TS extension stripped. The cross-file symbol-resolution key
/// (`./util` → `util`, `react` → `react`, `@scope/pkg` → `pkg`, `../a/b.js` → `b`).
fn module_stem(spec: &str) -> String {
    let last = spec.rsplit('/').next().unwrap_or(spec);
    for ext in [".d.ts", ".tsx", ".ts", ".jsx", ".js", ".mjs", ".cjs"] {
        if let Some(base) = last.strip_suffix(ext) {
            return base.to_string();
        }
    }
    last.to_string()
}

/// Resolve a Python relative import (`from .pkg.mod import …`) to the path string
/// whose `make_id` matches the imported file's node id. The relative branch
/// counts leading dots, climbs `dots-1` parents from the importing file's
/// directory, then appends `module/path.py` (or `__init__.py`).
fn resolve_relative_import(file_path: &str, raw: &str) -> String {
    let dots = raw.len() - raw.trim_start_matches('.').len();
    let module = raw.trim_start_matches('.');
    let mut base = std::path::Path::new(file_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    for _ in 0..dots.saturating_sub(1) {
        base = base.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    }
    let rel = if module.is_empty() {
        "__init__.py".to_string()
    } else {
        format!("{}.py", module.replace('.', "/"))
    };
    base.join(rel).to_string_lossy().into_owned()
}

#[cfg(all(test, feature = "lang-python"))]
mod tests;
