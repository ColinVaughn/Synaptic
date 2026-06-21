//! Export surfaces + cross-repo symbol resolution. Each member publishes an
//! **`export-surface.json`**: its public symbols
//! keyed by its package coordinate. The workspace resolver scans the composed
//! graph's `imports`/`imports_from` edges whose target package matches another
//! member's coordinate and links the importer to the exported symbol.
//!
//! **Granularity is bounded by what `graph.json` preserves.** A member's import
//! edge records only the imported *package/module* (the external stub's label) —
//! the exact imported symbol name is not persisted. So a submodule import
//! (`from billing.ledger import …`, `import ".../billing/ledger"`) resolves to a
//! module **exactly** (`EXTRACTED`), while a bare package import (`use billing::…`,
//! `import billing`) resolves **package-level** (`INFERRED`, `confidence ~0.75`).
//! Unmatched imports stay as third-party `external_package` stubs so nothing
//! dangles (the `scip_external` idea).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use synaptic_core::{Confidence, FileType, GraphData, NodeId};

use crate::coordinate::{Coordinate, Ecosystem};
use crate::{Result, WorkspaceError, MAX_GRAPH_BYTES, SURFACE_SCHEMA_VERSION};

/// One exported symbol: an importable name and the member-local node it resolves
/// to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedSymbol {
    /// Importable name (a node label — a symbol name, or a module/file name).
    pub name: String,
    /// The member-LOCAL node id (pre-`tag::` prefix).
    pub node_id: String,
    /// Informational: the node's file type.
    pub kind: String,
}

/// A member's published surface: its coordinate + public symbols.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSurface {
    /// Schema version; defaults to 0 for surfaces written before versioning.
    #[serde(default)]
    pub version: u32,
    pub repo: String,
    pub coordinate: Coordinate,
    pub symbols: Vec<ExportedSymbol>,
    /// For JVM/.NET members: the dominant dotted package/namespace prefix, used to
    /// resolve package-qualified cross-repo imports (the build coordinate is not
    /// what imports spell). `None` for ecosystems whose import path == coordinate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Build a member's export surface from its (un-prefixed) graph: every code node
/// defined in the member (non-empty `source_file`, i.e. not an external stub) is
/// exported, keyed by its label. A heuristic public surface — bounded by node
/// count, and matchable by module/file/symbol name.
pub fn build_export_surface(
    repo: &str,
    coordinate: Coordinate,
    graph: &GraphData,
) -> ExportSurface {
    let mut symbols = Vec::new();
    let mut seen = HashSet::new();
    for n in &graph.nodes {
        if n.file_type == FileType::Code && !n.source_file.is_empty() && !n.label.is_empty() {
            // Dedup by (name, node_id) so two distinct nodes with the same label
            // (rare) both survive but exact dups don't.
            if seen.insert((n.label.clone(), n.id.0.clone())) {
                symbols.push(ExportedSymbol {
                    name: n.label.clone(),
                    node_id: n.id.0.clone(),
                    kind: format!("{:?}", n.file_type),
                });
            }
        }
    }
    symbols.sort_by(|a, b| a.name.cmp(&b.name).then(a.node_id.cmp(&b.node_id)));
    let namespace = if matches!(coordinate.ecosystem, Ecosystem::Jvm | Ecosystem::DotNet) {
        let ids: Vec<String> = symbols.iter().map(|s| s.node_id.clone()).collect();
        dominant_namespace(&ids)
    } else {
        None
    };
    ExportSurface {
        version: SURFACE_SCHEMA_VERSION,
        repo: repo.to_string(),
        coordinate,
        symbols,
        namespace,
    }
}

/// Longest dotted prefix shared by ≥50% of the qualified symbol ids. Returns
/// `None` if no dotted prefix reaches the threshold (e.g. flat/unqualified ids).
fn dominant_namespace(ids: &[String]) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    // Candidate prefixes: each id without its last dotted segment, plus all of
    // that prefix's dotted ancestors.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for id in ids {
        if let Some((prefix, _)) = id.rsplit_once('.') {
            let mut p = prefix;
            loop {
                *counts.entry(p).or_default() += 1;
                match p.rsplit_once('.') {
                    Some((parent, _)) => p = parent,
                    None => break,
                }
            }
        }
    }
    let threshold = ids.len().div_ceil(2);
    counts
        .into_iter()
        .filter(|(_, c)| *c >= threshold)
        .max_by_key(|(p, _)| p.len())
        .map(|(p, _)| p.to_string())
}

