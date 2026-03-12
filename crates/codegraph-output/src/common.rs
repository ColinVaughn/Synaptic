//! Shared helpers for the output writers.

use std::collections::{BTreeMap, HashMap};

use codegraph_core::{Confidence, FileType, Node, NodeId};
use codegraph_graph::KnowledgeGraph;

/// Undirected degree (count of incident edges, self-loops once) per node id.
pub(crate) fn degrees(kg: &KnowledgeGraph) -> HashMap<NodeId, usize> {
    let mut d: HashMap<NodeId, usize> = HashMap::new();
    for n in kg.nodes() {
        d.entry(n.id.clone()).or_insert(0);
    }
    for e in kg.edges() {
        *d.entry(e.source.clone()).or_insert(0) += 1;
        if e.target != e.source {
            *d.entry(e.target.clone()).or_insert(0) += 1;
        }
    }
    d
}

/// Lowercase serde name of a file type (`"code"`, `"document"`, …).
pub(crate) fn file_type_str(ft: &FileType) -> String {
    serde_json::to_value(ft)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "code".to_string())
}

/// Uppercase serde name of a confidence (`"EXTRACTED"`, …).
pub(crate) fn confidence_str(c: &Confidence) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "EXTRACTED".to_string())
}

/// Categorical palette for community colouring, shared by the SVG and 3D
/// writers.
pub(crate) const COMMUNITY_COLORS: &[&str] = &[
    "#4caf50", "#2196f3", "#ff9800", "#e91e63", "#9c27b0", "#00bcd4", "#ffc107", "#795548",
    "#607d8b", "#f44336", "#3f51b5", "#8bc34a",
];

/// Colour for a community index (wraps around the palette).
pub(crate) fn community_color(idx: usize) -> &'static str {
    COMMUNITY_COLORS[idx % COMMUNITY_COLORS.len()]
}

/// Accent for federated cross-repo edges — distinct from the community palette
/// and the confidence colors (green/orange/red).
pub(crate) const CROSS_REPO_COLOR: &str = "#00e5ff";

/// Categorical palette for repos (federation). Separate name from
/// `COMMUNITY_COLORS` so the two can diverge.
pub(crate) const REPO_COLORS: &[&str] = &[
    "#e6194b", "#3cb44b", "#ffe119", "#4363d8", "#f58231", "#911eb4", "#46f0f0", "#f032e6",
    "#bcf60c", "#fabebe",
];

/// Colour for a repo index (wraps around the palette).
pub(crate) fn repo_color(idx: usize) -> &'static str {
    REPO_COLORS[idx % REPO_COLORS.len()]
}

/// Sorted unique repo tags → stable index. Empty when the graph is single-repo.
pub(crate) fn repo_index(kg: &KnowledgeGraph) -> BTreeMap<String, usize> {
    let mut tags: Vec<&str> = kg.nodes().filter_map(|n| n.repo.as_deref()).collect();
    tags.sort_unstable();
    tags.dedup();
    tags.iter()
        .enumerate()
        .map(|(i, t)| (t.to_string(), i))
        .collect()
}

/// True if a node is a third-party external-package stub (cross-repo resolution).
pub(crate) fn is_external_package(n: &Node) -> bool {
    n.extra.get("external_package").and_then(|v| v.as_bool()) == Some(true)
}

/// Escape text for an XML attribute value or element body.
pub(crate) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Drop XML-illegal control chars (keep tab/newline/cr).
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod fed_tests {
    use super::*;
    use crate::tests_support::{kg_federated, sample_kg};

    #[test]
    fn repo_index_is_sorted_and_stable() {
        let idx = repo_index(&kg_federated());
        assert_eq!(idx.get("app"), Some(&0));
        assert_eq!(idx.get("billing"), Some(&1));
        assert!(repo_index(&sample_kg()).is_empty(), "single-repo → empty");
    }

    #[test]
    fn external_package_predicate() {
        let kg = kg_federated();
        assert!(
            kg.nodes().any(is_external_package),
            "fixture has an external stub"
        );
        assert!(
            !sample_kg().nodes().any(is_external_package),
            "no externals in a plain graph"
        );
    }
}
