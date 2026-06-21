//! Graph analysis: god nodes, surprising connections, suggested questions,
//! import cycles, and graph diff.
//!
//! A small inline file-category / language-family
//! table avoids a `synaptic-detect` dependency. Bridge-node detection and the
//! no-community surprise fallback use Brandes betweenness (see [`crate::betweenness`]);
//! large-graph node betweenness is sampled deterministically (no RNG).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use synaptic_core::{Confidence, NodeId};

use crate::graph::KnowledgeGraph;

// noise tables
const BUILTIN_NOISE_LABELS: &[&str] = &[
    "str",
    "int",
    "float",
    "bool",
    "bytes",
    "bytearray",
    "complex",
    "object",
    "True",
    "False",
    "MagicMock",
    "Mock",
    "AsyncMock",
    "NonCallableMock",
    "NonCallableMagicMock",
    "PropertyMock",
    "patch",
    "sentinel",
    "Path",
    "Any",
    "Optional",
    "List",
    "Dict",
    "Set",
    "Tuple",
    "Union",
    "Callable",
    "Type",
    "ClassVar",
    "Final",
    "Literal",
    "Protocol",
    "Counter",
    "defaultdict",
    "OrderedDict",
    "datetime",
    "Enum",
    "os",
    "sys",
    "re",
    "json",
    "io",
    "abc",
    "typing",
];
const JSON_NOISE_LABELS: &[&str] = &[
    "start",
    "end",
    "name",
    "id",
    "type",
    "properties",
    "value",
    "key",
    "data",
    "items",
    "title",
    "description",
    "version",
    "dependencies",
    "devdependencies",
    "peerdependencies",
    "optionaldependencies",
    "bundleddependencies",
    "bundledependencies",
];

// file categorization (subset of the detect sets)
const CODE_EXTS: &[&str] = &[
    "py", "pyw", "ts", "tsx", "js", "jsx", "mjs", "go", "rs", "java", "kt", "kts", "scala", "c",
    "h", "cpp", "cc", "cxx", "hpp", "rb", "swift", "cs", "php", "r", "json", "vue", "svelte", "sh",
];
const PAPER_EXTS: &[&str] = &["pdf"];
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg"];

fn ext_of(path: &str) -> String {
    match path.rsplit_once('.') {
        Some((_, e)) if !e.contains('/') => e.to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn file_category(path: &str) -> &'static str {
    let e = ext_of(path);
    if CODE_EXTS.contains(&e.as_str()) {
        "code"
    } else if PAPER_EXTS.contains(&e.as_str()) {
        "paper"
    } else if IMAGE_EXTS.contains(&e.as_str()) {
        "image"
    } else {
        "doc"
    }
}

fn lang_family(path: &str) -> Option<&'static str> {
    Some(match ext_of(path).as_str() {
        "py" | "pyw" => "python",
        "js" | "jsx" | "mjs" | "ejs" | "ts" | "tsx" | "vue" | "svelte" => "js",
        "go" => "go",
        "rs" => "rust",
        "java" | "kt" | "kts" | "scala" => "jvm",
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" => "c",
        "rb" => "ruby",
        "swift" => "swift",
        "cs" => "dotnet",
        "php" => "php",
        "r" => "r",
        _ => return None,
    })
}

fn cross_language(a: &str, b: &str) -> bool {
    match (lang_family(a), lang_family(b)) {
        (Some(fa), Some(fb)) => fa != fb,
        _ => false,
    }
}

fn top_level_dir(path: &str) -> &str {
    match path.split_once('/') {
        Some((head, _)) => head,
        None => "",
    }
}

fn filename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// result types

/// A high-degree core abstraction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GodNode {
    pub id: NodeId,
    pub label: String,
    pub degree: usize,
}

/// A non-obvious connection between two entities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Surprise {
    pub source: String,
    pub target: String,
    pub source_files: [String; 2],
    pub confidence: Confidence,
    pub relation: String,
    pub why: String,
}

/// A question the graph is positioned to answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub kind: String,
    pub question: Option<String>,
    pub why: String,
}

/// A file-level circular import dependency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportCycle {
    pub cycle: Vec<String>,
    pub length: usize,
    pub why: String,
}

/// Bundled analysis output, persisted as `.synaptic_analysis.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub god_nodes: Vec<GodNode>,
    pub surprising: Vec<Surprise>,
    pub questions: Vec<Question>,
    pub import_cycles: Vec<ImportCycle>,
}

// shared helpers

fn degree_map(kg: &KnowledgeGraph) -> HashMap<NodeId, usize> {
    let mut nbr: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for n in kg.nodes() {
        nbr.entry(n.id.clone()).or_default();
    }
    for e in kg.edges() {
        if e.source == e.target {
            continue;
        }
        nbr.entry(e.source.clone())
            .or_default()
            .insert(e.target.clone());
        nbr.entry(e.target.clone())
            .or_default()
            .insert(e.source.clone());
    }
    nbr.into_iter().map(|(k, v)| (k, v.len())).collect()
}

fn label_of(kg: &KnowledgeGraph, id: &NodeId) -> String {
    kg.node(id)
        .map(|n| n.label.clone())
        .unwrap_or_else(|| id.0.clone())
}

fn source_of(kg: &KnowledgeGraph, id: &NodeId) -> String {
    kg.node(id)
        .map(|n| n.source_file.clone())
        .unwrap_or_default()
}

fn is_file_node(kg: &KnowledgeGraph, id: &NodeId, degrees: &HashMap<NodeId, usize>) -> bool {
    let Some(node) = kg.node(id) else {
        return false;
    };
    let label = &node.label;
    if label.is_empty() {
        return false;
    }
    if !node.source_file.is_empty() && label == filename(&node.source_file) {
        return true;
    }
    if label.starts_with('.') && label.ends_with("()") {
        return true;
    }
    if label.ends_with("()") && degrees.get(id).copied().unwrap_or(0) <= 1 {
        return true;
    }
    false
}

fn is_concept_node(kg: &KnowledgeGraph, id: &NodeId) -> bool {
    let source = source_of(kg, id);
    if source.is_empty() {
        return true;
    }
    !filename(&source).contains('.')
}

fn is_json_key_node(kg: &KnowledgeGraph, id: &NodeId) -> bool {
    let src = source_of(kg, id).to_ascii_lowercase();
    if !src.ends_with(".json") {
        return false;
    }
    let label = label_of(kg, id).trim().to_ascii_lowercase();
    JSON_NOISE_LABELS.contains(&label.as_str())
}

fn node_community(communities: &BTreeMap<u32, Vec<NodeId>>) -> HashMap<NodeId, u32> {
    let mut m = HashMap::new();
    for (cid, nodes) in communities {
        for n in nodes {
            m.insert(n.clone(), *cid);
        }
    }
    m
}

