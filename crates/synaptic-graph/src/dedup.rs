//! Near-duplicate entity merging.
//!
//! Pipeline: exact normalization → entropy gate → MinHash/LSH blocking →
//! Jaro-Winkler verification → same-community boost → union-find merge, then
//! rewire edges onto survivors.
//!
//! **Code symbols are never label-merged** (`file_type == Code`): a code node's
//! identity is its fully-qualified id, already collapsed by id; two same-named
//! symbols in different files are distinct (reference #1205). So this only acts
//! on non-code (document/paper/concept) nodes — i.e. it's a no-op until the
//! semantic layer (B5) adds such nodes.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use strsim::{damerau_levenshtein, jaro_winkler};
use synaptic_core::{Edge, FileType, Node, NodeId};

use crate::minhash::{MinHash, MinHashLsh};

const ENTROPY_THRESHOLD: f64 = 2.5;
const LSH_THRESHOLD: f64 = 0.7;
const MERGE_THRESHOLD: f64 = 92.0; // jaro_winkler * 100
const COMMUNITY_BOOST: f64 = 5.0;
const NUM_PERM: usize = 128;

/// Trailing version/variant suffix (chip SKUs, codename revisions). Used to
/// *block* merges of short sibling variants.
static VARIANT_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.*[a-z])([0-9]+[a-z]*|[a-z]{2,})$").expect("valid variant-suffix regex")
});
/// Chunk suffix `_c<digits>` marking a split semantic chunk (loser-preferred).
static CHUNK_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_c\d+$").expect("valid chunk-suffix regex"));

/// Lowercase + collapse non-alphanumeric runs to single spaces (Unicode-aware),
/// trimmed. (NFKC omitted — immaterial for fuzzy concept text.)
fn norm(label: &str) -> String {
    let mut out = String::new();
    let mut started = false;
    let mut pending_space = false;
    for ch in label.chars() {
        if ch.is_alphanumeric() {
            if pending_space && started {
                out.push(' ');
            }
            out.extend(ch.to_lowercase());
            started = true;
            pending_space = false;
        } else {
            pending_space = true;
        }
    }
    out
}

/// Shannon entropy (bits/char) of the normalized label.
fn entropy(label: &str) -> f64 {
    let s = norm(label);
    if s.is_empty() {
        return 0.0;
    }
    let mut freq: HashMap<char, usize> = HashMap::new();
    for ch in s.chars() {
        *freq.entry(ch).or_default() += 1;
    }
    let n = s.chars().count() as f64;
    -freq
        .values()
        .map(|&c| {
            let p = c as f64 / n;
            p * p.log2()
        })
        .sum::<f64>()
}

fn make_minhash(norm_label: &str) -> MinHash {
    let mut m = MinHash::new(NUM_PERM);
    let chars: Vec<char> = norm_label.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() < 3 {
        let s: String = chars.iter().collect();
        m.update(s.as_bytes());
    } else {
        for w in chars.windows(3) {
            let s: String = w.iter().collect();
            m.update(s.as_bytes());
        }
    }
    m
}

fn clen(s: &str) -> usize {
    s.chars().count()
}

/// Sibling model/SKU variants (same stem, different short suffix) — not dupes.
fn is_variant_pair(a: &str, b: &str) -> bool {
    if a == b || clen(a).max(clen(b)) >= 12 {
        return false;
    }
    match (VARIANT_SUFFIX.captures(a), VARIANT_SUFFIX.captures(b)) {
        (Some(ca), Some(cb)) => {
            ca.get(1).map(|m| m.as_str()) == cb.get(1).map(|m| m.as_str())
                && ca.get(2).map(|m| m.as_str()) != cb.get(2).map(|m| m.as_str())
        }
        _ => false,
    }
}

/// Block fuzzy merge of short labels unless it's a same-length single-char
/// substitution (a true typo).
fn short_label_blocked(a: &str, b: &str, jw_score: f64) -> bool {
    if clen(a).max(clen(b)) >= 12 {
        return false;
    }
    if jw_score >= 97.0 && clen(a) == clen(b) && damerau_levenshtein(a, b) <= 1 {
        return false;
    }
    true
}

