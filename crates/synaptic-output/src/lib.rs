//! Output writers for Synaptic. The canonical artifacts are `graph.json`
//! (NetworkX node-link) and `graph.html` (interactive vis-network explorer);
//! the remaining writers (GraphML, Cypher, Mermaid callflow, D3 tree, Obsidian
//! vault, wiki, SVG) live in their own modules and are re-exported here.
#![forbid(unsafe_code)]

use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;
use synaptic_graph::KnowledgeGraph;

mod common;
pub mod cypher;
pub mod dot;
pub mod force3d;
pub mod graphml;
pub mod mermaid;
pub mod obsidian;
#[cfg(feature = "push")]
pub mod push;
pub mod svg;
pub mod tree;
pub mod wiki;

#[cfg(test)]
mod golden;
#[cfg(test)]
mod tests_support;

pub use cypher::{to_cypher, to_cypher_string};
pub use dot::{to_dot, to_dot_string};
pub use force3d::{to_force3d, to_force3d_html};
pub use graphml::{to_graphml, to_graphml_string};
pub use mermaid::{to_mermaid, to_mermaid_string};
pub use obsidian::to_obsidian;
pub use svg::{to_svg, to_svg_string};
pub use tree::{to_tree_html, to_tree_html_string};
pub use wiki::to_wiki;

/// Diacritic-insensitive search key. MVP: lowercase only (full diacritic
/// stripping, including combining marks, is deferred).
fn norm_label(label: &str) -> String {
    label.to_lowercase()
}

/// The export-ready `GraphData`: node-link with `norm_label` added to each node
/// and `confidence_score` defaulted on each link. Shared by the streaming
/// [`to_json`] and the `Value`-producing [`to_json_value`] so the two cannot
/// drift apart.
fn export_graph_data(kg: &KnowledgeGraph) -> synaptic_core::GraphData {
    let mut gd = kg.to_graph_data();
    for node in &mut gd.nodes {
        let norm = norm_label(&node.label);
        node.extra
            .insert("norm_label".to_string(), Value::String(norm));
    }
    for link in &mut gd.links {
        if link.confidence_score.is_none() {
            link.confidence_score = Some(link.confidence.default_score());
        }
    }
    gd
}

/// Build the export-ready `graph.json` value: node-link with `norm_label` added
/// to each node and `confidence_score` defaulted on each link.
pub fn to_json_value(kg: &KnowledgeGraph) -> Value {
    serde_json::to_value(export_graph_data(kg)).expect("GraphData serializes")
}

/// A `Vec<T>` serialized one element at a time, each routed through
/// `serde_json::to_value` so its keys come out in the sorted (BTreeMap) order
/// `graph.json` has always used. Only one element's `Value` is alive at a time,
/// where the old path built a `Value` tree over the entire graph.
struct SortedElements<'a, T>(&'a [T]);

impl<T: serde::Serialize> serde::Serialize for SortedElements<'_, T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for item in self.0 {
            let value = serde_json::to_value(item).map_err(serde::ser::Error::custom)?;
            seq.serialize_element(&value)?;
        }
        seq.end()
    }
}

/// The graph's nodes, each converted to a `Value` on the fly with `norm_label`
/// injected — the enrichment `export_graph_data` applies to a cloned `GraphData`.
/// Doing it per node means `to_json` never clones the node set at all.
struct ExportNodes<'a>(&'a KnowledgeGraph);

impl serde::Serialize for ExportNodes<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.0.node_count()))?;
        for node in self.0.nodes() {
            let mut value = serde_json::to_value(node).map_err(serde::ser::Error::custom)?;
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "norm_label".to_string(),
                    Value::String(norm_label(&node.label)),
                );
            }
            seq.serialize_element(&value)?;
        }
        seq.end()
    }
}

/// The graph's edges, each converted to a `Value` on the fly with
/// `confidence_score` defaulted, matching `export_graph_data`.
struct ExportLinks<'a>(&'a KnowledgeGraph);

