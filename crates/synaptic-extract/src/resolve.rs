//! Cross-file pass: bind JS/TS and QL imports to real nodes now that the full
//! file set is known (the per-file extractor only emits specifier-labeled
//! stubs).
//!
//! Three kinds of import are handled:
//! - **relative code** (`./foo`, `../bar`) → bound to the in-corpus file node,
//!   choosing the extension/index from the file set;
//! - **relative non-code** (`./styles.css`, `./data.json`, `./logo.svg`) → minted
//!   as a distinct *asset node* (tagged `asset_kind`), canonicalised so a shared
//!   asset is one node and per-directory files don't collide on `make_id`;
//! - **path aliases** (`@/lib/api`) → expanded via the [`AliasResolver`] (parsed
//!   tsconfig `paths`) and bound to a real code file or an asset node.
//!
//! Bare packages (`react`) are left as stubs. Run after per-file extraction,
//! before `build_from_parts`.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};
use synaptic_core::{Confidence, Edge, FileType, Node, NodeId, NodeKind};

use crate::paths::file_node_id;
use crate::tsconfig::AliasResolver;

/// Known JS/TS module file extensions, longest-first so `.d.ts` beats `.ts`.
const JS_EXTS: &[&str] = &[".d.ts", ".tsx", ".ts", ".jsx", ".js", ".mjs", ".cjs"];

/// Outcome counts for the CLI summary line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolveStats {
    /// Relative code imports bound to a real file node.
    pub relative_bound: usize,
    /// Alias imports bound to a real code file node.
    pub alias_bound: usize,
    /// Imports repointed to an asset node (relative or alias, non-code target).
    pub assets: usize,
    /// Distinct asset nodes minted.
    pub asset_nodes: usize,
    /// QL module imports bound to a real `.qll`/`.ql` file node.
    pub ql_bound: usize,
    /// XAML code-behind class references bound to their real C# declaration.
    pub xaml_bound: usize,
}

/// Attach out-of-line C++ definitions (`Type::method`) to their class after all
/// headers and implementation files have been extracted.
pub fn attach_cpp_methods(nodes: &mut [Node], edges: &mut Vec<Edge>) -> usize {
    let mut owners: HashMap<String, Vec<(NodeId, String)>> = HashMap::new();
    for node in nodes
        .iter()
        .filter(|node| matches!(node.kind(), Some(NodeKind::Class | NodeKind::Struct)))
    {
        owners
            .entry(node.label.clone())
            .or_default()
            .push((node.id.clone(), node.source_file.clone()));
    }
    let mut seen: HashSet<(NodeId, NodeId)> = edges
        .iter()
        .filter(|edge| edge.relation == "method")
        .map(|edge| (edge.source.clone(), edge.target.clone()))
        .collect();
    let mut attached = 0;
    for node in nodes.iter_mut().filter(|node| {
        node.kind() == Some(NodeKind::Function)
            && std::path::Path::new(&node.source_file)
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| matches!(ext, "cpp" | "cc" | "cxx" | "mm"))
    }) {
        let Some(qualified) = node.label.strip_suffix("()") else {
            continue;
        };
        let Some((owner, method)) = qualified.rsplit_once("::") else {
            continue;
        };
        let owner = owner.rsplit("::").next().unwrap_or(owner);
        let Some(candidates) = owners.get(owner) else {
            continue;
        };
        let same_stem: Vec<_> = candidates
            .iter()
            .filter(|(_, header)| file_stem(header) == file_stem(&node.source_file))
            .collect();
        let class = match same_stem.as_slice() {
            [(id, _)] => (*id).clone(),
            _ if candidates.len() == 1 => candidates[0].0.clone(),
            _ => continue,
        };
        node.set_kind(if method == owner {
            NodeKind::Constructor
        } else {
            NodeKind::Method
        });
        if seen.insert((class.clone(), node.id.clone())) {
            edges.push(Edge {
                source: class,
                target: node.id.clone(),
                relation: "method".into(),
                confidence: Confidence::Extracted,
                source_file: node.source_file.clone(),
                source_location: node.source_location.clone(),
                confidence_score: None,
                weight: 1.0,
                context: Some("out_of_line_definition".into()),
                cross_repo: false,
                extra: Map::new(),
            });
        }
        attached += 1;
    }
    attached
}

