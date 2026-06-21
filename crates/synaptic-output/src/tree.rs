//! D3 collapsible-tree HTML over the containment hierarchy (file → class →
//! method / function), built from `contains`/`method` edges. Emits a
//! `{name, total_count, children}` tree + a D3 page.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use serde_json::{json, Value};
use synaptic_core::NodeId;
use synaptic_graph::KnowledgeGraph;

const SOURCE_EXTS: &[&str] = &[
    ".py", ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".go", ".rs", ".java",
];

/// Build the `{name, total_count, children}` hierarchy as JSON.
pub fn build_tree(kg: &KnowledgeGraph) -> Value {
    // parent -> children via the containment relations.
    let mut children: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut has_parent: HashSet<NodeId> = HashSet::new();
    for e in kg.edges() {
        if matches!(e.relation.as_str(), "contains" | "method") {
            children
                .entry(e.source.clone())
                .or_default()
                .push(e.target.clone());
            has_parent.insert(e.target.clone());
        }
    }
    let label_of = |id: &NodeId| -> String {
        kg.node(id)
            .map(|n| n.label.clone())
            .unwrap_or_else(|| id.0.clone())
    };

    // Roots: file-like nodes (label ends with a source ext) with no parent, in
    // graph order for determinism.
    let mut roots: Vec<NodeId> = kg
        .nodes()
        .filter(|n| !has_parent.contains(&n.id) && SOURCE_EXTS.iter().any(|e| n.label.ends_with(e)))
        .map(|n| n.id.clone())
        .collect();
    // Fall back to any parentless node with children (non-file roots).
    if roots.is_empty() {
        roots = kg
            .nodes()
            .filter(|n| !has_parent.contains(&n.id) && children.contains_key(&n.id))
            .map(|n| n.id.clone())
            .collect();
    }

    let mut visiting = HashSet::new();
    let root_nodes: Vec<Value> = roots
        .iter()
        .map(|r| build_node(r, &children, &label_of, &mut visiting))
        .collect();
    let total: u64 = root_nodes
        .iter()
        .filter_map(|n| n["total_count"].as_u64())
        .sum();
    json!({"name": "root", "total_count": total, "children": root_nodes})
}

fn build_node(
    id: &NodeId,
    children: &HashMap<NodeId, Vec<NodeId>>,
    label_of: &impl Fn(&NodeId) -> String,
    visiting: &mut HashSet<NodeId>,
) -> Value {
    if !visiting.insert(id.clone()) {
        // Cycle guard: render as a leaf.
        return json!({"name": label_of(id), "total_count": 1, "children": []});
    }
    let kids: Vec<Value> = children
        .get(id)
        .map(|cs| {
            cs.iter()
                .map(|c| build_node(c, children, label_of, visiting))
                .collect()
        })
        .unwrap_or_default();
    visiting.remove(id);
    let subtotal: u64 = kids.iter().filter_map(|k| k["total_count"].as_u64()).sum();
    json!({
        "name": label_of(id),
        "total_count": subtotal + 1,
        "children": kids,
    })
}