/// Write an export surface to `path`.
pub fn save_surface(path: &Path, surface: &ExportSurface) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(surface).map_err(|source| WorkspaceError::Json {
        path: path.display().to_string(),
        source,
    })?;
    std::fs::write(path, bytes).map_err(|source| WorkspaceError::Io {
        context: format!("writing {}", path.display()),
        source,
    })
}

/// Load an export surface from `path` (byte-capped).
pub fn load_surface(path: &Path) -> Result<ExportSurface> {
    let label = path.display().to_string();
    let meta = std::fs::metadata(path).map_err(|source| WorkspaceError::Io {
        context: format!("reading {label}"),
        source,
    })?;
    if meta.len() > MAX_GRAPH_BYTES {
        return Err(WorkspaceError::TooBig {
            path: label,
            size: meta.len(),
            limit: MAX_GRAPH_BYTES,
        });
    }
    let bytes = std::fs::read(path).map_err(|source| WorkspaceError::Io {
        context: format!("reading {label}"),
        source,
    })?;
    let surface: ExportSurface =
        serde_json::from_slice(&bytes).map_err(|source| WorkspaceError::Json {
            path: label.clone(),
            source,
        })?;
    if surface.version > SURFACE_SCHEMA_VERSION {
        return Err(WorkspaceError::SurfaceVersion {
            path: label,
            found: surface.version,
            supported: SURFACE_SCHEMA_VERSION,
        });
    }
    Ok(surface)
}

/// What [`resolve_cross_repo`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrossRepoReport {
    /// Module/symbol-exact cross-repo links (`EXTRACTED`).
    pub extracted: usize,
    /// Package-level cross-repo links (`INFERRED`).
    pub inferred: usize,
    /// External stubs retagged as third-party `external_package`.
    pub external_packages: usize,
}

/// A per-member lookup the resolver matches imports against.
struct MemberIndex {
    repo: String,
    /// The coordinate name as imports spell it. For Cargo this is the crate name
    /// with `-`→`_` (Rust `use` paths use the underscore lib name, not the
    /// hyphenated package name); other ecosystems use the coordinate verbatim.
    match_name: String,
    /// matchable key (label or its stem) → member-local node id.
    by_name: HashMap<String, String>,
    /// Deterministic package-level anchor: the smallest local node id.
    anchor: Option<String>,
}

/// Stem of a dotted/extensioned name: `ledger.py` → `ledger`, `Foo.bar` → `Foo`.
fn stem(name: &str) -> &str {
    name.rsplit_once('.').map(|(a, _)| a).unwrap_or(name)
}

/// Split an imported path against a coordinate name. Returns the remaining
/// sub-path when `imported` is, or is under, `coord` (`/` or `.` separated).
fn strip_coordinate(imported: &str, coord: &str) -> Option<String> {
    if imported == coord {
        return Some(String::new());
    }
    imported
        .strip_prefix(&format!("{coord}/"))
        .or_else(|| imported.strip_prefix(&format!("{coord}.")))
        .map(str::to_string)
}

/// Is this an import edge (the kind that can cross repos)?
fn is_import_edge(relation: &str, context: Option<&str>) -> bool {
    matches!(relation, "imports" | "imports_from") || context == Some("import")
}