fn file_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

#[cfg(all(test, feature = "lang-cpp"))]
mod cpp_method_tests {
    use super::attach_cpp_methods;
    use crate::cpp::extract_cpp_source;
    use synaptic_core::NodeKind;

    #[test]
    fn qualified_definition_attaches_to_header_class() {
        let header = extract_cpp_source(
            "Source/Game/Hero.h",
            b"class GAME_API AHero { public: void TakeDamage(float Amount); };",
        );
        let implementation = extract_cpp_source(
            "Source/Game/Hero.cpp",
            b"void AHero::TakeDamage(float Amount) {}",
        );
        let mut nodes = header.nodes;
        nodes.extend(implementation.nodes);
        let mut edges = header.edges;
        edges.extend(implementation.edges);

        assert_eq!(attach_cpp_methods(&mut nodes, &mut edges), 1);
        let class = nodes.iter().find(|node| node.label == "AHero").unwrap();
        let method = nodes
            .iter()
            .find(|node| node.label == "AHero::TakeDamage()")
            .unwrap();
        assert_eq!(method.kind(), Some(NodeKind::Method));
        assert!(edges.iter().any(|edge| {
            edge.relation == "method" && edge.source == class.id && edge.target == method.id
        }));
    }
}

/// Strip a known JS/TS extension, returning the extensionless path.
fn strip_js_ext(p: &str) -> Option<String> {
    JS_EXTS
        .iter()
        .find_map(|ext| p.strip_suffix(ext).map(str::to_string))
}

/// Classification of an import specifier by its file extension.
enum SpecKind {
    /// Code module (a code extension, or no extension at all → a module path).
    Code,
    /// Non-code asset; carries the `asset_kind` tag.
    Asset(&'static str),
}

/// Map the last path component's extension to a code/asset classification. Only
/// a *recognised* asset extension yields an asset; anything else (a code
/// extension, no extension, or an unrecognised one) is a code module path. This
/// matters because module specifiers routinely contain dots that are part of the
/// name, not an extension — `./index.core`, `./app.config`, `./Foo.test` all
/// resolve to `*.ts`, so a catch-all "unknown ext ⇒ asset" would mint phantom
/// asset nodes for them.
fn spec_kind(spec: &str) -> SpecKind {
    let last = spec.rsplit('/').next().unwrap_or(spec);
    if let Some((_, ext)) = last.rsplit_once('.')
        && !ext.is_empty()
        && let Some(kind) = classify_asset_ext(&ext.to_ascii_lowercase())
    {
        return SpecKind::Asset(kind);
    }
    SpecKind::Code
}

/// Coarse `asset_kind` for a recognised non-code extension, or `None` to leave it
/// as a code module path. The `asset` bucket is an explicit list (not a
/// catch-all) so dotted module names stay code.
fn classify_asset_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "css" | "scss" | "sass" | "less" | "styl" | "pcss" => "stylesheet",
        "json" | "json5" | "jsonc" | "yaml" | "yml" | "toml" | "xml" | "csv" | "tsv"
        | "graphql" | "gql" => "data",
        "svg" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "ico" | "bmp" => "image",
        "woff" | "woff2" | "ttf" | "otf" | "eot" => "font",
        "mp4" | "webm" | "mp3" | "wav" | "ogg" | "mov" => "media",
        "wasm" | "pdf" | "txt" | "md" | "mdx" | "glsl" | "vert" | "frag" | "wgsl" => "asset",
        _ => return None,
    })
}

