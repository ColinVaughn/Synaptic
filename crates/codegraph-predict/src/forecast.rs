//! The `ChangeForecast` data model and the pure-graph composition that produces
//! it. The git/worktree-heavy time-travel diff is run by the caller (it needs a
//! real repo); its result is folded in here via [`fold_diff_report`], keeping
//! this module pure, fast, and deterministic.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use codegraph_core::{Node, NodeId};
use codegraph_graph::KnowledgeGraph;
use codegraph_history::DiffReport;
use codegraph_query::{
    affected_nodes_multi, AffectedHit, ReverseImpactIndex, DEFAULT_AFFECTED_RELATIONS,
};

use crate::cochange::CoChange;
use crate::risk::{assess_risk, RiskFactors, RiskScore};

/// On-disk schema version for a forecast (bump on a breaking shape change).
pub const FORECAST_VERSION: u32 = 1;

/// A node referenced in a forecast: its graph id plus display metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRef {
    pub id: String,
    pub label: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub visibility: Option<String>,
}

/// One node reached by the reverse-impact walk from a changed node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactHit {
    pub id: String,
    pub label: String,
    pub file: String,
    /// Hops from the nearest changed node (1 = a direct dependent).
    pub depth: usize,
    /// The edge relation this node was first reached through.
    pub via_relation: String,
}

/// A directed dependency edge (module/file level), used in a dependency delta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepEdge {
    pub from: String,
    pub to: String,
}

/// Dependency edges added/removed by the change (from a time-travel diff).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyDelta {
    pub added: Vec<DepEdge>,
    pub removed: Vec<DepEdge>,
}

/// One concrete thing to check, with the command that checks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyStep {
    pub description: String,
    pub command: String,
}

/// The forecast for a proposed or pending change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeForecast {
    pub version: u32,
    /// The base revision the change is measured against, if any.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub base: Option<String>,
    pub changed_files: Vec<String>,
    /// Graph nodes defined in the changed files (what the change edits).
    pub changed_nodes: Vec<NodeRef>,
    /// Nodes that transitively depend on the changed nodes (at risk). Capped at
    /// `ForecastOptions::max_hits`; `blast_radius_total` is the true count.
    pub blast_radius: Vec<ImpactHit>,
    /// The true number of dependents before the display cap (>= `blast_radius.len()`).
    pub blast_radius_total: usize,
    /// The test subset of the blast radius: tests whose code transitively
    /// exercises a changed node, so they are the tests to run for this change.
    pub at_risk_tests: Vec<ImpactHit>,
    /// Changed nodes that are public: editing them risks outside callers.
    pub public_api_breaks: Vec<NodeRef>,
    /// New import cycles introduced (from a time-travel diff; else empty).
    pub new_cycles: Vec<Vec<String>>,
    /// Public APIs removed (from a time-travel diff; else empty).
    pub removed_apis: Vec<String>,
    pub dependency_delta: DependencyDelta,
    /// Files that historically change together with the changed files (mined from
    /// git history by the caller; empty otherwise). Catches coupling static
    /// analysis misses.
    #[serde(default)]
    pub co_change_suggestions: Vec<CoChange>,
    /// Heuristic change-risk score (advisory, uncalibrated).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub risk: Option<RiskScore>,
    /// Concrete pre/post-change checks for the agent to run.
    pub verify_checklist: Vec<VerifyStep>,
    pub summary: String,
}

/// Options for [`forecast_changes`].
#[derive(Debug, Clone)]
pub struct ForecastOptions {
    /// Reverse-impact hop bound.
    pub depth: usize,
    /// Edge relations that propagate impact.
    pub relations: Vec<String>,
    /// Cap on the number of blast-radius hits reported.
    pub max_hits: usize,
}

impl Default for ForecastOptions {
    fn default() -> Self {
        ForecastOptions {
            depth: 3,
            relations: DEFAULT_AFFECTED_RELATIONS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max_hits: 200,
        }
    }
}

