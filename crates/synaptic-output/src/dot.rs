//! DOT / Graphviz export. Renders the graph as a `digraph` (directed) or
//! `graph` (undirected) that `dot`, `neato`, etc. can lay out. Node ids and all
//! attribute values are emitted as double-quoted DOT strings with `"`/`\`/
//! newlines escaped, so an arbitrary label can't break the statement — the same
//! defensive posture as [`crate::cypher`] (which has the stricter job of
//! sanitising identifier-position tokens; DOT lets us quote everything).
//!
//! Older Synaptic had no DOT exporter (roadmap I-26).

use std::fs;
use std::io;
use std::path::Path;

use synaptic_graph::KnowledgeGraph;

use crate::common::file_type_str;

/// Escape a string for a DOT double-quoted literal.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            '\t' => out.push(' '),
            // Drop remaining C0 controls + DEL (a raw control could confuse a
            // downstream `dot` parser); keep everything else (incl. Unicode).
            c if (c < ' ') || c == '\u{7f}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Render the graph in the DOT language.
pub fn to_dot_string(kg: &KnowledgeGraph) -> String {
    let gd = kg.to_graph_data();
    let (keyword, arrow) = if gd.directed {
        ("digraph", "->")
    } else {
        ("graph", "--")
    };
    let mut lines = vec![
        format!("{keyword} Synaptic {{"),
        "  rankdir=LR;".to_string(),
        "  node [shape=box];".to_string(),
    ];
    for n in &gd.nodes {
        lines.push(format!(
            "  \"{}\" [label=\"{}\", kind=\"{}\"];",
            escape(&n.id.0),
            escape(&n.label),
            escape(&file_type_str(&n.file_type)),
        ));
    }
    for e in &gd.links {
        lines.push(format!(
            "  \"{}\" {arrow} \"{}\" [label=\"{}\"];",
            escape(&e.source.0),
            escape(&e.target.0),
            escape(&e.relation),
        ));
    }
    lines.push("}".to_string());
    lines.join("\n")
}

/// Write `graph.dot`.
pub fn to_dot(kg: &KnowledgeGraph, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, to_dot_string(kg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{kg_with_label, sample_kg};

    #[test]
    fn dot_has_header_nodes_and_edges() {
        let d = to_dot_string(&sample_kg());
        // sample_kg is undirected: `graph` + `--`.
        assert!(d.starts_with("graph Synaptic {"), "{d}");
        assert!(d.contains("\"a\" [label=\"A\""), "{d}");
        assert!(d.contains("\"a\" -- \"b\" [label=\"calls\"];"), "{d}");
        assert!(d.trim_end().ends_with('}'));
    }

    #[test]
    fn dot_escapes_quotes_and_newlines() {
        let d = to_dot_string(&kg_with_label("evil", "a\"b\nc"));
        assert!(d.contains("label=\"a\\\"b\\nc\""), "got: {d}");
        // The raw quote/newline must not appear unescaped inside the label.
        assert!(!d.contains("a\"b\nc"));
    }
}
