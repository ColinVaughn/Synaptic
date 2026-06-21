//! Generalize impact prediction beyond rename: given a symbol and the KIND of
//! edit (delete, signature change, visibility narrowing), classify the dependents
//! that would break vs merely need review. The refactor crate plans rename/move/
//! extract; this answers "what breaks if I delete / change the signature of /
//! make private symbol X" without producing an edit plan.

use serde::{Deserialize, Serialize};

use synaptic_core::{Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_query::{
    affected_nodes_multi, module_importers, resolve_seed, DEFAULT_AFFECTED_RELATIONS,
};

fn strip_call(label: &str) -> &str {
    label.split('(').next().unwrap_or(label).trim()
}

/// The outcome of resolving among real definitions.
enum DefMatch {
    One(NodeId),
    /// Several real definitions matched -- the caller must NOT fall back to a
    /// lenient resolver (that could pick a synthetic node), it should ask the user
    /// to qualify.
    Ambiguous,
    /// No real definition matched at all.
    None,
}

/// Resolve a unique edit target among REAL code definitions (nodes with a
/// non-empty `source_file`) whose file additionally passes `file_ok`. Prefers an
/// exact id/label/parens-stripped match, then a unique substring match.
/// Restricting to definitions keeps a synthetic import/module node that shares the
/// name (common in federated graphs, e.g. an `npm_bootstrap` node with an empty
/// file) from shadowing the actual symbol.
fn resolve_def(kg: &KnowledgeGraph, name: &str, file_ok: impl Fn(&Node) -> bool) -> DefMatch {
    let nl = name.to_lowercase();
    let is_def = |n: &&Node| !n.source_file.is_empty() && file_ok(n);
    let pick = |ids: Vec<NodeId>| match ids.as_slice() {
        [only] => DefMatch::One(only.clone()),
        [] => DefMatch::None,
        _ => DefMatch::Ambiguous,
    };
    let exact: Vec<NodeId> = kg
        .nodes()
        .filter(is_def)
        .filter(|n| {
            let ll = n.label.to_lowercase();
            n.id.0 == name || ll == nl || strip_call(&ll) == strip_call(&nl)
        })
        .map(|n| n.id.clone())
        .collect();
    if !exact.is_empty() {
        return pick(exact);
    }
    let subs: Vec<NodeId> = kg
        .nodes()
        .filter(is_def)
        .filter(|n| n.label.to_lowercase().contains(&nl))
        .map(|n| n.id.clone())
        .collect();
    pick(subs)
}

/// Resolve the target of an edit. Two improvements over the lenient `resolve_seed`
/// that matter on real (especially federated) graphs:
/// - a `name@file-substring` qualifier pins an ambiguous name -- one shared by
///   several files/repos -- to one file;
/// - resolution prefers real code definitions, so a synthetic import/module node
///   sharing the name does not shadow the actual symbol (and a name shared by
///   several real definitions is correctly reported as ambiguous rather than
///   silently resolving to one).
pub(crate) fn resolve_edit_target(kg: &KnowledgeGraph, symbol: &str) -> Option<NodeId> {
    if let Some((name, hint)) = symbol.split_once('@') {
        let name = name.trim();
        let hint = hint.trim().replace('\\', "/").to_lowercase();
        if name.is_empty() || hint.is_empty() {
            return None;
        }
        // The user qualified explicitly; do not fall back to the lenient resolver.
        return match resolve_def(kg, name, |n| {
            n.source_file
                .replace('\\', "/")
                .to_lowercase()
                .contains(&hint)
        }) {
            DefMatch::One(id) => Some(id),
            _ => None,
        };
    }
    // No hint: a unique real definition wins; several definitions are ambiguous
    // (qualify, do not guess); only when NO definition matches do we defer to the
    // lenient resolver (so a symbol that legitimately has no source file -- rare --
    // still resolves).
    match resolve_def(kg, symbol.trim(), |_| true) {
        DefMatch::One(id) => Some(id),
        DefMatch::Ambiguous => None,
        DefMatch::None => resolve_seed(kg, symbol),
    }
}

/// The kind of edit whose impact to predict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Remove the symbol entirely: every dependent breaks.
    Delete,
    /// Change the signature (params/return/shape): callers and type users may break.
    Signature,
    /// Narrow visibility (e.g. public -> private): references from other files break.
    Visibility,
}