/// Produce the pure-graph part of a forecast for a set of changed files.
///
/// Maps each changed file to the graph nodes it defines, walks the reverse-impact
/// blast radius from those nodes (deduped to the shallowest hop, the change's own
/// nodes excluded), flags the changed nodes that are public, and emits a verify
/// checklist. The cross-revision fields (cycles, removed APIs, dependency delta)
/// are left empty here and filled by [`fold_diff_report`].
pub fn forecast_changes(
    kg: &KnowledgeGraph,
    changed_files: &[String],
    opts: &ForecastOptions,
) -> ChangeForecast {
    let (changed_nodes, seeds) = scan_changed(kg, changed_files);
    let rels: Vec<&str> = opts.relations.iter().map(String::as_str).collect();
    let hits = affected_nodes_multi(kg, &seeds, &rels, opts.depth);
    assemble_forecast(kg, changed_files, opts, changed_nodes, hits)
}

/// Like [`forecast_changes`] but reusing a prebuilt [`ReverseImpactIndex`]
/// instead of rebuilding the reverse adjacency on every call -- for a long-lived
/// server that forecasts many changes against one static graph (build the index
/// once per graph load, query it per request).
///
/// The blast-radius walk uses the relations baked into `index`; `opts.relations`
/// is NOT consulted here, so the index MUST have been built with the relations
/// the caller intends (e.g. [`DEFAULT_AFFECTED_RELATIONS`], which
/// `ForecastOptions::default` also uses). `opts.depth` and `opts.max_hits` still
/// apply. The result is identical to [`forecast_changes`] (asserted by tests in
/// this module and in `codegraph-query`); only the adjacency is reused.
pub fn forecast_changes_with_index(
    kg: &KnowledgeGraph,
    index: &ReverseImpactIndex,
    changed_files: &[String],
    opts: &ForecastOptions,
) -> ChangeForecast {
    let (changed_nodes, seeds) = scan_changed(kg, changed_files);
    let hits = index.affected_multi(kg, &seeds, opts.depth);
    assemble_forecast(kg, changed_files, opts, changed_nodes, hits)
}

/// Map the changed files to the graph nodes they define (deterministically
/// ordered) plus the seed ids for the reverse-impact walk. The O(nodes) scan is
/// shared by both forecast entry points.
fn scan_changed(kg: &KnowledgeGraph, changed_files: &[String]) -> (Vec<NodeRef>, Vec<NodeId>) {
    let changed_set: HashSet<String> = changed_files.iter().map(|f| normalize_path(f)).collect();
    let mut changed_nodes: Vec<NodeRef> = Vec::new();
    let mut changed_ids: HashSet<NodeId> = HashSet::new();
    for n in kg.nodes() {
        // Only code symbols enter the change set; markdown headings and config
        // keys living in a changed file are excluded so they neither inflate the
        // count/output nor seed the blast-radius walk. The file match is checked
        // first so the kind check only runs for the few nodes in changed files.
        if changed_set.contains(&normalize_path(&n.source_file)) && n.is_code_symbol() {
            changed_ids.insert(n.id.clone());
            changed_nodes.push(node_ref(n));
        }
    }
    sort_node_refs(&mut changed_nodes);
    // Walk order-independence (asserted by codegraph-query tests) means the
    // HashSet's nondeterministic iteration order does not affect the result.
    let seeds: Vec<NodeId> = changed_ids.into_iter().collect();
    (changed_nodes, seeds)
}