// god nodes

/// Top `top_n` most-connected real entities, excluding file/concept/json-key/
/// builtin-noise nodes.
pub fn god_nodes(kg: &KnowledgeGraph, top_n: usize) -> Vec<GodNode> {
    let degrees = degree_map(kg);
    let mut sorted: Vec<(NodeId, usize)> = degrees.iter().map(|(k, v)| (k.clone(), *v)).collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut result = Vec::new();
    for (id, deg) in sorted {
        if is_file_node(kg, &id, &degrees) || is_concept_node(kg, &id) || is_json_key_node(kg, &id)
        {
            continue;
        }
        let label = label_of(kg, &id);
        if BUILTIN_NOISE_LABELS.contains(&label.as_str()) {
            continue;
        }
        result.push(GodNode {
            id,
            label,
            degree: deg,
        });
        if result.len() >= top_n {
            break;
        }
    }
    result
}

// surprise scoring

#[allow(clippy::too_many_arguments)]
fn surprise_score(
    relation: &str,
    confidence: Confidence,
    u: &NodeId,
    v: &NodeId,
    u_source: &str,
    v_source: &str,
    node_comm: &HashMap<NodeId, u32>,
    degrees: &HashMap<NodeId, usize>,
) -> (i64, Vec<String>) {
    let mut score: i64 = 0;
    let mut reasons: Vec<String> = Vec::new();

    let mut conf_bonus = match confidence {
        Confidence::Ambiguous => 3,
        Confidence::Inferred => 2,
        Confidence::Extracted => 1,
    };
    let cat_u = file_category(u_source);
    let cat_v = file_category(v_source);

    let suppress = confidence == Confidence::Inferred
        && (relation == "calls" || relation == "uses")
        && (cross_language(u_source, v_source) || {
            let mut s = [cat_u, cat_v];
            s.sort_unstable();
            s == ["code", "doc"]
        });
    if suppress {
        conf_bonus = 0;
    }

    score += conf_bonus;
    if matches!(confidence, Confidence::Ambiguous | Confidence::Inferred) {
        reasons.push(format!(
            "{} connection - not explicitly stated in source",
            match confidence {
                Confidence::Ambiguous => "ambiguous",
                Confidence::Inferred => "inferred",
                Confidence::Extracted => "extracted",
            }
        ));
    }

    if cat_u != cat_v && !suppress {
        score += 2;
        reasons.push(format!("crosses file types ({cat_u} ↔ {cat_v})"));
    }
    if top_level_dir(u_source) != top_level_dir(v_source) && !suppress {
        score += 2;
        reasons.push("connects across different repos/directories".to_string());
    }
    let cid_u = node_comm.get(u);
    let cid_v = node_comm.get(v);
    if let (Some(a), Some(b)) = (cid_u, cid_v) {
        if a != b && !suppress {
            score += 1;
            reasons.push("bridges separate communities".to_string());
        }
    }
    if relation == "semantically_similar_to" {
        score = (score as f64 * 1.5) as i64;
        reasons.push("semantically similar concepts with no structural link".to_string());
    }
    let du = degrees.get(u).copied().unwrap_or(0);
    let dv = degrees.get(v).copied().unwrap_or(0);
    if du.min(dv) <= 2 && du.max(dv) >= 5 {
        score += 1;
        reasons.push("peripheral node unexpectedly reaches a hub".to_string());
    }
    (score, reasons)
}

const STRUCTURAL_RELATIONS: &[&str] = &["imports", "imports_from", "contains", "method"];

/// Find non-obvious connections. Multi-source corpora rank cross-file edges by
/// composite surprise score; single-source corpora use community-bridge edges.
pub fn surprising_connections(
    kg: &KnowledgeGraph,
    communities: &BTreeMap<u32, Vec<NodeId>>,
    top_n: usize,
) -> Vec<Surprise> {
    let source_files: HashSet<String> = kg
        .nodes()
        .map(|n| n.source_file.clone())
        .filter(|s| !s.is_empty())
        .collect();
    if source_files.len() > 1 {
        cross_file_surprises(kg, communities, top_n)
    } else {
        cross_community_surprises(kg, communities, top_n)
    }
}

fn cross_file_surprises(
    kg: &KnowledgeGraph,
    communities: &BTreeMap<u32, Vec<NodeId>>,
    top_n: usize,
) -> Vec<Surprise> {
    let node_comm = node_community(communities);
    let degrees = degree_map(kg);
    let mut candidates: Vec<(i64, Surprise)> = Vec::new();
    for e in kg.edges() {
        if STRUCTURAL_RELATIONS.contains(&e.relation.as_str()) {
            continue;
        }
        if is_concept_node(kg, &e.source) || is_concept_node(kg, &e.target) {
            continue;
        }
        if is_file_node(kg, &e.source, &degrees) || is_file_node(kg, &e.target, &degrees) {
            continue;
        }
        let us = source_of(kg, &e.source);
        let vs = source_of(kg, &e.target);
        if us.is_empty() || vs.is_empty() || us == vs {
            continue;
        }
        let (score, reasons) = surprise_score(
            &e.relation,
            e.confidence,
            &e.source,
            &e.target,
            &us,
            &vs,
            &node_comm,
            &degrees,
        );
        candidates.push((
            score,
            Surprise {
                source: label_of(kg, &e.source),
                target: label_of(kg, &e.target),
                source_files: [us, vs],
                confidence: e.confidence,
                relation: e.relation.clone(),
                why: if reasons.is_empty() {
                    "cross-file semantic connection".to_string()
                } else {
                    reasons.join("; ")
                },
            },
        ));
    }
    // Sort by score desc; stable tie-break by (source, target) for determinism.
    candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.source.cmp(&b.1.source))
            .then_with(|| a.1.target.cmp(&b.1.target))
    });
    if candidates.is_empty() {
        return cross_community_surprises(kg, communities, top_n);
    }
    candidates.into_iter().take(top_n).map(|(_, s)| s).collect()
}

