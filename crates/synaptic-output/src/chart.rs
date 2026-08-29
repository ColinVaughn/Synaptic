//! High-level, self-contained architecture chart built from graph communities.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;
use synaptic_core::{Node, NodeId};
use synaptic_graph::{KnowledgeGraph, is_structural_edge, is_structural_node};

use crate::common::visual_kind;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChartData {
    node_count: usize,
    edge_count: usize,
    community_count: usize,
    built_at_commit: Option<String>,
    components: Vec<Component>,
    connections: Vec<Connection>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Component {
    id: u32,
    name: String,
    node_count: usize,
    internal_edges: usize,
    top_symbols: Vec<String>,
    kinds: Vec<String>,
    repos: Vec<String>,
    members: Vec<Member>,
    member_connections: Vec<MemberConnection>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Member {
    id: String,
    label: String,
    kind: String,
    source_file: String,
    degree: usize,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemberConnection {
    from: String,
    to: String,
    count: usize,
    relation: String,
    #[serde(skip)]
    score: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Connection {
    from: u32,
    to: u32,
    count: usize,
    relation: String,
    bidirectional: bool,
    cross_repo: bool,
    #[serde(skip)]
    score: usize,
}

#[derive(Default)]
struct CommunitySummary {
    node_count: usize,
    internal_edges: usize,
    boundary_edges: usize,
    names: BTreeMap<String, usize>,
    areas: BTreeMap<String, BTreeMap<String, usize>>,
    kinds: BTreeMap<String, usize>,
    repos: BTreeMap<String, usize>,
    symbols: BTreeMap<String, usize>,
}

#[derive(Default)]
struct ConnectionSummary {
    low_to_high: usize,
    high_to_low: usize,
    relations: BTreeMap<String, usize>,
    cross_repo: bool,
    score: usize,
}

#[derive(Default)]
struct MemberConnectionSummary {
    relations: BTreeMap<String, usize>,
    score: usize,
}

/// Render a bounded, interactive architecture chart from exact graph communities.
pub fn to_chart_html(kg: &KnowledgeGraph, max_communities: usize) -> String {
    let data = chart_data(kg, max_communities.max(1));
    let json = serde_json::to_string(&data)
        .expect("chart data serializes")
        .replace("</", "<\\/");
    CHART_HTML.replace("__CHART_DATA__", &json)
}

/// Write a self-contained `chart.html`.
pub fn to_chart(kg: &KnowledgeGraph, path: &Path, max_communities: usize) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, to_chart_html(kg, max_communities))
}

fn chart_data(kg: &KnowledgeGraph, max_communities: usize) -> ChartData {
    let mut node_community: HashMap<NodeId, u32> = HashMap::new();
    let mut degrees: HashMap<NodeId, usize> = HashMap::new();
    for node in kg.nodes().filter(|node| is_structural_node(node)) {
        if let Some(community) = node.community {
            node_community.insert(node.id.clone(), community);
            degrees.insert(node.id.clone(), 0);
        }
    }
    for edge in kg.edges().filter(|edge| is_structural_edge(edge)) {
        if degrees.contains_key(&edge.source) {
            *degrees.entry(edge.source.clone()).or_default() += 1;
        }
        if edge.target != edge.source && degrees.contains_key(&edge.target) {
            *degrees.entry(edge.target.clone()).or_default() += 1;
        }
    }

    let mut summaries: BTreeMap<u32, CommunitySummary> = BTreeMap::new();
    for node in kg.nodes().filter(|node| is_structural_node(node)) {
        let Some(&community) = node_community.get(&node.id) else {
            continue;
        };
        let summary = summaries.entry(community).or_default();
        summary.node_count += 1;
        if let Some((name, area)) = component_parts(node.source_file.as_str()) {
            *summary.names.entry(name.clone()).or_default() += 1;
            if let Some(area) = area {
                *summary
                    .areas
                    .entry(name)
                    .or_default()
                    .entry(area)
                    .or_default() += 1;
            }
        }
        *summary
            .kinds
            .entry(visual_kind(node).to_string())
            .or_default() += 1;
        if let Some(repo) = &node.repo {
            *summary.repos.entry(repo.clone()).or_default() += 1;
        }
        if !node.source_file.is_empty() && !node.label.starts_with('.') {
            let score = degrees.get(&node.id).copied().unwrap_or(0) * 10 + kind_bonus(node);
            summary
                .symbols
                .entry(node.label.clone())
                .and_modify(|current| *current = (*current).max(score))
                .or_insert(score);
        }
    }

    for edge in kg.edges().filter(|edge| is_structural_edge(edge)) {
        let (Some(&source), Some(&target)) = (
            node_community.get(&edge.source),
            node_community.get(&edge.target),
        ) else {
            continue;
        };
        if source == target {
            summaries.entry(source).or_default().internal_edges += 1;
        } else {
            summaries.entry(source).or_default().boundary_edges += 1;
            summaries.entry(target).or_default().boundary_edges += 1;
        }
    }

    let community_count = summaries.len();
    let mut selected: Vec<u32> = summaries.keys().copied().collect();
    selected.sort_by_key(|id| {
        let summary = &summaries[id];
        (
            Reverse(summary.node_count + summary.boundary_edges * 3),
            *id,
        )
    });
    selected.truncate(max_communities);
    let selected_set: HashSet<u32> = selected.iter().copied().collect();

    let mut raw_connections: BTreeMap<(u32, u32), ConnectionSummary> = BTreeMap::new();
    for edge in kg.edges().filter(|edge| is_structural_edge(edge)) {
        let (Some(&source), Some(&target)) = (
            node_community.get(&edge.source),
            node_community.get(&edge.target),
        ) else {
            continue;
        };
        if source == target || !selected_set.contains(&source) || !selected_set.contains(&target) {
            continue;
        }
        let key = if source < target {
            (source, target)
        } else {
            (target, source)
        };
        let connection = raw_connections.entry(key).or_default();
        if source == key.0 {
            connection.low_to_high += 1;
        } else {
            connection.high_to_low += 1;
        }
        *connection
            .relations
            .entry(edge.relation.to_string())
            .or_default() += 1;
        connection.cross_repo |= edge.cross_repo;
        connection.score += relation_priority(edge.relation.as_str());
    }
    let mut connections: Vec<Connection> = raw_connections
        .into_iter()
        .map(|((low, high), summary)| {
            let (from, to) = if summary.low_to_high >= summary.high_to_low {
                (low, high)
            } else {
                (high, low)
            };
            let relation = ranked_relation(&summary.relations);
            Connection {
                from,
                to,
                count: summary.relations.get(&relation).copied().unwrap_or(0),
                relation,
                bidirectional: summary.low_to_high > 0 && summary.high_to_low > 0,
                cross_repo: summary.cross_repo,
                score: summary.score,
            }
        })
        .collect();
    connections
        .sort_by_key(|connection| (Reverse(connection.score), connection.from, connection.to));
    connections.truncate(selected.len().max(1));

    let components: Vec<Component> = selected
        .into_iter()
        .map(|id| {
            let summary = &summaries[&id];
            let members = community_members(kg, id, &node_community, &degrees);
            let base = ranked(&summary.names, 1)
                .into_iter()
                .next()
                .unwrap_or_else(|| format!("Community {:02}", id + 1));
            let name = summary
                .areas
                .get(&base)
                .and_then(|areas| ranked(areas, 1).into_iter().next())
                .map_or_else(|| base.clone(), |area| format!("{base} / {area}"));
            Component {
                id,
                name,
                node_count: summary.node_count,
                internal_edges: summary.internal_edges,
                top_symbols: ranked(&summary.symbols, 4)
                    .into_iter()
                    .map(|symbol| truncate(&symbol, 36))
                    .collect(),
                kinds: ranked(&summary.kinds, 3),
                repos: ranked(&summary.repos, 2),
                member_connections: member_connections(kg, &members),
                members,
            }
        })
        .collect();

    ChartData {
        node_count: node_community.len(),
        edge_count: kg.edges().filter(|edge| is_structural_edge(edge)).count(),
        community_count,
        built_at_commit: kg.built_at_commit.clone(),
        components,
        connections,
    }
}

fn community_members(
    kg: &KnowledgeGraph,
    community: u32,
    node_community: &HashMap<NodeId, u32>,
    degrees: &HashMap<NodeId, usize>,
) -> Vec<Member> {
    let nodes: Vec<&Node> = kg
        .nodes()
        .filter(|node| {
            node_community.get(&node.id) == Some(&community)
                && !node.source_file.is_empty()
                && !node.label.starts_with('.')
        })
        .collect();
    let candidate_ids: HashSet<&NodeId> = nodes.iter().map(|node| &node.id).collect();
    let mut internal_degrees: HashMap<&NodeId, usize> = HashMap::new();
    let mut adjacency: HashMap<&NodeId, HashSet<&NodeId>> = HashMap::new();
    for edge in kg.edges().filter(|edge| is_structural_edge(edge)) {
        if edge.source == edge.target
            || !candidate_ids.contains(&edge.source)
            || !candidate_ids.contains(&edge.target)
        {
            continue;
        }
        *internal_degrees.entry(&edge.source).or_default() += 1;
        *internal_degrees.entry(&edge.target).or_default() += 1;
        adjacency
            .entry(&edge.source)
            .or_default()
            .insert(&edge.target);
        adjacency
            .entry(&edge.target)
            .or_default()
            .insert(&edge.source);
    }
    let mut selected = Vec::new();
    let mut selected_ids = HashSet::new();
    while selected.len() < 20 && selected.len() < nodes.len() {
        let Some(node) = nodes
            .iter()
            .filter(|node| !selected_ids.contains(&node.id))
            .min_by_key(|node| {
                let linked = adjacency
                    .get(&node.id)
                    .map_or(0, |neighbors| neighbors.intersection(&selected_ids).count());
                (
                    Reverse(linked),
                    Reverse(internal_degrees.get(&node.id).copied().unwrap_or(0)),
                    Reverse(degrees.get(&node.id).copied().unwrap_or(0)),
                    Reverse(kind_bonus(node)),
                    node.label.as_str(),
                    node.id.0.as_str(),
                )
            })
        else {
            break;
        };
        selected_ids.insert(&node.id);
        selected.push(*node);
    }
    selected
        .into_iter()
        .map(|node| Member {
            id: node.id.0.clone(),
            label: node.label.clone(),
            kind: visual_kind(node).to_string(),
            source_file: node.source_file.to_string(),
            degree: degrees.get(&node.id).copied().unwrap_or(0),
        })
        .collect()
}

fn member_connections(kg: &KnowledgeGraph, members: &[Member]) -> Vec<MemberConnection> {
    let member_ids: HashSet<&str> = members.iter().map(|member| member.id.as_str()).collect();
    let mut summaries: BTreeMap<(String, String), MemberConnectionSummary> = BTreeMap::new();
    for edge in kg.edges().filter(|edge| is_structural_edge(edge)) {
        if edge.source == edge.target
            || !member_ids.contains(edge.source.0.as_str())
            || !member_ids.contains(edge.target.0.as_str())
        {
            continue;
        }
        let summary = summaries
            .entry((edge.source.0.clone(), edge.target.0.clone()))
            .or_default();
        *summary
            .relations
            .entry(edge.relation.to_string())
            .or_default() += 1;
        summary.score += relation_priority(edge.relation.as_str());
    }
    let mut connections: Vec<MemberConnection> = summaries
        .into_iter()
        .map(|((from, to), summary)| {
            let relation = ranked_relation(&summary.relations);
            MemberConnection {
                from,
                to,
                count: summary.relations.get(&relation).copied().unwrap_or(0),
                relation,
                score: summary.score,
            }
        })
        .collect();
    connections.sort_by_key(|connection| {
        (
            Reverse(connection.score),
            connection.from.clone(),
            connection.to.clone(),
        )
    });
    let mut picked = Vec::new();
    let mut picked_keys = HashSet::new();
    let mut seen: HashSet<&str> = members
        .first()
        .map(|member| HashSet::from([member.id.as_str()]))
        .unwrap_or_default();
    for member in members.iter().skip(1) {
        if let Some(connection) = connections.iter().find(|connection| {
            (connection.from == member.id && seen.contains(connection.to.as_str()))
                || (connection.to == member.id && seen.contains(connection.from.as_str()))
        }) {
            picked_keys.insert((connection.from.clone(), connection.to.clone()));
            picked.push(connection.clone());
        }
        seen.insert(member.id.as_str());
    }
    for connection in connections {
        if picked.len() >= members.len() {
            break;
        }
        if picked_keys.insert((connection.from.clone(), connection.to.clone())) {
            picked.push(connection);
        }
    }
    picked
}

fn ranked(values: &BTreeMap<String, usize>, limit: usize) -> Vec<String> {
    let mut values: Vec<(&String, &usize)> = values.iter().collect();
    values.sort_by_key(|(label, count)| (Reverse(**count), label.as_str()));
    values
        .into_iter()
        .take(limit)
        .map(|(label, _)| label.clone())
        .collect()
}

fn ranked_relation(relations: &BTreeMap<String, usize>) -> String {
    relations
        .iter()
        .max_by_key(|(relation, count)| {
            (
                **count * relation_priority(relation),
                **count,
                Reverse(relation.as_str()),
            )
        })
        .map(|(relation, _)| relation.clone())
        .unwrap_or_else(|| "connects".to_string())
}

fn relation_priority(relation: &str) -> usize {
    match relation {
        "queries" | "writes_to" | "calls_proc" | "uses_api" => 7,
        "calls" => 6,
        "imports" | "imports_from" | "depends_on" => 5,
        "extends" | "implements" | "inherits" | "mixes_in" => 4,
        "references" | "contains" => 1,
        _ => 3,
    }
}

fn kind_bonus(node: &Node) -> usize {
    match visual_kind(node) {
        "module" | "namespace" | "package" => 8,
        "class" | "struct" | "interface" | "trait" | "enum" => 6,
        "table" | "view" | "procedure" => 5,
        _ => 2,
    }
}

fn component_parts(source_file: &str) -> Option<(String, Option<String>)> {
    let parts: Vec<&str> = source_file
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    let first = *parts.first()?;
    let containers = [
        "apps", "bin", "crates", "libs", "modules", "packages", "services",
    ];
    if containers.contains(&first) && parts.len() > 1 {
        let name = parts[1].to_string();
        let area = if let Some(src) = parts.iter().position(|part| *part == "src")
            && let Some(next) = parts.get(src + 1)
        {
            if looks_like_file(next) {
                let stem = file_stem(next);
                (!matches!(stem, "lib" | "main" | "mod")).then(|| stem.to_string())
            } else {
                Some((*next).to_string())
            }
        } else {
            None
        };
        return Some((name, area));
    }
    if matches!(first, "app" | "lib" | "src") {
        return parts
            .get(1)
            .map(|part| (file_stem(part).to_string(), None))
            .or_else(|| Some((first.to_string(), None)));
    }
    Some((
        if looks_like_file(first) {
            file_stem(first).to_string()
        } else {
            first.to_string()
        },
        None,
    ))
}

fn looks_like_file(part: &str) -> bool {
    part.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
}

fn file_stem(part: &str) -> &str {
    part.rsplit_once('.').map_or(part, |(stem, _)| stem)
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

const CHART_HTML: &str = r##"<!DOCTYPE html>
<html lang="en" data-theme="light">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Synaptic — Architecture Chart</title>
<style>
:root{--paper:#f3f0e9;--panel:#fbfaf6;--ink:#17171b;--muted:#6b6872;--rule:#c7c1b8;--grid:rgba(35,30,42,.065);--violet:#6639ba;--cyan:#087f8c;--shadow:rgba(31,25,40,.12);color-scheme:light}
html[data-theme="dark"]{--paper:#111116;--panel:#19191f;--ink:#f2eee7;--muted:#aaa4b2;--rule:#3c3943;--grid:rgba(237,230,246,.055);--violet:#aa84f4;--cyan:#63d3df;--shadow:rgba(0,0,0,.4);color-scheme:dark}
*{box-sizing:border-box}body{margin:0;min-width:320px;background-color:var(--paper);background-image:linear-gradient(var(--grid) 1px,transparent 1px),linear-gradient(90deg,var(--grid) 1px,transparent 1px);background-size:24px 24px;color:var(--ink);font-family:Inter,ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}
.skip{position:absolute;left:-9999px;top:8px;z-index:9;background:var(--ink);color:var(--paper);padding:8px 12px}.skip:focus{left:8px}.shell{width:min(1560px,calc(100% - 32px));margin:0 auto;padding:24px 0 32px}.mast{display:grid;grid-template-columns:minmax(260px,1fr) auto;gap:24px;align-items:end;border-bottom:1px solid var(--rule);padding:8px 2px 18px}.brand{font:700 12px/1 ui-monospace,"Cascadia Code",Consolas,monospace;letter-spacing:.18em;text-transform:uppercase;color:var(--violet)}h1{margin:10px 0 0;font-size:clamp(28px,4vw,54px);line-height:.96;letter-spacing:-.045em;font-weight:760}.metrics{display:flex;gap:18px;align-items:flex-end}.metric{min-width:88px;border-left:1px solid var(--rule);padding-left:12px}.metric strong{display:block;font:650 19px/1 ui-monospace,"Cascadia Code",Consolas,monospace}.metric span{display:block;margin-top:7px;color:var(--muted);font:600 9px/1 ui-monospace,"Cascadia Code",Consolas,monospace;letter-spacing:.12em;text-transform:uppercase}.toolbar{display:flex;gap:8px;align-items:center;padding:12px 0}.search{position:relative;flex:1;max-width:480px}.search input,.button{height:42px;border:1px solid var(--rule);background:var(--panel);color:var(--ink);font:600 12px/1 ui-monospace,"Cascadia Code",Consolas,monospace}.search input{width:100%;padding:0 14px 0 38px;outline:none}.search::before{content:"/";position:absolute;left:14px;top:12px;color:var(--violet);font:700 14px/1 ui-monospace,monospace}.search input:focus,.button:focus-visible{border-color:var(--violet);outline:2px solid color-mix(in srgb,var(--violet) 28%,transparent);outline-offset:1px}.button{display:inline-flex;align-items:center;padding:0 14px;cursor:pointer;text-decoration:none;transition:transform .18s ease,border-color .18s ease}.button[hidden]{display:none}.button:hover{border-color:var(--violet);transform:translateY(-1px)}.status{margin-left:auto;color:var(--muted);font:600 10px/1.4 ui-monospace,"Cascadia Code",Consolas,monospace;text-align:right}.frame{position:relative;background:color-mix(in srgb,var(--panel) 94%,transparent);border:1px solid var(--rule);box-shadow:0 18px 54px var(--shadow);overflow:auto}.frame::before,.frame::after{content:"";position:absolute;z-index:2;width:28px;height:28px;pointer-events:none}.frame::before{left:12px;top:12px;border-left:1px solid var(--cyan);border-top:1px solid var(--cyan)}.frame::after{right:12px;bottom:12px;border-right:1px solid var(--violet);border-bottom:1px solid var(--violet)}.chart-svg{display:block;width:100%;min-width:900px;height:auto}.chart-svg .canvas{fill:var(--panel)}.connection{fill:none;stroke:var(--muted);stroke-opacity:.28;transition:stroke-opacity .2s ease,stroke-width .2s ease}.connection.cross{stroke:var(--cyan)}.connection.active{stroke:var(--violet);stroke-opacity:.9!important}.connection.muted{stroke-opacity:.055}.edge-label{pointer-events:none;transition:opacity .2s ease}.edge-label.secondary{opacity:0}.edge-label.active{opacity:1}.edge-label.muted{opacity:0}.edge-label rect{fill:var(--panel);stroke:var(--rule)}.edge-label text{fill:var(--muted);font:600 9px ui-monospace,"Cascadia Code",Consolas,monospace}.component{cursor:pointer;outline:none;animation:arrive .45s both;animation-delay:calc(var(--i) * 38ms)}.component .plate{fill:var(--panel);stroke:var(--rule);stroke-width:1.2;transition:stroke .2s ease,filter .2s ease,opacity .2s ease}.component .rail{stroke:var(--violet);stroke-width:3}.component .index,.component .meta,.component .kind{fill:var(--muted);font-family:ui-monospace,"Cascadia Code",Consolas,monospace}.component .index{font-size:9px;letter-spacing:.11em}.component .name{fill:var(--ink);font:700 15px ui-sans-serif,system-ui,sans-serif;letter-spacing:-.02em}.component .meta{font-size:9px}.component .symbol{fill:var(--ink);font:500 10px ui-monospace,"Cascadia Code",Consolas,monospace}.component .kind{font-size:8px}.component:hover .plate,.component:focus-visible .plate,.component.active .plate{stroke:var(--violet);stroke-width:2;filter:drop-shadow(0 8px 12px var(--shadow))}.component.neighbor .plate{stroke:var(--cyan)}.component.muted{opacity:.22}.details{display:grid;grid-template-columns:minmax(220px,.7fr) 1.3fr auto;gap:24px;align-items:start;margin-top:12px;padding:18px 20px;border:1px solid var(--rule);background:var(--panel)}.details h2{margin:0;font-size:18px;letter-spacing:-.02em}.details p{margin:6px 0 0;color:var(--muted);font:500 11px/1.55 ui-monospace,"Cascadia Code",Consolas,monospace}.detail-symbols{display:flex;flex-wrap:wrap;gap:7px}.detail-symbols code{border-bottom:1px solid var(--rule);padding:3px 1px;color:var(--ink);font-size:11px}.commit{color:var(--muted);font:600 9px/1.5 ui-monospace,"Cascadia Code",Consolas,monospace;text-align:right}.empty{padding:80px 24px;text-align:center;color:var(--muted)}@keyframes arrive{from{opacity:0}to{opacity:1}}@media(max-width:760px){.shell{width:calc(100% - 20px);padding-top:10px}.mast{grid-template-columns:1fr}.metrics{display:grid;grid-template-columns:repeat(3,1fr)}.metric{min-width:0}.toolbar{flex-wrap:wrap}.search{order:1;max-width:none;width:100%;flex-basis:100%}.status{display:none}.details{grid-template-columns:1fr}.commit{text-align:left}}@media(prefers-reduced-motion:reduce){*,*::before,*::after{animation:none!important;transition:none!important;scroll-behavior:auto!important}}
.member{cursor:pointer;outline:none;animation:arrive .4s both;animation-delay:calc(var(--i) * 24ms)}.member .plate{fill:var(--panel);stroke:var(--rule);stroke-width:1.2;transition:stroke .2s ease,filter .2s ease}.member .rail{stroke:var(--cyan);stroke-width:3}.member .name,.view-title{fill:var(--ink);font:700 14px ui-sans-serif,system-ui,sans-serif;letter-spacing:-.02em}.member .meta,.member .kind,.member .source,.view-kicker{fill:var(--muted);font-family:ui-monospace,"Cascadia Code",Consolas,monospace}.member .meta{font-size:9px}.member .kind,.member .source{font-size:8px}.view-title{font-size:20px}.view-kicker{font-size:9px;letter-spacing:.11em}.member:hover .plate,.member:focus-visible .plate,.member.active .plate{stroke:var(--violet);stroke-width:2;filter:drop-shadow(0 8px 12px var(--shadow))}.member.neighbor .plate{stroke:var(--cyan)}.member.muted{opacity:.2}
</style>
<style>
:root{--kind-module:#266f9b;--kind-structure:#6639ba;--kind-callable:#087f8c;--kind-data:#a65b0b;--kind-file:#7a6750;--kind-symbol:#6b6872}
html[data-theme="dark"]{--kind-module:#67b7e5;--kind-structure:#aa84f4;--kind-callable:#63d3df;--kind-data:#f2aa58;--kind-file:#c9ae8e;--kind-symbol:#aaa4b2}
.shell{padding:12px 0 24px}.mast{padding:4px 2px 12px;align-items:center}.brand{font-size:10px}h1{margin-top:6px;font-size:clamp(26px,3vw,38px);line-height:1}.metric strong{font-size:16px}.metric span{margin-top:5px}.toolbar{padding:8px 0}.search input,.button{height:38px}.search::before{top:10px}
.chart-stage{position:relative}.connection{stroke-linecap:round;stroke-linejoin:round}.connection.weak{stroke-opacity:.11;stroke-dasharray:3 7}.edge-label.weak{opacity:0}.component,.member{animation:none}.component,.member{--kind-color:var(--kind-symbol)}.kind-module{--kind-color:var(--kind-module)}.kind-structure{--kind-color:var(--kind-structure)}.kind-callable{--kind-color:var(--kind-callable)}.kind-data{--kind-color:var(--kind-data)}.kind-file{--kind-color:var(--kind-file)}.component .rail,.member .rail{stroke:var(--kind-color)}.component .kind,.member .kind{fill:var(--kind-color);font-weight:700}.component .plate,.member .plate{transition:stroke .14s ease,filter .14s ease,opacity .14s ease}.component.muted,.member.muted{transition:opacity .14s ease}
.legend{display:flex;flex-wrap:wrap;gap:6px 14px;align-items:center;min-height:24px;padding:0 2px 7px;color:var(--muted);font:600 9px/1 ui-monospace,"Cascadia Code",Consolas,monospace;text-transform:uppercase;letter-spacing:.08em}.legend span{display:inline-flex;align-items:center;gap:6px}.legend i{width:8px;height:8px;background:var(--kind-color);border-radius:1px}
.details{position:absolute;z-index:4;top:14px;right:14px;width:min(370px,calc(100% - 28px));max-height:calc(100% - 28px);overflow:auto;display:block;margin:0;padding:16px 18px;border-color:color-mix(in srgb,var(--violet) 48%,var(--rule));box-shadow:0 18px 54px var(--shadow)}.details[hidden]{display:none}.detail-head{display:flex;align-items:start;justify-content:space-between;gap:12px}.detail-kicker{margin:0 0 7px;color:var(--violet);font:700 9px/1 ui-monospace,"Cascadia Code",Consolas,monospace;letter-spacing:.12em;text-transform:uppercase}.detail-close{width:32px;height:32px;flex:0 0 32px;border:1px solid var(--rule);background:transparent;color:var(--muted);cursor:pointer;font-size:18px;line-height:1}.detail-close:hover,.detail-close:focus-visible{border-color:var(--violet);color:var(--ink);outline:2px solid color-mix(in srgb,var(--violet) 24%,transparent);outline-offset:1px}.detail-symbols{display:grid;gap:6px;margin-top:14px}.detail-symbols code,.detail-link{display:block;width:100%;border:0;border-left:2px solid var(--rule);border-bottom:0;padding:7px 9px;background:color-mix(in srgb,var(--paper) 66%,transparent);color:var(--ink);font:600 10px/1.35 ui-monospace,"Cascadia Code",Consolas,monospace;text-align:left}.detail-link{cursor:pointer}.detail-link:hover,.detail-link:focus-visible{border-left-color:var(--cyan);background:color-mix(in srgb,var(--cyan) 9%,var(--panel));outline:none}.commit{margin-top:13px;text-align:left}
.chart-stage{display:grid;grid-template-columns:minmax(0,1fr) auto;align-items:start}.frame{min-width:0}.details{position:sticky;top:14px;right:auto;width:360px;max-height:calc(100vh - 28px);margin-left:10px}.edge-label.weak.active{opacity:1}
@media(max-width:1100px){.chart-stage{grid-template-columns:1fr}.details{position:static;grid-row:1;width:100%;max-height:none;margin:0 0 10px}.frame{grid-row:2}.status{display:none}}
@media(max-width:760px){.shell{padding-top:8px}.mast{gap:12px}.legend{overflow-x:auto;flex-wrap:nowrap}.metric strong{font-size:14px}}
</style>
</head>
<body>
<a class="skip" href="#chart">Skip to chart</a>
<main class="shell">
  <header class="mast">
    <div><div class="brand">Synaptic / Signal Atlas</div><h1>Repository architecture</h1></div>
    <div class="metrics" aria-label="Graph summary">
      <div class="metric"><strong id="node-count">—</strong><span>structural nodes</span></div>
      <div class="metric"><strong id="edge-count">—</strong><span>exact edges</span></div>
      <div class="metric"><strong id="community-count">—</strong><span>communities</span></div>
    </div>
  </header>
  <section class="toolbar" aria-label="Chart controls">
    <label class="search"><span hidden>Find a component or symbol</span><input id="search" type="search" aria-label="Find a component or symbol" placeholder="Find component or symbol" autocomplete="off"></label>
    <button class="button" id="overview" type="button" hidden>← Overview</button>
    <button class="button" id="theme" type="button" aria-pressed="false">Dark</button>
    <a class="button" id="download" href="#" download="synaptic-architecture.svg">Download SVG</a>
    <div class="status" id="status" aria-live="polite"></div>
  </section>
  <div class="legend" aria-label="Node type legend">
    <span class="kind-module"><i></i>module</span><span class="kind-structure"><i></i>type</span><span class="kind-callable"><i></i>callable</span><span class="kind-data"><i></i>data</span><span class="kind-file"><i></i>file</span>
  </div>
  <div class="chart-stage">
    <section class="frame" id="chart" aria-label="Architecture chart"></section>
    <aside class="details" id="details" aria-live="polite" hidden>
      <div class="detail-head"><div><div class="detail-kicker" id="detail-kicker">Subsystem</div><h2 id="detail-name"></h2><p id="detail-meta"></p></div><button class="detail-close" id="detail-close" type="button" aria-label="Close inspector">×</button></div>
      <div class="detail-symbols" id="detail-symbols"></div>
      <div class="commit" id="commit"></div>
    </aside>
  </div>
</main>
<script type="application/json" id="chart-data">__CHART_DATA__</script>
<script>
const data=JSON.parse(document.getElementById('chart-data').textContent);
const ns='http://www.w3.org/2000/svg',frame=document.getElementById('chart'),overview=document.getElementById('overview'),details=document.getElementById('details'),detailKicker=document.getElementById('detail-kicker');
let currentCommunity=null;
const svgEl=(name,attrs={})=>{const node=document.createElementNS(ns,name);for(const [key,value] of Object.entries(attrs))node.setAttribute(key,String(value));return node};
const text=(parent,value,attrs={})=>{const node=svgEl('text',attrs);node.textContent=value;parent.append(node);return node};
const fmt=value=>new Intl.NumberFormat().format(value);
const weakRelation=relation=>relation==='references'||relation==='contains';
function kindGroup(value){const kind=(value||'').toLowerCase();if(['module','namespace','package'].includes(kind))return'module';if(['class','struct','interface','trait','enum'].includes(kind))return'structure';if(['function','method','constructor','closure'].includes(kind))return'callable';if(['table','view','procedure','query'].includes(kind))return'data';if(['file','document'].includes(kind))return'file';return'symbol'}
function componentKind(component){const groups=component.kinds.map(kindGroup);return['data','structure','module','file','callable'].find(kind=>groups.includes(kind))||'symbol'}
document.getElementById('node-count').textContent=fmt(data.nodeCount);
document.getElementById('edge-count').textContent=fmt(data.edgeCount);
document.getElementById('community-count').textContent=fmt(data.communityCount);
document.getElementById('status').textContent=`Showing ${data.components.length} communities · ${data.connections.length} strongest flows`;
document.getElementById('commit').textContent=data.builtAtCommit?`GRAPH REV ${data.builtAtCommit.slice(0,12)}`:'GRAPH REV local';
if(!data.components.length){frame.innerHTML='<div class="empty">No structural communities found.</div>'}else render();
function render(){
  currentCommunity=null;overview.hidden=true;document.getElementById('status').textContent=`Showing ${data.components.length} communities · ${data.connections.length} strongest flows`;showDetails(null);
  const cols=data.components.length<=6?3:4,rows=Math.ceil(data.components.length/cols);
  const cardW=260,cardH=150,gapX=84,gapY=96,padX=66,padY=74;
  const width=padX*2+cols*cardW+(cols-1)*gapX,height=padY*2+rows*cardH+(rows-1)*gapY;
  const svg=svgEl('svg',{class:'chart-svg',viewBox:`0 0 ${width} ${height}`,role:'img','aria-labelledby':'svg-title svg-desc'});
  text(svg,'Repository architecture',{id:'svg-title'});text(svg,`${data.components.length} graph communities and ${data.connections.length} strongest inter-community flows`,{id:'svg-desc'});
  const defs=svgEl('defs'),marker=svgEl('marker',{id:'arrow',viewBox:'0 0 8 8',refX:7,refY:4,markerWidth:7,markerHeight:7,markerUnits:'userSpaceOnUse',orient:'auto-start-reverse'});
  marker.append(svgEl('path',{d:'M0 0L8 4L0 8Z',fill:'context-stroke'}));defs.append(marker);svg.append(defs,svgEl('rect',{class:'canvas',width,height}));
  const positions=new Map();
  data.components.forEach((component,index)=>{const row=Math.floor(index/cols),slot=index%cols,col=row%2?cols-1-slot:slot;positions.set(component.id,{x:padX+col*(cardW+gapX),y:padY+row*(cardH+gapY)})});
  const edgeLayer=svgEl('g',{class:'edges'}),labelLayer=svgEl('g',{class:'edge-labels'}),cardLayer=svgEl('g',{class:'components'});svg.append(edgeLayer,labelLayer,cardLayer);
  data.connections.forEach((connection,index)=>{
    const a=positions.get(connection.from),b=positions.get(connection.to);if(!a||!b)return;
    const route=routeOrthogonal(a,b,cardW,cardH,index),weak=weakRelation(connection.relation);
    const path=svgEl('path',{d:route.d,class:`connection${connection.crossRepo?' cross':''}${weak?' weak':''}`,'data-from':connection.from,'data-to':connection.to,'data-index':index,'marker-end':'url(#arrow)','stroke-width':Math.min(4,1.1+Math.log2(connection.count+1)*.48)});
    if(connection.bidirectional)path.setAttribute('marker-start','url(#arrow)');edgeLayer.append(path);
    const label=`${connection.relation.replaceAll('_',' ')} ×${connection.count}`,w=Math.max(66,label.length*6.1+12);
    const group=svgEl('g',{class:`edge-label${weak?' weak':''}${index>=Math.min(8,data.components.length)?' secondary':''}`,'data-from':connection.from,'data-to':connection.to});
    group.append(svgEl('rect',{x:route.mid.x-w/2,y:route.mid.y-9,width:w,height:18,rx:2}));text(group,label,{x:route.mid.x,y:route.mid.y+3,'text-anchor':'middle'});labelLayer.append(group);
  });
  data.components.forEach((component,index)=>{
    const pos=positions.get(component.id),group=svgEl('g',{class:`component kind-${componentKind(component)}`,transform:`translate(${pos.x} ${pos.y})`,tabindex:0,role:'button','data-id':component.id,'aria-label':`${component.name}, ${component.nodeCount} nodes`});
    group.append(svgEl('path',{class:'plate',d:`M0 0H${cardW-18}L${cardW} 18V${cardH}H0Z`}));group.append(svgEl('line',{class:'rail',x1:0,y1:0,x2:0,y2:cardH}));
    text(group,`C${String(component.id+1).padStart(2,'0')}`,{class:'index',x:18,y:24});text(group,component.name,{class:'name',x:18,y:50});
    text(group,`${fmt(component.nodeCount)} nodes / ${fmt(component.internalEdges)} internal edges`,{class:'meta',x:18,y:69});
    component.topSymbols.slice(0,3).forEach((symbol,i)=>text(group,`${i===0?'→':'·'} ${symbol}`,{class:'symbol',x:18,y:94+i*17}));
    text(group,component.kinds.join(' · '),{class:'kind',x:cardW-14,y:cardH-12,'text-anchor':'end'});
    group.addEventListener('click',()=>focus(component.id));group.addEventListener('keydown',event=>{if(event.key==='Enter'||event.key===' '){event.preventDefault();focus(component.id)}});cardLayer.append(group);
  });
  frame.replaceChildren(svg);window.chartSvg=svg;
}
function renderDrilldown(id){
  const component=data.components.find(item=>item.id===id);if(!component)return;
  currentCommunity=id;overview.hidden=false;showDetails(component);
  document.getElementById('status').textContent=`Inside ${component.name} · ${component.members.length} of ${fmt(component.nodeCount)} nodes · ${component.memberConnections.length} high-signal edges`;
  if(!component.members.length){frame.innerHTML='<div class="empty">No structural members found in this community.</div>';return}
  const cols=component.members.length<=6?3:4,rows=Math.ceil(component.members.length/cols);
  const cardW=236,cardH=108,gapX=76,gapY=78,padX=66,padTop=116,padBottom=66;
  const width=padX*2+cols*cardW+(cols-1)*gapX,height=padTop+padBottom+rows*cardH+(rows-1)*gapY;
  const svg=svgEl('svg',{class:'chart-svg',viewBox:`0 0 ${width} ${height}`,role:'img','aria-labelledby':'svg-title svg-desc'});
  const title=svgEl('title',{id:'svg-title'});title.textContent=`${component.name} internal architecture`;const desc=svgEl('desc',{id:'svg-desc'});desc.textContent=`${component.members.length} ranked symbols and ${component.memberConnections.length} internal graph connections`;
  const defs=svgEl('defs'),marker=svgEl('marker',{id:'arrow',viewBox:'0 0 8 8',refX:7,refY:4,markerWidth:7,markerHeight:7,markerUnits:'userSpaceOnUse',orient:'auto-start-reverse'});marker.append(svgEl('path',{d:'M0 0L8 4L0 8Z',fill:'context-stroke'}));defs.append(marker);svg.append(title,desc,defs,svgEl('rect',{class:'canvas',width,height}));
  text(svg,'SUBSYSTEM / INTERNAL GRAPH',{class:'view-kicker',x:padX,y:38});text(svg,component.name,{class:'view-title',x:padX,y:68});text(svg,`${component.members.length} of ${fmt(component.nodeCount)} ranked nodes`,{class:'view-kicker',x:width-padX,y:62,'text-anchor':'end'});
  const positions=new Map();component.members.forEach((member,index)=>{const row=Math.floor(index/cols),slot=index%cols,col=row%2?cols-1-slot:slot;positions.set(member.id,{x:padX+col*(cardW+gapX),y:padTop+row*(cardH+gapY)})});
  const edgeLayer=svgEl('g',{class:'edges'}),labelLayer=svgEl('g',{class:'edge-labels'}),cardLayer=svgEl('g',{class:'members'});svg.append(edgeLayer,labelLayer,cardLayer);
  component.memberConnections.forEach((connection,index)=>{
    const a=positions.get(connection.from),b=positions.get(connection.to);if(!a||!b)return;const route=routeOrthogonal(a,b,cardW,cardH,index),weak=weakRelation(connection.relation);
    edgeLayer.append(svgEl('path',{d:route.d,class:`connection internal-connection${weak?' weak':''}`,'data-member-from':connection.from,'data-member-to':connection.to,'marker-end':'url(#arrow)','stroke-width':Math.min(3.6,1+Math.log2(connection.count+1)*.45)}));const label=`${connection.relation.replaceAll('_',' ')} ×${connection.count}`,w=Math.max(66,label.length*6.1+12),group=svgEl('g',{class:`edge-label internal-label${weak?' weak':''}${index>=10?' secondary':''}`,'data-member-from':connection.from,'data-member-to':connection.to});group.append(svgEl('rect',{x:route.mid.x-w/2,y:route.mid.y-9,width:w,height:18,rx:2}));text(group,label,{x:route.mid.x,y:route.mid.y+3,'text-anchor':'middle'});labelLayer.append(group);
  });
  component.members.forEach((member,index)=>{const pos=positions.get(member.id),group=svgEl('g',{class:`member kind-${kindGroup(member.kind)}`,transform:`translate(${pos.x} ${pos.y})`,tabindex:0,role:'button','data-member-id':member.id,'aria-label':`${member.label}, ${member.kind}, ${member.degree} connections`});group.append(svgEl('path',{class:'plate',d:`M0 0H${cardW-16}L${cardW} 16V${cardH}H0Z`}));group.append(svgEl('line',{class:'rail',x1:0,y1:0,x2:0,y2:cardH}));text(group,clip(member.label,28),{class:'name',x:16,y:30});text(group,`${member.kind} · degree ${member.degree}`,{class:'meta',x:16,y:51});text(group,shortPath(member.sourceFile),{class:'source',x:16,y:76});text(group,`N${String(index+1).padStart(2,'0')}`,{class:'kind',x:cardW-12,y:cardH-10,'text-anchor':'end'});group.addEventListener('click',()=>focusMember(component,member.id));group.addEventListener('keydown',event=>{if(event.key==='Enter'||event.key===' '){event.preventDefault();focusMember(component,member.id)}});cardLayer.append(group)});
  frame.replaceChildren(svg);window.chartSvg=svg;
}
function clip(value,limit){return[...value].length<=limit?value:[...value].slice(0,limit-1).join('')+'…'}
function shortPath(value){if(!value)return'generated / no source path';const parts=value.replaceAll('\\','/').split('/');return clip(parts.slice(-2).join('/'),42)}
function routeOrthogonal(a,b,width,height,index){const from={x:a.x+width/2,y:a.y+height/2},to={x:b.x+width/2,y:b.y+height/2},horizontal=Math.abs(to.x-from.x)>=Math.abs(to.y-from.y),lane=(index%5-2)*7;if(horizontal){const sign=Math.sign(to.x-from.x)||1,start={x:from.x+sign*width/2,y:from.y+lane},end={x:to.x-sign*width/2,y:to.y-lane},bend=(start.x+end.x)/2+lane;return{d:`M${start.x} ${start.y}H${bend}V${end.y}H${end.x}`,mid:{x:bend,y:(start.y+end.y)/2}}}const sign=Math.sign(to.y-from.y)||1,start={x:from.x+lane,y:from.y+sign*height/2},end={x:to.x-lane,y:to.y-sign*height/2},bend=(start.y+end.y)/2+lane;return{d:`M${start.x} ${start.y}V${bend}H${end.x}V${end.y}`,mid:{x:(start.x+end.x)/2,y:bend}}}
function focus(id){renderDrilldown(id)}
function focusMember(component,id){
  const cards=[...document.querySelectorAll('.member')],edges=[...document.querySelectorAll('.internal-connection')],labels=[...document.querySelectorAll('.internal-label')],active=cards.find(card=>card.dataset.memberId===id),same=active?.classList.contains('active');
  cards.forEach(card=>card.classList.remove('active','neighbor','muted'));edges.forEach(edge=>edge.classList.remove('active','muted'));labels.forEach(label=>label.classList.remove('active','muted'));if(!active||same){showDetails(component);return}
  const neighbors=new Set([id]);edges.forEach(edge=>{const linked=edge.dataset.memberFrom===id||edge.dataset.memberTo===id;edge.classList.toggle('active',linked);edge.classList.toggle('muted',!linked);if(linked){neighbors.add(edge.dataset.memberFrom);neighbors.add(edge.dataset.memberTo)}});labels.forEach(label=>{const linked=label.dataset.memberFrom===id||label.dataset.memberTo===id;label.classList.toggle('active',linked);label.classList.toggle('muted',!linked)});cards.forEach(card=>{const memberId=card.dataset.memberId;card.classList.toggle('active',memberId===id);card.classList.toggle('neighbor',memberId!==id&&neighbors.has(memberId));card.classList.toggle('muted',!neighbors.has(memberId))});showMemberDetails(component,component.members.find(member=>member.id===id));
}
function showDetails(component){
  const name=document.getElementById('detail-name'),meta=document.getElementById('detail-meta'),symbols=document.getElementById('detail-symbols');symbols.replaceChildren();
  if(!component){details.hidden=true;return}
  details.hidden=false;detailKicker.textContent='Subsystem';name.textContent=component.name;meta.textContent=`Community ${component.id+1} · ${fmt(component.nodeCount)} nodes · ${fmt(component.internalEdges)} internal edges${component.repos.length?` · ${component.repos.join(', ')}`:''}`;
  component.topSymbols.forEach(symbol=>{const code=document.createElement('code');code.textContent=symbol;symbols.append(code)});
}
function showMemberDetails(component,member){
  if(!member)return;const name=document.getElementById('detail-name'),meta=document.getElementById('detail-meta'),symbols=document.getElementById('detail-symbols'),connections=component.memberConnections.filter(connection=>connection.from===member.id||connection.to===member.id),outgoing=connections.filter(connection=>connection.from===member.id).length;symbols.replaceChildren();details.hidden=false;detailKicker.textContent=`Selected symbol · ${outgoing} out / ${connections.length-outgoing} in`;name.textContent=member.label;meta.textContent=`${member.kind} · degree ${member.degree} · ${member.sourceFile||'no source path'}`;
  connections.slice(0,8).forEach(connection=>{const peerId=connection.from===member.id?connection.to:connection.from,peer=component.members.find(item=>item.id===peerId),button=document.createElement('button'),arrow=connection.from===member.id?'→':'←';button.type='button';button.className='detail-link';button.textContent=`${arrow} ${connection.relation.replaceAll('_',' ')} ×${connection.count} ${peer?.label||peerId}`;button.addEventListener('click',()=>focusMember(component,peerId));symbols.append(button)});if(!connections.length){const code=document.createElement('code');code.textContent='No displayed connections';symbols.append(code)}
}
document.getElementById('detail-close').addEventListener('click',()=>{details.hidden=true});
const search=document.getElementById('search');search.addEventListener('input',()=>{const query=search.value.trim().toLowerCase();if(currentCommunity===null){document.querySelectorAll('.component').forEach(card=>{const component=data.components.find(item=>item.id===Number(card.dataset.id)),haystack=[component.name,...component.topSymbols,...component.kinds,...component.repos,...component.members.flatMap(member=>[member.label,member.sourceFile])].join(' ').toLowerCase();card.classList.toggle('muted',Boolean(query)&&!haystack.includes(query))})}else{const component=data.components.find(item=>item.id===currentCommunity);document.querySelectorAll('.member').forEach(card=>{const member=component.members.find(item=>item.id===card.dataset.memberId),haystack=[member.label,member.kind,member.sourceFile].join(' ').toLowerCase();card.classList.toggle('muted',Boolean(query)&&!haystack.includes(query))})}});search.addEventListener('keydown',event=>{const query=search.value.trim().toLowerCase();if(event.key==='Enter'&&query){if(currentCommunity!==null){const component=data.components.find(item=>item.id===currentCommunity),member=component.members.find(item=>[item.label,item.kind,item.sourceFile].join(' ').toLowerCase().includes(query));if(member)focusMember(component,member.id)}else{const component=data.components.find(item=>[item.name,...item.topSymbols,...item.kinds,...item.repos,...item.members.flatMap(member=>[member.label,member.sourceFile])].join(' ').toLowerCase().includes(query)),member=component?.members.find(item=>[item.label,item.kind,item.sourceFile].join(' ').toLowerCase().includes(query));if(component){renderDrilldown(component.id);if(member)focusMember(component,member.id)}}}if(event.key==='Escape'){search.value='';if(currentCommunity!==null)render();else{search.dispatchEvent(new Event('input'));showDetails(null)}}});
overview.addEventListener('click',()=>{search.value='';render()});
document.getElementById('theme').addEventListener('click',event=>{const dark=document.documentElement.dataset.theme!=='dark';document.documentElement.dataset.theme=dark?'dark':'light';event.currentTarget.textContent=dark?'Light':'Dark';event.currentTarget.setAttribute('aria-pressed',String(dark))});
let downloadUrl;document.getElementById('download').addEventListener('click',event=>{
  const source=window.chartSvg;if(!source){event.preventDefault();return}const clone=source.cloneNode(true),box=source.viewBox.baseVal,dark=document.documentElement.dataset.theme==='dark',colors=dark?'--paper:#111116;--panel:#19191f;--ink:#f2eee7;--muted:#aaa4b2;--rule:#3c3943;--violet:#aa84f4;--cyan:#63d3df;--shadow:rgba(0,0,0,.4);--kind-module:#67b7e5;--kind-structure:#aa84f4;--kind-callable:#63d3df;--kind-data:#f2aa58;--kind-file:#c9ae8e;--kind-symbol:#aaa4b2':'--paper:#f3f0e9;--panel:#fbfaf6;--ink:#17171b;--muted:#6b6872;--rule:#c7c1b8;--violet:#6639ba;--cyan:#087f8c;--shadow:rgba(31,25,40,.12);--kind-module:#266f9b;--kind-structure:#6639ba;--kind-callable:#087f8c;--kind-data:#a65b0b;--kind-file:#7a6750;--kind-symbol:#6b6872';clone.setAttribute('xmlns',ns);clone.setAttribute('width',box.width);clone.setAttribute('height',box.height);clone.setAttribute('style',colors);
  const css=[...document.querySelectorAll('style')].map(style=>style.textContent).join('\\n'),style=document.createElementNS(ns,'style');style.textContent=css;clone.prepend(style);if(downloadUrl)URL.revokeObjectURL(downloadUrl);downloadUrl=URL.createObjectURL(new Blob([new XMLSerializer().serializeToString(clone)],{type:'image/svg+xml'}));event.currentTarget.href=downloadUrl;
});
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node};

    fn node(id: &str, label: &str, source: &str, community: u32) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: source.into(),
            community: Some(community),
            extra: Map::new(),
            ..Default::default()
        }
    }

    #[test]
    fn chart_is_offline_bounded_and_script_safe() {
        let kg = KnowledgeGraph::from_graph_data(GraphData {
            directed: true,
            nodes: vec![
                node("a", "API", "services/api/src/main.rs", 0),
                node("b", "</script><b>db", "services/db/src/lib.rs", 1),
                node("c", "Router", "services/api/src/router.rs", 0),
            ],
            links: vec![
                Edge {
                    source: NodeId("a".into()),
                    target: NodeId("b".into()),
                    relation: "calls".into(),
                    confidence: Confidence::Extracted,
                    source_file: "services/api/src/main.rs".into(),
                    source_location: None,
                    confidence_score: None,
                    weight: 1.0,
                    context: None,
                    cross_repo: false,
                    extra: Map::new(),
                },
                Edge {
                    source: NodeId("a".into()),
                    target: NodeId("c".into()),
                    relation: "calls".into(),
                    confidence: Confidence::Extracted,
                    source_file: "services/api/src/main.rs".into(),
                    source_location: None,
                    confidence_score: None,
                    weight: 1.0,
                    context: None,
                    cross_repo: false,
                    extra: Map::new(),
                },
            ],
            ..Default::default()
        });
        let html = to_chart_html(&kg, 12);
        assert!(html.contains("Signal Atlas"));
        assert!(html.contains("\"sourceFile\":\"services/api/src/main.rs\""));
        assert!(html.contains("\"memberConnections\":[{\"from\":\"a\",\"to\":\"c\",\"count\":1,\"relation\":\"calls\"}]"));
        assert!(html.contains("function renderDrilldown"));
        assert!(html.contains("function routeOrthogonal"));
        assert!(html.contains("id=\"details\""));
        assert!(html.contains("className='detail-link'"));
        assert!(html.contains(".connection.weak"));
        assert!(html.contains(".component,.member{animation:none}"));
        assert!(html.contains("\"name\":\"api / router\""));
        assert!(html.contains("\"name\":\"db\""));
        assert!(html.contains("<\\/script><b>db"));
        assert!(!html.contains("<script src="));
    }
}
