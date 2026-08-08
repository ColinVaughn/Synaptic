//! Where first-party code touches a vulnerable package, and what reaches it.
//!
//! The applicability combiner asks only whether a package is reached. That
//! answers "does this apply", not "where do I look" or "what ships it". This
//! module carries the concrete evidence: the call sites, the entry points that
//! reach them, and the scope a remediation therefore has to cover.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use synaptic_core::{GraphData, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_predict::{forecast_nodes, ForecastOptions};

/// How far back the reverse walk looks for an entry point.
///
/// Deep enough to cross a handler, a service layer and a helper chain; short
/// enough that a hub function cannot make the walk quadratic. A path longer
/// than this is reported as no path rather than a wrong one.
const MAX_ENTRY_DEPTH: usize = 16;

/// The `extra` key extractors use for surface nodes such as routes.
///
/// Deliberately not `kind`: `kind` on an external surface describes an
/// *outbound* call (`http`, `rpc`), which is a dependency, not an entry point.
/// Reading the wrong key would report the services this code calls as the
/// services it exposes.
const NODE_TYPE_KEY: &str = "_node_type";

/// An inbound surface that can carry traffic into first-party code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryPointKind {
    HttpRoute,
    WebSocket,
    GrpcService,
    QueueConsumer,
    CliCommand,
}

impl EntryPointKind {
    /// Map an extractor's surface tag onto an entry-point kind.
    ///
    /// Returns `None` for anything that is not an inbound surface.
    fn from_node_type(node_type: &str) -> Option<Self> {
        match node_type {
            "route" => Some(Self::HttpRoute),
            "ws_endpoint" | "ws_message" => Some(Self::WebSocket),
            "grpc_service" => Some(Self::GrpcService),
            "queue_topic" => Some(Self::QueueConsumer),
            "command" => Some(Self::CliCommand),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HttpRoute => "http_route",
            Self::WebSocket => "websocket",
            Self::GrpcService => "grpc_service",
            Self::QueueConsumer => "queue_consumer",
            Self::CliCommand => "cli_command",
        }
    }
}

/// An entry point that reaches a vulnerable call site, and the route it takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryPoint {
    pub kind: EntryPointKind,
    /// The surface's label, for example `/api/users`.
    pub label: String,
    pub id: String,
    /// Symbol labels from the entry point down to the reaching symbol,
    /// inclusive at both ends.
    pub path: Vec<String>,
}

/// One concrete place first-party code reaches a vulnerable package member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    /// Label of the enclosing first-party symbol.
    pub symbol: String,
    /// Graph node id of the enclosing symbol, for follow-up queries.
    pub symbol_id: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub line: Option<u32>,
    /// The package member reached from this site.
    pub member: String,
}

/// Parse a graph source location into a line number.
///
/// Locations are written `L42`. A bare number is accepted too, because one
/// extractor emits it that way and dropping the site would lose real evidence.
pub(crate) fn parse_line(location: Option<&str>) -> Option<u32> {
    let raw = location?.trim();
    let digits = raw.strip_prefix('L').unwrap_or(raw);
    digits.parse().ok()
}

/// How much first-party code an upgrade puts in scope for review.
///
/// This is the code-level counterpart to [`crate::RemediationPlan`], which
/// covers only the manifest change. An upgrade that compiles can still change
/// behaviour at every site that calls the package.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemediationScope {
    /// Whether a graph was available to derive this from.
    ///
    /// Load-bearing: without a graph every count below is zero, and zero must
    /// read as "not measured", never as "nothing to review".
    pub graph_backed: bool,
    /// First-party files holding a call site, to review after the upgrade.
    pub review_files: Vec<String>,
    /// Distinct first-party symbols that reach the package.
    pub calling_symbols: usize,
    /// Entry points shown to reach a call site.
    pub exposed_entry_points: usize,
    /// What the change forecaster says an upgrade puts at risk. Absent when no
    /// forecast was run, which again is not the same as "nothing at risk".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub impact: Option<ImpactForecast>,
}