impl serde::Serialize for ExportLinks<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.0.edge_count()))?;
        for edge in self.0.edges() {
            let mut value = serde_json::to_value(edge).map_err(serde::ser::Error::custom)?;
            if edge.confidence_score.is_none() {
                if let Some(obj) = value.as_object_mut() {
                    let score = edge.confidence.default_score();
                    obj.insert(
                        "confidence_score".to_string(),
                        serde_json::to_value(score).map_err(serde::ser::Error::custom)?,
                    );
                }
            }
            seq.serialize_element(&value)?;
        }
        seq.end()
    }
}

/// The graph serialized byte-for-byte as `to_value` + `to_string_pretty`
/// produced it: top-level keys and per-element keys in sorted order, because
/// `serde_json::Value` is backed by a `BTreeMap`. Plain struct serialization
/// would emit declaration order instead and silently reorder every key in a
/// committed `graph.json`.
struct SortedExport<'a>(&'a KnowledgeGraph);

impl serde::Serialize for SortedExport<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let kg = self.0;
        let len = 6 + usize::from(kg.built_at_commit.is_some());
        let mut m = s.serialize_map(Some(len))?;
        // Alphabetical, matching serde_json::Value's BTreeMap iteration order.
        if let Some(commit) = &kg.built_at_commit {
            m.serialize_entry("built_at_commit", commit)?;
        }
        m.serialize_entry("directed", &kg.directed)?;
        m.serialize_entry("graph", &serde_json::Map::new())?;
        m.serialize_entry("hyperedges", &SortedElements(&kg.hyperedges))?;
        m.serialize_entry("links", &ExportLinks(kg))?;
        m.serialize_entry("multigraph", &false)?;
        m.serialize_entry("nodes", &ExportNodes(kg))?;
        m.end()
    }
}

/// Write `graph.json` (pretty-printed node-link) atomically.
///
/// Streams into the target file, converting one node/edge to a `serde_json::Value`
/// at a time rather than building a `Value` tree over the whole graph and then a
/// whole-file `String`. The buffered path held the graph, a clone, the tree and
/// the printed text simultaneously — measured at 3.3x the resident graph (8.3x
/// the output file), which is ~6.9 GiB for an 836 MB `graph.json`. Output is
/// byte-identical; `golden::to_json_file_matches_pretty_printed_value` proves it.
pub fn to_json(kg: &KnowledgeGraph, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    synaptic_core::write_atomic_with(path, |w| {
        serde_json::to_writer_pretty(w, &SortedExport(kg)).map_err(io::Error::other)
    })
}

/// Render an interactive `graph.html` (vis-network from CDN; embeds the graph
/// JSON). Communities drive node color; node shape and the hover tooltip reflect
/// the node kind (table/column/…); edge color reflects the relation.
/// A simplified explorer — the full WebGL SPA is a future deliverable (§6.1).
/// Above this node count, `graph.html` renders a community-aggregated view (one
/// super-node per community) instead of every node, so the browser stays
/// responsive; the full node-level view stays available in `graph-3d.html`.
const HTML_AGGREGATE_THRESHOLD: usize = 5000;

/// Node ceiling for the whole-graph text exports (GraphML / Cypher / DOT / the
/// 3-D force layout). Unlike `graph.html` and `graph.svg`, these formats
/// aggregate nothing: they emit every node and every edge, so their size tracks
/// the graph exactly. A 379k-node corpus produced a 342 MB `graph.graphml`, a
/// 259 MB `graph-3d.html`, a 229 MB `graph.cypher` and a 163 MB `graph.dot` --
/// none of which any tool in the chain can open. Past this they are skipped and
/// the caller says so. `graph.json` is never skipped: it is the contract.
pub const BULK_EXPORT_MAX_NODES: usize = 150_000;

/// True when the whole-graph text exports are worth writing at this node count.
pub fn bulk_export_is_viable(node_count: usize) -> bool {
    node_count <= BULK_EXPORT_MAX_NODES
}

