//! `GRAPH_REPORT.md` generation: a human-readable summary of god nodes,
//! surprising connections, suggested questions, and import cycles.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use synaptic_core::{Confidence, NodeId};
use synaptic_graph::{cohesion_score, AnalysisResult, KnowledgeGraph};

/// Communities smaller than this are omitted from the per-community listing and
/// counted as a knowledge gap.
const THIN_COMMUNITY_SIZE: usize = 3;
/// Nodes with at most this many connections are flagged as isolated.
const ISOLATED_MAX_DEGREE: usize = 1;

/// Edge counts per confidence tier + the average score of scored INFERRED edges.
fn confidence_breakdown(kg: &KnowledgeGraph) -> (usize, usize, usize, Option<f64>) {
    let (mut ext, mut inf, mut amb) = (0usize, 0usize, 0usize);
    let (mut inf_sum, mut inf_scored) = (0.0f64, 0usize);
    for e in kg.edges() {
        match e.confidence {
            Confidence::Extracted => ext += 1,
            Confidence::Inferred => {
                inf += 1;
                if let Some(score) = e.confidence_score {
                    inf_sum += score as f64;
                    inf_scored += 1;
                }
            }
            Confidence::Ambiguous => amb += 1,
        }
    }
    let inf_avg = (inf_scored > 0).then(|| inf_sum / inf_scored as f64);
    (ext, inf, amb, inf_avg)
}

/// Incident-edge count per node (self-loops ignored).
fn node_degrees(kg: &KnowledgeGraph) -> HashMap<NodeId, usize> {
    let mut deg: HashMap<NodeId, usize> = kg.nodes().map(|n| (n.id.clone(), 0)).collect();
    for e in kg.edges() {
        if e.source == e.target {
            continue;
        }
        *deg.entry(e.source.clone()).or_default() += 1;
        *deg.entry(e.target.clone()).or_default() += 1;
    }
    deg
}

fn label_of(kg: &KnowledgeGraph, id: &NodeId) -> String {
    kg.node(id)
        .map(|n| n.label.clone())
        .unwrap_or_else(|| id.0.clone())
}