/// What an upgrade puts at risk beyond the files that call the package.
///
/// Derived by running the change forecaster over the calling symbols, so this
/// is the same reverse-impact walk `synaptic predict` uses, seeded from the
/// vulnerability instead of from a diff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactForecast {
    /// Symbols that transitively depend on a calling symbol.
    pub dependent_symbols: usize,
    /// Calling symbols that are public API, so behaviour changes are visible
    /// outside this repository.
    pub public_api_touched: Vec<String>,
    /// Tests whose code exercises a calling symbol. These are the tests to run
    /// after the upgrade, not the whole suite.
    pub at_risk_tests: Vec<String>,
}

/// Reverse-impact index over the whole graph, for forecasting an upgrade.
///
/// Held separately from [`ReachIndex`] because it owns a `KnowledgeGraph`,
/// which consumes the `GraphData` it is built from.
#[derive(Debug)]
pub struct ImpactIndex {
    graph: KnowledgeGraph,
}

impl ImpactIndex {
    pub fn new(graph: KnowledgeGraph) -> Self {
        Self { graph }
    }

    /// Forecast the blast radius of changing these symbols.
    pub fn forecast(&self, seed_symbol_ids: &[String]) -> ImpactForecast {
        let seeds: Vec<NodeId> = seed_symbol_ids
            .iter()
            .map(|id| NodeId(id.clone()))
            .collect();
        let forecast = forecast_nodes(&self.graph, &seeds, &ForecastOptions::default());

        ImpactForecast {
            // The true count, not the display-capped list: a scope that
            // under-reported its own blast radius would invite a reviewer to
            // stop early.
            dependent_symbols: forecast.blast_radius_total,
            public_api_touched: forecast
                .public_api_breaks
                .iter()
                .map(|node| node.label.clone())
                .collect(),
            at_risk_tests: forecast
                .at_risk_tests
                .iter()
                .map(|hit| hit.label.clone())
                .collect(),
        }
    }
}

