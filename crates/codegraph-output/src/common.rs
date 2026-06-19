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

/// Categorical palette for community coloring, shared by the SVG and 3D
/// writers.
pub(crate) const COMMUNITY_COLORS: &[&str] = &[
    "#4caf50", "#2196f3", "#ff9800", "#e91e63", "#9c27b0", "#00bcd4", "#ffc107", "#795548",
    "#607d8b", "#f44336", "#3f51b5", "#8bc34a",
];

/// Color for a community index (wraps around the palette).
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

/// Color for a repo index (wraps around the palette).
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

/// The node's visual kind: the real `NodeKind` the extractor set (table / column
/// / function / class / …), else its asset kind, else a coarse guess from the
/// label. Drives shape, color, and legends across the SVG / 3D / HTML viewers so
/// the SQL and cross-language layers are visible, not just "code".
pub(crate) fn visual_kind(n: &Node) -> &'static str {
    if let Some(k) = n.kind() {
        return k.as_str();
    }
    match n.extra.get("asset_kind").and_then(|v| v.as_str()) {
        Some("stylesheet") => return "stylesheet",
        Some("data") => return "data",
        Some("image") => return "image",
        Some("font") => return "font",
        Some("media") => return "media",
        Some(_) => return "asset",
        None => {}
    }
    if n.label.ends_with("()") {
        if n.label.starts_with('.') {
            "method"
        } else {
            "function"
        }
    } else {
        "symbol"
    }
}

/// Categorical color per visual kind (for "color by kind" mode + legends). SQL
/// objects get a warm/distinct family; code symbols a cool family.
pub(crate) fn kind_color(kind: &str) -> &'static str {
    match kind {
        "table" => "#ec407a",
        "view" => "#ffa726",
        "column" => "#64b5f6",
        "index" => "#26a69a",
        "trigger" => "#ab47bc",
        "procedure" => "#7e57c2",
        "policy" => "#ef5350",
        "role" => "#8d6e63",
        "function" => "#66bb6a",
        "method" => "#9ccc65",
        "constructor" => "#aed581",
        "class" | "struct" | "interface" | "trait" | "enum" | "type_alias" | "object"
        | "protocol" => "#42a5f5",
        "module" | "namespace" | "package" => "#5c6bc0",
        "property" | "field" | "constant" | "variable" => "#26c6da",
        "macro" => "#ff7043",
        "stylesheet" => "#f06292",
        "data" => "#ffd54f",
        "image" => "#4dd0e1",
        "font" => "#ce93d8",
        "media" => "#4db6ac",
        _ => "#b0bec5",
    }
}

/// Canonical shape token per visual kind. Each viewer maps it to its own drawing
/// primitive (SVG polygon, 3D mesh, vis-network shape).
pub(crate) fn kind_shape(kind: &str) -> &'static str {
    match kind {
        "table" => "diamond",
        "view" => "triangle",
        "column" => "dot",
        "index" => "square",
        "trigger" => "star",
        "procedure" => "hexagon",
        "policy" => "triangle_down",
        "role" => "square",
        "class" | "struct" | "interface" | "trait" | "enum" | "object" | "type_alias"
        | "protocol" => "square",
        "module" | "namespace" | "package" => "hexagon",
        "stylesheet" => "square",
        "data" => "diamond",
        "image" => "triangle",
        "font" => "hexagon",
        "media" => "pentagon",
        _ => "circle",
    }
}

/// Categorical color per edge relation, making cross-language (code → SQL)
/// bridges and SQL structural edges pop over generic code edges.
pub(crate) fn relation_color(rel: &str) -> &'static str {
    match rel {
        // cross-language: code -> SQL (the bridges).
        "queries" | "writes_to" | "calls_proc" => "#00e5ff",
        // SQL structure.
        "references" => "#ec407a",
        "has_column" => "#7e57c2",
        "has_index" | "indexes" => "#26a69a",
        "protected_by" => "#ef5350",
        "grants" => "#ff7043",
        "reads_from" => "#ffa726",
        // generic code.
        "calls" => "#78909c",
        "imports" | "imports_from" | "depends_on" => "#90a4ae",
        "extends" | "implements" | "inherits" | "mixes_in" => "#42a5f5",
        "contains" => "#546e7a",
        _ => "#8d9aa5",
    }
}

/// True for the cross-language code → SQL edges (the "bridges"), the headline
/// relation a cross-language graph exists to show.
pub(crate) fn is_bridge_relation(rel: &str) -> bool {
    matches!(rel, "queries" | "writes_to" | "calls_proc")
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

    fn node_with(label: &str, kind: Option<codegraph_core::NodeKind>) -> Node {
        let mut n = Node {
            id: NodeId(label.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: String::new(),
            source_location: None,
            community: None,
            repo: None,
            extra: Default::default(),
        };
        if let Some(k) = kind {
            n.set_kind(k);
        }
        n
    }

    #[test]
    fn visual_kind_prefers_real_nodekind_then_label() {
        use codegraph_core::NodeKind;
        assert_eq!(
            visual_kind(&node_with("orders", Some(NodeKind::Table))),
            "table"
        );
        assert_eq!(
            visual_kind(&node_with("order_id", Some(NodeKind::Column))),
            "column"
        );
        // no kind -> label heuristic.
        assert_eq!(visual_kind(&node_with("parse()", None)), "function");
        assert_eq!(visual_kind(&node_with(".visit()", None)), "method");
        assert_eq!(visual_kind(&node_with("KnowledgeGraph", None)), "symbol");
    }

    #[test]
    fn kind_shapes_and_colors_distinguish_sql() {
        assert_eq!(kind_shape("table"), "diamond");
        assert_eq!(kind_shape("column"), "dot");
        assert_eq!(kind_shape("view"), "triangle");
        assert_ne!(kind_color("table"), kind_color("column"));
        assert_ne!(kind_color("table"), kind_color("function"));
    }

    #[test]
    fn relation_colors_emphasise_bridges() {
        assert_eq!(relation_color("queries"), "#00e5ff");
        assert!(is_bridge_relation("queries") && is_bridge_relation("writes_to"));
        assert!(!is_bridge_relation("calls"));
        assert_ne!(relation_color("has_column"), relation_color("references"));
    }
}