impl EditKind {
    /// Parse an edit-kind name (case-insensitive). Returns `None` if unknown.
    pub fn parse(s: &str) -> Option<EditKind> {
        match s.to_ascii_lowercase().as_str() {
            "delete" | "remove" | "deletion" => Some(EditKind::Delete),
            "signature" | "sig" | "signature-change" => Some(EditKind::Signature),
            "visibility" | "private" | "narrow" => Some(EditKind::Visibility),
            _ => None,
        }
    }

    /// The canonical name of this edit kind.
    pub fn as_str(self) -> &'static str {
        match self {
            EditKind::Delete => "delete",
            EditKind::Signature => "signature",
            EditKind::Visibility => "visibility",
        }
    }
}

/// One dependent classified for an edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditDependent {
    pub label: String,
    pub file: String,
    pub depth: usize,
    pub via_relation: String,
    pub reason: String,
}

/// The predicted impact of an edit kind on a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditImpact {
    pub symbol: String,
    pub kind: String,
    pub target_file: String,
    /// Dependents that will (almost certainly) break.
    pub breaks: Vec<EditDependent>,
    /// Dependents to re-check (may or may not break).
    pub review: Vec<EditDependent>,
    pub summary: String,
}

fn norm(p: &str) -> String {
    p.replace('\\', "/")
}