/// Fallback for `cross_community_surprises` when there is no community info: rank
/// edges by Brandes betweenness and surface the top `top_n` structural bridges.
/// Skipped for graphs over 5000 nodes (betweenness is
/// O(V·E)).
fn edge_betweenness_surprises(kg: &KnowledgeGraph, top_n: usize) -> Vec<Surprise> {
    if kg.edges().next().is_none() || kg.nodes().count() > 5000 {
        return Vec::new();
    }
    // Representative edge (first seen) per unordered node pair.
    let mut edge_of: HashMap<(NodeId, NodeId), (NodeId, NodeId, String, Confidence)> =
        HashMap::new();
    for e in kg.edges() {
        if e.source == e.target {
            continue;
        }
        let key = if e.source <= e.target {
            (e.source.clone(), e.target.clone())
        } else {
            (e.target.clone(), e.source.clone())
        };
        edge_of.entry(key).or_insert((
            e.source.clone(),
            e.target.clone(),
            e.relation.clone(),
            e.confidence,
        ));
    }
    let mut scored: Vec<((NodeId, NodeId), f64)> = crate::betweenness::edge_betweenness(kg)
        .into_iter()
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut out = Vec::new();
    for (pair, score) in scored {
        let Some((src, tgt, rel, conf)) = edge_of.get(&pair) else {
            continue;
        };
        out.push(Surprise {
            source: label_of(kg, src),
            target: label_of(kg, tgt),
            source_files: [source_of(kg, src), source_of(kg, tgt)],
            confidence: *conf,
            relation: rel.clone(),
            why: format!("Bridges graph structure (betweenness={score:.3})"),
        });
        if out.len() >= top_n {
            break;
        }
    }
    out
}

fn cross_community_surprises(
    kg: &KnowledgeGraph,
    communities: &BTreeMap<u32, Vec<NodeId>>,
    top_n: usize,
) -> Vec<Surprise> {
    if communities.is_empty() {
        // No community info (single-source corpus): rank edges by betweenness and
        // surface the top structural bridges.
        return edge_betweenness_surprises(kg, top_n);
    }
    let node_comm = node_community(communities);
    let degrees = degree_map(kg);
    let order = |c: Confidence| match c {
        Confidence::Ambiguous => 0,
        Confidence::Inferred => 1,
        Confidence::Extracted => 2,
    };
    let mut surprises: Vec<(u8, (u32, u32), Surprise)> = Vec::new();
    for e in kg.edges() {
        let (Some(&a), Some(&b)) = (node_comm.get(&e.source), node_comm.get(&e.target)) else {
            continue;
        };
        if a == b {
            continue;
        }
        if is_file_node(kg, &e.source, &degrees) || is_file_node(kg, &e.target, &degrees) {
            continue;
        }
        if STRUCTURAL_RELATIONS.contains(&e.relation.as_str()) {
            continue;
        }
        let pair = if a <= b { (a, b) } else { (b, a) };
        surprises.push((
            order(e.confidence),
            pair,
            Surprise {
                source: label_of(kg, &e.source),
                target: label_of(kg, &e.target),
                source_files: [source_of(kg, &e.source), source_of(kg, &e.target)],
                confidence: e.confidence,
                relation: e.relation.clone(),
                why: format!("bridges community {a} → community {b}"),
            },
        ));
    }
    // AMBIGUOUS first, then INFERRED, then EXTRACTED; deterministic tie-break.
    surprises.sort_by(|x, y| {
        x.0.cmp(&y.0)
            .then_with(|| x.2.source.cmp(&y.2.source))
            .then_with(|| x.2.target.cmp(&y.2.target))
    });
    // One representative edge per community pair.
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    let mut out = Vec::new();
    for (_, pair, s) in surprises {
        if seen.insert(pair) {
            out.push(s);
            if out.len() >= top_n {
                break;
            }
        }
    }
    out
}

// suggested questions

