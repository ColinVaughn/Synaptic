//! GraphML export (Gephi / yEd / any GraphML tool). Community + confidence are
//! preserved as attributes; internal `_`-prefixed extras are dropped.

use std::fs;
use std::io;
use std::path::Path;

use codegraph_graph::KnowledgeGraph;

use crate::common::{confidence_str, file_type_str, xml_escape};

/// Render the graph as a GraphML document string.
pub fn to_graphml_string(kg: &KnowledgeGraph) -> String {
    let gd = kg.to_graph_data();
    let edgedefault = if gd.directed {
        "directed"
    } else {
        "undirected"
    };
    // Federation attrs (`repo`, `cross_repo`) are declared + emitted ONLY for
    // federated graphs, so single-repo GraphML is unchanged.
    let federated = gd.nodes.iter().any(|n| n.repo.is_some());
    // Enrichment attrs (kind/visibility/loc) are declared + emitted only when at
    // least one node carries them, so a graph built before enrichment is unchanged.
    let enriched = gd
        .nodes
        .iter()
        .any(|n| n.kind().is_some() || n.visibility().is_some() || n.span().is_some());
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">\n");
    let mut keys = vec![
        ("label", "node", "string"),
        ("file_type", "node", "string"),
        ("source_file", "node", "string"),
        ("community", "node", "long"),
        ("relation", "edge", "string"),
        ("confidence", "edge", "string"),
    ];
    if federated {
        keys.push(("repo", "node", "string"));
        keys.push(("cross_repo", "edge", "boolean"));
    }
    if enriched {
        keys.push(("kind", "node", "string"));
        keys.push(("visibility", "node", "string"));
        keys.push(("loc", "node", "long"));
    }
    for (id, target, ty) in keys {
        s.push_str(&format!(
            "  <key id=\"{id}\" for=\"{target}\" attr.name=\"{id}\" attr.type=\"{ty}\"/>\n"
        ));
    }
    s.push_str(&format!("  <graph edgedefault=\"{edgedefault}\">\n"));
    for n in &gd.nodes {
        s.push_str(&format!("    <node id=\"{}\">\n", xml_escape(&n.id.0)));
        s.push_str(&data("label", &xml_escape(&n.label)));
        s.push_str(&data(
            "file_type",
            &xml_escape(&file_type_str(&n.file_type)),
        ));
        s.push_str(&data("source_file", &xml_escape(&n.source_file)));
        s.push_str(&data(
            "community",
            &n.community.map(|c| c as i64).unwrap_or(-1).to_string(),
        ));
        if federated {
            if let Some(r) = &n.repo {
                s.push_str(&data("repo", &xml_escape(r)));
            }
        }
        if enriched {
            if let Some(k) = n.kind() {
                s.push_str(&data("kind", k.as_str()));
            }
            if let Some(v) = n.visibility() {
                s.push_str(&data("visibility", v.as_str()));
            }
            if let Some(loc) = n.loc() {
                s.push_str(&data("loc", &loc.to_string()));
            }
        }
        s.push_str("    </node>\n");
    }
    for e in &gd.links {
        s.push_str(&format!(
            "    <edge source=\"{}\" target=\"{}\">\n",
            xml_escape(&e.source.0),
            xml_escape(&e.target.0)
        ));
        s.push_str(&data("relation", &xml_escape(&e.relation)));
        s.push_str(&data("confidence", &confidence_str(&e.confidence)));
        if federated && e.cross_repo {
            s.push_str(&data("cross_repo", "true"));
        }
        s.push_str("    </edge>\n");
    }
    s.push_str("  </graph>\n</graphml>\n");
    s
}

fn data(key: &str, value: &str) -> String {
    format!("      <data key=\"{key}\">{value}</data>\n")
}

/// Write `graph.graphml`.
pub fn to_graphml(kg: &KnowledgeGraph, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, to_graphml_string(kg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::sample_kg;

    #[test]
    fn graphml_has_keys_nodes_edges_and_escapes() {
        let kg = sample_kg();
        let xml = to_graphml_string(&kg);
        assert!(xml.contains("<graphml"));
        assert!(xml.contains(
            "<key id=\"community\" for=\"node\" attr.name=\"community\" attr.type=\"long\"/>"
        ));
        assert!(xml.contains("<node id=\"a\">"));
        assert!(xml.contains("<data key=\"relation\">calls</data>"));
        assert!(xml.contains("<data key=\"confidence\">"));
    }

    #[test]
    fn graphml_emits_repo_and_cross_repo() {
        use crate::tests_support::kg_federated;
        let xml = to_graphml_string(&kg_federated());
        assert!(
            xml.contains("<key id=\"repo\" for=\"node\""),
            "repo key declared"
        );
        assert!(
            xml.contains("<key id=\"cross_repo\" for=\"edge\""),
            "cross_repo key declared"
        );
        assert!(
            xml.contains("<data key=\"repo\">app</data>"),
            "node repo value"
        );
        assert!(
            xml.contains("<data key=\"cross_repo\">true</data>"),
            "cross-repo edge value"
        );
    }

    #[test]
    fn graphml_escapes_markup_in_labels() {
        let kg = crate::tests_support::kg_with_label("evil", "<x>&\"'");
        let xml = to_graphml_string(&kg);
        assert!(!xml.contains("<x>&\"'"));
        assert!(xml.contains("&lt;x&gt;&amp;&quot;&apos;"));
    }
}