/// Resolve a relative specifier against the importer directory (posix),
/// normalizing `.`/`..`. `None` if it climbs above the root.
fn join_normalize(dir: &str, spec: &str) -> Option<String> {
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

/// Extensionless posix module key → code file node id, including `dir/index`.
fn build_code_index(nodes: &[Node]) -> HashMap<String, NodeId> {
    let mut by_key: HashMap<String, NodeId> = HashMap::new();
    let mut index_dirs: Vec<(String, NodeId)> = Vec::new();
    for n in nodes {
        if n.source_file.is_empty() || file_node_id(&n.source_file) != n.id {
            continue;
        }
        let posix = n.source_file.replace('\\', "/");
        if let Some(key) = strip_js_ext(&posix) {
            if key == "index" {
                index_dirs.push((String::new(), n.id.clone()));
            } else if let Some(dir) = key.strip_suffix("/index") {
                index_dirs.push((dir.to_string(), n.id.clone()));
            }
            by_key.entry(key).or_insert_with(|| n.id.clone());
        }
    }
    // `./foo` resolving to `foo/index.ts`; lower priority than a direct `foo.ts`.
    for (dir, id) in index_dirs {
        by_key.entry(dir).or_insert(id);
    }
    by_key
}

type QlFileIndex = HashMap<String, Vec<(String, NodeId)>>;

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

/// Module suffix -> QL source file(s). Precomputing every path suffix makes
/// resolution O(imports) rather than O(imports x all QL files), which matters
/// for CodeQL's tens of thousands of import edges.
fn build_ql_file_index(nodes: &[Node]) -> QlFileIndex {
    let mut index = HashMap::new();
    for node in nodes {
        if node.source_file.is_empty()
            || file_node_id(&node.source_file) != node.id
            || !matches!(
                std::path::Path::new(&node.source_file)
                    .extension()
                    .and_then(|ext| ext.to_str()),
                Some("ql" | "qll")
            )
        {
            continue;
        }
        let path = node.source_file.replace('\\', "/");
        for suffix in ql_path_suffixes(&path) {
            index
                .entry(suffix)
                .or_insert_with(Vec::new)
                .push((path.clone(), node.id.clone()));
        }
    }
    index
}

/// Filesystem suffix represented by a QL import. A module instantiation such as
/// `DataFlow::Global<Config>` is defined by the file for its first component.
fn ql_import_suffix(spec: &str) -> String {
    let compact: String = spec.chars().filter(|c| !c.is_whitespace()).collect();
    let base = compact.split("::").next().unwrap_or(&compact);
    let base = base.split('<').next().unwrap_or(base);
    base.replace('.', "/")
}

fn common_path_prefix_len(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(left, right)| left.eq_ignore_ascii_case(right))
        .count()
}

/// Resolve a QL module path to one source file. Exact suffixes are required. If
/// several packs contain that suffix, a same-pack candidate wins only when its
/// shared importer prefix is strictly longest.
fn resolve_ql_module(spec: &str, importer: &str, files: &QlFileIndex) -> Option<NodeId> {
    let suffix = ql_import_suffix(spec).to_ascii_lowercase();
    if suffix.is_empty() {
        return None;
    }
    let mut candidates: Vec<(&str, &NodeId)> = files
        .get(&suffix)?
        .iter()
        .map(|(path, id)| (path.as_str(), id))
        .collect();
    if candidates.len() == 1 {
        return candidates.pop().map(|(_, id)| id.clone());
    }

    let importer = importer.replace('\\', "/");
    let mut ranked: Vec<(usize, &NodeId)> = candidates
        .into_iter()
        .map(|(path, id)| (common_path_prefix_len(&importer, path), id))
        .collect();
    ranked.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    match ranked.as_slice() {
        [(score, id), rest @ ..]
            if rest
                .first()
                .is_none_or(|(next_score, _)| next_score < score) =>
        {
            Some((*id).clone())
        }
        _ => None,
    }
}

/// Build an asset file node for a canonical path. Real (non-empty `source_file`)
/// so it is locatable and survives the orphan-stub cleanup.
fn make_asset_node(canonical: &str, kind: &'static str) -> Node {
    let file_type = if kind == "image" {
        FileType::Image
    } else {
        FileType::Document
    };
    let mut extra: Map<String, Value> = Map::new();
    extra.insert("asset_kind".to_string(), json!(kind));
    Node {
        id: file_node_id(canonical),
        label: canonical.to_string(),
        file_type,
        source_file: canonical.to_string(),
        source_location: None,
        community: None,
        repo: None,
        extra,
        origin: Some("ast".into()),
        ..Default::default()
    }
}