/// Generate up to `top_n` questions the graph can answer;
/// bridge detection uses Brandes betweenness centrality.
pub fn suggest_questions(
    kg: &KnowledgeGraph,
    communities: &BTreeMap<u32, Vec<NodeId>>,
    community_labels: &BTreeMap<u32, String>,
    top_n: usize,
) -> Vec<Question> {
    let degrees = degree_map(kg);
    let node_comm = node_community(communities);
    let comm_label = |cid: u32| {
        community_labels
            .get(&cid)
            .cloned()
            .unwrap_or_else(|| format!("Community {cid}"))
    };
    let mut questions: Vec<Question> = Vec::new();

    // 1. AMBIGUOUS edges.
    for e in kg.edges() {
        if e.confidence == Confidence::Ambiguous {
            questions.push(Question {
                kind: "ambiguous_edge".to_string(),
                question: Some(format!(
                    "What is the exact relationship between `{}` and `{}`?",
                    label_of(kg, &e.source),
                    label_of(kg, &e.target)
                )),
                why: format!(
                    "Edge tagged AMBIGUOUS (relation: {}) - confidence is low.",
                    e.relation
                ),
            });
        }
    }
    // Budget short-circuit: only the first `top_n` questions survive the final
    // `truncate` (insertion order), so once we have enough, stop early and skip
    // later sections without changing the output.
    if top_n > 0 && questions.len() >= top_n {
        questions.truncate(top_n);
        return questions;
    }

    // 1b. Unresolved cross-language sinks: a subprocess invocation that did not
    // resolve to an in-repo binary/script (the `cross-language` pass leaves the
    // command stub in place). Surfaces an external runtime dependency to confirm.
    for n in kg.nodes() {
        if n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("command") {
            questions.push(Question {
                kind: "cross_language_sink".to_string(),
                question: Some(format!(
                    "What runs when this code invokes `{}`, and is that external command expected here?",
                    n.label
                )),
                why: "A subprocess invocation did not resolve to an in-repo binary or script."
                    .to_string(),
            });
        }
    }
    if top_n > 0 && questions.len() >= top_n {
        questions.truncate(top_n);
        return questions;
    }

    // 2. Bridge nodes (high betweenness): cross-community questions.
    // Rank non-file/non-concept nodes by Brandes betweenness, take the top 3, and
    // ask about each that actually spans communities.
    let neighbors = neighbor_map(kg);
    let betw = crate::betweenness::node_betweenness(kg);
    let mut ranked: Vec<(NodeId, f64)> = betw
        .into_iter()
        .filter(|(id, score)| {
            *score > 0.0 && !is_file_node(kg, id, &degrees) && !is_concept_node(kg, id)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    for (id, score) in ranked.into_iter().take(3) {
        let Some(&cid) = node_comm.get(&id) else {
            continue;
        };
        let others: BTreeSet<u32> = neighbors
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|nb| node_comm.get(nb).copied())
            .filter(|c| *c != cid)
            .collect();
        if others.is_empty() {
            continue;
        }
        let other_labels: Vec<String> = others
            .iter()
            .map(|c| format!("`{}`", comm_label(*c)))
            .collect();
        questions.push(Question {
            kind: "bridge_node".to_string(),
            question: Some(format!(
                "Why does `{}` connect `{}` to {}?",
                label_of(kg, &id),
                comm_label(cid),
                other_labels.join(", ")
            )),
            why: format!("High betweenness centrality ({score:.3}) - a cross-community bridge."),
        });
    }
    if top_n > 0 && questions.len() >= top_n {
        questions.truncate(top_n);
        return questions;
    }

    // 3. God nodes with >= 2 INFERRED edges.
    let mut by_degree: Vec<(NodeId, usize)> = degrees
        .iter()
        .filter(|(id, _)| !is_file_node(kg, id, &degrees))
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    by_degree.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (id, _) in by_degree.into_iter().take(5) {
        let inferred: Vec<&NodeId> = kg
            .edges()
            .filter(|e| e.confidence == Confidence::Inferred && (e.source == id || e.target == id))
            .map(|e| if e.source == id { &e.target } else { &e.source })
            .collect();
        if inferred.len() >= 2 {
            questions.push(Question {
                kind: "verify_inferred".to_string(),
                question: Some(format!(
                    "Are the {} inferred relationships involving `{}` (e.g. with `{}` and `{}`) actually correct?",
                    inferred.len(),
                    label_of(kg, &id),
                    label_of(kg, inferred[0]),
                    label_of(kg, inferred[1])
                )),
                why: format!("`{}` has {} INFERRED edges that need verification.", label_of(kg, &id), inferred.len()),
            });
        }
    }
    if top_n > 0 && questions.len() >= top_n {
        questions.truncate(top_n);
        return questions;
    }

    // 4. Isolated / weakly-connected nodes.
    let mut isolated: Vec<NodeId> = kg
        .nodes()
        .filter(|n| {
            degrees.get(&n.id).copied().unwrap_or(0) <= 1
                && !is_file_node(kg, &n.id, &degrees)
                && !is_concept_node(kg, &n.id)
        })
        .map(|n| n.id.clone())
        .collect();
    isolated.sort();
    if !isolated.is_empty() {
        let labels: Vec<String> = isolated
            .iter()
            .take(3)
            .map(|id| format!("`{}`", label_of(kg, id)))
            .collect();
        questions.push(Question {
            kind: "isolated_nodes".to_string(),
            question: Some(format!(
                "What connects {} to the rest of the system?",
                labels.join(", ")
            )),
            why: format!(
                "{} weakly-connected nodes found - possible documentation gaps.",
                isolated.len()
            ),
        });
    }

    if top_n > 0 && questions.len() >= top_n {
        questions.truncate(top_n);
        return questions;
    }

    // 5. Low-cohesion communities. Intra-community edge counts are gathered in a
    // single O(edges) pass, deduped by undirected pair exactly like
    // `cohesion_score`, instead of re-scanning every edge once per community
    // (which was O(communities × edges)). The score is identical.
    let mut intra_pairs: HashMap<u32, HashSet<(&NodeId, &NodeId)>> = HashMap::new();
    for e in kg.edges() {
        if e.source == e.target {
            continue;
        }
        let (Some(&ca), Some(&cb)) = (node_comm.get(&e.source), node_comm.get(&e.target)) else {
            continue;
        };
        if ca != cb {
            continue;
        }
        let key = if e.source <= e.target {
            (&e.source, &e.target)
        } else {
            (&e.target, &e.source)
        };
        intra_pairs.entry(ca).or_default().insert(key);
    }
    for (cid, nodes) in communities {
        let n = nodes.len();
        // Only communities of >= 5 nodes can produce a question; smaller ones
        // were filtered by the original `&& nodes.len() >= 5` guard.
        if n < 5 {
            continue;
        }
        let possible = (n * (n - 1)) as f64 / 2.0;
        let actual = intra_pairs.get(cid).map(|s| s.len()).unwrap_or(0) as f64;
        let score = actual / possible;
        if score < 0.15 {
            questions.push(Question {
                kind: "low_cohesion".to_string(),
                question: Some(format!(
                    "Should `{}` be split into smaller, more focused modules?",
                    comm_label(*cid)
                )),
                why: format!("Cohesion score {score:.3} - nodes are weakly interconnected."),
            });
        }
    }

    if questions.is_empty() {
        return vec![Question {
            kind: "no_signal".to_string(),
            question: None,
            why: "Not enough signal to generate questions (no ambiguous edges, bridges, inferred relationships, or low-cohesion communities).".to_string(),
        }];
    }
    questions.truncate(top_n);
    questions
}

fn neighbor_map(kg: &KnowledgeGraph) -> HashMap<NodeId, HashSet<NodeId>> {
    let mut m: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for n in kg.nodes() {
        m.entry(n.id.clone()).or_default();
    }
    for e in kg.edges() {
        if e.source == e.target {
            continue;
        }
        m.entry(e.source.clone())
            .or_default()
            .insert(e.target.clone());
        m.entry(e.target.clone())
            .or_default()
            .insert(e.source.clone());
    }
    m
}

// import cycles

/// Detect file-level circular import dependencies (relations `imports_from` /
/// `re_exports`, oriented by `source_file`). Returns cycles of length 2..=max,
/// shortest first, deduplicated by rotation.
pub fn find_import_cycles(
    kg: &KnowledgeGraph,
    max_cycle_length: usize,
    top_n: usize,
) -> Vec<ImportCycle> {
    // Build a directed file-level graph.
    let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for e in kg.edges() {
        if e.relation != "imports_from" && e.relation != "re_exports" {
            continue;
        }
        let edge_src = &e.source_file;
        if edge_src.is_empty() {
            continue;
        }
        let u_file = source_of(kg, &e.source);
        let v_file = source_of(kg, &e.target);
        let tgt = if &u_file == edge_src {
            v_file
        } else if &v_file == edge_src {
            u_file
        } else if !v_file.is_empty() && &v_file != edge_src {
            v_file
        } else {
            u_file
        };
        if tgt.is_empty() {
            continue;
        }
        adj.entry(edge_src.clone()).or_default().insert(tgt);
    }
    if adj.is_empty() {
        return Vec::new();
    }

    // Bounded DFS for simple cycles whose minimum node is the start (canonical).
    let mut cycles: Vec<Vec<String>> = Vec::new();
    let starts: Vec<String> = adj.keys().cloned().collect();
    for start in &starts {
        let mut path = vec![start.clone()];
        let mut visited: HashSet<String> = [start.clone()].into_iter().collect();
        dfs_cycles(
            start,
            start,
            &adj,
            max_cycle_length,
            &mut path,
            &mut visited,
            &mut cycles,
        );
    }

    cycles.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    cycles.dedup();
    cycles
        .into_iter()
        .take(top_n)
        .map(|c| ImportCycle {
            length: c.len(),
            cycle: c,
            why: "circular dependency".to_string(),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn dfs_cycles(
    start: &str,
    current: &str,
    adj: &BTreeMap<String, BTreeSet<String>>,
    max_len: usize,
    path: &mut Vec<String>,
    visited: &mut HashSet<String>,
    out: &mut Vec<Vec<String>>,
) {
    if let Some(neighbors) = adj.get(current) {
        for nb in neighbors {
            if nb == start {
                if path.len() >= 2 {
                    out.push(path.clone());
                }
                continue;
            }
            if nb.as_str() < start {
                continue; // canonical: start must be the minimum node
            }
            if visited.contains(nb) || path.len() >= max_len {
                continue;
            }
            visited.insert(nb.clone());
            path.push(nb.clone());
            dfs_cycles(start, nb, adj, max_len, path, visited, out);
            path.pop();
            visited.remove(nb);
        }
    }
}

// graph diff

/// What changed between two graph snapshots.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphDelta {
    pub new_nodes: Vec<NodeId>,
    pub removed_nodes: Vec<NodeId>,
    pub new_edges: Vec<(NodeId, NodeId, String)>,
    pub removed_edges: Vec<(NodeId, NodeId, String)>,
    pub summary: String,
}

fn edge_key(s: &NodeId, t: &NodeId, rel: &str) -> (NodeId, NodeId, String) {
    if s <= t {
        (s.clone(), t.clone(), rel.to_string())
    } else {
        (t.clone(), s.clone(), rel.to_string())
    }
}

/// Compare two graphs (undirected edge identity).
pub fn graph_diff(old: &KnowledgeGraph, new: &KnowledgeGraph) -> GraphDelta {
    let old_nodes: HashSet<NodeId> = old.nodes().map(|n| n.id.clone()).collect();
    let new_nodes_set: HashSet<NodeId> = new.nodes().map(|n| n.id.clone()).collect();
    let mut new_nodes: Vec<NodeId> = new_nodes_set.difference(&old_nodes).cloned().collect();
    let mut removed_nodes: Vec<NodeId> = old_nodes.difference(&new_nodes_set).cloned().collect();
    new_nodes.sort();
    removed_nodes.sort();

    let old_edges: HashSet<(NodeId, NodeId, String)> = old
        .edges()
        .map(|e| edge_key(&e.source, &e.target, &e.relation))
        .collect();
    let new_edges_set: HashSet<(NodeId, NodeId, String)> = new
        .edges()
        .map(|e| edge_key(&e.source, &e.target, &e.relation))
        .collect();
    let mut new_edges: Vec<_> = new_edges_set.difference(&old_edges).cloned().collect();
    let mut removed_edges: Vec<_> = old_edges.difference(&new_edges_set).cloned().collect();
    new_edges.sort();
    removed_edges.sort();

    let mut parts = Vec::new();
    let plural = |n: usize, w: &str| {
        if n == 1 {
            format!("1 {w}")
        } else {
            format!("{n} {w}s")
        }
    };
    if !new_nodes.is_empty() {
        parts.push(plural(new_nodes.len(), "new node"));
    }
    if !new_edges.is_empty() {
        parts.push(plural(new_edges.len(), "new edge"));
    }
    if !removed_nodes.is_empty() {
        parts.push(format!("{} removed", plural(removed_nodes.len(), "node")));
    }
    if !removed_edges.is_empty() {
        parts.push(format!("{} removed", plural(removed_edges.len(), "edge")));
    }
    let summary = if parts.is_empty() {
        "no changes".to_string()
    } else {
        parts.join(", ")
    };

    GraphDelta {
        new_nodes,
        removed_nodes,
        new_edges,
        removed_edges,
        summary,
    }
}

// strongly connected components (tarjan)

/// Strongly connected components over edges whose relation is in `relations`
/// (empty = all relations). Each returned component is a sorted `Vec<NodeId>`;
/// trivial singletons (no self-loop) are omitted, so the result is exactly the
/// set of directed cycles' node sets. Iterative Tarjan — no recursion depth limit.
pub fn strongly_connected_components(kg: &KnowledgeGraph, relations: &[&str]) -> Vec<Vec<NodeId>> {
    let want = |r: &str| relations.is_empty() || relations.contains(&r);
    let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for n in kg.nodes() {
        adj.entry(n.id.clone()).or_default();
    }
    for e in kg.edges() {
        if want(e.relation.as_str()) {
            adj.entry(e.source.clone())
                .or_default()
                .push(e.target.clone());
        }
    }

    let mut index = 0i64;
    let mut indices: HashMap<NodeId, i64> = HashMap::new();
    let mut low: HashMap<NodeId, i64> = HashMap::new();
    let mut on_stack: HashSet<NodeId> = HashSet::new();
    let mut stack: Vec<NodeId> = Vec::new();
    let mut out: Vec<Vec<NodeId>> = Vec::new();

    let mut order: Vec<NodeId> = adj.keys().cloned().collect();
    order.sort();
    for start in order {
        if indices.contains_key(&start) {
            continue;
        }
        // Explicit DFS stack of (node, next-child cursor) to avoid recursion.
        let mut call: Vec<(NodeId, usize)> = vec![(start.clone(), 0)];
        while let Some((v, ci)) = call.last().cloned() {
            if ci == 0 {
                indices.insert(v.clone(), index);
                low.insert(v.clone(), index);
                index += 1;
                stack.push(v.clone());
                on_stack.insert(v.clone());
            }
            let children = adj.get(&v).cloned().unwrap_or_default();
            if ci < children.len() {
                let w = children[ci].clone();
                call.last_mut().unwrap().1 += 1;
                if !indices.contains_key(&w) {
                    call.push((w, 0));
                } else if on_stack.contains(&w) {
                    let lw = indices[&w];
                    let lv = low[&v];
                    low.insert(v.clone(), lv.min(lw));
                }
            } else {
                if low[&v] == indices[&v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack.remove(&w);
                        comp.push(w.clone());
                        if w == v {
                            break;
                        }
                    }
                    let self_loop = adj.get(&v).map(|c| c.contains(&v)).unwrap_or(false);
                    if comp.len() > 1 || self_loop {
                        comp.sort();
                        out.push(comp);
                    }
                }
                call.pop();
                if let Some((parent, _)) = call.last().cloned() {
                    let lp = low[&parent];
                    let lv = low[&v];
                    low.insert(parent, lp.min(lv));
                }
            }
        }
    }
    out.sort();
    out
}

// aggregator

/// Headline graph counts + edge-confidence breakdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStats {
    pub nodes: usize,
    pub edges: usize,
    pub communities: usize,
    pub extracted: usize,
    pub inferred: usize,
    pub ambiguous: usize,
}

/// Compute [`GraphStats`] for a graph (node/edge/community counts + the
/// EXTRACTED/INFERRED/AMBIGUOUS edge tally the `graph_stats` MCP tool reports).
pub fn graph_stats(kg: &KnowledgeGraph) -> GraphStats {
    let mut communities: HashSet<u32> = HashSet::new();
    for n in kg.nodes() {
        if let Some(c) = n.community {
            communities.insert(c);
        }
    }
    let (mut extracted, mut inferred, mut ambiguous) = (0usize, 0usize, 0usize);
    for e in kg.edges() {
        match e.confidence {
            Confidence::Extracted => extracted += 1,
            Confidence::Inferred => inferred += 1,
            Confidence::Ambiguous => ambiguous += 1,
        }
    }
    GraphStats {
        nodes: kg.node_count(),
        edges: kg.edge_count(),
        communities: communities.len(),
        extracted,
        inferred,
        ambiguous,
    }
}

/// Run the full analysis bundle (`god_nodes` 10, `surprising` 5, `questions` 7,
/// `import_cycles` 20). Persisted as `.synaptic_analysis.json`.
pub fn analyze(
    kg: &KnowledgeGraph,
    communities: &BTreeMap<u32, Vec<NodeId>>,
    community_labels: &BTreeMap<u32, String>,
) -> AnalysisResult {
    AnalysisResult {
        god_nodes: god_nodes(kg, 10),
        surprising: surprising_connections(kg, communities, 5),
        questions: suggest_questions(kg, communities, community_labels, 7),
        import_cycles: find_import_cycles(kg, 5, 20),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Edge, FileType, GraphData, Node};

    struct N {
        id: &'static str,
        label: &'static str,
        source_file: &'static str,
    }
    struct E {
        s: &'static str,
        t: &'static str,
        rel: &'static str,
        conf: Confidence,
    }

    fn build(nodes: &[N], edges: &[E]) -> KnowledgeGraph {
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: nodes
                .iter()
                .map(|n| Node {
                    id: NodeId(n.id.into()),
                    label: n.label.into(),
                    file_type: FileType::Code,
                    source_file: n.source_file.into(),
                    source_location: Some("L1".into()),
                    community: None,
                    repo: None,
                    extra: Map::new(),
                })
                .collect(),
            links: edges
                .iter()
                .map(|e| Edge {
                    source: NodeId(e.s.into()),
                    target: NodeId(e.t.into()),
                    relation: e.rel.into(),
                    confidence: e.conf,
                    source_file: "".into(),
                    source_location: Some("L1".into()),
                    confidence_score: None,
                    weight: 1.0,
                    context: None,
                    cross_repo: false,
                    extra: Map::new(),
                })
                .collect(),
            hyperedges: vec![],
            built_at_commit: None,
        };
        KnowledgeGraph::from_graph_data(gd)
    }

    fn n(id: &'static str, label: &'static str, sf: &'static str) -> N {
        N {
            id,
            label,
            source_file: sf,
        }
    }
    fn e(s: &'static str, t: &'static str, rel: &'static str, conf: Confidence) -> E {
        E { s, t, rel, conf }
    }

    #[test]
    fn god_nodes_ranked_by_degree_descending() {
        // hub connected to 3 leaves; leaves degree 1.
        let kg = build(
            &[
                n("hub", "Hub", "src/a.py"),
                n("l1", "L1", "src/b.py"),
                n("l2", "L2", "src/c.py"),
                n("l3", "L3", "src/d.py"),
            ],
            &[
                e("hub", "l1", "references", Confidence::Extracted),
                e("hub", "l2", "references", Confidence::Extracted),
                e("hub", "l3", "references", Confidence::Extracted),
            ],
        );
        let gods = god_nodes(&kg, 10);
        let degs: Vec<usize> = gods.iter().map(|g| g.degree).collect();
        let mut sorted = degs.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(degs, sorted);
        assert_eq!(gods[0].label, "Hub");
    }

    #[test]
    fn scc_finds_directed_cycles_only() {
        // a->b->c->a is one SCC; d is acyclic (omitted).
        let kg = build(
            &[
                n("a", "A", "a.rs"),
                n("b", "B", "b.rs"),
                n("c", "C", "c.rs"),
                n("d", "D", "d.rs"),
            ],
            &[
                e("a", "b", "calls", Confidence::Extracted),
                e("b", "c", "calls", Confidence::Extracted),
                e("c", "a", "calls", Confidence::Extracted),
                e("a", "d", "calls", Confidence::Extracted),
            ],
        );
        let sccs = strongly_connected_components(&kg, &[]);
        assert_eq!(sccs.len(), 1, "exactly one non-trivial SCC");
        assert_eq!(
            sccs[0],
            vec![NodeId("a".into()), NodeId("b".into()), NodeId("c".into())]
        );
        // Relation filter excludes everything when no edges match.
        assert!(strongly_connected_components(&kg, &["imports"]).is_empty());
    }

    #[test]
    fn god_nodes_excludes_json_noise() {
        // json key node has high degree but must be filtered.
        let mut nodes = vec![
            n("real", "AuthService", "src/auth.py"),
            n("jn", "name", "schema.json"),
        ];
        let mut edges = Vec::new();
        for i in 0..8 {
            let id: &'static str = Box::leak(format!("p{i}").into_boxed_str());
            let lbl: &'static str = Box::leak(format!("Peer{i}").into_boxed_str());
            let sf: &'static str = Box::leak(format!("src/p{i}.py").into_boxed_str());
            nodes.push(n(id, lbl, sf));
            edges.push(e("jn", id, "references", Confidence::Extracted));
            edges.push(e("real", id, "references", Confidence::Extracted));
        }
        let kg = build(&nodes, &edges);
        let labels: Vec<String> = god_nodes(&kg, 10).into_iter().map(|g| g.label).collect();
        assert!(!labels.contains(&"name".to_string()));
        assert!(labels.contains(&"AuthService".to_string()));
    }

    #[test]
    fn god_nodes_excludes_builtin_noise() {
        let kg = build(
            &[
                n("s", "str", "src/a.py"),
                n("real", "Service", "src/b.py"),
                n("x", "X", "src/c.py"),
            ],
            &[
                e("s", "real", "references", Confidence::Extracted),
                e("s", "x", "references", Confidence::Extracted),
                e("real", "x", "references", Confidence::Extracted),
            ],
        );
        let labels: Vec<String> = god_nodes(&kg, 10).into_iter().map(|g| g.label).collect();
        assert!(!labels.contains(&"str".to_string()));
    }

    #[test]
    fn ambiguous_scores_higher_than_extracted() {
        let nc: HashMap<NodeId, u32> = [("a", 0u32), ("c", 0), ("b", 1), ("d", 1)]
            .iter()
            .map(|(n, c)| (NodeId(n.to_string()), *c))
            .collect();
        let degrees: HashMap<NodeId, usize> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| (NodeId(n.to_string()), 1usize))
            .collect();
        let (amb, _) = surprise_score(
            "calls",
            Confidence::Ambiguous,
            &NodeId("a".into()),
            &NodeId("b".into()),
            "repo1/model.py",
            "repo2/train.py",
            &nc,
            &degrees,
        );
        let (ext, _) = surprise_score(
            "calls",
            Confidence::Extracted,
            &NodeId("c".into()),
            &NodeId("d".into()),
            "repo1/data.py",
            "repo2/eval.py",
            &nc,
            &degrees,
        );
        assert!(amb > ext, "{amb} should beat {ext}");
    }

    #[test]
    fn cross_language_inferred_calls_suppressed() {
        let nc: HashMap<NodeId, u32> = HashMap::new();
        let degrees: HashMap<NodeId, usize> = ["py_auth", "ts_member", "py_a", "py_b"]
            .iter()
            .map(|n| (NodeId(n.to_string()), 1usize))
            .collect();
        let (cross, _) = surprise_score(
            "calls",
            Confidence::Inferred,
            &NodeId("py_auth".into()),
            &NodeId("ts_member".into()),
            "backend/auth.py",
            "frontend/types.ts",
            &nc,
            &degrees,
        );
        let (same, _) = surprise_score(
            "calls",
            Confidence::Extracted,
            &NodeId("py_a".into()),
            &NodeId("py_b".into()),
            "backend/service.py",
            "backend/utils.py",
            &nc,
            &degrees,
        );
        assert!(cross <= same);
    }

    #[test]
    fn surprising_connections_excludes_concept_nodes() {
        let kg = build(
            &[
                n("t", "Transformer", "src/model.py"),
                n("u", "Trainer", "src/train.py"),
                n("concept", "Abstract Concept", ""),
            ],
            &[
                e("t", "u", "references", Confidence::Inferred),
                e("t", "concept", "relates_to", Confidence::Inferred),
            ],
        );
        let comms: BTreeMap<u32, Vec<NodeId>> = [
            (0u32, vec![NodeId("t".into())]),
            (1u32, vec![NodeId("u".into())]),
        ]
        .into_iter()
        .collect();
        let surprises = surprising_connections(&kg, &comms, 5);
        let labels: Vec<String> = surprises
            .iter()
            .flat_map(|s| [s.source.clone(), s.target.clone()])
            .collect();
        assert!(!labels.contains(&"Abstract Concept".to_string()));
        assert!(surprises
            .iter()
            .all(|s| s.source_files[0] != s.source_files[1]));
    }

    #[test]
    fn import_cycles_detect_2_and_3_cycles() {
        // a<->b 2-cycle; b->c->d->b 3-cycle. Edges oriented by source_file.
        let nodes = &[
            n("a", "a.ts", "src/a.ts"),
            n("b", "b.ts", "src/b.ts"),
            n("c", "c.ts", "src/c.ts"),
            n("d", "d.ts", "src/d.ts"),
        ];
        // helper to set edge.source_file = the importer's file
        let kg = {
            let mk = |s: &str, t: &str, sf: &str| Edge {
                source: NodeId(s.into()),
                target: NodeId(t.into()),
                relation: "imports_from".into(),
                confidence: Confidence::Extracted,
                source_file: sf.into(),
                source_location: Some("L1".into()),
                confidence_score: None,
                weight: 1.0,
                context: None,
                cross_repo: false,
                extra: Map::new(),
            };
            let gd = GraphData {
                directed: true,
                multigraph: false,
                graph: Map::new(),
                nodes: nodes
                    .iter()
                    .map(|x| Node {
                        id: NodeId(x.id.into()),
                        label: x.label.into(),
                        file_type: FileType::Code,
                        source_file: x.source_file.into(),
                        source_location: Some("L1".into()),
                        community: None,
                        repo: None,
                        extra: Map::new(),
                    })
                    .collect(),
                links: vec![
                    mk("a", "b", "src/a.ts"),
                    mk("b", "a", "src/b.ts"),
                    mk("b", "c", "src/b.ts"),
                    mk("c", "d", "src/c.ts"),
                    mk("d", "b", "src/d.ts"),
                ],
                hyperedges: vec![],
                built_at_commit: None,
            };
            KnowledgeGraph::from_graph_data(gd)
        };
        let cycles = find_import_cycles(&kg, 5, 20);
        let sets: Vec<BTreeSet<String>> = cycles
            .iter()
            .map(|c| c.cycle.iter().cloned().collect())
            .collect();
        assert!(sets.iter().any(|s| s.is_superset(
            &["src/a.ts".to_string(), "src/b.ts".to_string()]
                .into_iter()
                .collect()
        )));
        assert!(sets.iter().any(|s| s.is_superset(
            &[
                "src/b.ts".to_string(),
                "src/c.ts".to_string(),
                "src/d.ts".to_string()
            ]
            .into_iter()
            .collect()
        )));
        // max length respected
        let short = find_import_cycles(&kg, 2, 20);
        assert!(short.iter().all(|c| c.length <= 2));
    }

    #[test]
    fn graph_diff_reports_added_and_no_changes() {
        let g_old = build(&[n("n1", "Alpha", "a.py"), n("n2", "Beta", "b.py")], &[]);
        let g_new = build(
            &[
                n("n1", "Alpha", "a.py"),
                n("n2", "Beta", "b.py"),
                n("n3", "Gamma", "c.py"),
            ],
            &[],
        );
        let d = graph_diff(&g_old, &g_new);
        assert_eq!(d.new_nodes, vec![NodeId("n3".into())]);
        assert!(d.removed_nodes.is_empty());
        assert!(d.summary.contains("1 new node"));

        let same = graph_diff(&g_old, &g_old);
        assert_eq!(same.summary, "no changes");
    }

    #[test]
    fn suggest_questions_flags_ambiguous() {
        let kg = build(
            &[n("a", "Alpha", "a.py"), n("b", "Beta", "b.py")],
            &[e("a", "b", "calls", Confidence::Ambiguous)],
        );
        let comms: BTreeMap<u32, Vec<NodeId>> =
            [(0u32, vec![NodeId("a".into()), NodeId("b".into())])]
                .into_iter()
                .collect();
        let qs = suggest_questions(&kg, &comms, &BTreeMap::new(), 7);
        assert!(qs.iter().any(|q| q.kind == "ambiguous_edge"));
    }

    #[test]
    fn suggest_questions_flags_unresolved_command_sink() {
        use synaptic_core::{FileType, GraphData, Node};
        let mk = |id: &str, label: &str, sf: &str| Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: sf.into(),
            source_location: None,
            community: None,
            repo: None,
            extra: serde_json::Map::new(),
        };
        let caller = mk("c", "deploy()", "d.py");
        let mut stub = mk("cmd::mytool", "mytool", "");
        stub.extra
            .insert("_node_type".into(), serde_json::json!("command"));
        let kg = KnowledgeGraph::from_graph_data(GraphData {
            nodes: vec![caller, stub],
            links: vec![],
            ..Default::default()
        });
        let qs = suggest_questions(&kg, &BTreeMap::new(), &BTreeMap::new(), 7);
        assert!(
            qs.iter().any(|q| q.kind == "cross_language_sink"),
            "expected a cross_language_sink question"
        );
    }

    #[test]
    fn suggest_questions_flags_low_cohesion_single_pass() {
        // A 5-node community with a single internal edge: cohesion = 1/10 = 0.1
        // < 0.15 -> a `low_cohesion` question. Exercises the single-pass cohesion.
        let kg = build(
            &[
                n("c0", "C0", "src/c0.py"),
                n("c1", "C1", "src/c1.py"),
                n("c2", "C2", "src/c2.py"),
                n("c3", "C3", "src/c3.py"),
                n("c4", "C4", "src/c4.py"),
            ],
            &[e("c0", "c1", "references", Confidence::Extracted)],
        );
        let comms: BTreeMap<u32, Vec<NodeId>> = [(
            0u32,
            vec![
                NodeId("c0".into()),
                NodeId("c1".into()),
                NodeId("c2".into()),
                NodeId("c3".into()),
                NodeId("c4".into()),
            ],
        )]
        .into_iter()
        .collect();
        let qs = suggest_questions(&kg, &comms, &BTreeMap::new(), 7);
        assert!(qs.iter().any(|q| q.kind == "low_cohesion"));
    }

    #[test]
    fn suggest_questions_bridge_uses_betweenness() {
        // Two clusters joined only through `B`, so B is the betweenness bridge.
        let kg = build(
            &[
                n("a1", "A1", "a1.py"),
                n("a2", "A2", "a2.py"),
                n("a3", "A3", "a3.py"),
                n("B", "Bridge", "b.py"),
                n("c1", "C1", "c1.py"),
                n("c2", "C2", "c2.py"),
                n("c3", "C3", "c3.py"),
            ],
            &[
                e("a1", "a2", "calls", Confidence::Extracted),
                e("a2", "a3", "calls", Confidence::Extracted),
                e("a3", "B", "calls", Confidence::Extracted),
                e("B", "c1", "calls", Confidence::Extracted),
                e("c1", "c2", "calls", Confidence::Extracted),
                e("c2", "c3", "calls", Confidence::Extracted),
            ],
        );
        let comms: BTreeMap<u32, Vec<NodeId>> = [
            (
                0u32,
                vec![
                    NodeId("a1".into()),
                    NodeId("a2".into()),
                    NodeId("a3".into()),
                    NodeId("B".into()),
                ],
            ),
            (
                1u32,
                vec![
                    NodeId("c1".into()),
                    NodeId("c2".into()),
                    NodeId("c3".into()),
                ],
            ),
        ]
        .into_iter()
        .collect();
        let qs = suggest_questions(&kg, &comms, &BTreeMap::new(), 7);
        let bridge = qs
            .iter()
            .find(|q| q.kind == "bridge_node")
            .expect("a bridge_node question");
        assert!(
            bridge.why.contains("betweenness"),
            "why should cite betweenness: {}",
            bridge.why
        );
        assert!(
            bridge.question.as_ref().unwrap().contains("Bridge"),
            "question names the bridge node: {:?}",
            bridge.question
        );
    }

    #[test]
    fn surprises_fall_back_to_edge_betweenness_without_communities() {
        // Two triangles joined by a single bridge edge L3<->R1. With no community
        // info, the fallback ranks edges by betweenness, so the bridge is the top.
        let kg = build(
            &[
                n("L1", "L1", "l.py"),
                n("L2", "L2", "l.py"),
                n("L3", "L3", "l.py"),
                n("R1", "R1", "r.py"),
                n("R2", "R2", "r.py"),
                n("R3", "R3", "r.py"),
            ],
            &[
                e("L1", "L2", "calls", Confidence::Extracted),
                e("L2", "L3", "calls", Confidence::Extracted),
                e("L3", "L1", "calls", Confidence::Extracted),
                e("L3", "R1", "references", Confidence::Extracted), // bridge
                e("R1", "R2", "calls", Confidence::Extracted),
                e("R2", "R3", "calls", Confidence::Extracted),
                e("R3", "R1", "calls", Confidence::Extracted),
            ],
        );
        let empty: BTreeMap<u32, Vec<NodeId>> = BTreeMap::new();
        let s = cross_community_surprises(&kg, &empty, 5);
        assert!(
            !s.is_empty(),
            "fallback should surface bridges without communities"
        );
        assert!(
            s.iter().any(|x| x.why.contains("betweenness")),
            "why should cite betweenness"
        );
        let top = &s[0];
        assert!(
            (top.source == "L3" && top.target == "R1")
                || (top.source == "R1" && top.target == "L3"),
            "top surprise is the bridge edge, got {} -> {}",
            top.source,
            top.target
        );
    }

    #[test]
    fn suggest_questions_budget_short_circuits() {
        // 8 AMBIGUOUS edges: section 1 alone yields 8 questions; the budget
        // short-circuit returns the first `top_n` (7) and never reaches the
        // low-cohesion section (whose 5-node community would otherwise qualify).
        let nodes: Vec<N> = (0..9)
            .map(|i| match i {
                0 => n("a0", "A0", "src/a0.py"),
                1 => n("a1", "A1", "src/a1.py"),
                2 => n("a2", "A2", "src/a2.py"),
                3 => n("a3", "A3", "src/a3.py"),
                4 => n("a4", "A4", "src/a4.py"),
                5 => n("a5", "A5", "src/a5.py"),
                6 => n("a6", "A6", "src/a6.py"),
                7 => n("a7", "A7", "src/a7.py"),
                _ => n("a8", "A8", "src/a8.py"),
            })
            .collect();
        let edges: Vec<E> = (0..8)
            .map(|i| {
                let s: &'static str = ["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7"][i];
                let t: &'static str = ["a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8"][i];
                e(s, t, "calls", Confidence::Ambiguous)
            })
            .collect();
        let kg = build(&nodes, &edges);
        let comms: BTreeMap<u32, Vec<NodeId>> = [(
            0u32,
            (0..9).map(|i| NodeId(format!("a{i}"))).collect::<Vec<_>>(),
        )]
        .into_iter()
        .collect();
        let qs = suggest_questions(&kg, &comms, &BTreeMap::new(), 7);
        assert_eq!(qs.len(), 7);
        assert!(qs.iter().all(|q| q.kind == "ambiguous_edge"));
    }

    #[test]
    fn analyze_bundles_everything() {
        let kg = build(
            &[n("a", "Alpha", "src/a.py"), n("b", "Beta", "src/b.py")],
            &[e("a", "b", "references", Confidence::Inferred)],
        );
        let comms: BTreeMap<u32, Vec<NodeId>> = [
            (0u32, vec![NodeId("a".into())]),
            (1u32, vec![NodeId("b".into())]),
        ]
        .into_iter()
        .collect();
        let res = analyze(&kg, &comms, &BTreeMap::new());
        // a-b is a cross-file, cross-community INFERRED reference, so surprising.
        assert!(!res.surprising.is_empty());
    }
}