/// Resolve an imported package/module label to a member, importer-independently:
/// coordinate match (module-exact → EXTRACTED, else package-level → INFERRED),
/// else a single-owner symbol fallback (EXTRACTED). Returns
/// `(target_repo, target_local_id, confidence, score)`.
fn resolve_label(
    imported: &str,
    members: &[MemberIndex],
    symbol_owners: &HashMap<String, Vec<(String, String)>>,
) -> Option<(String, String, Confidence, f32)> {
    // Coordinate match: longest coordinate wins.
    let mut best: Option<(&MemberIndex, String)> = None;
    for m in members {
        if let Some(rem) = strip_coordinate(imported, &m.match_name) {
            if best
                .as_ref()
                .is_none_or(|(b, _)| b.match_name.len() < m.match_name.len())
            {
                best = Some((m, rem));
            }
        }
    }
    if let Some((member, remaining)) = best {
        let exact = if remaining.is_empty() {
            None
        } else {
            let last = remaining.rsplit(['/', '.']).next().unwrap_or(&remaining);
            member.by_name.get(last).cloned()
        };
        return match exact {
            Some(id) => Some((member.repo.clone(), id, Confidence::Extracted, 1.0)),
            None => member
                .anchor
                .clone()
                .map(|a| (member.repo.clone(), a, Confidence::Inferred, 0.75)),
        };
    }
    // Single-owner symbol fallback (e.g. Rust `use crate::Item` -> stub "Item").
    // Resolve when exactly one member exports the symbol (even across several of
    // its own nodes; pick the smallest node id deterministically). A symbol owned
    // by two+ members is ambiguous from the label alone, so leave it unresolved.
    let owners = symbol_owners.get(imported)?;
    let mut repos: Vec<&str> = owners.iter().map(|(r, _)| r.as_str()).collect();
    repos.sort_unstable();
    repos.dedup();
    if repos.len() != 1 {
        return None;
    }
    let local = owners.iter().map(|(_, n)| n).min().cloned()?;
    Some((owners[0].0.clone(), local, Confidence::Extracted, 1.0))
}