/// AST-extracted code symbol — excluded from all label-based merging.
fn is_code(n: &Node) -> bool {
    n.file_type == FileType::Code
}

/// Canonical survivor: prefer no chunk suffix, then the shorter id (first on tie).
fn pick_winner(ids: &[NodeId]) -> NodeId {
    ids.iter()
        .min_by_key(|id| (CHUNK_SUFFIX.is_match(&id.0) as u8, id.0.len()))
        .cloned()
        .expect("non-empty group")
}

#[derive(Default)]
struct UnionFind {
    parent: HashMap<NodeId, NodeId>,
}

impl UnionFind {
    fn find(&mut self, x: &NodeId) -> NodeId {
        self.parent.entry(x.clone()).or_insert_with(|| x.clone());
        let mut cur = x.clone();
        while self.parent[&cur] != cur {
            let gp = self.parent[&self.parent[&cur]].clone();
            self.parent.insert(cur.clone(), gp.clone());
            cur = gp;
        }
        cur
    }

    fn union(&mut self, x: &NodeId, y: &NodeId) {
        let rx = self.find(x);
        let ry = self.find(y);
        if rx != ry {
            self.parent.insert(ry, rx);
        }
    }

    fn components(&mut self) -> HashMap<NodeId, Vec<NodeId>> {
        let keys: Vec<NodeId> = self.parent.keys().cloned().collect();
        let mut groups: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for k in keys {
            let root = self.find(&k);
            groups.entry(root).or_default().push(k);
        }
        groups
    }
}