/// Mint (or reuse) the asset node for `canonical`, returning its id.
fn intern_asset(
    new_nodes: &mut Vec<Node>,
    existing: &mut HashSet<NodeId>,
    canonical: &str,
    kind: &'static str,
) -> NodeId {
    let id = file_node_id(canonical);
    if existing.insert(id.clone()) {
        new_nodes.push(make_asset_node(canonical, kind));
    }
    id
}

/// Back-compat shim: bind only relative code imports (no aliases, no asset
/// minting beyond what relative non-code imports trigger). Returns the count of
/// relative code imports bound, matching the original `resolve_relative_imports`.
pub fn resolve_relative_imports(nodes: &mut Vec<Node>, edges: &mut [Edge]) -> usize {
    resolve_imports(nodes, edges, &AliasResolver::default()).relative_bound
}

/// Bind relative imports, alias imports, and non-code (asset) imports to real
/// nodes. See the module docs. Returns per-kind [`ResolveStats`].
pub fn resolve_imports(
    nodes: &mut Vec<Node>,
    edges: &mut [Edge],
    aliases: &AliasResolver,
) -> ResolveStats {
    let by_key = build_code_index(nodes);
    let ql_files = build_ql_file_index(nodes);
    let mut existing: HashSet<NodeId> = nodes.iter().map(|n| n.id.clone()).collect();
    let label_of: HashMap<NodeId, String> = nodes
        .iter()
        .map(|n| (n.id.clone(), n.label.clone()))
        .collect();
    let mut xaml_code: HashMap<(String, String), Vec<NodeId>> = HashMap::new();
    for node in nodes.iter().filter(|node| !node.source_file.is_empty()) {
        xaml_code
            .entry((
                node.source_file.replace('\\', "/").to_ascii_lowercase(),
                node.label.to_ascii_lowercase(),
            ))
            .or_default()
            .push(node.id.clone());
    }

    let mut new_nodes: Vec<Node> = Vec::new();
    let mut rewired_from: HashSet<NodeId> = HashSet::new();
    let mut stats = ResolveStats::default();

    for e in edges.iter_mut() {
        if e.context.as_deref() == Some("xaml_code_behind") {
            let Some(label) = label_of.get(&e.target) else {
                continue;
            };
            let xaml = e.source_file.replace('\\', "/");
            let plain = xaml.strip_suffix(".xaml").map(|path| format!("{path}.cs"));
            let candidates = [Some(format!("{xaml}.cs")), plain];
            let target = candidates.into_iter().flatten().find_map(|path| {
                let ids =
                    xaml_code.get(&(path.to_ascii_lowercase(), label.to_ascii_lowercase()))?;
                (ids.len() == 1).then(|| ids[0].clone())
            });
            if let Some(target) = target {
                rewired_from.insert(e.target.clone());
                e.target = target;
                stats.xaml_bound += 1;
            }
            continue;
        }
        if e.context.as_deref() == Some("ql_import") {
            let Some(spec) = label_of.get(&e.target) else {
                continue;
            };
            if let Some(id) = resolve_ql_module(spec, &e.source_file, &ql_files)
                && id != e.source
            {
                rewired_from.insert(e.target.clone());
                e.target = id;
                stats.ql_bound += 1;
            }
            continue;
        }
        if e.context.as_deref() != Some("import") {
            continue;
        }
        let Some(spec) = label_of.get(&e.target).cloned() else {
            continue;
        };
        let importer = e.source_file.replace('\\', "/");
        let importer_dir = importer.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

        if spec.starts_with('.') {
            // Relative import.
            match spec_kind(&spec) {
                SpecKind::Code => {
                    let Some(joined) = join_normalize(importer_dir, &spec) else {
                        continue;
                    };
                    let key = strip_js_ext(&joined).unwrap_or(joined);
                    if let Some(id) = by_key.get(&key)
                        && *id != e.source
                    {
                        rewired_from.insert(e.target.clone());
                        e.target = id.clone();
                        stats.relative_bound += 1;
                    }
                }
                SpecKind::Asset(kind) => {
                    let Some(canonical) = join_normalize(importer_dir, &spec) else {
                        continue;
                    };
                    let id = intern_asset(&mut new_nodes, &mut existing, &canonical, kind);
                    if id != e.source {
                        rewired_from.insert(e.target.clone());
                        e.target = id;
                        stats.assets += 1;
                    }
                }
            }
        } else {
            // Non-relative: try path aliases (bare packages yield no candidates).
            for cand in aliases.resolve(&importer, &spec) {
                match spec_kind(&cand) {
                    SpecKind::Code => {
                        let key = strip_js_ext(&cand).unwrap_or_else(|| cand.clone());
                        if let Some(id) = by_key.get(&key) {
                            if *id != e.source {
                                rewired_from.insert(e.target.clone());
                                e.target = id.clone();
                                stats.alias_bound += 1;
                            }
                            break;
                        }
                    }
                    SpecKind::Asset(kind) => {
                        let id = intern_asset(&mut new_nodes, &mut existing, &cand, kind);
                        if id != e.source {
                            rewired_from.insert(e.target.clone());
                            e.target = id;
                            stats.assets += 1;
                        }
                        break;
                    }
                }
            }
        }
    }

    stats.asset_nodes = new_nodes.len();
    nodes.append(&mut new_nodes);

    // Drop specifier stubs (relative-labeled, or any we rewired away from) that
    // are no longer referenced by an edge. Bare-package stubs (`react`) keep
    // their import edge, so they survive.
    if stats.relative_bound + stats.alias_bound + stats.assets + stats.ql_bound + stats.xaml_bound
        > 0
    {
        let referenced: HashSet<&NodeId> =
            edges.iter().flat_map(|e| [&e.source, &e.target]).collect();
        nodes.retain(|n| {
            let is_stub = n.source_file.is_empty()
                && (n.label.starts_with('.') || rewired_from.contains(&n.id));
            !is_stub || referenced.contains(&n.id)
        });
    }
    stats
}