/// vis-network shape for a node kind (table → diamond, column → dot, view →
/// triangle, index/class → square, procedure → hexagon, trigger → star, code →
/// dot).
fn vis_shape(kind: &str) -> &'static str {
    match crate::common::kind_shape(kind) {
        "diamond" => "diamond",
        "triangle" => "triangle",
        "triangle_down" => "triangleDown",
        "square" => "square",
        "star" => "star",
        "hexagon" | "pentagon" => "hexagon",
        _ => "dot",
    }
}

/// vis-network hover title: kind + the SQL facts (dialect, type, PK/FK, RLS) + file.
fn html_node_title(n: &synaptic_core::Node, kind: &str) -> String {
    let mut t = format!("{} \u{b7} {kind}", n.label);
    let s = |k: &str| n.extra.get(k).and_then(|v| v.as_str());
    let b = |k: &str| n.extra.get(k).and_then(|v| v.as_bool()) == Some(true);
    if let Some(d) = s("dialect") {
        t.push_str(&format!(" \u{b7} {d}"));
    }
    if let Some(dt) = s("data_type") {
        t.push_str(&format!(" \u{b7} {dt}"));
    }
    if b("pk") {
        t.push_str(" \u{b7} PK");
    }
    if let Some(fk) = s("fk_target") {
        t.push_str(&format!(" \u{b7} FK->{fk}"));
    }
    if b("rls_enabled") {
        t.push_str(" \u{b7} RLS");
    }
    if b("security_invoker") {
        t.push_str(" \u{b7} security_invoker");
    }
    if !n.source_file.is_empty() {
        t.push_str(&format!(" ({})", n.source_file));
    }
    t
}