/// Merge near-duplicate non-code entities and rewire edges onto the survivors.
/// `communities` (node id → community) drives the same-community score boost;
/// pass an empty map to disable it. Returns the deduped `(nodes, edges)`.
pub fn deduplicate_entities(
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    communities: &HashMap<NodeId, u32>,
) -> (Vec<Node>, Vec<Edge>) {
    // Cross-repo dedup is unsafe (labels collide across repos by coincidence).
    // We have no Result to return, so warn on stderr
    // and skip merging rather than silently no-op'ing (which masks the
    // misconfiguration). Federation handles per-repo dedup separately.
    let repos: HashSet<&str> = nodes.iter().filter_map(|n| n.repo.as_deref()).collect();
    if repos.len() > 1 {
        eprintln!(
            "[synaptic] dedup: nodes span {} repos; cross-repo dedup is disabled — \
             run per-repo before merging. Skipping dedup.",
            repos.len()
        );
        return (nodes, edges);
    }
    if nodes.len() <= 1 {
        return (nodes, edges);
    }

    // Pre-dedup: first occurrence of each id wins.
    let mut unique: Vec<Node> = Vec::with_capacity(nodes.len());
    let mut seen_ids: HashSet<NodeId> = HashSet::new();
    for n in nodes {
        if seen_ids.insert(n.id.clone()) {
            unique.push(n);
        }
    }
    if unique.len() <= 1 {
        return (unique, edges);
    }

    let mut uf = UnionFind::default();

    // pass 1: exact normalization (within the same source_file only)
    let mut norm_to: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, n) in unique.iter().enumerate() {
        if is_code(n) {
            continue;
        }
        let key = norm(&n.label);
        if !key.is_empty() {
            norm_to.entry(key).or_default().push(i);
        }
    }
    for group in norm_to.values() {
        if group.len() <= 1 {
            continue;
        }
        let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
        for &i in group {
            by_file
                .entry(unique[i].source_file.as_str())
                .or_default()
                .push(i);
        }
        for (sf, file_group) in by_file {
            if sf.is_empty() || file_group.len() <= 1 {
                continue; // no source_file, can't prove same symbol
            }
            let ids: Vec<NodeId> = file_group.iter().map(|&i| unique[i].id.clone()).collect();
            let winner = pick_winner(&ids);
            for id in &ids {
                uf.union(&winner, id);
            }
        }
    }

    // pass 2: MinHash/LSH + Jaro-Winkler (high-entropy non-code, unique norm)
    let mut candidates: Vec<usize> = Vec::new();
    let mut seen_norms: HashSet<String> = HashSet::new();
    for (i, n) in unique.iter().enumerate() {
        if is_code(n) {
            continue;
        }
        let key = norm(&n.label);
        if key.is_empty() || !seen_norms.insert(key) {
            continue;
        }
        if entropy(&n.label) >= ENTROPY_THRESHOLD {
            candidates.push(i);
        }
    }

    if candidates.len() >= 2 {
        let mut lsh = MinHashLsh::new(LSH_THRESHOLD, NUM_PERM);
        let mut mh: HashMap<NodeId, MinHash> = HashMap::new();
        let mut norm_of: HashMap<NodeId, String> = HashMap::new();
        let mut node_of: HashMap<NodeId, usize> = HashMap::new();
        for &i in &candidates {
            let id = unique[i].id.clone();
            let nl = norm(&unique[i].label);
            let m = make_minhash(&nl);
            lsh.insert(&id.0, &m);
            mh.insert(id.clone(), m);
            norm_of.insert(id.clone(), nl);
            node_of.insert(id.clone(), i);
        }

        for &i in &candidates {
            let id = unique[i].id.clone();
            let norm_label = norm_of[&id].clone();
            let neighbors = lsh.query(&mh[&id]);
            for nbr_key in neighbors {
                let nbr = NodeId(nbr_key);
                if nbr == id || uf.find(&id) == uf.find(&nbr) {
                    continue;
                }
                let Some(nbr_norm) = norm_of.get(&nbr).cloned() else {
                    continue;
                };
                let mut score = jaro_winkler(&norm_label, &nbr_norm) * 100.0;
                if is_variant_pair(&norm_label, &nbr_norm)
                    || short_label_blocked(&norm_label, &nbr_norm, score)
                {
                    continue;
                }
                // Prefix-extension pairs (parseConfig / parseConfigFile) are
                // almost never duplicates, block regardless of score.
                let (lo, hi) = if clen(&norm_label) <= clen(&nbr_norm) {
                    (&norm_label, &nbr_norm)
                } else {
                    (&nbr_norm, &norm_label)
                };
                if hi.starts_with(lo.as_str()) && hi != lo {
                    continue;
                }
                if let (Some(&c1), Some(&c2)) = (communities.get(&id), communities.get(&nbr)) {
                    if c1 == c2 && clen(&norm_label).min(clen(&nbr_norm)) >= 12 {
                        score += COMMUNITY_BOOST;
                    }
                }
                if score >= MERGE_THRESHOLD {
                    // Identical labels in different files means same-named distinct
                    // symbols; require same source_file (mirrors pass 1).
                    if norm_label == nbr_norm {
                        let sf_a = unique[i].source_file.as_str();
                        let sf_b = node_of
                            .get(&nbr)
                            .map(|&j| unique[j].source_file.as_str())
                            .unwrap_or("");
                        if sf_a != sf_b {
                            continue;
                        }
                    }
                    let winner = pick_winner(&[id.clone(), nbr.clone()]);
                    uf.union(&winner, &id);
                    uf.union(&winner, &nbr);
                }
            }
        }
    }

    apply_components(unique, edges, &mut uf)
}

/// Build a remap from union-find components (winner per component via
/// `pick_winner`, deterministic on ties via `unique` order), drop merged-away
/// nodes, and rewire edges onto survivors (dropping self-edges).
fn apply_components(
    unique: Vec<Node>,
    edges: Vec<Edge>,
    uf: &mut UnionFind,
) -> (Vec<Node>, Vec<Edge>) {
    let mut remap: HashMap<NodeId, NodeId> = HashMap::new();
    for (_root, members) in uf.components() {
        if members.len() <= 1 {
            continue;
        }
        let ordered: Vec<NodeId> = unique
            .iter()
            .filter(|n| members.contains(&n.id))
            .map(|n| n.id.clone())
            .collect();
        if ordered.is_empty() {
            continue;
        }
        let winner = pick_winner(&ordered);
        for m in members {
            if m != winner {
                remap.insert(m, winner.clone());
            }
        }
    }

    if remap.is_empty() {
        return (unique, edges);
    }

    let deduped_nodes: Vec<Node> = unique
        .into_iter()
        .filter(|n| !remap.contains_key(&n.id))
        .collect();
    let mut deduped_edges: Vec<Edge> = Vec::with_capacity(edges.len());
    for mut e in edges {
        if let Some(w) = remap.get(&e.source) {
            e.source = w.clone();
        }
        if let Some(w) = remap.get(&e.target) {
            e.target = w.clone();
        }
        if e.source != e.target {
            deduped_edges.push(e);
        }
    }
    (deduped_nodes, deduped_edges)
}