#[cfg(all(test, feature = "lang-csharp", feature = "lang-dotnet"))]
mod xaml_tests {
    use super::*;
    use crate::{csharp::extract_csharp_source, dotnet::extract_dotnet_source};

    #[test]
    fn code_behind_binds_for_plain_cs_and_xaml_cs_conventions() {
        for code_path in ["Views/MainWindow.cs", "Views/MainWindow.xaml.cs"] {
            let xaml = extract_dotnet_source(
                "Views/MainWindow.xaml",
                br#"<Window xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml" x:Class="Demo.MainWindow" />"#,
            );
            let code = extract_csharp_source(
                code_path,
                b"namespace Demo; public partial class MainWindow {}",
            );
            let mut nodes = xaml.nodes;
            nodes.extend(code.nodes);
            let mut edges = xaml.edges;
            edges.extend(code.edges);
            let stats = resolve_imports(&mut nodes, &mut edges, &AliasResolver::default());
            assert_eq!(stats.xaml_bound, 1, "{code_path}");
            let edge = edges
                .iter()
                .find(|edge| edge.context.as_deref() == Some("xaml_code_behind"))
                .unwrap();
            assert!(
                nodes
                    .iter()
                    .any(|node| node.id == edge.target && node.source_file == code_path)
            );
        }
    }
}

#[cfg(all(test, feature = "lang-ql"))]
mod ql_tests {
    use super::*;
    use crate::ql::extract_ql_source;
    use crate::result::ExtractionResult;

    fn aggregate(results: Vec<ExtractionResult>) -> (Vec<Node>, Vec<Edge>) {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for result in results {
            nodes.extend(result.nodes);
            edges.extend(result.edges);
        }
        (nodes, edges)
    }

    #[test]
    fn ql_import_binds_to_library_file() {
        let query = extract_ql_source(
            "java/ql/src/Security/Query.ql",
            b"import semmle.code.java.DataFlow\nfrom int x select x\n",
        );
        let library = extract_ql_source(
            "java/ql/lib/semmle/code/java/DataFlow.qll",
            b"predicate flow(int x, int y) { x = y }\n",
        );
        let (mut nodes, mut edges) = aggregate(vec![query, library]);
        let stats = resolve_imports(&mut nodes, &mut edges, &AliasResolver::default());
        assert_eq!(stats.ql_bound, 1);
        assert!(edges.iter().any(|edge| {
            edge.context.as_deref() == Some("ql_import")
                && edge.target == file_node_id("java/ql/lib/semmle/code/java/DataFlow.qll")
        }));
        assert!(
            !nodes.iter().any(|node| {
                node.source_file.is_empty() && node.label == "semmle.code.java.DataFlow"
            }),
            "rewired QL module stub should be removed"
        );
    }