pub fn to_html_string(kg: &KnowledgeGraph) -> String {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    // Borrow the graph: `to_graph_data()` deep-cloned every node and edge
    // (~1.8 GiB on a 379k-node graph) purely to read them. The vis output is
    // bounded by aggregation, but the clone never was.

    // Degree per node (self-loops ignored), drives node size and the degree filter.
    let mut deg: HashMap<&str, usize> = HashMap::new();
    for e in kg.edges() {
        if e.source != e.target {
            *deg.entry(e.source.0.as_str()).or_default() += 1;
            *deg.entry(e.target.0.as_str()).or_default() += 1;
        }
    }

    let aggregated = kg.node_count() > HTML_AGGREGATE_THRESHOLD;
    let mut vis_nodes = Vec::new();
    let mut vis_edges = Vec::new();
    let mut relations: BTreeSet<String> = BTreeSet::new();
    let mut communities: BTreeSet<i64> = BTreeSet::new();
    let mut max_deg = 1usize;
    let mut has_columns = false;
    let notice;

    if aggregated {
        // Collapse each community to one super-node + inter-community edges.
        let mut members: BTreeMap<i64, usize> = BTreeMap::new();
        let mut node_comm: HashMap<&str, i64> = HashMap::new();
        for n in kg.nodes() {
            let c = n.community.map(|c| c as i64).unwrap_or(-1);
            *members.entry(c).or_default() += 1;
            node_comm.insert(n.id.0.as_str(), c);
        }
        for (c, count) in &members {
            max_deg = max_deg.max(*count);
            vis_nodes.push(serde_json::json!({
                "id": format!("community-{c}"),
                "label": if *c < 0 { "(unclustered)".into() } else { format!("Community {c}") },
                "group": c, "value": count, "deg": count,
                "title": format!("{count} nodes"),
            }));
        }
        let mut pair: BTreeMap<(i64, i64), usize> = BTreeMap::new();
        for e in kg.edges() {
            let (Some(&a), Some(&b)) = (
                node_comm.get(e.source.0.as_str()),
                node_comm.get(e.target.0.as_str()),
            ) else {
                continue;
            };
            if a == b {
                continue;
            }
            *pair
                .entry(if a <= b { (a, b) } else { (b, a) })
                .or_default() += 1;
        }
        for (i, ((a, b), n)) in pair.iter().enumerate() {
            vis_edges.push(serde_json::json!({
                "id": i, "from": format!("community-{a}"), "to": format!("community-{b}"),
                "relation": "between", "value": n, "title": format!("{n} edges"),
                "color": { "color": "#888", "opacity": 0.5 },
            }));
        }
        notice = format!(
            "Graph has {} nodes (over {}); showing {} community super-nodes. \
             Open graph-3d.html for the full node-level view.",
            kg.node_count(),
            HTML_AGGREGATE_THRESHOLD,
            members.len()
        );
    } else {
        for n in kg.nodes() {
            let d = deg.get(n.id.0.as_str()).copied().unwrap_or(0);
            max_deg = max_deg.max(d);
            let group = n.community.map(|c| c as i64).unwrap_or(-1);
            communities.insert(group);
            let kind = crate::common::visual_kind(n);
            has_columns |= kind == "column";
            vis_nodes.push(serde_json::json!({
                "id": n.id.0, "label": n.label, "group": group,
                "value": d + 1, "deg": d, "kind": kind,
                "shape": vis_shape(kind),
                "title": html_node_title(n, kind),
            }));
        }
        for (i, e) in kg.edges().enumerate() {
            relations.insert(e.relation.clone());
            // Color by relation so SQL structure and code->SQL bridges stand out.
            let color = crate::common::relation_color(&e.relation);
            vis_edges.push(serde_json::json!({
                "id": i, "from": e.source.0, "to": e.target.0,
                "relation": e.relation, "title": e.relation,
                "color": { "color": color, "opacity": 0.65 },
            }));
        }
        notice = String::new();
    }

    // JSON embedded in <script>: neutralize "</script>" (and any "</…") so a
    // node label/relation can't break out of the script element. serde_json does
    // not escape '/', so this is required (rewrite `</` -> `<\/`).
    let nodes_json = json_for_html(&vis_nodes);
    let edges_json = json_for_html(&vis_edges);
    let relations_vec: Vec<Value> = relations.into_iter().map(Value::String).collect();
    let relations_json = json_for_html(&relations_vec);
    let community_opts: String = communities
        .iter()
        .filter(|c| **c >= 0)
        .map(|c| format!("<option value=\"{c}\">Community {c}</option>"))
        .collect();
    let notice_html = if notice.is_empty() {
        String::new()
    } else {
        format!(
            "<div style=\"padding:6px 8px;background:#fff8e1;border-bottom:1px solid #f0e0a0;font-size:13px\">{notice}</div>"
        )
    };
    // The hide-columns toggle only appears when columns are present (it bounds the
    // dominant node class in a SQL graph without a re-extract).
    let column_toggle = if has_columns && !aggregated {
        "<label><input type=\"checkbox\" id=\"hidecols\"> hide SQL columns</label>"
    } else {
        ""
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Synaptic</title>
<script src="https://unpkg.com/vis-network/standalone/umd/vis-network.min.js"></script>
<style>
  html, body {{ margin: 0; height: 100%; font-family: system-ui, sans-serif; }}
  #graph {{ width: 100%; height: 90vh; border-bottom: 1px solid #ddd; }}
  #bar {{ padding: 8px; display: flex; gap: 12px; align-items: center; flex-wrap: wrap; }}
  #search {{ padding: 4px 8px; }}
  #relations label {{ margin-right: 8px; white-space: nowrap; }}
  details {{ display: inline-block; }}
</style>
</head>
<body>
<div id="bar">
  <strong>Synaptic</strong>
  <span>{node_count} nodes · {edge_count} edges</span>
  <input id="search" placeholder="search label…">
  <label>min degree <input id="mindeg" type="range" min="0" max="{max_deg}" value="0"><span id="mindegval">0</span></label>
  <select id="community"><option value="">all communities</option>{community_opts}</select>
  <details><summary>relations</summary><div id="relations"></div></details>
  {column_toggle}
</div>
{notice_html}
<div id="graph"></div>
<script>
  const allNodes = {nodes_json};
  const allEdges = {edges_json};
  const relations = {relations_json};
  const nodes = new vis.DataSet(allNodes);
  const edges = new vis.DataSet(allEdges);
  const network = new vis.Network(document.getElementById('graph'), {{ nodes, edges }}, {{
    nodes: {{ shape: 'dot', scaling: {{ min: 6, max: 36 }}, font: {{ size: 12 }} }},
    physics: {{ stabilization: true, barnesHut: {{ gravitationalConstant: -8000 }} }},
    interaction: {{ hover: true, tooltipDelay: 120 }},
  }});

  // Relation toggles.
  const relState = {{}};
  const relDiv = document.getElementById('relations');
  const edgesByRelation = new Map();
  allEdges.forEach(e => {{
    const relationEdges = edgesByRelation.get(e.relation);
    if (relationEdges) relationEdges.push(e);
    else edgesByRelation.set(e.relation, [e]);
  }});
  relations.forEach(r => {{
    relState[r] = true;
    const l = document.createElement('label');
    l.innerHTML = '<input type="checkbox" checked data-rel="' + r + '"> ' + r;
    relDiv.appendChild(l);
  }});

  let visibleIds = new Set(allNodes.map(n => n.id));
  const nodeHidden = new Map(allNodes.map(n => [n.id, false]));
  const edgeHidden = new Map(allEdges.map(e => [e.id, false]));
  const pendingRelations = new Set();
  let filterFrame = null;
  let fullFilterPending = false;

  function collectEdgeChanges(candidates, changes) {{
    for (const e of candidates) {{
      const hidden = relState[e.relation] === false || !visibleIds.has(e.from) || !visibleIds.has(e.to);
      if (edgeHidden.get(e.id) !== hidden) {{
        edgeHidden.set(e.id, hidden);
        changes.push({{ id: e.id, hidden }});
      }}
    }}
  }}

  function flushFilters() {{
    filterFrame = null;
    const edgeChanges = [];

    if (!fullFilterPending) {{
      pendingRelations.forEach(relation => {{
        collectEdgeChanges(edgesByRelation.get(relation) || [], edgeChanges);
      }});
      pendingRelations.clear();
      if (edgeChanges.length) edges.update(edgeChanges);
      return;
    }}

    const minDeg = +document.getElementById('mindeg').value;
    const comm = document.getElementById('community').value;
    const hc = document.getElementById('hidecols');
    const hideCols = hc && hc.checked;
    const nextVisible = new Set();
    const nodeChanges = [];

    for (const n of allNodes) {{
      const visible = (n.deg || 0) >= minDeg
        && (comm === '' || String(n.group) === comm)
        && !(hideCols && n.kind === 'column');
      if (visible) nextVisible.add(n.id);
      const hidden = !visible;
      if (nodeHidden.get(n.id) !== hidden) {{
        nodeHidden.set(n.id, hidden);
        nodeChanges.push({{ id: n.id, hidden }});
      }}
    }}

    visibleIds = nextVisible;
    collectEdgeChanges(allEdges, edgeChanges);
    fullFilterPending = false;
    pendingRelations.clear();
    if (nodeChanges.length) nodes.update(nodeChanges);
    if (edgeChanges.length) edges.update(edgeChanges);
  }}

  function scheduleFilterFlush() {{
    if (filterFrame === null) filterFrame = requestAnimationFrame(flushFilters);
  }}

  function applyFilters() {{
    fullFilterPending = true;
    scheduleFilterFlush();
  }}

  function applyRelationFilter(relation) {{
    pendingRelations.add(relation);
    scheduleFilterFlush();
  }}
  document.getElementById('mindeg').addEventListener('input', ev => {{
    document.getElementById('mindegval').textContent = ev.target.value;
    applyFilters();
  }});
  document.getElementById('community').addEventListener('change', applyFilters);
  const hcEl = document.getElementById('hidecols');
  if (hcEl) hcEl.addEventListener('change', applyFilters);
  relDiv.addEventListener('change', ev => {{
    if (ev.target.dataset && ev.target.dataset.rel) {{
      const relation = ev.target.dataset.rel;
      relState[relation] = ev.target.checked;
      applyRelationFilter(relation);
    }}
  }});
  document.getElementById('search').addEventListener('input', ev => {{
    const q = ev.target.value.toLowerCase();
    if (!q) {{ network.unselectAll(); return; }}
    const hits = allNodes.filter(n => (n.label || '').toLowerCase().includes(q)).map(n => n.id);
    network.selectNodes(hits);
    if (hits.length) network.focus(hits[0], {{ scale: 1.0, animation: true }});
  }});
</script>
</body>
</html>
"#,
        node_count = kg.node_count(),
        edge_count = kg.edge_count(),
        max_deg = max_deg,
        community_opts = community_opts,
        notice_html = notice_html,
        column_toggle = column_toggle,
        nodes_json = nodes_json,
        edges_json = edges_json,
        relations_json = relations_json,
    )
}