/// Lower bound of the "ambiguous" fuzzy band (upper bound is [`MERGE_THRESHOLD`]).
const TIEBREAK_LOW: f64 = 75.0;

/// Concept pairs whose fuzzy score is *ambiguous* — in `[75, MERGE_THRESHOLD)` —
/// too similar to ignore, too different to auto-merge. These are the candidates
/// an LLM tiebreaker resolves. Call on the
/// already-deduped node set; returns deterministic `(a, b)` id pairs (a ≤ b).
///
/// Uses an **all-pairs** comparison over the entropy-gated candidate set — NOT
/// LSH blocking. LSH only buckets pairs with high
/// character-trigram overlap, but an ambiguous pair can sit in the `[75, 92)`
/// Jaro-Winkler band while having low trigram Jaccard (e.g. "data pipeline
/// stage" vs "data processing stage"); LSH would never surface those to the
/// tiebreaker. O(n²) is acceptable because this runs only on concept nodes (a
/// small set), and the structural pass-2 merge already used LSH for the bulk.
pub fn ambiguous_concept_pairs(
    nodes: &[Node],
    communities: &HashMap<NodeId, u32>,
) -> Vec<(NodeId, NodeId)> {
    let mut cand: Vec<&Node> = Vec::new();
    let mut norms: Vec<String> = Vec::new();
    let mut seen_norms: HashSet<String> = HashSet::new();
    for n in nodes {
        if is_code(n) {
            continue;
        }
        let key = norm(&n.label);
        if key.is_empty() || !seen_norms.insert(key.clone()) {
            continue;
        }
        if entropy(&n.label) >= ENTROPY_THRESHOLD {
            cand.push(n);
            norms.push(key);
        }
    }
    if cand.len() < 2 {
        return vec![];
    }

    let mut out: Vec<(NodeId, NodeId)> = Vec::new();
    for i in 0..cand.len() {
        for j in (i + 1)..cand.len() {
            let (na, nb) = (&norms[i], &norms[j]);
            let mut score = jaro_winkler(na, nb) * 100.0;
            if is_variant_pair(na, nb) || short_label_blocked(na, nb, score) {
                continue;
            }
            let (lo, hi) = if clen(na) <= clen(nb) {
                (na, nb)
            } else {
                (nb, na)
            };
            if hi.starts_with(lo.as_str()) && hi != lo {
                continue;
            }
            let (id_a, id_b) = (&cand[i].id, &cand[j].id);
            if let (Some(&c1), Some(&c2)) = (communities.get(id_a), communities.get(id_b)) {
                if c1 == c2 && clen(na).min(clen(nb)) >= 12 {
                    score += COMMUNITY_BOOST;
                }
            }
            if (TIEBREAK_LOW..MERGE_THRESHOLD).contains(&score) {
                let pair = if id_a <= id_b {
                    (id_a.clone(), id_b.clone())
                } else {
                    (id_b.clone(), id_a.clone())
                };
                out.push(pair);
            }
        }
    }
    out.sort();
    out
}

