//! Wiki export: an `index.md` plus per-community and per-god-node articles with
//! `[[wikilinks]]`. Community labels are `Community N` until the semantic layer
//! supplies names.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use synaptic_core::{Confidence, NodeId};
use synaptic_graph::{cohesion_score, KnowledgeGraph};

use crate::common::degrees;

const GOD_NODE_COUNT: usize = 10;
/// Members listed under "Key concepts" before falling back to the full list.
const KEY_CONCEPT_COUNT: usize = 10;

fn conf_idx(c: Confidence) -> usize {
    match c {
        Confidence::Extracted => 0,
        Confidence::Inferred => 1,
        Confidence::Ambiguous => 2,
    }
}

fn safe_name(label: &str) -> String {
    let s: String = label
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim().trim_end_matches('.').trim().to_string();
    if s.is_empty() {
        "node".to_string()
    } else {
        s
    }
}

/// Write a wiki under `dir`: `index.md`, `community-<id>.md`, and a page per god
/// node. `community_labels` supplies semantic community names (empty → `Community
/// N`). Returns the count of files written.
pub fn to_wiki(
    kg: &KnowledgeGraph,
    community_labels: &BTreeMap<u32, String>,
    dir: &Path,
) -> io::Result<usize> {
    fs::create_dir_all(dir)?;
    let label_of = |id: &NodeId| {
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
    let deg = degrees(kg);

    // community id -> member node ids (sorted by degree desc for stable display).
    let mut communities: BTreeMap<u32, Vec<NodeId>> = BTreeMap::new();
    let mut node_comm: std::collections::HashMap<NodeId, u32> = std::collections::HashMap::new();
    for n in kg.nodes() {
        if let Some(c) = n.community {
            communities.entry(c).or_default().push(n.id.clone());
            node_comm.insert(n.id.clone(), c);
        }
    }
    for members in communities.values_mut() {
        members.sort_by(|a, b| deg.get(b).cmp(&deg.get(a)).then(a.cmp(b)));
    }

    // Inter-community shared-edge counts (unordered community pair -> count) and
    // per-community internal-edge confidence tallies, in one edge pass.
    let mut shared: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    let mut internal_conf: BTreeMap<u32, [usize; 3]> = BTreeMap::new();
    for e in kg.edges() {
        let (Some(&a), Some(&b)) = (node_comm.get(&e.source), node_comm.get(&e.target)) else {
            continue;
        };
        if a == b {
            let slot = internal_conf.entry(a).or_default();
            slot[conf_idx(e.confidence)] += 1;
        } else {
            let key = if a <= b { (a, b) } else { (b, a) };
            *shared.entry(key).or_default() += 1;
        }
    }

    // God nodes: top-degree overall.
    let mut by_deg: Vec<(&NodeId, usize)> = deg.iter().map(|(k, v)| (k, *v)).collect();
    by_deg.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let god: Vec<&NodeId> = by_deg
        .iter()
        .take(GOD_NODE_COUNT)
        .map(|(id, _)| *id)
        .collect();

    // Assign a unique, filesystem-safe filename per god node up front. Labels
    // can sanitize to the same stem (e.g. `foo/bar` and `foo:bar` -> `foo_bar`);
    // without dedup one page silently overwrites another. Links to god pages use
    // this same name so they resolve.
    let mut god_file: BTreeMap<NodeId, String> = BTreeMap::new();
    let mut used_god: HashSet<String> = HashSet::new();
    for id in &god {
        let base = safe_name(&label_of(id));
        let mut name = base.clone();
        let mut i = 2;
        while !used_god.insert(name.clone()) {
            name = format!("{base} ({i})");
            i += 1;
        }
        god_file.insert((*id).clone(), name);
    }

    let mut written = 0usize;

    // index.md
    let mut idx = String::from("# Knowledge Wiki\n\n");
    idx.push_str(&format!(
        "{} nodes · {} edges · {} communities\n\n",
        kg.node_count(),
        kg.edge_count(),
        communities.len()
    ));
    idx.push_str("## Communities\n\n");
    for (cid, members) in &communities {
        idx.push_str(&format!(
            "- [[community-{cid}|{}]] — {} nodes\n",
            community_name(*cid),
            members.len()
        ));
    }
    idx.push_str("\n## Key nodes\n\n");
    for id in &god {
        // Link to the god page's actual (deduped) filename, displaying the label.
        let fname = &god_file[*id];
        let label = label_of(id);
        let conns = deg.get(*id).unwrap_or(&0);
        if *fname == label {
            idx.push_str(&format!("- [[{fname}]] — {conns} connections\n"));
        } else {
            idx.push_str(&format!("- [[{fname}|{label}]] — {conns} connections\n"));
        }
    }
    fs::write(dir.join("index.md"), idx)?;
    written += 1;

    // per-community articles
    for (cid, members) in &communities {
        let name = community_name(*cid);
        let coh = cohesion_score(kg, members);
        let mut a = format!("# {name}\n\n");
        a.push_str(&format!(
            "Community {cid} · {} member nodes · cohesion {coh:.2}\n\n",
            members.len()
        ));

        // Key concepts: the highest-degree members (the list is degree-sorted).
        a.push_str("## Key concepts\n\n");
        for id in members.iter().take(KEY_CONCEPT_COUNT) {
            a.push_str(&format!(
                "- [[{}]] — {} connections\n",
                label_of(id),
                deg.get(id).copied().unwrap_or(0)
            ));
        }

        // Source files spanned by this community.
        let mut files: Vec<String> = members
            .iter()
            .filter_map(|id| kg.node(id))
            .map(|n| n.source_file.clone())
            .filter(|f| !f.is_empty())
            .collect();
        files.sort();
        files.dedup();
        if !files.is_empty() {
            a.push_str("\n## Source files\n\n");
            for f in files.iter().take(20) {
                a.push_str(&format!("- `{f}`\n"));
            }
        }

        // Related communities, by shared-edge count.
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
            a.push_str("\n## Related communities\n\n");
            for (other, n) in &related {
                a.push_str(&format!(
                    "- [[community-{other}|{}]] — {n} shared edge(s)\n",
                    community_name(*other)
                ));
            }
        }

        // Audit trail: confidence mix of this community's internal edges.
        let tally = internal_conf.get(cid).copied().unwrap_or([0, 0, 0]);
        let total: usize = tally.iter().sum();
        if total > 0 {
            let pct = |x: usize| (x as f64 * 100.0 / total as f64).round() as u32;
            a.push_str(&format!(
                "\n## Audit trail\n\n- {}% EXTRACTED · {}% INFERRED · {}% AMBIGUOUS ({total} internal edges)\n",
                pct(tally[0]),
                pct(tally[1]),
                pct(tally[2])
            ));
        }

        a.push_str("\n## All members\n\n");
        for id in members {
            a.push_str(&format!("- [[{}]]\n", label_of(id)));
        }
        a.push_str("\n---\n\n*Part of the Synaptic wiki. See [[index]] to navigate.*\n");
        fs::write(dir.join(format!("community-{cid}.md")), a)?;
        written += 1;
    }

    // god-node articles (neighbors grouped by relation)
    for id in &god {
        let mut by_rel: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for e in kg.edges() {
            if &e.source == *id {
                by_rel
                    .entry(e.relation.clone())
                    .or_default()
                    .push(label_of(&e.target));
            } else if &e.target == *id {
                by_rel
                    .entry(format!("{} (in)", e.relation))
                    .or_default()
                    .push(label_of(&e.source));
            }
        }
        let mut a = format!("# {}\n\n", label_of(id));
        if let Some(c) = kg.node(id).and_then(|n| n.community) {
            a.push_str(&format!(
                "**Community:** [[community-{c}|{}]]\n\n",
                community_name(c)
            ));
        }
        for (rel, targets) in &by_rel {
            a.push_str(&format!("## {rel}\n\n"));
            for t in targets {
                a.push_str(&format!("- [[{t}]]\n"));
            }
            a.push('\n');
        }
        a.push_str("---\n\n*Part of the Synaptic wiki. See [[index]] to navigate.*\n");
        fs::write(dir.join(format!("{}.md", god_file[*id])), a)?;
        written += 1;
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::sample_kg;

    #[test]
    fn writes_index_community_and_god_pages() {
        let dir = tempfile::tempdir().unwrap();
        let n = to_wiki(&sample_kg(), &BTreeMap::new(), dir.path()).unwrap();
        assert!(n >= 2);
        let idx = fs::read_to_string(dir.path().join("index.md")).unwrap();
        assert!(idx.contains("# Knowledge Wiki"));
        assert!(idx.contains("## Communities"));
        assert!(idx.contains("## Key nodes"));
        // At least one community page exists.
        let community_pages = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("community-"))
            .count();
        assert!(community_pages >= 1);
    }

    #[test]
    fn community_article_is_enriched_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let kg = sample_kg();
        let cids: std::collections::BTreeSet<u32> =
            kg.nodes().filter_map(|n| n.community).collect();
        let labels: BTreeMap<u32, String> =
            cids.iter().map(|c| (*c, format!("Domain-{c}"))).collect();
        to_wiki(&kg, &labels, dir.path()).unwrap();
        let page = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("community-"))
            .expect("a community page");
        let content = fs::read_to_string(page.path()).unwrap();
        assert!(content.contains("Domain-"), "semantic name:\n{content}");
        assert!(content.contains("cohesion"), "cohesion:\n{content}");
        assert!(
            content.contains("## Key concepts"),
            "key concepts:\n{content}"
        );
        assert!(
            content.contains("## Audit trail"),
            "audit trail:\n{content}"
        );
        assert!(
            content.contains("EXTRACTED") && content.contains("INFERRED"),
            "audit confidences:\n{content}"
        );
    }

    #[test]
    fn god_node_filenames_are_deduped() {
        // Two labels that sanitize to the same stem "foo_bar" must not overwrite
        // each other's page.
        let dir = tempfile::tempdir().unwrap();
        let kg = crate::tests_support::kg_two_linked("n1", "foo/bar", "n2", "foo:bar");
        let n = to_wiki(&kg, &BTreeMap::new(), dir.path()).unwrap();
        assert!(dir.path().join("foo_bar.md").exists(), "first god page");
        assert!(
            dir.path().join("foo_bar (2).md").exists(),
            "second god page deduped, not overwritten"
        );
        // index + two distinct god pages (no communities in this fixture).
        assert_eq!(n, 3);
    }
}