/// Render the D3 tree HTML page.
pub fn to_tree_html_string(kg: &KnowledgeGraph) -> String {
    // Embedded in <script>: neutralize `</` so a label can't break out.
    let tree_json = serde_json::to_string(&build_tree(kg))
        .unwrap_or_else(|_| "{}".into())
        .replace("</", "<\\/");
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Synaptic — Tree</title>
<script src="https://d3js.org/d3.v7.min.js"></script>
<style>
  body {{ margin: 0; font-family: system-ui, sans-serif; }}
  .node circle {{ fill: #4caf50; stroke: #2e7d32; stroke-width: 1.5px; }}
  .node text {{ font-size: 12px; }}
  .link {{ fill: none; stroke: #bbb; stroke-width: 1.2px; }}
  svg {{ cursor: grab; }}
  svg:active {{ cursor: grabbing; }}
  #controls {{ position: fixed; top: 8px; right: 8px; z-index: 10; display: flex; gap: 6px;
    align-items: center; background: rgba(255,255,255,0.92); padding: 6px 8px;
    border: 1px solid #ddd; border-radius: 6px; font-size: 12px; }}
  #controls button {{ font-size: 14px; min-width: 28px; height: 28px; cursor: pointer; }}
  #controls .hint {{ color: #666; }}
</style>
</head>
<body>
<div id="controls">
  <button id="zin" title="Zoom in">+</button>
  <button id="zout" title="Zoom out">&minus;</button>
  <button id="fit" title="Fit to window">Fit</button>
  <span class="hint">drag to pan · scroll to zoom</span>
</div>
<svg width="100%" height="100vh"></svg>
<script>
  const data = {tree_json};
  const root = d3.hierarchy(data);
  const dx = 18, dy = 180;
  d3.tree().nodeSize([dx, dy])(root);
  const svg = d3.select("svg");
  const g = svg.append("g");
  g.selectAll(".link").data(root.links()).join("path").attr("class", "link")
    .attr("d", d3.linkHorizontal().x(d => d.y).y(d => d.x));
  const node = g.selectAll(".node").data(root.descendants()).join("g")
    .attr("class", "node").attr("transform", d => `translate(${{d.y}},${{d.x}})`);
  node.append("circle").attr("r", 4);
  node.append("text").attr("dy", "0.31em").attr("x", d => d.children ? -8 : 8)
    .attr("text-anchor", d => d.children ? "end" : "start")
    .text(d => d.data.name + (d.data.total_count > 1 ? ` (${{d.data.total_count}})` : ""));

  // Pan (drag) + zoom (wheel / buttons). A huge tree is unusable without this.
  const zoom = d3.zoom().scaleExtent([0.01, 4]).on("zoom", ev => g.attr("transform", ev.transform));
  svg.call(zoom).on("dblclick.zoom", null);
  // Fit the whole tree into the viewport on load, so even a very large tree is
  // visible (and you can then zoom in / pan to explore).
  function fit() {{
    const b = g.node().getBBox();
    if (!b.width || !b.height) return;
    const sw = svg.node().clientWidth, sh = svg.node().clientHeight;
    const scale = Math.min(sw / (b.width + 80), sh / (b.height + 80), 1);
    const tx = (sw - b.width * scale) / 2 - b.x * scale;
    const ty = (sh - b.height * scale) / 2 - b.y * scale;
    svg.transition().duration(250)
       .call(zoom.transform, d3.zoomIdentity.translate(tx, ty).scale(scale));
  }}
  fit();
  document.getElementById("fit").onclick = fit;
  document.getElementById("zin").onclick = () => svg.transition().call(zoom.scaleBy, 1.4);
  document.getElementById("zout").onclick = () => svg.transition().call(zoom.scaleBy, 1 / 1.4);
</script>
</body>
</html>
"#,
        tree_json = tree_json,
    )
}

/// Write `tree.html`.
pub fn to_tree_html(kg: &KnowledgeGraph, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, to_tree_html_string(kg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node};
    use synaptic_graph::KnowledgeGraph;

    fn n(id: &str, label: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: "m.py".into(),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }
    fn e(s: &str, t: &str, rel: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            source_file: "m.py".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn kg() -> KnowledgeGraph {
        // m.py contains class Widget; Widget has method .render().
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                n("m_py", "m.py"),
                n("m_widget", "Widget"),
                n("m_widget_render", ".render()"),
            ],
            links: vec![
                e("m_py", "m_widget", "contains"),
                e("m_widget", "m_widget_render", "method"),
            ],
            hyperedges: vec![],
            built_at_commit: None,
        };
        KnowledgeGraph::from_graph_data(gd)
    }

    #[test]
    fn tree_nests_file_class_method() {
        let t = build_tree(&kg());
        let file = &t["children"][0];
        assert_eq!(file["name"], "m.py");
        assert_eq!(file["total_count"], 3); // file + class + method
        let class = &file["children"][0];
        assert_eq!(class["name"], "Widget");
        assert_eq!(class["children"][0]["name"], ".render()");
    }

    #[test]
    fn tree_html_embeds_data_and_d3() {
        let html = to_tree_html_string(&kg());
        assert!(html.contains("d3.hierarchy"));
        assert!(html.contains("m.py"));
    }

    #[test]
    fn tree_html_is_pan_zoomable() {
        // A large tree must be navigable: drag-to-pan + scroll/buttons-to-zoom,
        // with an initial fit-to-view (so "zoomed out very far" is the default).
        let html = to_tree_html_string(&kg());
        assert!(html.contains("d3.zoom"), "tree must wire up d3 pan/zoom");
        assert!(
            html.contains("getBBox"),
            "tree must fit the graph to the viewport"
        );
    }
}