    #[test]
    fn ambiguous_ql_module_prefers_importers_pack() {
        let query = extract_ql_source("java/ql/src/Query.ql", b"import shared.Utils\nselect 1\n");
        let java = extract_ql_source(
            "java/ql/lib/shared/Utils.qll",
            b"predicate javaOnly() { any() }",
        );
        let cpp = extract_ql_source(
            "cpp/ql/lib/shared/Utils.qll",
            b"predicate cppOnly() { any() }",
        );
        let (mut nodes, mut edges) = aggregate(vec![query, java, cpp]);
        let stats = resolve_imports(&mut nodes, &mut edges, &AliasResolver::default());
        assert_eq!(stats.ql_bound, 1);
        assert!(edges.iter().any(|edge| {
            edge.context.as_deref() == Some("ql_import")
                && edge.target == file_node_id("java/ql/lib/shared/Utils.qll")
        }));
    }
}

#[cfg(all(test, feature = "lang-typescript"))]
mod tests {
    use super::*;
    use crate::ecmascript::extract_ts_source;
    use crate::result::ExtractionResult;
    use crate::tsconfig::{AliasEntry, AliasResolver};

    fn aggregate(rs: Vec<ExtractionResult>) -> (Vec<Node>, Vec<Edge>) {
        let (mut nodes, mut edges) = (Vec::new(), Vec::new());
        for r in rs {
            nodes.extend(r.nodes);
            edges.extend(r.edges);
        }
        (nodes, edges)
    }