/// Derive the review scope an upgrade carries from the reachability evidence.
pub fn remediation_scope(
    call_sites: &[CallSite],
    entry_points: &[EntryPoint],
    impact: Option<&ImpactForecast>,
    graph_backed: bool,
) -> RemediationScope {
    let mut review_files: Vec<String> = call_sites
        .iter()
        .map(|site| site.file.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    review_files.sort();
    let calling_symbols = call_sites
        .iter()
        .map(|site| site.symbol_id.as_str())
        .collect::<BTreeSet<_>>()
        .len();

    RemediationScope {
        graph_backed,
        review_files,
        calling_symbols,
        exposed_entry_points: entry_points.len(),
        impact: impact.cloned(),
    }
}

/// Reverse-reachability index over the first-party call graph.
///
/// Built once per scan and queried per finding, because a repository has one
/// call graph but many findings, and rebuilding it per finding is how the
/// usage oracle used to waste 563 us a call.
#[derive(Debug, Clone, Default)]
pub struct ReachIndex {
    /// Node id -> the nodes that reach it in one hop.
    callers: BTreeMap<String, Vec<String>>,
    /// Node id -> display label.
    labels: BTreeMap<String, String>,
    /// Node id -> entry-point kind, for surface nodes only.
    surfaces: BTreeMap<String, EntryPointKind>,
}

/// Relations traversed backwards to get from a call site to an entry point.
///
/// Restricted to relations that carry control into a symbol. `contains` and
/// `imports_from` are excluded on purpose: they lead to file and module nodes,
/// from which every symbol in the file looks reachable, which would report
/// entry points that cannot actually run the vulnerable code.
fn is_control_relation(relation: &str) -> bool {
    matches!(
        relation,
        "calls" | "method" | "handled_by" | "invokes" | "calls_service" | "implements"
    )
}

impl ReachIndex {
    pub fn new(graph: &GraphData) -> Self {
        let mut callers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        // Nodes with at least one outgoing control edge: the ones that actually
        // dispatch into first-party code.
        let mut dispatches: BTreeSet<&str> = BTreeSet::new();
        for edge in &graph.links {
            if !is_control_relation(&edge.relation) {
                continue;
            }
            dispatches.insert(edge.source.0.as_str());
            callers
                .entry(edge.target.0.clone())
                .or_default()
                .push(edge.source.0.clone());
        }

        let mut labels = BTreeMap::new();
        let mut surfaces = BTreeMap::new();
        for node in &graph.nodes {
            labels.insert(node.id.0.clone(), node.label.clone());
            let node_type = node
                .extra
                .get(NODE_TYPE_KEY)
                .and_then(|value| value.as_str());
            // A surface tag alone is not enough. Across real repositories most
            // `route` nodes are outbound third-party URLs this code calls, and
            // they are told apart from served routes only by whether anything
            // hangs off them: a served route dispatches (`handled_by`), a
            // called URL is a leaf.
            if !dispatches.contains(node.id.0.as_str()) {
                continue;
            }
            if let Some(kind) = node_type.and_then(EntryPointKind::from_node_type) {
                surfaces.insert(node.id.0.clone(), kind);
            }
        }
        for reaching in callers.values_mut() {
            reaching.sort();
            reaching.dedup();
        }

        Self {
            callers,
            labels,
            surfaces,
        }
    }

    /// Entry points that reach any of these call sites, with the path taken.
    ///
    /// Absence of a path is not proof that nothing reaches the site: the walk
    /// is bounded, and dynamic dispatch is invisible to it. Callers must treat
    /// an empty result as "not shown", never as "not exposed".
    pub fn entry_points(&self, call_sites: &[CallSite]) -> Vec<EntryPoint> {
        let seeds: BTreeSet<&str> = call_sites
            .iter()
            .map(|site| site.symbol_id.as_str())
            .collect();
        let mut found: BTreeMap<&str, EntryPoint> = BTreeMap::new();

        for seed in seeds {
            // Breadth-first, so the first path reaching any surface is a
            // shortest one. A shortest path is the most reviewable
            // explanation of how traffic gets to the vulnerable call.
            let mut previous: BTreeMap<&str, &str> = BTreeMap::new();
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            let mut queue: VecDeque<(&str, usize)> = VecDeque::new();
            seen.insert(seed);
            queue.push_back((seed, 0));

            while let Some((current, depth)) = queue.pop_front() {
                if let Some(kind) = self.surfaces.get(current) {
                    found.entry(current).or_insert_with(|| EntryPoint {
                        kind: *kind,
                        label: self.label_of(current),
                        id: current.to_string(),
                        path: self.path_from(current, seed, &previous),
                    });
                    // A surface is the boundary. Whatever registered it is not
                    // a more specific answer, so this branch stops here.
                    continue;
                }
                if depth >= MAX_ENTRY_DEPTH {
                    continue;
                }
                for caller in self.callers.get(current).into_iter().flatten() {
                    if seen.insert(caller.as_str()) {
                        previous.insert(caller.as_str(), current);
                        queue.push_back((caller.as_str(), depth + 1));
                    }
                }
            }
        }

        let mut entries: Vec<EntryPoint> = found.into_values().collect();
        entries.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then(left.label.cmp(&right.label))
                .then(left.id.cmp(&right.id))
        });
        entries
    }

    /// Whether this node is registered as an inbound surface.
    pub fn is_surface(&self, id: &str) -> bool {
        self.surfaces.contains_key(id)
    }

    fn label_of(&self, id: &str) -> String {
        self.labels
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    /// Walk the predecessor chain from an entry point down to the seed.
    fn path_from(&self, entry: &str, seed: &str, previous: &BTreeMap<&str, &str>) -> Vec<String> {
        let mut path = vec![self.label_of(entry)];
        let mut current = entry;
        while current != seed {
            let Some(next) = previous.get(current) else {
                break;
            };
            path.push(self.label_of(next));
            current = next;
        }
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaptic_core::{Confidence, Edge, FileType, Node, NodeId};

    fn node(id: &str, label: &str, file: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: file.into(),
            source_location: None,
            community: None,
            repo: None,
            extra: Default::default(),
        }
    }

    fn surface(id: &str, label: &str, node_type: &str) -> Node {
        let mut node = node(id, label, "");
        node.extra
            .insert(NODE_TYPE_KEY.into(), node_type.to_string().into());
        node
    }

    fn link(source: &str, target: &str, relation: &str) -> Edge {
        Edge {
            source: NodeId(source.into()),
            target: NodeId(target.into()),
            relation: relation.into(),
            confidence: Confidence::Extracted,
            source_file: "src/lib.rs".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Default::default(),
        }
    }

    fn site(symbol_id: &str) -> CallSite {
        CallSite {
            symbol: symbol_id.into(),
            symbol_id: symbol_id.into(),
            file: "src/lib.rs".into(),
            line: Some(1),
            member: "Value.get".into(),
        }
    }

    fn graph(nodes: Vec<Node>, links: Vec<Edge>) -> GraphData {
        GraphData {
            nodes,
            links,
            ..Default::default()
        }
    }

    #[test]
    fn a_route_that_reaches_a_call_site_is_reported_with_its_path() {
        let index = ReachIndex::new(&graph(
            vec![
                surface("r1", "/api/users", "route"),
                node("handler", "list_users()", "src/api.rs"),
                node("helper", "load()", "src/db.rs"),
            ],
            vec![
                link("r1", "handler", "handled_by"),
                link("handler", "helper", "calls"),
            ],
        ));

        let entries = index.entry_points(&[site("helper")]);

        assert_eq!(entries.len(), 1, "got {entries:?}");
        assert_eq!(entries[0].kind, EntryPointKind::HttpRoute);
        assert_eq!(entries[0].label, "/api/users");
        assert_eq!(
            entries[0].path,
            vec!["/api/users", "list_users()", "load()"]
        );
    }

    #[test]
    fn a_route_that_reaches_nothing_relevant_is_not_reported() {
        let index = ReachIndex::new(&graph(
            vec![
                surface("r1", "/api/users", "route"),
                node("handler", "list_users()", "src/api.rs"),
                node("lonely", "unused()", "src/other.rs"),
            ],
            vec![link("r1", "handler", "handled_by")],
        ));

        let entries = index.entry_points(&[site("lonely")]);

        assert!(entries.is_empty(), "got {entries:?}");
    }

    #[test]
    fn a_route_node_that_handles_nothing_is_not_treated_as_an_entry_point() {
        // Observed across three real repositories: most `_node_type: route`
        // nodes are OUTBOUND third-party URLs the code calls (Stripe, GitHub,
        // Google), not routes it serves. They are distinguishable only by
        // having no outgoing control edge. Registering them as surfaces is
        // wrong even though the backward walk cannot currently reach them.
        let index = ReachIndex::new(&graph(
            vec![
                surface("r_out", "/v1/subscriptions/{param}", "route"),
                node("caller", "charge()", "src/pay.rs"),
            ],
            vec![link("caller", "r_out", "calls_service")],
        ));

        assert!(
            !index.is_surface("r_out"),
            "an outbound URL was registered as an inbound surface"
        );
    }

    #[test]
    fn a_route_node_that_handles_something_is_still_an_entry_point() {
        let index = ReachIndex::new(&graph(
            vec![
                surface("r_in", "/api/users", "route"),
                node("handler", "list_users()", "src/api.rs"),
            ],
            vec![link("r_in", "handler", "handled_by")],
        ));

        assert!(
            index.is_surface("r_in"),
            "a genuinely routed handler must stay an entry point"
        );
    }

    #[test]
    fn an_outbound_http_surface_is_not_an_entry_point() {
        // `kind: http` marks a service this code calls. Reporting it as an
        // entry point would invert the direction of the exposure.
        let mut outbound = node("h1", "Http: https://api.stripe.com POST /v1/customers", "");
        outbound
            .extra
            .insert("kind".into(), "http".to_string().into());
        let index = ReachIndex::new(&graph(
            vec![outbound, node("caller", "charge()", "src/pay.rs")],
            vec![link("caller", "h1", "calls")],
        ));

        let entries = index.entry_points(&[site("caller")]);

        assert!(entries.is_empty(), "outbound call reported: {entries:?}");
    }

    #[test]
    fn every_inbound_surface_kind_is_recognised() {
        for (node_type, expected) in [
            ("route", EntryPointKind::HttpRoute),
            ("ws_endpoint", EntryPointKind::WebSocket),
            ("ws_message", EntryPointKind::WebSocket),
            ("grpc_service", EntryPointKind::GrpcService),
            ("queue_topic", EntryPointKind::QueueConsumer),
            ("command", EntryPointKind::CliCommand),
        ] {
            let index = ReachIndex::new(&graph(
                vec![
                    surface("s1", "surface", node_type),
                    node("handler", "run()", "src/lib.rs"),
                ],
                vec![link("s1", "handler", "handled_by")],
            ));

            let entries = index.entry_points(&[site("handler")]);

            assert_eq!(entries.len(), 1, "{node_type} was not recognised");
            assert_eq!(entries[0].kind, expected, "{node_type} mapped wrongly");
        }
    }

    #[test]
    fn containment_edges_do_not_make_a_whole_file_reachable() {
        // `contains` runs file -> symbol. Traversing it backwards would let any
        // route in a file appear to reach any call site in that file.
        let index = ReachIndex::new(&graph(
            vec![
                surface("r1", "/api/users", "route"),
                node("file", "api.rs", "src/api.rs"),
                node("handler", "list_users()", "src/api.rs"),
                node("unrelated", "other()", "src/api.rs"),
            ],
            vec![
                link("r1", "handler", "handled_by"),
                link("file", "handler", "contains"),
                link("file", "unrelated", "contains"),
            ],
        ));

        let entries = index.entry_points(&[site("unrelated")]);

        assert!(
            entries.is_empty(),
            "file containment leaked reachability: {entries:?}"
        );
    }

    #[test]
    fn a_path_longer_than_the_depth_bound_is_not_reported() {
        let mut nodes = vec![surface("r1", "/deep", "route")];
        let mut links = Vec::new();
        let chain: Vec<String> = (0..=MAX_ENTRY_DEPTH + 2)
            .map(|step| format!("n{step}"))
            .collect();
        for name in &chain {
            nodes.push(node(name, name, "src/lib.rs"));
        }
        links.push(link("r1", &chain[0], "handled_by"));
        for pair in chain.windows(2) {
            links.push(link(&pair[0], &pair[1], "calls"));
        }
        let index = ReachIndex::new(&graph(nodes, links));

        let entries = index.entry_points(&[site(chain.last().unwrap())]);

        assert!(entries.is_empty(), "walk exceeded its bound: {entries:?}");
    }

    #[test]
    fn two_call_sites_under_one_route_report_that_route_once() {
        let index = ReachIndex::new(&graph(
            vec![
                surface("r1", "/api/users", "route"),
                node("handler", "list_users()", "src/api.rs"),
                node("a", "first()", "src/db.rs"),
                node("b", "second()", "src/db.rs"),
            ],
            vec![
                link("r1", "handler", "handled_by"),
                link("handler", "a", "calls"),
                link("handler", "b", "calls"),
            ],
        ));

        let entries = index.entry_points(&[site("a"), site("b")]);

        assert_eq!(entries.len(), 1, "route duplicated: {entries:?}");
    }

    fn site_in(symbol_id: &str, file: &str, line: u32) -> CallSite {
        CallSite {
            symbol: symbol_id.into(),
            symbol_id: symbol_id.into(),
            file: file.into(),
            line: Some(line),
            member: "Value.get".into(),
        }
    }

    /// A graph where `helper()` is called by a public API and by a test.
    fn impact_graph() -> GraphData {
        let mut public_caller = node("caller", "list_users()", "src/api.rs");
        public_caller
            .extra
            .insert("visibility".into(), "public".to_string().into());
        let mut test_caller = node("t1", "helper_works()", "tests/helper_test.rs");
        test_caller.extra.insert("_is_test".into(), true.into());

        graph(
            vec![
                node("helper", "helper()", "src/db.rs"),
                public_caller,
                test_caller,
            ],
            vec![
                link("caller", "helper", "calls"),
                link("t1", "helper", "calls"),
            ],
        )
    }

    fn impact_index() -> ImpactIndex {
        ImpactIndex::new(KnowledgeGraph::from_graph_data(impact_graph()))
    }

    #[test]
    fn the_forecast_counts_symbols_that_depend_on_the_calling_symbol() {
        let forecast = impact_index().forecast(&["helper".to_string()]);

        assert_eq!(
            forecast.dependent_symbols, 2,
            "both the public caller and the test depend on it: {forecast:?}"
        );
    }

    #[test]
    fn the_forecast_names_the_tests_that_exercise_the_calling_symbol() {
        let forecast = impact_index().forecast(&["helper".to_string()]);

        assert!(
            forecast
                .at_risk_tests
                .iter()
                .any(|test| test.contains("helper_works")),
            "the covering test must be named, got {:?}",
            forecast.at_risk_tests
        );
    }

    #[test]
    fn a_seed_the_graph_does_not_know_forecasts_nothing() {
        let forecast = impact_index().forecast(&["absent".to_string()]);

        assert_eq!(forecast.dependent_symbols, 0);
        assert!(forecast.at_risk_tests.is_empty());
    }

    #[test]
    fn the_scope_folds_in_the_impact_forecast_it_was_given() {
        let forecast = ImpactForecast {
            dependent_symbols: 7,
            public_api_touched: vec!["list_users()".into()],
            at_risk_tests: vec!["helper_works()".into()],
        };

        let scope = remediation_scope(&[site_in("a", "src/db.rs", 1)], &[], Some(&forecast), true);

        assert_eq!(scope.impact.as_ref().map(|i| i.dependent_symbols), Some(7));
    }

    #[test]
    fn a_scope_without_a_forecast_carries_no_impact_rather_than_a_zero_one() {
        // A zeroed forecast would read as "nothing depends on this". Absence
        // has to stay absent.
        let scope = remediation_scope(&[site_in("a", "src/db.rs", 1)], &[], None, true);

        assert!(scope.impact.is_none());
    }

    #[test]
    fn the_scope_lists_each_call_site_file_once() {
        let scope = remediation_scope(
            &[
                site_in("a", "src/db.rs", 10),
                site_in("b", "src/db.rs", 20),
                site_in("c", "src/api.rs", 5),
            ],
            &[],
            None,
            true,
        );

        assert_eq!(scope.review_files, vec!["src/api.rs", "src/db.rs"]);
    }

    #[test]
    fn the_scope_counts_distinct_calling_symbols_not_call_sites() {
        let scope = remediation_scope(
            &[
                site_in("a", "src/db.rs", 10),
                site_in("a", "src/db.rs", 20),
                site_in("b", "src/db.rs", 30),
            ],
            &[],
            None,
            true,
        );

        assert_eq!(scope.calling_symbols, 2);
    }

    #[test]
    fn the_scope_counts_the_entry_points_it_was_given() {
        let entry = EntryPoint {
            kind: EntryPointKind::HttpRoute,
            label: "/api/users".into(),
            id: "r1".into(),
            path: vec!["/api/users".into()],
        };

        let scope = remediation_scope(&[site_in("a", "src/db.rs", 1)], &[entry], None, true);

        assert_eq!(scope.exposed_entry_points, 1);
    }

    #[test]
    fn a_scope_derived_without_a_graph_says_so() {
        // An empty scope from a repository with no graph means "not measured".
        // Presenting that as a clean review surface would be a lie.
        let scope = remediation_scope(&[], &[], None, false);

        assert!(!scope.graph_backed);
        assert!(scope.review_files.is_empty());
    }

    #[test]
    fn a_prefixed_location_parses_to_its_line() {
        assert_eq!(parse_line(Some("L42")), Some(42));
    }

    #[test]
    fn a_bare_number_location_parses_to_its_line() {
        assert_eq!(parse_line(Some("7")), Some(7));
    }

    #[test]
    fn an_absent_or_unparseable_location_yields_no_line() {
        assert_eq!(parse_line(None), None);
        assert_eq!(parse_line(Some("somewhere")), None);
    }
}