/// Resolve cross-repo imports on a composed (already `tag::`-prefixed) graph.
///
/// Each external stub that is an import target is resolved (once) to a member.
/// **Every** edge whose target is a resolved stub — `imports`/`imports_from` and
/// also `references`/`inherits`/etc. pointing at the same external symbol — is
/// rewired into that member (`cross_repo`), unless it is a self-reference (the
/// importing edge's own repo owns the target). Import edges take the resolution's
/// confidence; other AST edges keep theirs. Import-target stubs that match no
/// member are retagged `external_package`; orphaned rewired stubs are dropped.
pub fn resolve_cross_repo(
    mut g: GraphData,
    surfaces: &[ExportSurface],
    aliases: &crate::alias::AliasMap,
) -> (GraphData, CrossRepoReport) {
    // Build per-member indexes (longest coordinate preferred at match time).
    let members: Vec<MemberIndex> = surfaces
        .iter()
        .map(|s| {
            let mut by_name = HashMap::new();
            let mut anchor: Option<String> = None;
            for sym in &s.symbols {
                by_name
                    .entry(sym.name.clone())
                    .or_insert(sym.node_id.clone());
                by_name
                    .entry(stem(&sym.name).to_string())
                    .or_insert(sym.node_id.clone());
                anchor = Some(match anchor {
                    Some(a) if a <= sym.node_id => a,
                    _ => sym.node_id.clone(),
                });
            }
            let match_name = match s.coordinate.ecosystem {
                Ecosystem::Cargo => s.coordinate.name.replace('-', "_"),
                // JVM/.NET imports spell the package namespace, not the build
                // coordinate (groupId:artifactId / AssemblyName).
                Ecosystem::Jvm | Ecosystem::DotNet => s
                    .namespace
                    .clone()
                    .unwrap_or_else(|| s.coordinate.name.clone()),
                _ => s.coordinate.name.clone(),
            };
            MemberIndex {
                repo: s.repo.clone(),
                match_name,
                by_name,
                anchor,
            }
        })
        .collect();

    // Member tag -> its package-level anchor node id, for import-map alias links.
    let anchor_by_tag: HashMap<&str, &str> = members
        .iter()
        .filter_map(|m| m.anchor.as_deref().map(|a| (m.repo.as_str(), a)))
        .collect();

    // Cross-member symbol owners (exact label -> [(repo, local id)]). Powers the
    // single-candidate symbol fallback for imports whose stub is the imported
    // item rather than the package, e.g. Rust `use billing::Ledger` produces a
    // stub labeled "Ledger", which matches no coordinate but does name a symbol.
    let mut symbol_owners: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for s in surfaces {
        for sym in &s.symbols {
            symbol_owners
                .entry(sym.name.clone())
                .or_default()
                .push((s.repo.clone(), sym.node_id.clone()));
        }
    }

    // Snapshot node facts (repo, externalness, label) before mutating.
    let node_repo: HashMap<NodeId, Option<String>> = g
        .nodes
        .iter()
        .map(|n| (n.id.clone(), n.repo.clone()))
        .collect();
    let externals: HashMap<NodeId, String> = g
        .nodes
        .iter()
        .filter(|n| n.source_file.is_empty() && !n.label.is_empty())
        .map(|n| (n.id.clone(), n.label.clone()))
        .collect();

    let mut report = CrossRepoReport::default();

    // (1) Decide a resolution per import-target stub (importer-independent), once.
    // `resolution`: stub id -> (full target id, target repo, confidence, score).
    let mut resolution: HashMap<NodeId, (NodeId, String, Confidence, f32)> = HashMap::new();
    let mut unmatched: HashSet<NodeId> = HashSet::new();
    for e in &g.links {
        if !is_import_edge(&e.relation, e.context.as_deref()) {
            continue;
        }
        let Some(imported) = externals.get(&e.target) else {
            continue;
        };
        if resolution.contains_key(&e.target) || unmatched.contains(&e.target) {
            continue; // already decided
        }
        // An alias (import map / tsconfig `paths` / module-federation remote, e.g.
        // `@PCMatic/Hub` or `@app/Button` -> member `hub`) is the authoritative
        // cross-repo link for these architectures; try it before coordinate / symbol
        // matching. Package-level: INFERRED, targets the member's anchor.
        let decided = aliases
            .resolve(imported.as_str())
            .and_then(|tag| {
                anchor_by_tag.get(tag).map(|anchor| {
                    (
                        tag.to_string(),
                        anchor.to_string(),
                        Confidence::Inferred,
                        0.75f32,
                    )
                })
            })
            .or_else(|| resolve_label(imported, &members, &symbol_owners));
        match decided {
            Some((repo, local, conf, score)) => {
                let target = NodeId(format!("{repo}::{local}"));
                resolution.insert(e.target.clone(), (target, repo, conf, score));
            }
            None => {
                unmatched.insert(e.target.clone());
            }
        }
    }

    // (2) Apply resolutions to ALL edges (import + references/inherits/etc.) whose
    // target is a resolved stub, skipping self-references. Import edges adopt the
    // resolution's confidence; other AST edges keep their own (they are real
    // extracted edges that merely pointed at an external stub).
    let mut rewired_stubs: HashSet<NodeId> = HashSet::new();
    for e in &mut g.links {
        let Some((target, target_repo, conf, score)) = resolution.get(&e.target) else {
            continue;
        };
        let src_repo = node_repo.get(&e.source).and_then(|r| r.as_deref());
        if src_repo == Some(target_repo.as_str()) {
            continue; // self-reference: the importer owns the target
        }
        rewired_stubs.insert(e.target.clone());
        let is_import = is_import_edge(&e.relation, e.context.as_deref());
        e.target = target.clone();
        e.cross_repo = true;
        if is_import {
            e.confidence = *conf;
            e.confidence_score = Some(*score);
            match conf {
                Confidence::Extracted => report.extracted += 1,
                _ => report.inferred += 1,
            }
        }
    }

    // (3) Dedup rewired collisions (e.g. several modules -> one anchor), keeping
    // the highest-confidence edge for each (source, target, relation).
    let mut idx: HashMap<(NodeId, NodeId, String), usize> = HashMap::new();
    let mut deduped: Vec<synaptic_core::Edge> = Vec::with_capacity(g.links.len());
    for e in std::mem::take(&mut g.links) {
        let key = (e.source.clone(), e.target.clone(), e.relation.clone());
        match idx.get(&key) {
            Some(&i) => {
                let prev = deduped[i].confidence_score.unwrap_or(0.0);
                if e.confidence_score.unwrap_or(0.0) > prev {
                    deduped[i] = e;
                }
            }
            None => {
                idx.insert(key, deduped.len());
                deduped.push(e);
            }
        }
    }
    g.links = deduped;

    // Referenced node set after rewiring.
    let mut referenced: HashSet<NodeId> = HashSet::new();
    for e in &g.links {
        referenced.insert(e.source.clone());
        referenced.insert(e.target.clone());
    }
    for h in &g.hyperedges {
        referenced.extend(h.nodes.iter().cloned());
    }

    // Drop rewired stubs that are now orphaned; mark unresolved import targets as
    // third-party external packages.
    g.nodes
        .retain(|n| !rewired_stubs.contains(&n.id) || referenced.contains(&n.id));
    for n in &mut g.nodes {
        if unmatched.contains(&n.id) {
            n.extra.insert(
                "external_package".to_string(),
                serde_json::Value::Bool(true),
            );
            report.external_packages += 1;
        }
    }

    (g, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinate::Ecosystem;
    use crate::federate::compose;
    use serde_json::Map;
    use synaptic_core::{Edge, Node};

    fn coord(name: &str, eco: Ecosystem) -> Coordinate {
        Coordinate {
            ecosystem: eco,
            name: name.into(),
        }
    }

    fn node(id: &str, label: &str, source_file: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: source_file.into(),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    fn import_edge(src: &str, tgt: &str, rel: &str) -> Edge {
        Edge {
            source: NodeId(src.into()),
            target: NodeId(tgt.into()),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            source_file: "x".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: Some("import".into()),
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn gd(nodes: Vec<Node>, links: Vec<Edge>) -> GraphData {
        GraphData {
            nodes,
            links,
            ..Default::default()
        }
    }

    #[test]
    fn surface_exports_code_nodes_not_externals() {
        let g = gd(
            vec![
                node("Ledger", "Ledger", "ledger.rs"),
                node("ext", "serde", ""), // external, excluded
            ],
            vec![],
        );
        let s = build_export_surface("billing", coord("billing", Ecosystem::Cargo), &g);
        assert_eq!(s.symbols.len(), 1);
        assert_eq!(s.symbols[0].name, "Ledger");
        assert_eq!(s.symbols[0].node_id, "Ledger");
    }

    #[test]
    fn jvm_surface_synthesizes_dominant_namespace() {
        // Three classes share the com.acme.billing prefix; one outlier.
        let g = gd(
            vec![
                node("com.acme.billing.Ledger", "Ledger", "Ledger.java"),
                node("com.acme.billing.Invoice", "Invoice", "Invoice.java"),
                node("com.acme.billing.util.Money", "Money", "Money.java"),
                node("com.other.Thing", "Thing", "Thing.java"),
            ],
            vec![],
        );
        let s = build_export_surface("billing", coord("com.acme:billing", Ecosystem::Jvm), &g);
        assert_eq!(s.namespace.as_deref(), Some("com.acme.billing"));
    }

    #[test]
    fn cargo_surface_has_no_namespace() {
        let g = gd(vec![node("Ledger", "Ledger", "ledger.rs")], vec![]);
        let s = build_export_surface("billing", coord("billing", Ecosystem::Cargo), &g);
        assert_eq!(s.namespace, None);
    }

    #[test]
    fn surface_round_trips_through_json() {
        let d = tempfile::tempdir().unwrap();
        let g = gd(vec![node("Ledger", "Ledger", "ledger.rs")], vec![]);
        let s = build_export_surface("billing", coord("billing", Ecosystem::Cargo), &g);
        let p = d.path().join("export-surface.json");
        save_surface(&p, &s).unwrap();
        assert_eq!(load_surface(&p).unwrap(), s);
    }

    #[test]
    fn surface_load_rejects_future_version() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("export-surface.json");
        std::fs::write(
            &p,
            r#"{"version":9999,"repo":"x","coordinate":{"ecosystem":"cargo","name":"x"},"symbols":[]}"#,
        )
        .unwrap();
        let err = load_surface(&p).expect_err("future version must error");
        assert!(
            matches!(err, crate::WorkspaceError::SurfaceVersion { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn surface_load_accepts_missing_version() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("export-surface.json");
        std::fs::write(
            &p,
            r#"{"repo":"x","coordinate":{"ecosystem":"cargo","name":"x"},"symbols":[]}"#,
        )
        .unwrap();
        assert!(load_surface(&p).is_ok());
    }

    #[test]
    fn package_level_import_resolves_inferred() {
        // repoa: `use billing::...` -> stub labeled "billing" (no sub-path).
        let a = gd(
            vec![node("app", "app", "app.rs"), node("billing", "billing", "")],
            vec![import_edge("app", "billing", "imports_from")],
        );
        let b = gd(vec![node("Ledger", "Ledger", "ledger.rs")], vec![]);
        let composed = compose(vec![("repoa".into(), a), ("repob".into(), b.clone())]);
        let surfaces = vec![build_export_surface(
            "repob",
            coord("billing", Ecosystem::Cargo),
            &b,
        )];
        let (g, report) =
            resolve_cross_repo(composed, &surfaces, &crate::alias::AliasMap::default());
        assert_eq!(report.inferred, 1);
        assert_eq!(report.extracted, 0);
        let e = g.links.iter().find(|e| e.source.0 == "repoa::app").unwrap();
        assert!(e.cross_repo);
        assert_eq!(e.confidence, Confidence::Inferred);
        assert_eq!(e.target.0, "repob::Ledger"); // anchor (only symbol)
                                                 // The matched stub is gone (orphaned).
        assert!(!g.nodes.iter().any(|n| n.id.0 == "repoa::billing"));
    }

    #[test]
    fn submodule_import_resolves_module_exact_extracted() {
        // Python: `from billing.ledger import X` -> stub labeled "billing.ledger".
        let a = gd(
            vec![
                node("app", "app.py", "app.py"),
                node("billing.ledger", "billing.ledger", ""),
            ],
            vec![import_edge("app", "billing.ledger", "imports_from")],
        );
        let b = gd(
            vec![
                node("ledger.py", "ledger.py", "ledger.py"),
                node("Other", "Other", "other.py"),
            ],
            vec![],
        );
        let composed = compose(vec![("repoa".into(), a), ("repob".into(), b.clone())]);
        let surfaces = vec![build_export_surface(
            "repob",
            coord("billing", Ecosystem::Python),
            &b,
        )];
        let (g, report) =
            resolve_cross_repo(composed, &surfaces, &crate::alias::AliasMap::default());
        assert_eq!(report.extracted, 1, "module-exact match");
        let e = g.links.iter().find(|e| e.source.0 == "repoa::app").unwrap();
        assert!(e.cross_repo);
        assert_eq!(e.confidence, Confidence::Extracted);
        assert_eq!(e.target.0, "repob::ledger.py");
    }

    #[test]
    fn symbol_fallback_resolves_rust_use_item() {
        // Rust `use billing::Ledger;` yields a stub labeled "Ledger" (the item,
        // not the crate): no coordinate match, but a single member exports it.
        let a = gd(
            vec![node("app", "app", "app.rs"), node("Ledger", "Ledger", "")],
            vec![import_edge("app", "Ledger", "imports_from")],
        );
        let b = gd(vec![node("Ledger", "Ledger", "ledger.rs")], vec![]);
        let composed = compose(vec![("repoa".into(), a), ("repob".into(), b.clone())]);
        let surfaces = vec![build_export_surface(
            "repob",
            coord("billing", Ecosystem::Cargo),
            &b,
        )];
        let (g, report) =
            resolve_cross_repo(composed, &surfaces, &crate::alias::AliasMap::default());
        assert_eq!(report.extracted, 1, "single-candidate symbol match");
        let e = g.links.iter().find(|e| e.source.0 == "repoa::app").unwrap();
        assert!(e.cross_repo);
        assert_eq!(e.target.0, "repob::Ledger");
    }

    #[test]
    fn non_import_edge_to_external_symbol_also_resolves() {
        // app imports Ledger (import edge) AND widget references Ledger (a
        // `references` edge to the SAME external stub). Both must rewire into the
        // owning member; the references edge must not be stranded on the stub.
        let a = gd(
            vec![
                node("app", "app", "app.py"),
                node("widget", "widget", "widget.py"),
                node("Ledger", "Ledger", ""),
            ],
            vec![
                import_edge("app", "Ledger", "imports_from"),
                Edge {
                    source: NodeId("widget".into()),
                    target: NodeId("Ledger".into()),
                    relation: "references".into(),
                    confidence: Confidence::Extracted,
                    source_file: "widget.py".into(),
                    source_location: None,
                    confidence_score: None,
                    weight: 1.0,
                    context: None,
                    cross_repo: false,
                    extra: Map::new(),
                },
            ],
        );
        let b = gd(vec![node("Ledger", "Ledger", "ledger.py")], vec![]);
        let composed = compose(vec![("repoa".into(), a), ("repob".into(), b.clone())]);
        let surfaces = vec![build_export_surface(
            "repob",
            coord("billing", Ecosystem::Cargo),
            &b,
        )];
        let (g, _report) =
            resolve_cross_repo(composed, &surfaces, &crate::alias::AliasMap::default());
        // BOTH edges now point into repob and are cross_repo.
        let refs: Vec<&synaptic_core::Edge> = g
            .links
            .iter()
            .filter(|e| e.target.0 == "repob::Ledger")
            .collect();
        assert_eq!(
            refs.len(),
            2,
            "both import + references rewired: {:?}",
            g.links
        );
        assert!(refs.iter().all(|e| e.cross_repo));
        // The stub is gone (no edge references it anymore).
        assert!(!g.nodes.iter().any(|n| n.id.0 == "repoa::Ledger"));
    }

    #[test]
    fn ambiguous_symbol_is_left_unresolved() {
        // Two members export "Handler"; the import can't be pinned, so unmatched.
        let a = gd(
            vec![node("app", "app", "app.rs"), node("Handler", "Handler", "")],
            vec![import_edge("app", "Handler", "imports_from")],
        );
        let b = gd(vec![node("Handler", "Handler", "b.rs")], vec![]);
        let c = gd(vec![node("Handler", "Handler", "c.rs")], vec![]);
        let composed = compose(vec![
            ("repoa".into(), a),
            ("repob".into(), b.clone()),
            ("repoc".into(), c.clone()),
        ]);
        let surfaces = vec![
            build_export_surface("repob", coord("b", Ecosystem::Cargo), &b),
            build_export_surface("repoc", coord("c", Ecosystem::Cargo), &c),
        ];
        let (_, report) =
            resolve_cross_repo(composed, &surfaces, &crate::alias::AliasMap::default());
        assert_eq!(report.extracted + report.inferred, 0, "ambiguous → skipped");
        assert_eq!(report.external_packages, 1);
    }

    #[test]
    fn cargo_coordinate_matches_underscore_import_path() {
        // Cargo package "synaptic-core" is imported as `use synaptic_core::{...}`
        // -> stub "synaptic_core". The `-` to `_` normalization must match it.
        let a = gd(
            vec![
                node("app", "app", "app.rs"),
                node("synaptic_core", "synaptic_core", ""),
            ],
            vec![import_edge("app", "synaptic_core", "imports_from")],
        );
        let b = gd(vec![node("Node", "Node", "node.rs")], vec![]);
        let composed = compose(vec![("repoa".into(), a), ("core".into(), b.clone())]);
        let surfaces = vec![build_export_surface(
            "core",
            coord("synaptic-core", Ecosystem::Cargo),
            &b,
        )];
        let (g, report) =
            resolve_cross_repo(composed, &surfaces, &crate::alias::AliasMap::default());
        assert_eq!(
            report.inferred, 1,
            "package-level match via -/_ normalization"
        );
        let e = g.links.iter().find(|e| e.source.0 == "repoa::app").unwrap();
        assert!(e.cross_repo && e.target.0.starts_with("core::"));
    }

    #[test]
    fn unknown_package_becomes_external_package() {
        let a = gd(
            vec![node("app", "app", "app.rs"), node("tokio", "tokio", "")],
            vec![import_edge("app", "tokio", "imports_from")],
        );
        let b = gd(vec![node("Ledger", "Ledger", "ledger.rs")], vec![]);
        let composed = compose(vec![("repoa".into(), a), ("repob".into(), b.clone())]);
        let surfaces = vec![build_export_surface(
            "repob",
            coord("billing", Ecosystem::Cargo),
            &b,
        )];
        let (g, report) =
            resolve_cross_repo(composed, &surfaces, &crate::alias::AliasMap::default());
        assert_eq!(report.external_packages, 1);
        assert_eq!(report.extracted + report.inferred, 0);
        let tokio = g.nodes.iter().find(|n| n.label == "tokio").unwrap();
        assert_eq!(
            tokio.extra.get("external_package"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn jvm_package_qualified_import_resolves_via_namespace() {
        // app imports com.acme.billing.Ledger; billing member owns that class under
        // its dominant namespace com.acme.billing.
        let a = gd(
            vec![
                node("com.app.Main", "Main", "Main.java"),
                node("com.acme.billing.Ledger", "com.acme.billing.Ledger", ""),
            ],
            vec![import_edge(
                "com.app.Main",
                "com.acme.billing.Ledger",
                "imports_from",
            )],
        );
        let b = gd(
            vec![
                node("com.acme.billing.Ledger", "Ledger", "Ledger.java"),
                node("com.acme.billing.Invoice", "Invoice", "Invoice.java"),
            ],
            vec![],
        );
        let composed = compose(vec![("repoa".into(), a), ("billing".into(), b.clone())]);
        let surfaces = vec![build_export_surface(
            "billing",
            coord("com.acme:billing", Ecosystem::Jvm),
            &b,
        )];
        let (g, report) =
            resolve_cross_repo(composed, &surfaces, &crate::alias::AliasMap::default());
        assert!(
            report.extracted + report.inferred >= 1,
            "resolved cross-repo: {report:?}"
        );
        let e = g
            .links
            .iter()
            .find(|e| e.source.0 == "repoa::com.app.Main")
            .unwrap();
        assert!(e.cross_repo && e.target.0.starts_with("billing::"));
    }

    #[test]
    fn import_map_alias_resolves_cross_repo() {
        // root-config imports "@PCMatic/Hub" (stub); alias map says that's `hub`.
        let a = gd(
            vec![
                node("app", "app", "rc.js"),
                node("@PCMatic/Hub", "@PCMatic/Hub", ""),
            ],
            vec![import_edge("app", "@PCMatic/Hub", "imports_from")],
        );
        let b = gd(vec![node("Widget", "Widget", "w.js")], vec![]);
        let composed = compose(vec![("root-config".into(), a), ("hub".into(), b.clone())]);
        let surfaces = vec![build_export_surface(
            "hub",
            coord("hub", Ecosystem::Npm),
            &b,
        )];
        let mut aliases = crate::alias::AliasMap::default();
        aliases.insert(
            crate::alias::AliasKind::Exact,
            "@PCMatic/Hub".to_string(),
            "hub".to_string(),
        );
        let (g, report) = resolve_cross_repo(composed, &surfaces, &aliases);
        assert_eq!(
            report.inferred, 1,
            "alias resolved package-level: {report:?}"
        );
        let e = g
            .links
            .iter()
            .find(|e| e.source.0 == "root-config::app")
            .unwrap();
        assert!(
            e.cross_repo && e.target.0.starts_with("hub::"),
            "{:?}",
            e.target
        );
    }

    #[test]
    fn prefix_alias_resolves_cross_repo() {
        // `app` imports a tsconfig-`paths`-style subpath `@app/Button` (stub); the
        // prefix alias `@app` -> member `hub` resolves it package-level cross-repo.
        let a = gd(
            vec![
                node("page", "page", "p.ts"),
                node("@app/Button", "@app/Button", ""),
            ],
            vec![import_edge("page", "@app/Button", "imports_from")],
        );
        let b = gd(vec![node("Widget", "Widget", "w.ts")], vec![]);
        let composed = compose(vec![("app".into(), a), ("hub".into(), b.clone())]);
        let surfaces = vec![build_export_surface(
            "hub",
            coord("hub", Ecosystem::Npm),
            &b,
        )];
        let mut aliases = crate::alias::AliasMap::default();
        aliases.insert(
            crate::alias::AliasKind::Prefix,
            "@app".to_string(),
            "hub".to_string(),
        );
        let (g, report) = resolve_cross_repo(composed, &surfaces, &aliases);
        assert_eq!(report.inferred, 1, "prefix alias resolved: {report:?}");
        let e = g.links.iter().find(|e| e.source.0 == "app::page").unwrap();
        assert!(
            e.cross_repo && e.target.0.starts_with("hub::"),
            "{:?}",
            e.target
        );
    }

    #[test]
    fn does_not_resolve_self_imports_as_cross_repo() {
        // An import whose package coordinate is the importer's own repo is ignored.
        let a = gd(
            vec![node("app", "app", "app.rs"), node("billing", "billing", "")],
            vec![import_edge("app", "billing", "imports_from")],
        );
        let composed = compose(vec![("repoa".into(), a.clone())]);
        let surfaces = vec![build_export_surface(
            "repoa",
            coord("billing", Ecosystem::Cargo),
            &a,
        )];
        let (_, report) =
            resolve_cross_repo(composed, &surfaces, &crate::alias::AliasMap::default());
        assert_eq!(report.extracted + report.inferred, 0);
    }
}
