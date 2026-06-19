//! Obsidian vault export: one Markdown note per node with YAML frontmatter and
//! neighbors as `[[wikilinks]]` grouped by relation. Dataview/canvas refinements
//! deferred.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use codegraph_core::NodeId;
use codegraph_graph::{cohesion_score, KnowledgeGraph};

use crate::common::file_type_str;

/// Graph-view color palette (RGB ints) cycled per community in
/// `.obsidian/graph.json`.
const COMMUNITY_RGB: &[u32] = &[
    0x4F_8D_C0, 0xC0_5F_4F, 0x5F_C0_7A, 0xB0_8D_4F, 0x8D_5F_C0, 0x4F_B8_C0, 0xC0_4F_9E, 0x7A_C0_4F,
];

/// Filesystem-safe note name derived from a label (Obsidian resolves
/// `[[label]]` against the note's basename).
fn safe_name(label: &str) -> String {
    let mut s: String = label
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    s = s.trim().trim_end_matches('.').trim().to_string();
    if s.is_empty() {
        s = "node".to_string();
    }
    if s.chars().count() > 200 {
        s = s.chars().take(200).collect();
    }
    s
}

/// Write an Obsidian vault under `dir`: one note per node, per-community overview
/// notes (with a Dataview live query), and `.obsidian/graph.json` coloring the
/// graph view by community. `community_labels` supplies semantic names (empty →
/// `Community N`). Returns the count of notes written.
pub fn to_obsidian(
    kg: &KnowledgeGraph,
    community_labels: &BTreeMap<u32, String>,
    dir: &Path,
) -> io::Result<usize> {
    fs::create_dir_all(dir)?;
    let label_of = |id: &NodeId| -> String {
        kg.node(id)
            .map(|n| n.label.clone())
            .unwrap_or_else(|| id.0.clone())
    };
    let community_name = |cid: u32| {
        community_labels
            .get(&cid)
            .cloned()
            .unwrap_or_else(|| format!("Community {cid}"))
    };

    // Assign a unique, filesystem-safe note name per node up front, so wikilinks
    // can target the actual filename. Labels can be filesystem-unsafe (sanitized
    // away) or collide (deduped with a ` (N)` suffix); linking by raw label would
    // dangle or point at the wrong note. Deterministic in node-iteration order.
    let mut name_of: HashMap<NodeId, String> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for n in kg.nodes() {
        let base = safe_name(&n.label);
        let mut name = base.clone();
        let mut i = 2;
        while !used.insert(name.clone()) {
            name = format!("{base} ({i})");
            i += 1;
        }
        name_of.insert(n.id.clone(), name);
    }

    // node id -> (relation -> neighbor ids)
    let mut neighbors: BTreeMap<NodeId, BTreeMap<String, Vec<NodeId>>> = BTreeMap::new();
    for e in kg.edges() {
        neighbors
            .entry(e.source.clone())
            .or_default()
            .entry(e.relation.clone())
            .or_default()
            .push(e.target.clone());
        neighbors
            .entry(e.target.clone())
            .or_default()
            .entry(format!("{} (in)", e.relation))
            .or_default()
            .push(e.source.clone());
    }

    let mut written = 0usize;
    for n in kg.nodes() {
        let name = &name_of[&n.id];

        let mut body = String::new();
        body.push_str("---\n");
        body.push_str(&format!("id: {}\n", yaml(&n.id.0)));
        body.push_str(&format!("file_type: {}\n", file_type_str(&n.file_type)));
        if let Some(c) = n.community {
            body.push_str(&format!("community: {c}\n"));
        }
        if !n.source_file.is_empty() {
            body.push_str(&format!("source_file: {}\n", yaml(&n.source_file)));
        }
        body.push_str("---\n\n");
        body.push_str(&format!("# {}\n\n", n.label));
        if let Some(rels) = neighbors.get(&n.id) {
            for (rel, targets) in rels {
                body.push_str(&format!("## {rel}\n\n"));
                for t in targets {
                    // Link to the target's actual note filename; show its label.
                    let tname = name_of
                        .get(t)
                        .cloned()
                        .unwrap_or_else(|| safe_name(&label_of(t)));
                    let tlabel = label_of(t);
                    if tname == tlabel {
                        body.push_str(&format!("- [[{tname}]]\n"));
                    } else {
                        body.push_str(&format!("- [[{tname}|{tlabel}]]\n"));
                    }
                }
                body.push('\n');
            }
        }
        fs::write(dir.join(format!("{name}.md")), body)?;
        written += 1;
    }

    // community overview notes + graph-view coloring
    let mut communities: BTreeMap<u32, Vec<NodeId>> = BTreeMap::new();
    let mut node_comm: HashMap<NodeId, u32> = HashMap::new();
    for n in kg.nodes() {
        if let Some(c) = n.community {
            communities.entry(c).or_default().push(n.id.clone());
            node_comm.insert(n.id.clone(), c);
        }
    }
    // Inter-community shared-edge counts.
    let mut shared: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for e in kg.edges() {
        if let (Some(&a), Some(&b)) = (node_comm.get(&e.source), node_comm.get(&e.target)) {
            if a != b {
                let key = if a <= b { (a, b) } else { (b, a) };
                *shared.entry(key).or_default() += 1;
            }
        }
    }

    for (cid, members) in &communities {
        let name = community_name(*cid);
        let coh = cohesion_score(kg, members);
        let desc = if coh >= 0.7 {
            "tightly connected"
        } else if coh >= 0.4 {
            "moderately connected"
        } else {
            "loosely connected"
        };
        let mut a = String::from("---\ntype: community\n");
        a.push_str(&format!(
            "cohesion: {coh:.2}\nmembers: {}\n---\n\n",
            members.len()
        ));
        a.push_str(&format!("# {name}\n\n"));
        a.push_str(&format!("**Cohesion:** {coh:.2} — {desc}\n"));
        a.push_str(&format!("**Members:** {} nodes\n\n", members.len()));

        a.push_str("## Members\n\n");
        for id in members {
            let nname = name_of.get(id).cloned().unwrap_or_else(|| label_of(id));
            a.push_str(&format!("- [[{nname}]]\n"));
        }

        a.push_str("\n## Live Query (requires Dataview plugin)\n\n");
        a.push_str("```dataview\n");
        a.push_str(&format!("LIST FROM \"\" WHERE community = {cid}\n"));
        a.push_str("```\n");

        let mut related: Vec<(u32, usize)> = shared
            .iter()
            .filter_map(|(&(x, y), &n)| {
                if x == *cid {
                    Some((y, n))
                } else if y == *cid {
                    Some((x, n))
                } else {
                    None
                }
            })
            .collect();
        related.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if !related.is_empty() {
            a.push_str("\n## Connections to other communities\n\n");
            for (other, n) in &related {
                a.push_str(&format!(
                    "- {n} edge(s) to [[_Community-{other}|{}]]\n",
                    community_name(*other)
                ));
            }
        }
        fs::write(dir.join(format!("_Community-{cid}.md")), a)?;
        written += 1;
    }

    // `.obsidian/graph.json`: color the graph view by community via a
    // frontmatter search on each note's `community:` field.
    let obs_dir = dir.join(".obsidian");
    fs::create_dir_all(&obs_dir)?;
    let mut groups = String::new();
    for (i, cid) in communities.keys().enumerate() {
        if i > 0 {
            groups.push_str(",\n");
        }
        let rgb = COMMUNITY_RGB[(*cid as usize) % COMMUNITY_RGB.len()];
        groups.push_str(&format!(
            "    {{ \"query\": \"[\\\"community\\\":{cid}]\", \"color\": {{ \"a\": 1, \"rgb\": {rgb} }} }}"
        ));
    }
    let graph_json = format!("{{\n  \"colorGroups\": [\n{groups}\n  ]\n}}\n");
    fs::write(obs_dir.join("graph.json"), graph_json)?;

    Ok(written)
}