/// Offline counterpart to the LLM dedup tiebreaker: resolve ambiguous concept
/// `pairs` (from [`ambiguous_concept_pairs`]) with a *conservative,
/// deterministic* rule so the default pipeline closes the loop without a model.
///
/// A pair is confirmed only when the two labels are the **same multiset of
/// normalized word-tokens** — i.e. a word reordering or duplication (e.g. "Order
/// Management" vs "Management Order"), which is near-certainly the same concept.
/// Genuinely ambiguous pairs (different words, abbreviations, partial overlaps —
/// the bulk of the `[75, 92)` band) are deliberately **not** merged: auto-merging
/// the fuzzy band offline risks corrupting the graph, which is exactly why the
/// band is surfaced for an LLM (`--semantic`) or human in the first place. The
/// caller applies the returned pairs via [`merge_pairs`] and should surface the
/// remainder for review. Deterministic: the result preserves input order.
pub fn deterministic_tiebreak(nodes: &[Node], pairs: &[(NodeId, NodeId)]) -> Vec<(NodeId, NodeId)> {
    let label_of: HashMap<&NodeId, &str> =
        nodes.iter().map(|n| (&n.id, n.label.as_str())).collect();
    // Sorted multiset of normalized word-tokens for a node, or `None` if its label
    // is unknown / has no tokens.
    let token_multiset = |id: &NodeId| -> Option<Vec<String>> {
        let label = label_of.get(id)?;
        let mut toks: Vec<String> = norm(label).split_whitespace().map(String::from).collect();
        if toks.is_empty() {
            return None;
        }
        toks.sort();
        Some(toks)
    };
    pairs
        .iter()
        .filter(|(a, b)| match (token_multiset(a), token_multiset(b)) {
            (Some(ta), Some(tb)) => ta == tb,
            _ => false,
        })
        .cloned()
        .collect()
}