    fn asset_node<'a>(nodes: &'a [Node], label: &str) -> Option<&'a Node> {
        nodes.iter().find(|n| n.label == label)
    }

    #[test]
    fn relative_import_binds_to_file_node() {
        let a = extract_ts_source("src/a.ts", b"import { x } from './b';\n");
        let b = extract_ts_source("src/b.ts", b"export const x = 1;\n");
        let (mut nodes, mut edges) = aggregate(vec![a, b]);
        let n = resolve_relative_imports(&mut nodes, &mut edges);
        assert_eq!(n, 1, "exactly one relative import rewired");
        let b_id = file_node_id("src/b.ts");
        assert!(
            edges
                .iter()
                .any(|e| e.relation == "imports_from" && e.target == b_id),
            "imports_from should target b.ts"
        );
        assert!(
            !nodes.iter().any(|nn| nn.label == "./b"),
            "the './b' stub should be dropped"
        );
    }

    #[test]
    fn relative_import_resolves_to_index_file() {
        let a = extract_ts_source("src/a.ts", b"import { x } from './bar';\n");
        let idx = extract_ts_source("src/bar/index.ts", b"export const x = 1;\n");
        let (mut nodes, mut edges) = aggregate(vec![a, idx]);
        assert_eq!(resolve_relative_imports(&mut nodes, &mut edges), 1);
        let idx_id = file_node_id("src/bar/index.ts");
        assert!(
            edges
                .iter()
                .any(|e| e.relation == "imports_from" && e.target == idx_id)
        );
    }

    #[test]
    fn relative_import_resolves_to_root_index_file() {
        let a = extract_ts_source("test/a.ts", b"import root from '../';\n");
        let idx = extract_ts_source("index.ts", b"export default function root() {}\n");
        let (mut nodes, mut edges) = aggregate(vec![a, idx]);
        assert_eq!(resolve_relative_imports(&mut nodes, &mut edges), 1);
        let idx_id = file_node_id("index.ts");
        assert!(
            edges
                .iter()
                .any(|e| e.relation == "imports_from" && e.target == idx_id)
        );
    }

    #[test]
    fn relative_import_traverses_parent() {
        let a = extract_ts_source("src/sub/a.ts", b"import { x } from '../util';\n");
        let util = extract_ts_source("src/util.ts", b"export const x = 1;\n");
        let (mut nodes, mut edges) = aggregate(vec![a, util]);
        assert_eq!(resolve_relative_imports(&mut nodes, &mut edges), 1);
        let util_id = file_node_id("src/util.ts");
        assert!(
            edges
                .iter()
                .any(|e| e.relation == "imports_from" && e.target == util_id)
        );
    }

    #[test]
    fn dot_prefix_collision_resolves_both_importers() {
        // A sibling `./foo` and a `../foo` from a subdir both target the same
        // real file, but their specifiers collapse to one stub id under make_id
        // (it trims leading dots). The shared stub can carry only one label, so
        // the per-edge resolver could rewire only one importer and strand the
        // other as a phantom. Both must resolve, and no stub may survive.
        let sibling = extract_ts_source("src/features/loader.ts", b"import { x } from './foo';\n");
        let test = extract_ts_source(
            "src/features/__tests__/foo.test.ts",
            b"import { x } from '../foo';\n",
        );
        let foo = extract_ts_source("src/features/foo.ts", b"export const x = 1;\n");
        let (mut nodes, mut edges) = aggregate(vec![sibling, test, foo]);
        let bound = resolve_relative_imports(&mut nodes, &mut edges);
        assert_eq!(bound, 2, "both ./foo and ../foo should bind");
        let foo_id = file_node_id("src/features/foo.ts");
        let to_foo = edges
            .iter()
            .filter(|e| e.relation == "imports_from" && e.target == foo_id)
            .count();
        assert_eq!(to_foo, 2, "both importers point at the real foo.ts");
        assert!(
            !nodes
                .iter()
                .any(|n| n.source_file.is_empty() && n.label.ends_with("foo")),
            "no phantom ./foo or ../foo specifier stub should survive"
        );
    }

    #[test]
    fn bare_import_is_left_as_stub() {
        let a = extract_ts_source("src/a.ts", b"import React from 'react';\n");
        let (mut nodes, mut edges) = aggregate(vec![a]);
        assert_eq!(resolve_relative_imports(&mut nodes, &mut edges), 0);
        assert!(nodes.iter().any(|nn| nn.label == "react"));
    }

    #[test]
    fn relative_css_mints_stylesheet_asset_node() {
        let a = extract_ts_source("src/Button.ts", b"import './Button.css';\n");
        let (mut nodes, mut edges) = aggregate(vec![a]);
        let stats = resolve_imports(&mut nodes, &mut edges, &AliasResolver::default());
        assert_eq!(stats.assets, 1);
        assert_eq!(stats.asset_nodes, 1);
        let n = asset_node(&nodes, "src/Button.css").expect("asset node exists");
        assert_eq!(n.file_type, FileType::Document);
        assert_eq!(n.id, file_node_id("src/Button.css"));
        assert_eq!(
            n.extra.get("asset_kind").and_then(|v| v.as_str()),
            Some("stylesheet")
        );
        assert!(edges
            .iter()
            .any(|e| e.relation == "imports_from" && e.target == file_node_id("src/Button.css")));
        assert!(!nodes.iter().any(|nn| nn.label == "./Button.css"));
    }

    #[test]
    fn shared_asset_is_a_single_node() {
        // Two components importing the SAME ../theme.css: one node, degree 2.
        let a = extract_ts_source("src/a/Card.ts", b"import '../theme.css';\n");
        let b = extract_ts_source("src/b/Panel.ts", b"import '../theme.css';\n");
        let (mut nodes, mut edges) = aggregate(vec![a, b]);
        let stats = resolve_imports(&mut nodes, &mut edges, &AliasResolver::default());
        assert_eq!(stats.assets, 2, "two import edges repointed");
        assert_eq!(stats.asset_nodes, 1, "but a single shared asset node");
        let theme = file_node_id("src/theme.css");
        let deg = edges.iter().filter(|e| e.target == theme).count();
        assert_eq!(deg, 2, "shared theme.css has degree 2");
    }

    #[test]
    fn distinct_local_styles_do_not_collide() {
        // Each component imports its OWN ./styles.css: two distinct nodes
        // (the old make_id([spec]) keying collapsed these into one).
        let a = extract_ts_source("src/a/Card.ts", b"import './styles.css';\n");
        let b = extract_ts_source("src/b/Panel.ts", b"import './styles.css';\n");
        let (mut nodes, mut edges) = aggregate(vec![a, b]);
        let stats = resolve_imports(&mut nodes, &mut edges, &AliasResolver::default());
        assert_eq!(stats.asset_nodes, 2, "distinct paths → distinct nodes");
        assert!(asset_node(&nodes, "src/a/styles.css").is_some());
        assert!(asset_node(&nodes, "src/b/styles.css").is_some());
    }

    #[test]
    fn dotted_module_name_is_not_an_asset() {
        // `../index.core` resolves to index.core.ts, a code module not an asset,
        // even though `.core` looks like an extension.
        let a = extract_ts_source("pkg/sub/a.ts", b"import { x } from '../index.core';\n");
        let core = extract_ts_source("pkg/index.core.ts", b"export const x = 1;\n");
        let (mut nodes, mut edges) = aggregate(vec![a, core]);
        let stats = resolve_imports(&mut nodes, &mut edges, &AliasResolver::default());
        assert_eq!(stats.relative_bound, 1, "bound as code");
        assert_eq!(stats.asset_nodes, 0, "no phantom asset minted");
        let core_id = file_node_id("pkg/index.core.ts");
        assert!(
            edges
                .iter()
                .any(|e| e.relation == "imports_from" && e.target == core_id)
        );
    }

    #[test]
    fn json_and_image_get_correct_kinds() {
        let a = extract_ts_source(
            "src/app.ts",
            b"import data from './data.json';\nimport logo from './logo.svg';\n",
        );
        let (mut nodes, mut edges) = aggregate(vec![a]);
        resolve_imports(&mut nodes, &mut edges, &AliasResolver::default());
        let d = asset_node(&nodes, "src/data.json").unwrap();
        assert_eq!(
            d.extra.get("asset_kind").and_then(|v| v.as_str()),
            Some("data")
        );
        assert_eq!(d.file_type, FileType::Document);
        let img = asset_node(&nodes, "src/logo.svg").unwrap();
        assert_eq!(
            img.extra.get("asset_kind").and_then(|v| v.as_str()),
            Some("image")
        );
        assert_eq!(img.file_type, FileType::Image);
    }

    fn alias_resolver() -> AliasResolver {
        AliasResolver::from_entries(vec![AliasEntry {
            config_dir: String::new(),
            base_url: ".".to_string(),
            paths: vec![("@/*".to_string(), vec!["src/*".to_string()])],
        }])
    }

    #[test]
    fn alias_binds_to_code_file() {
        let a = extract_ts_source("src/app/Foo.ts", b"import { api } from '@/lib/api';\n");
        let api = extract_ts_source("src/lib/api.ts", b"export const api = 1;\n");
        let (mut nodes, mut edges) = aggregate(vec![a, api]);
        let stats = resolve_imports(&mut nodes, &mut edges, &alias_resolver());
        assert_eq!(stats.alias_bound, 1);
        let api_id = file_node_id("src/lib/api.ts");
        assert!(
            edges
                .iter()
                .any(|e| e.relation == "imports_from" && e.target == api_id)
        );
        assert!(!nodes.iter().any(|nn| nn.label == "@/lib/api"));
    }

    #[test]
    fn alias_to_css_mints_asset() {
        let a = extract_ts_source("src/app/Foo.ts", b"import '@/styles/theme.css';\n");
        let (mut nodes, mut edges) = aggregate(vec![a]);
        let stats = resolve_imports(&mut nodes, &mut edges, &alias_resolver());
        assert_eq!(stats.assets, 1);
        let n = asset_node(&nodes, "src/styles/theme.css").unwrap();
        assert_eq!(
            n.extra.get("asset_kind").and_then(|v| v.as_str()),
            Some("stylesheet")
        );
    }

    #[test]
    fn unresolved_alias_is_left_as_stub() {
        // Alias resolves to a path with no matching code file and a code-ish
        // (no-extension) target, so it is left as a stub, not minted.
        let a = extract_ts_source("src/app/Foo.ts", b"import { z } from '@/missing/mod';\n");
        let (mut nodes, mut edges) = aggregate(vec![a]);
        let stats = resolve_imports(&mut nodes, &mut edges, &alias_resolver());
        assert_eq!(stats.alias_bound, 0);
        assert!(nodes.iter().any(|nn| nn.label == "@/missing/mod"));
    }
}