/// Compose the final forecast from the changed nodes and the reverse-impact hits
/// (the changed nodes themselves are already excluded from `hits` by the walk).
/// Shared by both forecast entry points; cross-revision fields are left empty for
/// [`fold_diff_report`] to fill.
fn assemble_forecast(
    kg: &KnowledgeGraph,
    changed_files: &[String],
    opts: &ForecastOptions,
    changed_nodes: Vec<NodeRef>,
    hits: Vec<AffectedHit>,
) -> ChangeForecast {
    let mut all: Vec<(ImpactHit, bool)> = hits
        .into_iter()
        .filter_map(|hit| {
            kg.node(&hit.node_id).map(|n| {
                (
                    ImpactHit {
                        id: n.id.0.clone(),
                        label: n.label.clone(),
                        file: n.source_file.clone(),
                        depth: hit.depth,
                        via_relation: hit.via_relation,
                    },
                    n.is_test(),
                )
            })
        })
        .collect();
    all.sort_by(|a, b| impact_cmp(&a.0, &b.0));

    // Tests at risk: the test subset, NOT capped by max_hits (it is the
    // actionable "what to run" list). Blast radius: everything, capped.
    let at_risk_tests: Vec<ImpactHit> = all
        .iter()
        .filter(|(_, is_test)| *is_test)
        .map(|(h, _)| h.clone())
        .collect();
    // The true dependent count (before the display cap) feeds the risk score.
    let full_blast_count = all.len();
    let mut blast_radius: Vec<ImpactHit> = all.into_iter().map(|(h, _)| h).collect();
    blast_radius.truncate(opts.max_hits);

    let public_api_breaks: Vec<NodeRef> = changed_nodes
        .iter()
        .filter(|nr| nr.visibility.as_deref() == Some("public"))
        .cloned()
        .collect();

    // Graph-only risk; the CLI refines it with git churn/history via `refine_risk`.
    let risk = Some(assess_risk(&RiskFactors {
        changed_files: changed_files.len(),
        changed_nodes: changed_nodes.len(),
        blast_radius: full_blast_count,
        public_api_breaks: public_api_breaks.len(),
        lines_changed: 0,
        recent_commits_touching: 0,
    }));

    let verify_checklist = build_checklist(
        &changed_nodes,
        &blast_radius,
        &public_api_breaks,
        &at_risk_tests,
    );

    let mut forecast = ChangeForecast {
        version: FORECAST_VERSION,
        base: None,
        changed_files: changed_files.to_vec(),
        changed_nodes,
        blast_radius,
        blast_radius_total: full_blast_count,
        at_risk_tests,
        public_api_breaks,
        new_cycles: Vec::new(),
        removed_apis: Vec::new(),
        dependency_delta: DependencyDelta::default(),
        co_change_suggestions: Vec::new(),
        risk,
        verify_checklist,
        summary: String::new(),
    };
    forecast.summary = summary_line(&forecast);
    forecast
}

/// Recompute the forecast's one-line summary after the caller has enriched it
/// (co-change, diff, risk). Idempotent.
pub fn refresh_summary(forecast: &mut ChangeForecast) {
    forecast.summary = summary_line(forecast);
}

/// Re-score the forecast's risk with git-derived size/history (the graph-only
/// fields are read back from the forecast). Call after gathering `git` churn.
/// Uses `blast_radius_total` (the true count) so a small `--max-hits` does not
/// understate diffusion.
pub fn refine_risk(forecast: &mut ChangeForecast, lines_changed: usize, recent_commits: usize) {
    forecast.risk = Some(assess_risk(&RiskFactors {
        changed_files: forecast.changed_files.len(),
        changed_nodes: forecast.changed_nodes.len(),
        blast_radius: forecast.blast_radius_total,
        public_api_breaks: forecast.public_api_breaks.len(),
        lines_changed,
        recent_commits_touching: recent_commits,
    }));
    forecast.summary = summary_line(forecast);
}

/// Fold a time-travel `DiffReport` (HEAD vs working tree) into a forecast,
/// populating the cross-revision fields the pure-graph pass cannot know and
/// refreshing the summary.
pub fn fold_diff_report(forecast: &mut ChangeForecast, report: &DiffReport) {
    forecast.new_cycles = report.new_cycles.clone();
    forecast.removed_apis = report
        .removed_apis
        .iter()
        .map(|a| format!("{} ({})", a.label, a.source_file))
        .collect();
    forecast.dependency_delta = DependencyDelta {
        added: report.added_dependencies.iter().map(dep_edge).collect(),
        removed: report.removed_dependencies.iter().map(dep_edge).collect(),
    };
    forecast.summary = summary_line(forecast);
}

/// Repo-relative path with separators normalized to forward slashes so a
/// `git diff` path (always `/`) matches a `Node.source_file` regardless of OS.
fn normalize_path(p: &str) -> String {
    p.replace('\\', "/")
}