/// Apply confirmed merges (e.g. an LLM tiebreaker's "yes" pairs): union each
/// pair, then drop merged-away nodes and rewire edges onto the survivor.
pub fn merge_pairs(
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    pairs: &[(NodeId, NodeId)],
) -> (Vec<Node>, Vec<Edge>) {
    if pairs.is_empty() {
        return (nodes, edges);
    }
    let mut uf = UnionFind::default();
    for (a, b) in pairs {
        uf.union(a, b);
    }
    apply_components(nodes, edges, &mut uf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn n(id: &str, label: &str, ft: FileType, sf: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: ft,
            source_file: sf.into(),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    fn edge(s: &str, t: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: "mentions".into(),
            confidence: synaptic_core::Confidence::Inferred,
            source_file: "d.md".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn code_nodes_are_never_merged() {
        // Two same-labeled code symbols in different files must NOT merge.
        let nodes = vec![
            n("a_draw", ".draw()", FileType::Code, "a.py"),
            n("b_draw", ".draw()", FileType::Code, "b.py"),
        ];
        let (out, _) = deduplicate_entities(nodes, vec![], &HashMap::new());
        assert_eq!(out.len(), 2, "code symbols stay distinct");
    }

    #[test]
    fn exact_concept_duplicates_in_same_file_merge() {
        let nodes = vec![
            n("c1", "Knowledge Graph", FileType::Concept, "doc.md"),
            n("c2", "knowledge-graph", FileType::Concept, "doc.md"),
        ];
        let edges = vec![edge("c2", "c1")];
        let (out, out_edges) = deduplicate_entities(nodes, edges, &HashMap::new());
        assert_eq!(out.len(), 1, "normalized-equal concepts in one file merge");
        // The self-referential edge (c2->c1, both remap to winner) is dropped.
        assert!(
            out_edges.is_empty(),
            "self-edge after merge dropped: {out_edges:?}"
        );
    }

    #[test]
    fn fuzzy_typo_concepts_merge() {
        // High-entropy long labels differing by a single mid-word substitution
        // (not a prefix-extension) -> fuzzy merge. "Consensus" vs "Consensos".
        let nodes = vec![
            n(
                "c1",
                "Distributed Consensus Algorithm",
                FileType::Concept,
                "",
            ),
            n(
                "c2",
                "Distributed Consensos Algorithm",
                FileType::Concept,
                "",
            ),
        ];
        let (out, _) = deduplicate_entities(nodes, vec![], &HashMap::new());
        assert_eq!(out.len(), 1, "near-identical concepts fuzzy-merge");
    }

    #[test]
    fn distinct_concepts_do_not_merge() {
        let nodes = vec![
            n(
                "c1",
                "Distributed Consensus Algorithm",
                FileType::Concept,
                "",
            ),
            n("c2", "Photosynthesis In Plants", FileType::Concept, ""),
        ];
        let (out, _) = deduplicate_entities(nodes, vec![], &HashMap::new());
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn prefix_extension_is_not_merged() {
        // "parse config" vs "parse config file": one extends the other, so distinct.
        let nodes = vec![
            n("c1", "parse configuration", FileType::Concept, ""),
            n("c2", "parse configuration file", FileType::Concept, ""),
        ];
        let (out, _) = deduplicate_entities(nodes, vec![], &HashMap::new());
        assert_eq!(out.len(), 2, "prefix-extension pairs stay distinct");
    }

    #[test]
    fn ambiguous_pairs_use_all_pairs_not_lsh() {
        // Two concepts in the [75,92) JW band but with low character-trigram
        // overlap: LSH blocking would never bucket them together, so the
        // all-pairs scan is what surfaces them to the tiebreaker.
        let nodes = vec![
            n("c1", "Data Pipeline Stage", FileType::Concept, ""),
            n("c2", "Data Processing Stage", FileType::Concept, ""),
        ];
        let pairs = ambiguous_concept_pairs(&nodes, &HashMap::new());
        assert_eq!(
            pairs.len(),
            1,
            "ambiguous low-overlap pair surfaced: {pairs:?}"
        );
    }

    #[test]
    fn deterministic_tiebreak_merges_word_reorderings() {
        // Same words, different order: a near-certain duplicate the LLM-free
        // pipeline can safely merge.
        let nodes = vec![
            n("c1", "Order Management", FileType::Concept, ""),
            n("c2", "Management Order", FileType::Concept, ""),
        ];
        let pairs = vec![(NodeId("c1".into()), NodeId("c2".into()))];
        let confirmed = deterministic_tiebreak(&nodes, &pairs);
        assert_eq!(confirmed, pairs, "word-reordering is a safe merge");
    }

    #[test]
    fn deterministic_tiebreak_leaves_genuinely_ambiguous_pairs() {
        // Different middle word means genuinely ambiguous, must NOT be auto-merged
        // (this is the case only an LLM/human should resolve).
        let nodes = vec![
            n("c1", "Data Pipeline Stage", FileType::Concept, ""),
            n("c2", "Data Processing Stage", FileType::Concept, ""),
        ];
        let pairs = vec![(NodeId("c1".into()), NodeId("c2".into()))];
        assert!(
            deterministic_tiebreak(&nodes, &pairs).is_empty(),
            "ambiguous pairs are left for the LLM/human, not auto-merged"
        );
    }

    #[test]
    fn deterministic_tiebreak_resolves_end_to_end_via_merge_pairs() {
        // A pair the deterministic tiebreaker confirms collapses to one node when
        // applied through merge_pairs: the full offline resolution path.
        let nodes = vec![
            n("c1", "User Authentication Flow", FileType::Concept, "d.md"),
            n("c2", "Authentication User Flow", FileType::Concept, "d.md"),
        ];
        let edges = vec![edge("c2", "other")];
        let pairs = vec![(NodeId("c1".into()), NodeId("c2".into()))];
        let confirmed = deterministic_tiebreak(&nodes, &pairs);
        assert_eq!(confirmed.len(), 1);
        let (merged, merged_edges) = merge_pairs(nodes, edges, &confirmed);
        assert_eq!(merged.len(), 1, "the two concepts collapse to one");
        // The edge from the merged-away node is rewired onto the survivor.
        assert_eq!(merged_edges.len(), 1);
        assert_eq!(merged_edges[0].source, NodeId("c1".into()));
    }

    #[test]
    fn deterministic_tiebreak_ignores_unknown_ids() {
        let nodes = vec![n("c1", "Whatever", FileType::Concept, "")];
        let pairs = vec![(NodeId("c1".into()), NodeId("missing".into()))];
        assert!(deterministic_tiebreak(&nodes, &pairs).is_empty());
    }

    #[test]
    fn winner_prefers_no_chunk_suffix_then_shorter_id() {
        assert_eq!(
            pick_winner(&[NodeId("topic_c1".into()), NodeId("topic".into())]),
            NodeId("topic".into())
        );
        assert_eq!(
            pick_winner(&[NodeId("longer_id".into()), NodeId("short".into())]),
            NodeId("short".into())
        );
    }
}