/// Quote a scalar for YAML frontmatter if it could be misread.
fn yaml(s: &str) -> String {
    if s.is_empty() || s.contains([':', '#', '"', '\'', '\n']) || s.starts_with(' ') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::sample_kg;

    #[test]
    fn writes_one_note_per_node_with_frontmatter_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let n = to_obsidian(&sample_kg(), &BTreeMap::new(), dir.path()).unwrap();
        assert!(n >= 3, "3 node notes + community overview note(s)");
        let a = fs::read_to_string(dir.path().join("A.md")).unwrap();
        assert!(a.starts_with("---\n"));
        assert!(a.contains("id: a"));
        assert!(a.contains("community:"));
        assert!(a.contains("# A"));
        // a -> b via calls; the note links to [[B]].
        assert!(a.contains("[[B]]"), "expected wikilink to B: {a}");
    }

    #[test]
    fn colliding_labels_get_distinct_files() {
        let dir = tempfile::tempdir().unwrap();
        let kg = crate::tests_support::kg_with_label("x", "Dup");
        to_obsidian(&kg, &BTreeMap::new(), dir.path()).unwrap();
        assert!(dir.path().join("Dup.md").exists());
    }

    #[test]
    fn writes_community_notes_and_graph_view_config() {
        let dir = tempfile::tempdir().unwrap();
        let kg = sample_kg();
        let cids: std::collections::BTreeSet<u32> =
            kg.nodes().filter_map(|n| n.community).collect();
        let labels: BTreeMap<u32, String> =
            cids.iter().map(|c| (*c, format!("Domain-{c}"))).collect();
        to_obsidian(&kg, &labels, dir.path()).unwrap();

        // .obsidian/graph.json colors nodes by community.
        let gj = fs::read_to_string(dir.path().join(".obsidian/graph.json")).unwrap();
        assert!(gj.contains("colorGroups"), "graph.json: {gj}");
        assert!(gj.contains("community"), "color query by community: {gj}");

        // A community overview note with a Dataview live query + semantic name.
        let note = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("_Community-"))
            .expect("a community overview note");
        let content = fs::read_to_string(note.path()).unwrap();
        assert!(content.contains("Domain-"), "semantic name:\n{content}");
        assert!(
            content.contains("type: community"),
            "frontmatter:\n{content}"
        );
        assert!(
            content.contains("```dataview"),
            "dataview block:\n{content}"
        );
        assert!(content.contains("Cohesion:"), "cohesion:\n{content}");
    }

    #[test]
    fn wikilinks_resolve_to_deduped_filenames() {
        // Two nodes share the label "Dup": files "Dup.md" and "Dup (2).md".
        // The link between them must target the actual (deduped) basename, not a
        // bare ambiguous `[[Dup]]`.
        let dir = tempfile::tempdir().unwrap();
        let kg = crate::tests_support::kg_two_linked("n1", "Dup", "n2", "Dup");
        to_obsidian(&kg, &BTreeMap::new(), dir.path()).unwrap();
        assert!(dir.path().join("Dup.md").exists());
        assert!(dir.path().join("Dup (2).md").exists());
        let first = fs::read_to_string(dir.path().join("Dup.md")).unwrap();
        assert!(
            first.contains("[[Dup (2)"),
            "outgoing link resolves to the deduped file: {first}"
        );
    }
}