/// Render `GRAPH_REPORT.md` from a graph + its analysis. `community_labels` gives
/// semantic community names (empty → `Community {cid}` placeholders).
pub fn graph_report(
    kg: &KnowledgeGraph,
    analysis: &AnalysisResult,
    communities: &BTreeMap<u32, Vec<NodeId>>,
    community_labels: &BTreeMap<u32, String>,
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Graph Report\n");

    let (ext, inf, amb, inf_avg) = confidence_breakdown(kg);
    let total_edges = ext + inf + amb;

    let _ = writeln!(s, "## Overview\n");
    let _ = writeln!(s, "- **Nodes:** {}", kg.node_count());
    let _ = writeln!(s, "- **Edges:** {}", kg.edge_count());
    let _ = writeln!(s, "- **Communities:** {}", communities.len());
    if total_edges > 0 {
        let pct = |x: usize| (x as f64 * 100.0 / total_edges as f64).round() as u32;
        let mut line = format!(
            "- **Extraction:** {}% EXTRACTED · {}% INFERRED · {}% AMBIGUOUS",
            pct(ext),
            pct(inf),
            pct(amb)
        );
        if let Some(avg) = inf_avg {
            let _ = write!(line, " ({inf} INFERRED edges, avg confidence {avg:.2})");
        }
        let _ = writeln!(s, "{line}");
    }
    let _ = writeln!(
        s,
        "- **Built at commit:** {}",
        kg.built_at_commit.as_deref().unwrap_or("n/a")
    );
    let _ = writeln!(s);

    let _ = writeln!(s, "## God Nodes\n");
    if analysis.god_nodes.is_empty() {
        let _ = writeln!(s, "_None found._\n");
    } else {
        let _ = writeln!(s, "The most-connected core abstractions:\n");
        for (i, g) in analysis.god_nodes.iter().enumerate() {
            let _ = writeln!(s, "{}. `{}` — degree {}", i + 1, g.label, g.degree);
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "## Surprising Connections\n");
    if analysis.surprising.is_empty() {
        let _ = writeln!(s, "_None found._\n");
    } else {
        for c in &analysis.surprising {
            let _ = writeln!(
                s,
                "- `{}` → `{}` ({}, {}) — {}",
                c.source,
                c.target,
                c.relation,
                confidence_str(c.confidence),
                c.why
            );
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "## Suggested Questions\n");
    let real_questions: Vec<_> = analysis
        .questions
        .iter()
        .filter(|q| q.question.is_some())
        .collect();
    if real_questions.is_empty() {
        let why = analysis
            .questions
            .first()
            .map(|q| q.why.as_str())
            .unwrap_or("Not enough signal to generate questions.");
        let _ = writeln!(s, "_{why}_\n");
    } else {
        for (i, q) in real_questions.iter().enumerate() {
            let _ = writeln!(s, "{}. {}", i + 1, q.question.as_deref().unwrap_or(""));
            let _ = writeln!(s, "   - _{}_", q.why);
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "## Import Cycles\n");
    if analysis.import_cycles.is_empty() {
        let _ = writeln!(s, "_None detected._\n");
    } else {
        for c in &analysis.import_cycles {
            let mut chain = c.cycle.clone();
            if let Some(first) = chain.first().cloned() {
                chain.push(first); // close the loop visually
            }
            let _ = writeln!(s, "- {} (length {})", chain.join(" → "), c.length);
        }
        let _ = writeln!(s);
    }

    // Communities (with cohesion)
    let thin = communities
        .values()
        .filter(|m| m.len() < THIN_COMMUNITY_SIZE)
        .count();
    let _ = writeln!(s, "## Communities\n");
    if communities.is_empty() {
        let _ = writeln!(s, "_None._\n");
    } else {
        let _ = writeln!(
            s,
            "{} total{}.\n",
            communities.len(),
            if thin > 0 {
                format!(" ({thin} thin <{THIN_COMMUNITY_SIZE} nodes omitted)")
            } else {
                String::new()
            }
        );
        for (cid, members) in communities {
            if members.len() < THIN_COMMUNITY_SIZE {
                continue;
            }
            let coh = cohesion_score(kg, members);
            let sample: Vec<String> = members.iter().take(5).map(|id| label_of(kg, id)).collect();
            let name = community_labels
                .get(cid)
                .cloned()
                .unwrap_or_else(|| format!("Community {cid}"));
            let _ = writeln!(
                s,
                "- **{name}** (community {cid}) — {} nodes, cohesion {coh:.2}: {}",
                members.len(),
                sample.join(", ")
            );
        }
        let _ = writeln!(s);
    }

    // Ambiguous edges (review these)
    let ambiguous: Vec<_> = kg
        .edges()
        .filter(|e| e.confidence == Confidence::Ambiguous)
        .collect();
    let _ = writeln!(s, "## Ambiguous Edges\n");
    if ambiguous.is_empty() {
        let _ = writeln!(s, "_None._\n");
    } else {
        let _ = writeln!(s, "Low-confidence relationships worth a human look:\n");
        for e in ambiguous.iter().take(20) {
            let _ = writeln!(
                s,
                "- `{}` → `{}` ({})  [AMBIGUOUS]",
                label_of(kg, &e.source),
                label_of(kg, &e.target),
                e.relation
            );
        }
        if ambiguous.len() > 20 {
            let _ = writeln!(s, "- … and {} more", ambiguous.len() - 20);
        }
        let _ = writeln!(s);
    }

    // Knowledge gaps
    let degrees = node_degrees(kg);
    let isolated: Vec<String> = kg
        .nodes()
        .filter(|n| degrees.get(&n.id).copied().unwrap_or(0) <= ISOLATED_MAX_DEGREE)
        .map(|n| n.label.clone())
        .collect();
    let amb_pct = if total_edges > 0 {
        (amb as f64 * 100.0 / total_edges as f64).round() as u32
    } else {
        0
    };
    let _ = writeln!(s, "## Knowledge Gaps\n");
    if isolated.is_empty() && thin == 0 && amb_pct < 20 {
        let _ = writeln!(s, "_None._\n");
    } else {
        if !isolated.is_empty() {
            let shown: Vec<String> = isolated.iter().take(5).map(|l| format!("`{l}`")).collect();
            let more = if isolated.len() > 5 {
                format!(" (+{} more)", isolated.len() - 5)
            } else {
                String::new()
            };
            let _ = writeln!(
                s,
                "- **{} isolated node(s):** {}{more}",
                isolated.len(),
                shown.join(", ")
            );
            let _ = writeln!(
                s,
                "  These have ≤{ISOLATED_MAX_DEGREE} connection(s) — possible missing edges or undocumented components."
            );
        }
        if thin > 0 {
            let _ = writeln!(
                s,
                "- **{thin} thin communities (<{THIN_COMMUNITY_SIZE} nodes)** omitted from the Communities list."
            );
        }
        if amb_pct >= 20 {
            let _ = writeln!(
                s,
                "- **High ambiguity: {amb_pct}% of edges are AMBIGUOUS** — review the Ambiguous Edges section."
            );
        }
        let _ = writeln!(s);
    }

    s
}

fn confidence_str(c: synaptic_core::Confidence) -> &'static str {
    match c {
        synaptic_core::Confidence::Extracted => "EXTRACTED",
        synaptic_core::Confidence::Inferred => "INFERRED",
        synaptic_core::Confidence::Ambiguous => "AMBIGUOUS",
    }
}

/// Write `GRAPH_REPORT.md`.
pub fn write_report(
    kg: &KnowledgeGraph,
    analysis: &AnalysisResult,
    communities: &BTreeMap<u32, Vec<NodeId>>,
    community_labels: &BTreeMap<u32, String>,
    path: &Path,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        graph_report(kg, analysis, communities, community_labels),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node};
    use synaptic_graph::{analyze, apply_communities, cluster, ClusterOptions};

    fn kg() -> (KnowledgeGraph, BTreeMap<u32, Vec<NodeId>>) {
        let node = |id: &str, sf: &str| Node {
            id: NodeId(id.into()),
            label: id.to_uppercase(),
            file_type: FileType::Code,
            source_file: sf.into(),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
        };
        let edge = |s: &str, t: &str| Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: "references".into(),
            confidence: Confidence::Inferred,
            source_file: "".into(),
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
            nodes: vec![node("a", "src/a.py"), node("b", "src/b.py")],
            links: vec![edge("a", "b")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut g = KnowledgeGraph::from_graph_data(gd);
        let comms = cluster(&g, &ClusterOptions::default());
        apply_communities(&mut g, &comms);
        (g, comms)
    }

    #[test]
    fn report_has_all_sections() {
        let (g, comms) = kg();
        let analysis = analyze(&g, &comms, &BTreeMap::new());
        let md = graph_report(&g, &analysis, &comms, &BTreeMap::new());
        assert!(md.contains("# Graph Report"));
        assert!(md.contains("## Overview"));
        assert!(md.contains("## God Nodes"));
        assert!(md.contains("## Surprising Connections"));
        assert!(md.contains("## Suggested Questions"));
        assert!(md.contains("## Import Cycles"));
        assert!(md.contains("**Nodes:** 2"));
        assert!(md.contains("**Edges:** 1"));
    }

    #[test]
    fn report_has_quality_sections() {
        let node = |id: &str| Node {
            id: NodeId(id.into()),
            label: id.to_uppercase(),
            file_type: FileType::Code,
            source_file: format!("src/{id}.py"),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
        };
        let edge = |s: &str, t: &str, c: Confidence, score: Option<f32>| Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: "references".into(),
            confidence: c,
            source_file: String::new(),
            source_location: Some("L1".into()),
            confidence_score: score,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        };
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            // `d` has no edges, so it's isolated (a knowledge gap).
            nodes: vec![node("a"), node("b"), node("c"), node("d")],
            links: vec![
                edge("a", "b", Confidence::Extracted, None),
                edge("b", "c", Confidence::Inferred, Some(0.8)),
                edge("a", "c", Confidence::Ambiguous, None),
            ],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut g = KnowledgeGraph::from_graph_data(gd);
        let comms = cluster(&g, &ClusterOptions::default());
        apply_communities(&mut g, &comms);
        let analysis = analyze(&g, &comms, &BTreeMap::new());
        // Semantic community names replace the `Community N` placeholder.
        let labels: BTreeMap<u32, String> = comms
            .keys()
            .map(|c| (*c, format!("SEMANTIC-NAME-{c}")))
            .collect();
        let md = graph_report(&g, &analysis, &comms, &labels);
        assert!(
            md.contains("SEMANTIC-NAME"),
            "semantic community name should render:\n{md}"
        );

        assert!(md.contains("## Communities"), "missing Communities:\n{md}");
        assert!(
            md.contains("## Ambiguous Edges"),
            "missing Ambiguous Edges:\n{md}"
        );
        assert!(
            md.contains("## Knowledge Gaps"),
            "missing Knowledge Gaps:\n{md}"
        );
        // Extraction breakdown with all three confidence tiers + avg INFERRED.
        assert!(
            md.contains("EXTRACTED") && md.contains("INFERRED") && md.contains("AMBIGUOUS"),
            "missing extraction breakdown:\n{md}"
        );
        assert!(
            md.contains("0.80"),
            "missing avg INFERRED confidence:\n{md}"
        );
        // The ambiguous edge is surfaced, and the isolated node `d` flagged.
        assert!(
            md.contains("[AMBIGUOUS]"),
            "ambiguous edge not listed:\n{md}"
        );
        assert!(md.contains("isolated"), "isolated node not flagged:\n{md}");
    }

    #[test]
    fn report_writes_to_disk() {
        let (g, comms) = kg();
        let analysis = analyze(&g, &comms, &BTreeMap::new());
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("synaptic-out/GRAPH_REPORT.md");
        write_report(&g, &analysis, &comms, &BTreeMap::new(), &p).unwrap();
        assert!(p.exists());
        assert!(std::fs::read_to_string(&p)
            .unwrap()
            .contains("# Graph Report"));
    }
}