/// Predict the impact of `kind` on `symbol`. Returns `None` if the symbol cannot
/// be resolved unambiguously. Walks the reverse-impact set and classifies each
/// dependent by the edit kind and the relation it depends through.
pub fn assess_edit(
    kg: &KnowledgeGraph,
    symbol: &str,
    kind: EditKind,
    depth: usize,
) -> Option<EditImpact> {
    let id = resolve_edit_target(kg, symbol)?;
    let target = kg.node(&id);
    let target_file = target.map(|n| n.source_file.clone()).unwrap_or_default();
    let target_norm = norm(&target_file);
    let target_repo = target.and_then(|n| n.repo.clone());

    let hits = affected_nodes_multi(
        kg,
        std::slice::from_ref(&id),
        DEFAULT_AFFECTED_RELATIONS,
        depth,
    );
    let mut breaks = Vec::new();
    let mut review = Vec::new();
    let mut seen: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    for hit in hits {
        let Some(node) = kg.node(&hit.node_id) else {
            continue;
        };
        seen.insert(hit.node_id.clone());
        let file = node.source_file.clone();
        let rel = hit.via_relation.clone();
        // Different file OR different federated repo counts as cross-file: a same
        // relative path in another repo is not the same file.
        let cross_file = norm(&file) != target_norm || node.repo != target_repo;

        let classified = match kind {
            EditKind::Delete => Some((
                true,
                "depends on a symbol that would no longer exist".to_string(),
            )),
            EditKind::Signature => match rel.as_str() {
                // A bare import still resolves after a signature change; the call
                // sites are where breakage shows up, so route imports to review.
                "imports" | "imports_from" | "re_exports" => {
                    Some((false, "imports the symbol; re-check its uses".to_string()))
                }
                _ => Some((
                    true,
                    format!("{rel} the symbol; a signature change may break it"),
                )),
            },
            EditKind::Visibility => {
                if cross_file {
                    Some((
                        true,
                        "references the symbol from another file; narrowing visibility blocks it"
                            .to_string(),
                    ))
                } else {
                    None // same-file use is unaffected by narrowing to private
                }
            }
        };

        if let Some((will_break, reason)) = classified {
            let dep = EditDependent {
                label: node.label.clone(),
                file,
                depth: hit.depth,
                via_relation: rel,
                reason,
            };
            if will_break {
                breaks.push(dep);
            } else {
                review.push(dep);
            }
        }
    }

    // Module-level importers: files that import the symbol's module but whose link
    // is only a stub edge the symbol-level walk cannot reach (e.g. a test that does
    // `import { sym } from './mod'` and calls it at top level). For delete this is
    // the dangerous direction to under-report, so an exported symbol with such
    // importers never silently reports zero.
    for mi in module_importers(kg, &id) {
        if !seen.insert(mi.node_id.clone()) {
            continue; // already counted via a resolved edge
        }
        let Some(node) = kg.node(&mi.node_id) else {
            continue;
        };
        let (will_break, reason) = match kind {
            EditKind::Delete => {
                if mi.confirmed {
                    (
                        true,
                        "imports this symbol; it would no longer exist".to_string(),
                    )
                } else {
                    (
                        false,
                        "imports this module; re-check whether it uses the deleted symbol"
                            .to_string(),
                    )
                }
            }
            EditKind::Signature => (
                false,
                "imports the symbol; re-check its uses after the signature change".to_string(),
            ),
            EditKind::Visibility => {
                if mi.confirmed {
                    (
                        true,
                        "imports the symbol from another file; narrowing visibility blocks it"
                            .to_string(),
                    )
                } else {
                    (
                        false,
                        "imports this module from another file; re-check after narrowing visibility"
                            .to_string(),
                    )
                }
            }
        };
        let dep = EditDependent {
            label: node.label.clone(),
            file: node.source_file.clone(),
            depth: 1,
            via_relation: mi.via_relation,
            reason,
        };
        if will_break {
            breaks.push(dep);
        } else {
            review.push(dep);
        }
    }

    let summary = format!(
        "{} {}: {} dependent(s) will break, {} to review",
        kind.as_str(),
        symbol,
        breaks.len(),
        review.len()
    );
    Some(EditImpact {
        symbol: symbol.to_string(),
        kind: kind.as_str().to_string(),
        target_file,
        breaks,
        review,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};

    fn node(id: &str, label: &str, file: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: file.into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
        }
    }

    fn edge(s: &str, t: &str, r: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: r.into(),
            confidence: Confidence::Extracted,
            source_file: "x".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn kg() -> KnowledgeGraph {
        // Service (svc.py) is depended on by: c1 (calls, a.py), m1 (imports, b.py),
        // s1 (references, svc.py same file).
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("svc", "Service", "svc.py"),
                node("c1", "caller", "a.py"),
                node("m1", "importer", "b.py"),
                node("s1", "sibling", "svc.py"),
            ],
            links: vec![
                edge("c1", "svc", "calls"),
                edge("m1", "svc", "imports"),
                edge("s1", "svc", "references"),
            ],
            hyperedges: vec![],
            built_at_commit: None,
        };
        KnowledgeGraph::from_graph_data(gd)
    }

    #[test]
    fn edit_kind_parses_synonyms() {
        assert_eq!(EditKind::parse("Delete"), Some(EditKind::Delete));
        assert_eq!(EditKind::parse("sig"), Some(EditKind::Signature));
        assert_eq!(EditKind::parse("private"), Some(EditKind::Visibility));
        assert_eq!(EditKind::parse("frobnicate"), None);
    }

    #[test]
    fn delete_breaks_every_dependent() {
        let r = assess_edit(&kg(), "Service", EditKind::Delete, 5).unwrap();
        assert_eq!(r.breaks.len(), 3, "{r:?}");
        assert!(r.review.is_empty());
    }

    #[test]
    fn signature_breaks_callers_but_reviews_imports() {
        let r = assess_edit(&kg(), "Service", EditKind::Signature, 5).unwrap();
        let breaks: Vec<&str> = r.breaks.iter().map(|d| d.label.as_str()).collect();
        let review: Vec<&str> = r.review.iter().map(|d| d.label.as_str()).collect();
        assert!(
            breaks.contains(&"caller") && breaks.contains(&"sibling"),
            "{breaks:?}"
        );
        assert_eq!(review, vec!["importer"], "imports routed to review");
    }

    #[test]
    fn visibility_breaks_only_cross_file_references() {
        let r = assess_edit(&kg(), "Service", EditKind::Visibility, 5).unwrap();
        let breaks: Vec<&str> = r.breaks.iter().map(|d| d.label.as_str()).collect();
        // caller (a.py) and importer (b.py) are cross-file -> break; sibling
        // (svc.py, same file) is unaffected by narrowing to private.
        assert!(breaks.contains(&"caller") && breaks.contains(&"importer"));
        assert!(!breaks.contains(&"sibling"), "same-file use unaffected");
        assert!(r.review.is_empty());
    }

    #[test]
    fn unknown_symbol_returns_none() {
        assert!(assess_edit(&kg(), "Nonexistent", EditKind::Delete, 5).is_none());
    }

    #[test]
    fn delete_surfaces_module_level_test_importer() {
        // Repro of the a11ycore miss: darkMode.test.ts does
        // `import { toggleDarkMode } from './darkMode'` and calls it at top level,
        // so the only edge is a module-level imports_from to the './darkMode' stub
        // (no resolved symbol edge). Deleting toggleDarkMode must still flag the test.
        let mut imp = edge("test_file", "stub_darkmode", "imports_from");
        imp.extra.insert(
            "imported".into(),
            serde_json::json!(["toggleDarkMode", "setTheme"]),
        );
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("toggle", "toggleDarkMode", "src/darkMode.ts"),
                node("test_file", "darkMode.test.ts", "tests/darkMode.test.ts"),
                // module stub: import specifier as label, no source file.
                node("stub_darkmode", "./darkMode", ""),
            ],
            links: vec![imp],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        let r = assess_edit(&kg, "toggleDarkMode", EditKind::Delete, 3).unwrap();
        assert!(
            r.breaks.iter().any(|d| d.file.contains("darkMode.test.ts")),
            "the test importer must be flagged as breaking, got {r:?}"
        );
        assert!(
            !(r.breaks.is_empty() && r.review.is_empty()),
            "an exported symbol with a module importer must never report a bare 0"
        );
    }

    #[test]
    fn delete_routes_opaque_module_importer_to_review() {
        // A namespace/default import records no symbol names -> uncertain, so it is
        // surfaced for review rather than assumed to break.
        let imp = edge("ns_file", "stub_darkmode", "imports_from"); // no `imported` tag
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("toggle", "toggleDarkMode", "src/darkMode.ts"),
                node("ns_file", "consumer.ts", "src/consumer.ts"),
                node("stub_darkmode", "./darkMode", ""),
            ],
            links: vec![imp],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        let r = assess_edit(&kg, "toggleDarkMode", EditKind::Delete, 3).unwrap();
        assert!(
            r.review.iter().any(|d| d.file.contains("consumer.ts")),
            "opaque importer should be in review, got {r:?}"
        );
    }

    #[test]
    fn resolve_edit_target_disambiguates_an_ambiguous_name_by_file() {
        // Two `helper`s in different files -- the common real-world case the bare
        // name cannot resolve.
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("a_helper", "helper()", "src/a.py"),
                node("b_helper", "helper()", "src/b.py"),
            ],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        // Bare ambiguous name -> no unique target.
        assert!(resolve_edit_target(&kg, "helper").is_none());
        // `name@file` pins it; a partial file substring is enough.
        assert_eq!(
            resolve_edit_target(&kg, "helper@src/a.py").unwrap().0,
            "a_helper"
        );
        assert_eq!(
            resolve_edit_target(&kg, "helper@b.py").unwrap().0,
            "b_helper"
        );
        // A hint that matches no file -> None.
        assert!(resolve_edit_target(&kg, "helper@nope.py").is_none());
        // The qualifier also flows through assess_edit.
        let r = assess_edit(&kg, "helper@a.py", EditKind::Delete, 3).unwrap();
        assert_eq!(r.target_file, "src/a.py");
    }

    #[test]
    fn resolve_edit_target_prefers_real_defs_over_synthetic_nodes() {
        // A real foo() definition plus a synthetic 'foo' import/module node with no
        // source file (the federated-graph case: an `npm_bootstrap`-style node).
        let real_plus_synthetic = |reals: &[(&str, &str, &str)]| {
            let mut nodes: Vec<Node> = reals
                .iter()
                .map(|(id, label, file)| node(id, label, file))
                .collect();
            nodes.push(node("synthetic", "foo", "")); // empty source_file
            KnowledgeGraph::from_graph_data(GraphData {
                directed: true,
                multigraph: false,
                graph: Map::new(),
                nodes,
                links: vec![],
                hyperedges: vec![],
                built_at_commit: None,
            })
        };
        // One real def -> the synthetic node must not shadow it.
        let kg = real_plus_synthetic(&[("real", "foo()", "src/a.js")]);
        assert_eq!(resolve_edit_target(&kg, "foo").unwrap().0, "real");
        // Two real defs -> ambiguous; must NOT fall through to the synthetic node.
        let kg2 = real_plus_synthetic(&[("a", "foo()", "a.js"), ("b", "foo()", "b.js")]);
        assert!(
            resolve_edit_target(&kg2, "foo").is_none(),
            "ambiguous real defs must not silently resolve to a synthetic node"
        );
    }

    #[test]
    fn visibility_treats_same_path_in_another_repo_as_cross_file() {
        let mut target = node("svc", "Service", "util.py");
        target.repo = Some("repoA".into());
        let mut dep = node("d", "user", "util.py"); // same relative path...
        dep.repo = Some("repoB".into()); // ...but a different federated repo
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![target, dep],
            links: vec![edge("d", "svc", "references")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        let r = assess_edit(&kg, "Service", EditKind::Visibility, 5).unwrap();
        // Same path, different repo -> cross-file -> breaks (not dropped as same-file).
        assert_eq!(r.breaks.len(), 1, "{r:?}");
        assert_eq!(r.breaks[0].label, "user");
    }
}
