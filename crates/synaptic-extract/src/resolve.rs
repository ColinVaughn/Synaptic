//! Cross-file pass: bind JS/TS imports to real nodes now that the full file set
//! is known (the per-file extractor only emits specifier-labeled stubs).
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

use serde_json::{json, Map, Value};
use synaptic_core::{Edge, FileType, Node, NodeId};

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
    if let Some((_, ext)) = last.rsplit_once('.') {
        if !ext.is_empty() {
            if let Some(kind) = classify_asset_ext(&ext.to_ascii_lowercase()) {
                return SpecKind::Asset(kind);
            }
        }
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
            if let Some(dir) = key.strip_suffix("/index") {
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

/// Build an asset file node for a canonical path. Real (non-empty `source_file`)
/// so it is locatable and survives the orphan-stub cleanup.
fn make_asset_node(canonical: &str, kind: &'static str) -> Node {
    let file_type = if kind == "image" {
        FileType::Image
    } else {
        FileType::Document
    };
    let mut extra: Map<String, Value> = Map::new();
    extra.insert("_origin".to_string(), json!("ast"));
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
    let mut existing: HashSet<NodeId> = nodes.iter().map(|n| n.id.clone()).collect();
    let label_of: HashMap<NodeId, String> = nodes
        .iter()
        .map(|n| (n.id.clone(), n.label.clone()))
        .collect();

    let mut new_nodes: Vec<Node> = Vec::new();
    let mut rewired_from: HashSet<NodeId> = HashSet::new();
    let mut stats = ResolveStats::default();

    for e in edges.iter_mut() {
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
                    if let Some(id) = by_key.get(&key) {
                        if *id != e.source {
                            rewired_from.insert(e.target.clone());
                            e.target = id.clone();
                            stats.relative_bound += 1;
                        }
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
    if stats.relative_bound + stats.alias_bound + stats.assets > 0 {
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
        assert!(edges
            .iter()
            .any(|e| e.relation == "imports_from" && e.target == idx_id));
    }

    #[test]
    fn relative_import_traverses_parent() {
        let a = extract_ts_source("src/sub/a.ts", b"import { x } from '../util';\n");
        let util = extract_ts_source("src/util.ts", b"export const x = 1;\n");
        let (mut nodes, mut edges) = aggregate(vec![a, util]);
        assert_eq!(resolve_relative_imports(&mut nodes, &mut edges), 1);
        let util_id = file_node_id("src/util.ts");
        assert!(edges
            .iter()
            .any(|e| e.relation == "imports_from" && e.target == util_id));
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
        assert!(edges
            .iter()
            .any(|e| e.relation == "imports_from" && e.target == core_id));
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
        assert!(edges
            .iter()
            .any(|e| e.relation == "imports_from" && e.target == api_id));
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
