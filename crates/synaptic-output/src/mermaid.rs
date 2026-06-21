//! Mermaid call-flow HTML: a `graph LR` of the graph's edges, rendered by
//! mermaid.js from a CDN. Capped to keep mermaid responsive; the full
//! interactive explorer is `graph.html`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use synaptic_core::NodeId;
use synaptic_graph::KnowledgeGraph;

/// Max edges rendered before truncating (mermaid degrades on huge diagrams).
const MAX_EDGES: usize = 250;

/// Make a label safe inside a quoted mermaid node (`id["label"]`). Mermaid's
/// parser is fragile with arbitrary code/heading text, so fold every character
/// that can break a quoted label or change its parse mode to a safe visual
/// equivalent: backticks (which trigger markdown-string mode), straight and
/// smart double-quotes (the label delimiter), `|` (the edge-label delimiter),
/// and angle brackets (parsed as HTML); collapse whitespace. An empty result
/// becomes a placeholder so we never emit `id[""]`.
fn mermaid_label(s: &str) -> String {
    let folded: String = s
        .chars()
        .map(|c| match c {
            '`' | '"' | '\u{201C}' | '\u{201D}' => '\'',
            '|' => '/',
            '<' => '(',
            '>' => ')',
            '\n' | '\r' | '\t' => ' ',
            _ => c,
        })
        .collect();
    let trimmed = folded.trim();
    if trimmed.is_empty() {
        "(unnamed)".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Sanitize a raw node id into a valid Mermaid identifier. `make_id` permits
/// Unicode letters, leading digits, and even empty ids; emitting those raw
/// produces invalid Mermaid that fails to render the whole diagram. Fold to
/// `[A-Za-z0-9_]`, then prefix `n` if it's empty or doesn't start with a letter.
fn sanitize_mermaid_id(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    match s.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => s,
        _ => format!("n{s}"),
    }
}

/// Render a Mermaid call-flow HTML page.
pub fn to_mermaid_string(kg: &KnowledgeGraph) -> String {
    let gd = kg.to_graph_data();
    let label_of = |id: &synaptic_core::NodeId| -> String {
        gd.nodes
            .iter()
            .find(|n| &n.id == id)
            .map(|n| n.label.clone())
            .unwrap_or_else(|| id.0.clone())
    };

    // Stable raw-id -> ASCII Mermaid-id map (sanitized, with a numeric suffix to
    // disambiguate fold collisions, deterministic in node-iteration order), so
    // node declarations and edge endpoints always use the same valid identifier.
    let mut mid: HashMap<NodeId, String> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for n in &gd.nodes {
        let base = sanitize_mermaid_id(&n.id.0);
        let mut m = base.clone();
        let mut i = 2;
        while !used.insert(m.clone()) {
            m = format!("{base}_{i}");
            i += 1;
        }
        mid.insert(n.id.clone(), m);
    }
    let mid_of = |id: &NodeId| {
        mid.get(id)
            .cloned()
            .unwrap_or_else(|| sanitize_mermaid_id(&id.0))
    };

    let total = gd.links.len();
    let shown = total.min(MAX_EDGES);
    let mut diagram = String::from("graph LR\n");
    let mut emitted_nodes: HashSet<NodeId> = HashSet::new();
    for e in gd.links.iter().take(shown) {
        for ep in [&e.source, &e.target] {
            if emitted_nodes.insert(ep.clone()) {
                diagram.push_str(&format!(
                    "  {}[\"{}\"]\n",
                    mid_of(ep),
                    mermaid_label(&label_of(ep))
                ));
            }
        }
        diagram.push_str(&format!(
            "  {} -->|{}| {}\n",
            mid_of(&e.source),
            mermaid_label(&e.relation),
            mid_of(&e.target)
        ));
    }
    let note = if total > shown {
        format!("<p class=\"note\">Showing {shown} of {total} edges.</p>")
    } else {
        String::new()
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Synaptic — Call Flow</title>
<script src="https://unpkg.com/mermaid/dist/mermaid.min.js"></script>
<style>
  body {{ margin: 0; font-family: system-ui, sans-serif; background: #0f172a; color: #e2e8f0; }}
  header {{ padding: 12px 16px; border-bottom: 1px solid #334155; }}
  .note {{ padding: 0 16px; color: #94a3b8; }}
  .mermaid {{ padding: 16px; }}
</style>
</head>
<body>
<header><strong>Synaptic</strong> · call flow ({node_count} nodes · {edge_count} edges)</header>
{note}
<pre class="mermaid">
{diagram}</pre>
<script>mermaid.initialize({{ startOnLoad: true, theme: "dark" }});</script>
</body>
</html>
"#,
        node_count = gd.nodes.len(),
        edge_count = total,
        note = note,
        diagram = diagram,
    )
}

/// Write `callflow.html`.
pub fn to_mermaid(kg: &KnowledgeGraph, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, to_mermaid_string(kg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::sample_kg;

    #[test]
    fn mermaid_renders_graph_and_edges() {
        let html = to_mermaid_string(&sample_kg());
        assert!(html.contains("graph LR"));
        assert!(html.contains("a[\"A\"]"));
        assert!(html.contains("a -->|calls| b"));
        assert!(html.contains("mermaid.initialize"));
    }

    #[test]
    fn mermaid_label_is_parse_safe() {
        // Backticks trigger mermaid's markdown-string mode; straight/smart quotes
        // and `|` (edge-label delimiter) break a quoted label. All must be
        // neutralized so arbitrary code labels can't produce "Syntax error".
        let out = mermaid_label("`GET /health` say \u{201C}hi\u{201D} a|b \"x\"");
        assert!(!out.contains('`'), "backtick not neutralized: {out}");
        assert!(!out.contains('"'), "double-quote not neutralized: {out}");
        assert!(
            !out.contains('\u{201C}') && !out.contains('\u{201D}'),
            "smart quote: {out}"
        );
        assert!(!out.contains('|'), "pipe not neutralized: {out}");
        assert!(
            !out.contains("&quot;"),
            "must not emit raw entity text: {out}"
        );
        // An all-whitespace / empty label still yields something renderable.
        assert!(!mermaid_label("   ").is_empty());
    }

    #[test]
    fn mermaid_ids_are_sanitized() {
        // Unicode letters and a leading digit are folded; empty becomes "n".
        assert_eq!(sanitize_mermaid_id("ok_id"), "ok_id");
        assert_eq!(sanitize_mermaid_id("2048_py"), "n2048_py");
        assert_eq!(sanitize_mermaid_id(""), "n");
        assert_eq!(sanitize_mermaid_id("café_run"), "caf__run");
        // No invalid leading-digit identifier survives.
        assert!(sanitize_mermaid_id("9x").starts_with('n'));
    }
}