/// Write `graph.html`.
pub fn to_html(kg: &KnowledgeGraph, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, to_html_string(kg))?;
    Ok(())
}

/// Serialize to JSON safe for embedding inside an HTML `<script>` block.
pub(crate) fn json_for_html(value: &[Value]) -> String {
    serde_json::to_string(value)
        .expect("serde_json::Value re-serializes")
        .replace("</", "<\\/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
    use synaptic_graph::{apply_communities, cluster, ClusterOptions};

    fn sample_kg() -> KnowledgeGraph {
        let node = |id: &str, sf: &str| Node {
            id: NodeId(id.into()),
            label: id.to_uppercase(),
            file_type: FileType::Code,
            source_file: sf.into(),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
            ..Default::default()
        };
        let edge = |s: &str, t: &str, c: Confidence| Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: "calls".into(),
            confidence: c,
            source_file: "a.py".into(),
            source_location: Some("L1".into()),
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        };
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![node("a", "a.py"), node("b", "b.py"), node("c", "c.py")],
            links: vec![
                edge("a", "b", Confidence::Inferred),
                edge("b", "c", Confidence::Extracted),
            ],
            hyperedges: vec![],
            built_at_commit: Some("deadbeef".into()),
        };
        let mut kg = KnowledgeGraph::from_graph_data(gd);
        let comms = cluster(&kg, &ClusterOptions::default());
        apply_communities(&mut kg, &comms);
        kg
    }

    #[test]
    fn json_has_node_link_shape_with_norm_label_and_scores() {
        let kg = sample_kg();
        let v = to_json_value(&kg);
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("links")); // not "edges"
        assert!(obj.contains_key("nodes"));
        assert!(obj.contains_key("hyperedges"));
        assert_eq!(obj["built_at_commit"], serde_json::json!("deadbeef"));
        // norm_label added to every node; community present.
        for n in obj["nodes"].as_array().unwrap() {
            assert!(n.get("norm_label").is_some());
            assert!(n.get("community").is_some());
        }
        // confidence_score defaulted on every link.
        for l in obj["links"].as_array().unwrap() {
            assert!(l.get("confidence_score").and_then(Value::as_f64).is_some());
        }
    }

    #[test]
    fn html_is_kind_aware() {
        let gd: GraphData = serde_json::from_value(serde_json::json!({
            "nodes": [
                {"id":"app.f","label":"f()","file_type":"code","source_file":"a.py","community":0},
                {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table","community":0,"dialect":"sqlserver"},
                {"id":"sql:orders:col:total","label":"total","file_type":"code","source_file":"s.sql","kind":"column","community":0,"data_type":"int"}
            ],
            "links": [
                {"source":"app.f","target":"sql:orders","relation":"queries","confidence":"INFERRED","source_file":"a.py"},
                {"source":"sql:orders","target":"sql:orders:col:total","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}
            ]
        }))
        .unwrap();
        let html = to_html_string(&KnowledgeGraph::from_graph_data(gd));
        assert!(
            html.contains("\"shape\":\"diamond\""),
            "table -> diamond shape"
        );
        assert!(html.contains("\"kind\":\"column\""), "node carries kind");
        assert!(
            html.contains(crate::common::relation_color("queries")),
            "queries edge colored by relation"
        );
        assert!(
            html.contains("hide SQL columns"),
            "hide-columns toggle present"
        );
    }

    #[test]
    fn json_carries_repo_and_cross_repo_when_federated() {
        let v = to_json_value(&crate::tests_support::kg_federated());
        let nodes = v.get("nodes").unwrap().as_array().unwrap();
        assert!(nodes
            .iter()
            .any(|n| n.get("repo").and_then(|r| r.as_str()) == Some("app")));
        let links = v.get("links").unwrap().as_array().unwrap();
        assert!(links
            .iter()
            .any(|l| l.get("cross_repo").and_then(|c| c.as_bool()) == Some(true)));
    }

    #[test]
    fn json_round_trips_to_disk() {
        let kg = sample_kg();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("synaptic-out/graph.json");
        to_json(&kg, &p).unwrap();
        let back: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(back["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(back["links"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn html_embeds_graph_and_counts() {
        let kg = sample_kg();
        let html = to_html_string(&kg);
        assert!(html.contains("vis-network"));
        assert!(html.contains("3 nodes · 2 edges"));
        assert!(html.contains("\"from\""));
        assert!(html.contains("new vis.Network"));
    }

    #[test]
    fn html_escapes_script_breakout_in_labels() {
        let mut n = Node {
            id: NodeId("evil".into()),
            label: "</script><img src=x onerror=alert(1)>".into(),
            file_type: FileType::Code,
            source_file: "a.py".into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
            ..Default::default()
        };
        n.community = Some(0);
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![n],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        let html = to_html_string(&kg);
        // The raw closing tag must NOT appear; it must be neutralized to `<\/script>`.
        assert!(
            !html.contains("</script><img"),
            "script breakout not neutralized"
        );
        assert!(html.contains("<\\/script>"));
    }

    #[test]
    fn html_has_filter_controls() {
        let html = to_html_string(&sample_kg());
        assert!(html.contains("id=\"mindeg\""), "degree slider");
        assert!(html.contains("id=\"relations\""), "relation toggles");
        assert!(html.contains("id=\"community\""), "community filter");
        assert!(html.contains("applyFilters"), "filter logic present");
    }

    #[test]
    fn html_batches_and_deduplicates_filter_updates() {
        let html = to_html_string(&sample_kg());
        assert!(
            html.contains("const edgesByRelation = new Map()"),
            "relation changes use a prebuilt edge index"
        );
        assert!(
            html.contains("requestAnimationFrame(flushFilters)"),
            "bursty filter events are batched to one browser frame"
        );
        assert!(
            html.contains("if (nodeChanges.length) nodes.update(nodeChanges)"),
            "nodes receive only changed hidden states"
        );
        assert!(
            html.contains("if (edgeChanges.length) edges.update(edgeChanges)"),
            "edges receive only changed hidden states"
        );
        assert!(
            html.contains("applyRelationFilter(relation)"),
            "relation toggles update their indexed edge subset"
        );
        assert!(
            !html.contains("nodes.update(allNodes.map"),
            "filters must not submit every node on each event"
        );
        assert!(
            !html.contains("edges.update(allEdges.map"),
            "filters must not submit every edge on each event"
        );
    }

    #[test]
    fn html_aggregates_large_graphs() {
        // Over the threshold: one super-node per community, with a notice.
        let count = HTML_AGGREGATE_THRESHOLD + 10;
        let nodes: Vec<Node> = (0..count)
            .map(|i| Node {
                id: NodeId(format!("n{i}")),
                label: format!("N{i}"),
                file_type: FileType::Code,
                source_file: "a.py".into(),
                source_location: Some("L1".into()),
                community: Some((i % 4) as u32),
                repo: None,
                extra: Map::new(),
                ..Default::default()
            })
            .collect();
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let html = to_html_string(&KnowledgeGraph::from_graph_data(gd));
        assert!(html.contains("community super-nodes"), "aggregation notice");
        assert!(html.contains("community-0"), "community super-node present");
        // The 5010 raw node ids must NOT be embedded individually.
        assert!(
            !html.contains("\"n4999\""),
            "raw nodes should be aggregated away"
        );
    }
}