fn node_ref(n: &Node) -> NodeRef {
    NodeRef {
        id: n.id.0.clone(),
        label: n.label.clone(),
        file: n.source_file.clone(),
        kind: n.kind().map(|k| k.as_str().to_string()),
        visibility: n.visibility().map(|v| v.as_str().to_string()),
    }
}

fn dep_edge(d: &codegraph_history::ModuleDep) -> DepEdge {
    DepEdge {
        from: d.from.clone(),
        to: d.to.clone(),
    }
}

/// Deterministic ordering for impact hits: shallowest first, then by location.
fn impact_cmp(a: &ImpactHit, b: &ImpactHit) -> std::cmp::Ordering {
    a.depth
        .cmp(&b.depth)
        .then_with(|| a.file.cmp(&b.file))
        .then_with(|| a.label.cmp(&b.label))
        .then_with(|| a.id.cmp(&b.id))
}

fn sort_node_refs(refs: &mut [NodeRef]) {
    refs.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// Build a small, deterministic set of pre/post-change checks for the agent.
fn build_checklist(
    changed: &[NodeRef],
    blast: &[ImpactHit],
    public: &[NodeRef],
    tests: &[ImpactHit],
) -> Vec<VerifyStep> {
    let mut steps = Vec::new();
    if let (false, Some(rep)) = (blast.is_empty(), changed.first()) {
        steps.push(VerifyStep {
            description: format!(
                "Before editing, review the {} node(s) that transitively depend on this change",
                blast.len()
            ),
            command: format!("codegraph affected \"{}\"", arg(&rep.label)),
        });
    }
    if !tests.is_empty() {
        steps.push(VerifyStep {
            description: format!(
                "Run the {} test(s) that exercise this code before and after editing",
                tests.len()
            ),
            command: tests
                .iter()
                .map(|t| t.file.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(" "),
        });
    }
    for nr in public {
        steps.push(VerifyStep {
            description: format!(
                "`{}` is a public API; confirm external callers still compile after the change",
                nr.label
            ),
            command: format!("codegraph affected \"{}\"", arg(&nr.label)),
        });
    }
    steps.push(VerifyStep {
        description:
            "After editing, re-run the forecast to confirm no new import cycles or removed public APIs"
                .to_string(),
        command: "codegraph predict".to_string(),
    });
    steps
}

/// Strip double-quotes from a label embedded in a double-quoted suggested
/// command so the printed `codegraph affected "..."` stays well-formed.
fn arg(label: &str) -> String {
    label.replace('"', "")
}

/// One-line headline summarizing the forecast.
fn summary_line(f: &ChangeForecast) -> String {
    let mut parts = vec![
        format!("{} changed file(s)", f.changed_files.len()),
        format!("{} changed node(s)", f.changed_nodes.len()),
        format!("{} at-risk dependent(s)", f.blast_radius.len()),
    ];
    if !f.at_risk_tests.is_empty() {
        parts.push(format!("{} at-risk test(s)", f.at_risk_tests.len()));
    }
    if !f.public_api_breaks.is_empty() {
        parts.push(format!(
            "{} public API(s) at risk",
            f.public_api_breaks.len()
        ));
    }
    if !f.new_cycles.is_empty() {
        parts.push(format!("{} new cycle(s)", f.new_cycles.len()));
    }
    if !f.removed_apis.is_empty() {
        parts.push(format!("{} removed API(s)", f.removed_apis.len()));
    }
    if !f.co_change_suggestions.is_empty() {
        parts.push(format!(
            "{} co-change suggestion(s)",
            f.co_change_suggestions.len()
        ));
    }
    if let Some(r) = &f.risk {
        parts.push(format!("{} risk ({}/100)", r.level, r.score));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{Confidence, Edge, FileType, GraphData, Node, Visibility};
    use codegraph_history::{DriftReport, ModuleDep, RemovedApi};
    use serde_json::Map;

    fn node(id: &str, label: &str, file: &str, vis: Option<Visibility>) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: file.into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
        };
        if let Some(v) = vis {
            n.set_visibility(v);
        }
        n
    }

    fn edge(s: &str, t: &str, r: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: r.into(),
            confidence: Confidence::Extracted,
            source_file: "x".into(),
            source_location: Some("L1".into()),
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> KnowledgeGraph {
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links: edges,
            hyperedges: vec![],
            built_at_commit: None,
        };
        KnowledgeGraph::from_graph_data(gd)
    }

    #[test]
    fn maps_changed_files_to_nodes_and_walks_blast_radius() {
        // c -> b -> a  (c depends on b depends on a). Editing a affects b then c.
        let kg = graph(
            vec![
                node("a", "alpha", "src/a.py", None),
                node("b", "beta", "src/b.py", None),
                node("c", "gamma", "src/c.py", None),
            ],
            vec![edge("b", "a", "calls"), edge("c", "b", "calls")],
        );
        let f = forecast_changes(&kg, &["src/a.py".to_string()], &ForecastOptions::default());
        assert_eq!(f.changed_files, vec!["src/a.py".to_string()]);
        assert_eq!(
            f.changed_nodes
                .iter()
                .map(|n| n.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        let ids: Vec<&str> = f.blast_radius.iter().map(|h| h.id.as_str()).collect();
        assert!(ids.contains(&"b") && ids.contains(&"c"));
        assert!(
            !ids.contains(&"a"),
            "the changed node is not its own dependent"
        );
        let b = f.blast_radius.iter().find(|h| h.id == "b").unwrap();
        let c = f.blast_radius.iter().find(|h| h.id == "c").unwrap();
        assert_eq!(b.depth, 1);
        assert_eq!(c.depth, 2);
        let bi = f.blast_radius.iter().position(|h| h.id == "b").unwrap();
        let ci = f.blast_radius.iter().position(|h| h.id == "c").unwrap();
        assert!(bi < ci, "shallower dependents listed first");
    }

    #[test]
    fn changed_nodes_exclude_docs_and_config_artifacts() {
        // A changed file's real code symbol is kept; a markdown heading and a JSON
        // config-key node in changed files must be excluded from changed_nodes.
        let mut heading = node("h", "Build or Obtain the Bundle", "README.md", None);
        heading.file_type = FileType::Document;
        let mut cfgkey = node("k", "browserslist", "package.json", None);
        cfgkey
            .extra
            .insert("_node_type".into(), serde_json::json!("config_key"));
        let kg = graph(
            vec![node("a", "alpha", "src/a.ts", None), heading, cfgkey],
            vec![],
        );
        let f = forecast_changes(
            &kg,
            &[
                "src/a.ts".to_string(),
                "README.md".to_string(),
                "package.json".to_string(),
            ],
            &ForecastOptions::default(),
        );
        let ids: Vec<&str> = f.changed_nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["a"], "only the code symbol is a changed node");
    }

    #[test]
    fn forecast_with_prebuilt_index_matches_plain() {
        // forecast_changes_with_index, given an index over the default relations,
        // must produce the identical forecast forecast_changes builds internally --
        // the server's per-request cache must not change results.
        let kg = graph(
            vec![
                node("a", "alpha", "src/a.py", Some(Visibility::Public)),
                node("b", "beta", "src/b.py", None),
                node("t", "test_alpha", "tests/test_alpha.py", None),
            ],
            vec![edge("b", "a", "calls"), edge("t", "a", "calls")],
        );
        let opts = ForecastOptions::default();
        let changed = vec!["src/a.py".to_string()];
        let plain = forecast_changes(&kg, &changed, &opts);
        let rels: Vec<&str> = opts.relations.iter().map(String::as_str).collect();
        let index = ReverseImpactIndex::build(&kg, &rels);
        let indexed = forecast_changes_with_index(&kg, &index, &changed, &opts);
        assert_eq!(plain, indexed);
    }

    #[test]
    fn blast_radius_via_relation_is_deterministic_on_ties() {
        // d depends on both changed nodes x and y at depth 1, through different
        // relations. The reported via_relation must be deterministic (the
        // lexicographically smallest) regardless of HashSet iteration order.
        let kg = graph(
            vec![
                node("x", "ex", "fx.py", None),
                node("y", "why", "fy.py", None),
                node("d", "dee", "fd.py", None),
            ],
            vec![edge("d", "x", "calls"), edge("d", "y", "references")],
        );
        let f = forecast_changes(
            &kg,
            &["fx.py".to_string(), "fy.py".to_string()],
            &ForecastOptions::default(),
        );
        let d = f.blast_radius.iter().find(|h| h.id == "d").unwrap();
        assert_eq!(d.depth, 1);
        assert_eq!(
            d.via_relation, "calls",
            "ties resolve to the smallest relation deterministically"
        );
    }

    #[test]
    fn surfaces_tests_that_exercise_the_change() {
        // test_login (under tests/) calls login; a non-test helper also calls it.
        // Editing login.py puts the test in at_risk_tests and both in blast_radius.
        let kg = graph(
            vec![
                node("login", "login", "src/login.py", None),
                node("t", "test_login", "tests/test_login.py", None),
                node("helper", "helper", "src/helper.py", None),
            ],
            vec![
                edge("t", "login", "calls"),
                edge("helper", "login", "calls"),
            ],
        );
        let f = forecast_changes(
            &kg,
            &["src/login.py".to_string()],
            &ForecastOptions::default(),
        );
        let test_ids: Vec<&str> = f.at_risk_tests.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(test_ids, vec!["t"], "only the test dependent is at-risk");
        assert!(f.blast_radius.iter().any(|h| h.id == "t"));
        assert!(f.blast_radius.iter().any(|h| h.id == "helper"));
        assert!(f.summary.contains("at-risk test"));
    }

    #[test]
    fn flags_public_changed_nodes_only() {
        let kg = graph(
            vec![
                node("pub_fn", "login", "src/auth.py", Some(Visibility::Public)),
                node(
                    "priv_fn",
                    "_helper",
                    "src/auth.py",
                    Some(Visibility::Private),
                ),
                node("untyped", "thing", "src/auth.py", None),
            ],
            vec![],
        );
        let f = forecast_changes(
            &kg,
            &["src/auth.py".to_string()],
            &ForecastOptions::default(),
        );
        let pub_ids: Vec<&str> = f.public_api_breaks.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(pub_ids, vec!["pub_fn"]);
    }

    #[test]
    fn forecast_includes_a_risk_score() {
        let kg = graph(
            vec![
                node("a", "alpha", "src/a.py", Some(Visibility::Public)),
                node("b", "beta", "src/b.py", None),
            ],
            vec![edge("b", "a", "calls")],
        );
        let f = forecast_changes(&kg, &["src/a.py".to_string()], &ForecastOptions::default());
        let r = f.risk.expect("forecast carries a risk score");
        assert!(r.score <= 100);
        assert!(["low", "medium", "high"].contains(&r.level.as_str()));
        assert!(
            f.summary.contains("risk"),
            "summary names the risk: {}",
            f.summary
        );
    }

    #[test]
    fn refine_risk_uses_true_blast_total_not_the_display_cap() {
        // 6 dependents, but max_hits=1 truncates the displayed blast radius to 1.
        // refine_risk (with no git data) must reproduce the graph-only score,
        // proving it scores diffusion from the true total, not the cap.
        let kg = graph(
            vec![
                node("a", "alpha", "src/a.py", None),
                node("d1", "d1", "src/d1.py", None),
                node("d2", "d2", "src/d2.py", None),
                node("d3", "d3", "src/d3.py", None),
                node("d4", "d4", "src/d4.py", None),
                node("d5", "d5", "src/d5.py", None),
                node("d6", "d6", "src/d6.py", None),
            ],
            vec![
                edge("d1", "a", "calls"),
                edge("d2", "a", "calls"),
                edge("d3", "a", "calls"),
                edge("d4", "a", "calls"),
                edge("d5", "a", "calls"),
                edge("d6", "a", "calls"),
            ],
        );
        let opts = ForecastOptions {
            max_hits: 1,
            ..ForecastOptions::default()
        };
        let f = forecast_changes(&kg, &["src/a.py".to_string()], &opts);
        assert_eq!(f.blast_radius.len(), 1, "display capped");
        assert_eq!(f.blast_radius_total, 6, "true total preserved");
        let graph_only = f.risk.as_ref().unwrap().score;
        let mut refined = f.clone();
        refine_risk(&mut refined, 0, 0);
        assert_eq!(
            refined.risk.as_ref().unwrap().score,
            graph_only,
            "refine with no git data must not drop below the graph-only score"
        );
    }

    #[test]
    fn refine_risk_raises_score_with_churn_and_history() {
        let kg = graph(vec![node("a", "alpha", "src/a.py", None)], vec![]);
        let mut f = forecast_changes(&kg, &["src/a.py".to_string()], &ForecastOptions::default());
        let before = f.risk.as_ref().unwrap().score;
        refine_risk(&mut f, 500, 25);
        assert!(
            f.risk.as_ref().unwrap().score >= before,
            "git churn/history cannot lower the score"
        );
    }

    #[test]
    fn normalizes_backslash_paths() {
        let kg = graph(vec![node("a", "alpha", "src/a.py", None)], vec![]);
        // A changed file given with Windows separators still matches src/a.py.
        let f = forecast_changes(&kg, &["src\\a.py".to_string()], &ForecastOptions::default());
        assert_eq!(f.changed_nodes.len(), 1);
    }

    #[test]
    fn respects_depth_bound() {
        let kg = graph(
            vec![
                node("a", "alpha", "src/a.py", None),
                node("b", "beta", "src/b.py", None),
                node("c", "gamma", "src/c.py", None),
            ],
            vec![edge("b", "a", "calls"), edge("c", "b", "calls")],
        );
        let opts = ForecastOptions {
            depth: 1,
            ..ForecastOptions::default()
        };
        let f = forecast_changes(&kg, &["src/a.py".to_string()], &opts);
        let ids: Vec<&str> = f.blast_radius.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["b"], "depth 1 reaches only direct dependents");
    }

    #[test]
    fn populates_a_verify_checklist_and_summary() {
        let kg = graph(
            vec![
                node("a", "alpha", "src/a.py", Some(Visibility::Public)),
                node("b", "beta", "src/b.py", None),
            ],
            vec![edge("b", "a", "calls")],
        );
        let f = forecast_changes(&kg, &["src/a.py".to_string()], &ForecastOptions::default());
        assert!(
            !f.verify_checklist.is_empty(),
            "a change with impact yields checks"
        );
        assert!(
            f.summary.contains("changed"),
            "summary describes the change"
        );
    }

    #[test]
    fn fold_diff_report_merges_cycles_apis_and_deps() {
        let kg = graph(vec![node("a", "alpha", "src/a.py", None)], vec![]);
        let mut f = forecast_changes(&kg, &["src/a.py".to_string()], &ForecastOptions::default());
        let report = DiffReport {
            rev1: "HEAD".into(),
            rev2: "WORKING_TREE".into(),
            summary: "x".into(),
            added_dependencies: vec![ModuleDep {
                from: "m1".into(),
                to: "m2".into(),
            }],
            removed_dependencies: vec![],
            removed_apis: vec![RemovedApi {
                id: "a".into(),
                label: "alpha".into(),
                source_file: "src/a.py".into(),
                referenced_by: 3,
            }],
            drift: DriftReport {
                communities_before: 1,
                communities_after: 1,
                coupling_before: 0.0,
                coupling_after: 0.0,
                modules: vec![],
            },
            new_cycles: vec![vec!["x".into(), "y".into(), "x".into()]],
            hotspots: vec![],
        };
        fold_diff_report(&mut f, &report);
        assert_eq!(f.new_cycles.len(), 1);
        assert_eq!(
            f.dependency_delta.added,
            vec![DepEdge {
                from: "m1".into(),
                to: "m2".into()
            }]
        );
        assert_eq!(f.removed_apis.len(), 1);
        assert!(f.removed_apis[0].contains("alpha"));
        assert!(
            f.summary.contains("cycle"),
            "summary refreshed with diff findings"
        );
    }
}
