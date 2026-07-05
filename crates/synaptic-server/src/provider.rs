//! The graph-access abstraction the MCP tools run against.
//!
//! `GraphProvider` decouples the 28 tools from "one in-RAM graph". `Single` is
//! today's behavior (one materialized graph + its indexes); `Shard` (added later)
//! materializes per-repo shards on demand and fans out, so a federated serve never
//! holds the union in RAM. Single-repo is the one-shard case, so the existing
//! server tests are the regression net for the whole refactor.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use synaptic_core::{Edge, GraphData, Node, NodeId};
use synaptic_graph::{god_nodes, graph_stats, GodNode, GraphStats, KnowledgeGraph};
use synaptic_query::{
    rank_result_edges, resolve_detailed, DynamicHazardIndex, EdgeRef, QueryIndex, QueryResult,
    Recency, Resolution, ReverseImpactIndex, TraversalMode, DEFAULT_AFFECTED_RELATIONS,
};
use synaptic_store::{ShardStore, AFFECTED_INDEX_BLOB, QUERY_INDEX_BLOB};

use crate::aggregate::AggregateCache;

/// The default shard tag for a single (non-federated) graph.
pub(crate) const LOCAL: &str = "local";

/// Default number of shards kept materialized in RAM at once. Overridable via
/// `SYNAPTIC_SHARD_LRU`. Bounds the working-set memory for a federated serve.
const DEFAULT_SHARD_LRU: usize = 8;

/// A single shard materialized into the graph plus the indexes every tool needs.
pub struct MaterializedShard {
    pub kg: KnowledgeGraph,
    pub query_index: QueryIndex,
    pub affected_index: ReverseImpactIndex,
    pub hazard_index: DynamicHazardIndex,
}

impl MaterializedShard {
    /// Build all indexes from the graph.
    pub fn build(kg: KnowledgeGraph) -> Self {
        Self::from_prepared(kg, None, None)
    }

    /// Reuse already-deserialized indexes (persisted blobs) where present; build
    /// the rest. The hazard index is cheap and always rebuilt.
    pub fn from_prepared(
        kg: KnowledgeGraph,
        query_index: Option<QueryIndex>,
        affected_index: Option<ReverseImpactIndex>,
    ) -> Self {
        let query_index = query_index.unwrap_or_else(|| QueryIndex::build(&kg));
        let affected_index = affected_index
            .unwrap_or_else(|| ReverseImpactIndex::build(&kg, DEFAULT_AFFECTED_RELATIONS));
        let hazard_index = DynamicHazardIndex::build(&kg);
        MaterializedShard {
            kg,
            query_index,
            affected_index,
            hazard_index,
        }
    }
}

/// Pre-built indexes for the single-graph construction path (a redb single-shard
/// load supplies persisted ones; json supplies none).
#[derive(Default)]
pub struct Prepared {
    pub query_index: Option<QueryIndex>,
    pub affected_index: Option<ReverseImpactIndex>,
}

/// A node resolution annotated with the owning shard tag. The seed tools use the
/// tag to pick which shard to materialize and walk.
#[derive(Debug, PartialEq, Eq)]
pub enum ScopedResolution {
    Unique(String, NodeId),
    Ambiguous(Vec<(String, NodeId)>),
    NotFound,
}

/// How the tools reach the graph: one in-RAM graph (`Single`) or per-repo shards
/// materialized on demand (`Shard`). `Shard` is boxed — it carries the store,
/// LRU, and aggregate cache and is much larger than `Single`.
pub enum GraphProvider {
    Single(SingleGraph),
    Shard(Box<ShardProvider>),
}

/// A bounded, least-recently-used cache of materialized shards. Keeps the
/// working-set memory of a federated serve bounded: at most `cap` shards resident.
struct ShardLru {
    cap: usize,
    map: HashMap<String, Arc<MaterializedShard>>,
    /// Most-recent first.
    order: Vec<String>,
}

impl ShardLru {
    fn new(cap: usize) -> Self {
        ShardLru {
            cap: cap.max(1),
            map: HashMap::new(),
            order: Vec::new(),
        }
    }
    fn touch(&mut self, tag: &str) {
        self.order.retain(|t| t != tag);
        self.order.insert(0, tag.to_string());
    }
    fn get(&mut self, tag: &str) -> Option<Arc<MaterializedShard>> {
        let hit = self.map.get(tag).cloned();
        if hit.is_some() {
            self.touch(tag);
        }
        hit
    }
    fn put(&mut self, tag: String, shard: Arc<MaterializedShard>) {
        self.map.insert(tag.clone(), shard);
        self.touch(&tag);
        while self.order.len() > self.cap {
            if let Some(evict) = self.order.pop() {
                self.map.remove(&evict);
            }
        }
    }
    fn resident_count(&self) -> usize {
        self.map.len()
    }
}

/// Per-repo shards materialized on demand from a [`ShardStore`], with a bounded
/// LRU so a federated serve never holds the whole union in RAM.
pub struct ShardProvider {
    store: ShardStore,
    lru: Mutex<ShardLru>,
    /// Content fingerprint of all shards (sorted `tag:source_hash`); the cache key
    /// for streaming aggregates, bumped whenever any shard changes.
    version: String,
    /// Cross-repo edges (traversed only on the opt-in cross-repo path).
    bridge: Vec<Edge>,
    /// Whether walks may follow bridge edges into other shards
    /// (`SYNAPTIC_CROSS_REPO`; per-repo isolation otherwise).
    cross_repo: bool,
    #[allow(dead_code)] // consumed by the streaming aggregator tasks
    agg: AggregateCache,
}

impl ShardProvider {
    /// Materialize a shard (LRU-cached), loading its persisted indexes when present.
    fn get_shard(&self, tag: &str) -> Result<Arc<MaterializedShard>, String> {
        if let Some(s) = self.lru.lock().unwrap().get(tag) {
            return Ok(s);
        }
        let kg = self.store.materialize(tag).map_err(|e| e.to_string())?;
        let hash = self
            .store
            .manifest()
            .entry(tag)
            .map(|e| e.source_hash.clone());
        let (qi, ai) = match &hash {
            Some(h) => (
                self.store
                    .get_index_blob(tag, QUERY_INDEX_BLOB, h)
                    .ok()
                    .flatten()
                    .and_then(|b| QueryIndex::from_bytes(&b).ok()),
                self.store
                    .get_index_blob(tag, AFFECTED_INDEX_BLOB, h)
                    .ok()
                    .flatten()
                    .and_then(|b| ReverseImpactIndex::from_bytes(&b).ok()),
            ),
            None => (None, None),
        };
        let shard = Arc::new(MaterializedShard::from_prepared(kg, qi, ai));
        self.lru.lock().unwrap().put(tag.to_string(), shard.clone());
        Ok(shard)
    }

    /// Number of shards currently resident in the LRU (for tests/diagnostics).
    pub fn resident_count(&self) -> usize {
        self.lru.lock().unwrap().resident_count()
    }

    /// Stream every shard (one resident at a time, subject to the LRU).
    fn for_each(
        &self,
        f: &mut dyn FnMut(&str, &MaterializedShard) -> Result<(), String>,
    ) -> Result<(), String> {
        let tags: Vec<String> = self
            .store
            .list_shards()
            .iter()
            .map(|e| e.tag.clone())
            .collect();
        for tag in tags {
            let sh = self.get_shard(&tag)?;
            f(&tag, &sh)?;
        }
        Ok(())
    }

    /// Exact global `GraphStats`, computed once by streaming the shards + bridge.
    fn stats(&self) -> &GraphStats {
        self.agg.stats(|| {
            let mut acc = crate::aggregate::StatsAcc::default();
            let _ = self.for_each(&mut |_t, sh| {
                acc.add_shard(&sh.kg);
                Ok(())
            });
            acc.add_edges(&self.bridge);
            acc.finish()
        })
    }

    /// Exact global god-node ranking: each shard ranks its own nodes with a
    /// degree bump for their distinct bridge neighbors (degree is a distinct-
    /// neighbor count, and a bridge neighbor is always in another shard, so
    /// in-shard + bridge sets never overlap), then the shard lists are merged
    /// and re-ranked. Equals `god_nodes` on the union.
    fn god_nodes_all(&self) -> &[GodNode] {
        self.agg.god_nodes(|| {
            let mut nbrs: HashMap<NodeId, std::collections::HashSet<NodeId>> = HashMap::new();
            for e in &self.bridge {
                if e.source == e.target {
                    continue;
                }
                nbrs.entry(e.source.clone())
                    .or_default()
                    .insert(e.target.clone());
                nbrs.entry(e.target.clone())
                    .or_default()
                    .insert(e.source.clone());
            }
            let extra: HashMap<NodeId, usize> =
                nbrs.into_iter().map(|(id, s)| (id, s.len())).collect();
            let mut all: Vec<GodNode> = Vec::new();
            let _ = self.for_each(&mut |_t, sh| {
                all.extend(synaptic_graph::god_nodes_with_extra(&sh.kg, &extra));
                Ok(())
            });
            all.sort_by(|a, b| b.degree.cmp(&a.degree).then_with(|| a.id.cmp(&b.id)));
            all
        })
    }

    /// The merged global query index (global df; bridge pairs in the
    /// adjacency) plus each node's owning shard, computed once per content
    /// version by folding the shards' own (persisted) indexes one at a time.
    /// Memory: index metadata (tokens/adjacency ids) for every node, not the
    /// graphs themselves.
    fn global_query(&self) -> &(QueryIndex, HashMap<NodeId, String>) {
        self.agg.global_query(|| {
            let mut m = QueryIndex::empty();
            let mut owner: HashMap<NodeId, String> = HashMap::new();
            let _ = self.for_each(&mut |tag, sh| {
                m.absorb(&sh.query_index);
                for n in sh.kg.nodes() {
                    owner.insert(n.id.clone(), tag.to_string());
                }
                Ok(())
            });
            let pairs: Vec<(NodeId, NodeId)> = self
                .bridge
                .iter()
                .map(|e| (e.source.clone(), e.target.clone()))
                .collect();
            m.add_bridge_pairs(&pairs);
            m.finalize_merge();
            (m, owner)
        })
    }

    /// Exact global community map: communities are graph-global ids (federation
    /// re-clusters the composed graph), so a community can span shards; merge
    /// per-shard member lists and normalize order. Equals `communities_of` on
    /// the union.
    fn communities_all(&self) -> &BTreeMap<u32, Vec<NodeId>> {
        self.agg.communities(|| {
            let mut merged: BTreeMap<u32, Vec<NodeId>> = BTreeMap::new();
            let _ = self.for_each(&mut |_t, sh| {
                for (cid, members) in communities_of(&sh.kg) {
                    merged.entry(cid).or_default().extend(members);
                }
                Ok(())
            });
            for v in merged.values_mut() {
                v.sort();
            }
            merged
        })
    }
}

/// Content fingerprint of a store: sorted `tag:source_hash` joined; equality
/// key for the aggregate cache (changes when any shard changes).
fn shard_version(store: &ShardStore) -> String {
    let mut parts: Vec<String> = store
        .list_shards()
        .iter()
        .map(|e| format!("{}:{}", e.tag, e.source_hash))
        .collect();
    parts.sort();
    parts.join("|")
}

/// One materialized graph plus the whole-graph aggregates the tools read. The
/// aggregates are computed once at construction (== today's eager prebuild);
/// later tasks move them behind the streaming aggregate cache for the shard case.
pub struct SingleGraph {
    shard: Arc<MaterializedShard>,
    communities: BTreeMap<u32, Vec<NodeId>>,
    stats: GraphStats,
    god_nodes_all: Vec<GodNode>,
}

impl GraphProvider {
    /// Build a single-graph provider from node-link data.
    pub fn single(gd: GraphData, prepared: Prepared) -> Self {
        Self::single_from_kg(KnowledgeGraph::from_graph_data(gd), prepared)
    }

    /// Build a shard provider over a federated store. Shards are materialized on
    /// demand; the bridge is loaded up front (small).
    pub fn from_store(store: ShardStore) -> Self {
        let version = shard_version(&store);
        let bridge = store.read_bridge_edges().unwrap_or_default();
        let cap = std::env::var("SYNAPTIC_SHARD_LRU")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_SHARD_LRU);
        let cross_repo = std::env::var("SYNAPTIC_CROSS_REPO")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        GraphProvider::Shard(Box::new(ShardProvider {
            store,
            lru: Mutex::new(ShardLru::new(cap)),
            version: version.clone(),
            bridge,
            cross_repo,
            agg: AggregateCache::new(version),
        }))
    }

    /// Build a single-graph provider from an already-built graph (the reload path).
    pub fn single_from_kg(kg: KnowledgeGraph, prepared: Prepared) -> Self {
        let communities = communities_of(&kg);
        let stats = graph_stats(&kg);
        let god_nodes_all = god_nodes(&kg, usize::MAX);
        let shard = Arc::new(MaterializedShard::from_prepared(
            kg,
            prepared.query_index,
            prepared.affected_index,
        ));
        GraphProvider::Single(SingleGraph {
            shard,
            communities,
            stats,
            god_nodes_all,
        })
    }

    /// Repo tags (shard names). A single graph has the one `local` shard; a
    /// federated store lists its shards in tag order.
    pub fn tags(&self) -> Vec<String> {
        match self {
            GraphProvider::Single(_) => vec![LOCAL.to_string()],
            GraphProvider::Shard(s) => s
                .store
                .list_shards()
                .iter()
                .map(|e| e.tag.clone())
                .collect(),
        }
    }

    /// The materialized shard for a tag (LRU-cached for the shard case).
    pub fn shard(&self, tag: &str) -> Result<Arc<MaterializedShard>, String> {
        match self {
            GraphProvider::Single(s) => Ok(s.shard.clone()),
            GraphProvider::Shard(s) => s.get_shard(tag),
        }
    }

    /// Stream each shard (for exact aggregation). One shard resident at a time
    /// (subject to the LRU), so the union is never all in RAM at once.
    pub fn for_each_shard(
        &self,
        f: &mut dyn FnMut(&str, &MaterializedShard) -> Result<(), String>,
    ) -> Result<(), String> {
        match self {
            GraphProvider::Single(s) => f(LOCAL, &s.shard),
            GraphProvider::Shard(s) => s.for_each(f),
        }
    }

    /// Cross-repo bridge edges (empty for a single graph).
    pub fn bridge(&self) -> &[Edge] {
        match self {
            GraphProvider::Single(_) => &[],
            GraphProvider::Shard(s) => &s.bridge,
        }
    }

    /// Resolve a free-text query to a node, annotated with its shard tag. For a
    /// single graph this is today's resolver (tag `local`). For a federated store
    /// it fans out: each shard resolves independently and the hits are merged —
    /// one hit overall is `Unique`, several is `Ambiguous` (each candidate carries
    /// its repo).
    pub fn resolve(&self, query: &str) -> ScopedResolution {
        match self {
            GraphProvider::Single(s) => match resolve_detailed(&s.shard.kg, query) {
                Resolution::Unique(id) => ScopedResolution::Unique(LOCAL.to_string(), id),
                Resolution::Ambiguous(ids) => ScopedResolution::Ambiguous(
                    ids.into_iter().map(|id| (LOCAL.to_string(), id)).collect(),
                ),
                Resolution::NotFound => ScopedResolution::NotFound,
            },
            GraphProvider::Shard(_) => {
                let mut hits: Vec<(String, NodeId)> = Vec::new();
                let mut multi_in_one = false;
                let _ = self.for_each_shard(&mut |tag, sh| {
                    match resolve_detailed(&sh.kg, query) {
                        Resolution::Unique(id) => hits.push((tag.to_string(), id)),
                        Resolution::Ambiguous(ids) => {
                            multi_in_one = true;
                            hits.extend(ids.into_iter().map(|id| (tag.to_string(), id)));
                        }
                        Resolution::NotFound => {}
                    }
                    Ok(())
                });
                match hits.len() {
                    0 => ScopedResolution::NotFound,
                    1 if !multi_in_one => {
                        let (tag, id) = hits.into_iter().next().expect("len == 1");
                        ScopedResolution::Unique(tag, id)
                    }
                    _ => ScopedResolution::Ambiguous(hits),
                }
            }
        }
    }

    /// Query-subgraph retrieval (ranking + result edges). `Single` is today's
    /// path, bit-identical. `Shard` ranks on the cached global index (global
    /// df, bridge adjacency), then collects result edges from each owning
    /// shard plus the bridge; equals the union query. A shard that fails to
    /// materialize contributes no edges (its ranked ids still list).
    pub fn query_with_recency(
        &self,
        question: &str,
        max_nodes: usize,
        mode: TraversalMode,
        recency: Option<&Recency>,
    ) -> QueryResult {
        match self {
            GraphProvider::Single(s) => s.shard.query_index.query_with_recency(
                &s.shard.kg,
                question,
                max_nodes,
                mode,
                recency,
            ),
            GraphProvider::Shard(sp) => {
                let (gidx, owner) = sp.global_query();
                let ranked = gidx.rank(question, max_nodes, mode, recency);
                let node_set: std::collections::HashSet<&NodeId> = ranked.nodes.iter().collect();
                let mut tags: Vec<&str> = ranked
                    .nodes
                    .iter()
                    .filter_map(|id| owner.get(id).map(String::as_str))
                    .collect();
                tags.sort_unstable();
                tags.dedup();
                let mut edges: Vec<EdgeRef> = Vec::new();
                for tag in tags {
                    if let Ok(sh) = sp.get_shard(tag) {
                        edges.extend(
                            sh.kg
                                .edges()
                                .filter(|e| {
                                    node_set.contains(&e.source) && node_set.contains(&e.target)
                                })
                                .map(|e| EdgeRef {
                                    source: e.source.clone(),
                                    target: e.target.clone(),
                                    relation: e.relation.clone(),
                                }),
                        );
                    }
                }
                edges.extend(
                    sp.bridge
                        .iter()
                        .filter(|e| node_set.contains(&e.source) && node_set.contains(&e.target))
                        .map(|e| EdgeRef {
                            source: e.source.clone(),
                            target: e.target.clone(),
                            relation: e.relation.clone(),
                        }),
                );
                let edges = rank_result_edges(edges, &ranked.nodes, &ranked.scores);
                QueryResult {
                    seeds: ranked.seeds,
                    nodes: ranked.nodes,
                    scores: ranked.scores,
                    edges,
                }
            }
        }
    }

    /// structural_search execution: SYNQL query, named pattern, or file
    /// outline (precedence in that order, matching the tool contract).
    /// `Single` runs against the one graph, bit-identical to today. `Shard`
    /// folds a query/outline across shards with `LIMIT` deferred
    /// (`FederatedRun`), and unions a detector pattern's rows per shard.
    /// Relationship matches that would cross the federation bridge are out of
    /// scope (per-repo isolation); a pattern error only propagates when no
    /// shard succeeded (mirroring the union, where one repo's missing
    /// communities does not disable the detector for the rest).
    pub fn structural_search(
        &self,
        query: Option<&str>,
        pattern: Option<&str>,
        file: Option<&str>,
    ) -> Result<synaptic_synql::QueryResult, String> {
        match self {
            GraphProvider::Single(s) => {
                let raw = if let Some(p) = pattern {
                    synaptic_synql::patterns::run_pattern(&s.shard.kg, p)
                } else if let Some(q) = query {
                    synaptic_synql::run(&s.shard.kg, q)
                } else if let Some(f) = file {
                    synaptic_synql::file_outline(&s.shard.kg, f)
                } else {
                    return Err("Provide a SYNQL query, a pattern name, or a file.".to_string());
                };
                raw.map_err(|e| format!("search error: {e}"))
            }
            GraphProvider::Shard(sp) => {
                if let Some(p) = pattern {
                    let mut rows: Vec<Vec<NodeId>> = Vec::new();
                    let mut columns: Vec<String> = Vec::new();
                    let mut any_ok = false;
                    let mut last_err: Option<String> = None;
                    let _ = sp.for_each(&mut |_t, sh| {
                        match synaptic_synql::patterns::run_pattern(&sh.kg, p) {
                            Ok(r) => {
                                any_ok = true;
                                columns = r.columns;
                                rows.extend(r.rows);
                            }
                            Err(e) => last_err = Some(format!("search error: {e}")),
                        }
                        Ok(())
                    });
                    if !any_ok {
                        return Err(
                            last_err.unwrap_or_else(|| "search error: no shards".to_string())
                        );
                    }
                    rows.sort();
                    rows.dedup();
                    return Ok(synaptic_synql::QueryResult {
                        columns,
                        rows,
                        aggregates: None,
                    });
                }
                let mut fr = if let Some(q) = query {
                    synaptic_synql::FederatedRun::query(q)
                } else if let Some(f) = file {
                    synaptic_synql::FederatedRun::file_outline(f)
                } else {
                    return Err("Provide a SYNQL query, a pattern name, or a file.".to_string());
                }
                .map_err(|e| format!("search error: {e}"))?;
                let _ = sp.for_each(&mut |_t, sh| {
                    fr.add(&sh.kg);
                    Ok(())
                });
                Ok(fr.finish())
            }
        }
    }

    /// Per-repo `(nodes, edges)` counts for the federation tools. Nodes count
    /// by their `repo` tag; edges attribute to the SOURCE node's repo,
    /// including bridge edges, matching the union graph (a cross-repo edge
    /// counts toward its source repo). Untagged nodes are excluded, so a
    /// single-repo graph yields an empty map.
    pub fn repo_counts(&self) -> BTreeMap<String, (usize, usize)> {
        fn counts_of(kg: &KnowledgeGraph, counts: &mut BTreeMap<String, (usize, usize)>) {
            for n in kg.nodes() {
                if let Some(r) = n.repo.as_deref() {
                    counts.entry(r.to_string()).or_default().0 += 1;
                }
            }
            for e in kg.edges() {
                if let Some(r) = kg.node(&e.source).and_then(|n| n.repo.as_deref()) {
                    counts.entry(r.to_string()).or_default().1 += 1;
                }
            }
        }
        let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        match self {
            GraphProvider::Single(s) => counts_of(&s.shard.kg, &mut counts),
            GraphProvider::Shard(sp) => {
                let _ = sp.for_each(&mut |_t, sh| {
                    counts_of(&sh.kg, &mut counts);
                    // Bridge edges live outside the shards; attribute each to
                    // its source repo while that shard is resident.
                    for e in &sp.bridge {
                        if let Some(r) = sh.kg.node(&e.source).and_then(|n| n.repo.as_deref()) {
                            counts.entry(r.to_string()).or_default().1 += 1;
                        }
                    }
                    Ok(())
                });
            }
        }
        counts
    }

    /// Change forecast over the changed-file set. `Single` is today's path.
    /// `Shard` folds every shard (a shard none of whose files changed
    /// contributes nothing); each walk stays within its shard (per-repo
    /// isolation), and totals/risk/summary assemble globally.
    pub fn forecast(
        &self,
        changed_files: &[String],
        opts: &synaptic_predict::ForecastOptions,
    ) -> synaptic_predict::ChangeForecast {
        match self {
            GraphProvider::Single(s) => synaptic_predict::forecast_changes_with_index(
                &s.shard.kg,
                &s.shard.affected_index,
                changed_files,
                opts,
            ),
            GraphProvider::Shard(sp) => {
                let mut fold = synaptic_predict::ForecastFold::new(changed_files, opts);
                let _ = sp.for_each(&mut |_t, sh| {
                    fold.add(&sh.kg, &sh.affected_index);
                    Ok(())
                });
                fold.finish()
            }
        }
    }

    /// Clone the node behind `id`. `Single` reads the one graph; `Shard` finds
    /// the owning shard via the global owner map. The clone keeps renderers
    /// borrow-free across both providers.
    pub fn node_cloned(&self, id: &NodeId) -> Option<Node> {
        match self {
            GraphProvider::Single(s) => s.shard.kg.node(id).cloned(),
            GraphProvider::Shard(sp) => {
                let (_, owner) = sp.global_query();
                let tag = owner.get(id)?;
                sp.get_shard(tag).ok()?.kg.node(id).cloned()
            }
        }
    }

    /// Visit every node (all shards, one resident at a time for `Shard`).
    pub fn for_each_node(&self, f: &mut dyn FnMut(&Node)) {
        match self {
            GraphProvider::Single(s) => {
                for n in s.shard.kg.nodes() {
                    f(n);
                }
            }
            GraphProvider::Shard(sp) => {
                let _ = sp.for_each(&mut |_t, sh| {
                    for n in sh.kg.nodes() {
                        f(n);
                    }
                    Ok(())
                });
            }
        }
    }

    /// True when this provider serves per-repo shards (the federated store).
    pub fn is_sharded(&self) -> bool {
        matches!(self, GraphProvider::Shard(_))
    }

    /// Whether cross-repo bridge traversal is opted in (`SYNAPTIC_CROSS_REPO`).
    /// Always false for a single graph (its edges are all in-graph anyway).
    pub fn cross_repo(&self) -> bool {
        match self {
            GraphProvider::Single(_) => false,
            GraphProvider::Shard(sp) => sp.cross_repo,
        }
    }

    /// Builder override for the cross-repo opt-in (tests; the env is read at
    /// construction). No effect on a single graph.
    pub fn with_cross_repo(mut self, on: bool) -> Self {
        if let GraphProvider::Shard(sp) = &mut self {
            sp.cross_repo = on;
        }
        self
    }

    /// Builder override for the resident-shard LRU cap (tests; the env is read
    /// at construction). No effect on a single graph.
    pub fn with_lru_cap(mut self, cap: usize) -> Self {
        if let GraphProvider::Shard(sp) = &mut self {
            sp.lru = Mutex::new(ShardLru::new(cap));
        }
        self
    }

    /// The bridge edges incident to `id` (either endpoint). Empty for a single
    /// graph. Cloned: the bridge is small relative to any shard.
    pub fn bridge_edges_of(&self, id: &NodeId) -> Vec<Edge> {
        match self {
            GraphProvider::Single(_) => Vec::new(),
            GraphProvider::Shard(sp) => sp
                .bridge
                .iter()
                .filter(|e| &e.source == id || &e.target == id)
                .cloned()
                .collect(),
        }
    }

    /// The bridge relation connecting `a` and `b` (either direction), if any;
    /// the lexicographically smallest wins for determinism. Lets a path render
    /// annotate its cross-shard hop.
    pub fn bridge_relation(&self, a: &NodeId, b: &NodeId) -> Option<String> {
        match self {
            GraphProvider::Single(_) => None,
            GraphProvider::Shard(sp) => sp
                .bridge
                .iter()
                .filter(|e| {
                    (&e.source == a && &e.target == b) || (&e.source == b && &e.target == a)
                })
                .map(|e| e.relation.clone())
                .min(),
        }
    }

    /// Visit every edge: all shards' edges plus the cross-repo bridge, which
    /// together are exactly the union graph's edge set.
    pub fn for_each_edge(&self, f: &mut dyn FnMut(&Edge)) {
        match self {
            GraphProvider::Single(s) => {
                for e in s.shard.kg.edges() {
                    f(e);
                }
            }
            GraphProvider::Shard(sp) => {
                let _ = sp.for_each(&mut |_t, sh| {
                    for e in sh.kg.edges() {
                        f(e);
                    }
                    Ok(())
                });
                for e in &sp.bridge {
                    f(e);
                }
            }
        }
    }

    /// The materialized shard owning `id` (`Single`: the one shard; `Shard`:
    /// owner-map lookup). `None` for an unknown id or a failed materialization.
    pub fn owner_shard(&self, id: &NodeId) -> Option<Arc<MaterializedShard>> {
        match self {
            GraphProvider::Single(s) => Some(s.shard.clone()),
            GraphProvider::Shard(sp) => {
                let (_, owner) = sp.global_query();
                sp.get_shard(owner.get(id)?).ok()
            }
        }
    }

    /// Degree of `id` within its owning shard, plus its distinct bridge
    /// neighbors (matching the union graph's distinct-neighbor degree, the
    /// same rule the god-node ranking uses). 0 for an unknown id.
    pub fn degree_of(&self, id: &NodeId) -> usize {
        match self {
            GraphProvider::Single(s) => s.shard.kg.degree(id),
            GraphProvider::Shard(sp) => {
                let (_, owner) = sp.global_query();
                let in_shard = owner
                    .get(id)
                    .and_then(|tag| sp.get_shard(tag).ok())
                    .map(|sh| sh.kg.degree(id))
                    .unwrap_or(0);
                let bridge_nbrs: std::collections::HashSet<&NodeId> = sp
                    .bridge
                    .iter()
                    .filter_map(|e| {
                        if &e.source == id {
                            Some(&e.target)
                        } else if &e.target == id {
                            Some(&e.source)
                        } else {
                            None
                        }
                    })
                    .collect();
                in_shard + bridge_nbrs.len()
            }
        }
    }

    /// Total opaque (keyless) dynamic-dispatch sites across all shards.
    pub fn opaque_hazards_total(&self) -> usize {
        match self {
            GraphProvider::Single(s) => s.shard.hazard_index.opaque_total(),
            GraphProvider::Shard(sp) => {
                let mut total = 0usize;
                let _ = sp.for_each(&mut |_t, sh| {
                    total += sh.hazard_index.opaque_total();
                    Ok(())
                });
                total
            }
        }
    }

    // --- Whole-graph accessors the tool handlers read through. For `Single` they
    // borrow the one shard's graph/indexes and its eager aggregates. The `Shard`
    // arms panic: every tool must be migrated to fan-out (resolve+shard / stream)
    // before a federated store is served, so these are a missed-migration guard
    // and are unreachable once Task 13 wires `Shard` into construction. ---

    pub fn kg(&self) -> &KnowledgeGraph {
        match self {
            GraphProvider::Single(s) => &s.shard.kg,
            GraphProvider::Shard(_) => unmigrated("kg"),
        }
    }

    pub fn query_index(&self) -> &QueryIndex {
        match self {
            GraphProvider::Single(s) => &s.shard.query_index,
            GraphProvider::Shard(_) => unmigrated("query_index"),
        }
    }

    pub fn affected_index(&self) -> &ReverseImpactIndex {
        match self {
            GraphProvider::Single(s) => &s.shard.affected_index,
            GraphProvider::Shard(_) => unmigrated("affected_index"),
        }
    }

    pub fn hazard_index(&self) -> &DynamicHazardIndex {
        match self {
            GraphProvider::Single(s) => &s.shard.hazard_index,
            GraphProvider::Shard(_) => unmigrated("hazard_index"),
        }
    }

    pub fn communities(&self) -> &BTreeMap<u32, Vec<NodeId>> {
        match self {
            GraphProvider::Single(s) => &s.communities,
            GraphProvider::Shard(s) => s.communities_all(),
        }
    }

    pub fn stats(&self) -> &GraphStats {
        match self {
            GraphProvider::Single(s) => &s.stats,
            GraphProvider::Shard(s) => s.stats(),
        }
    }

    pub fn god_nodes_all(&self) -> &[GodNode] {
        match self {
            GraphProvider::Single(s) => &s.god_nodes_all,
            GraphProvider::Shard(s) => s.god_nodes_all(),
        }
    }
}

/// A whole-graph accessor was called on a `ShardProvider` — the calling tool has
/// not been migrated to per-shard fan-out yet. Never reached in shipped builds
/// (construction only makes a `Shard` once every tool is migrated).
fn unmigrated(method: &str) -> ! {
    panic!("BUG: GraphProvider::{method}() called on a ShardProvider; this tool must be migrated to fan-out before serving a federated store");
}

/// Build the community-membership map: community id -> the real code symbols that
/// belong to it. Skips external stubs and non-code-symbol nodes (docs/config),
/// matching the server's long-standing community listing. Moved here from the
/// server so the provider owns aggregate construction.
fn communities_of(kg: &KnowledgeGraph) -> BTreeMap<u32, Vec<NodeId>> {
    let mut communities: BTreeMap<u32, Vec<NodeId>> = BTreeMap::new();
    for n in kg.nodes() {
        if n.is_external_stub() || !n.is_code_symbol() {
            continue;
        }
        if let Some(c) = n.community {
            communities.entry(c).or_default().push(n.id.clone());
        }
    }
    for v in communities.values_mut() {
        v.sort();
    }
    communities
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};

    fn gd() -> GraphData {
        GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                Node {
                    id: NodeId("a".into()),
                    label: "a".into(),
                    file_type: FileType::Code,
                    source_file: "a.rs".into(),
                    source_location: None,
                    community: None,
                    repo: None,
                    extra: Map::new(),
                },
                Node {
                    id: NodeId("b".into()),
                    label: "b".into(),
                    file_type: FileType::Code,
                    source_file: "b.rs".into(),
                    source_location: None,
                    community: None,
                    repo: None,
                    extra: Map::new(),
                },
            ],
            links: vec![Edge {
                source: NodeId("a".into()),
                target: NodeId("b".into()),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                source_file: "a.rs".into(),
                source_location: None,
                confidence_score: None,
                weight: 1.0,
                context: None,
                cross_repo: false,
                extra: Map::new(),
            }],
            hyperedges: vec![],
            built_at_commit: None,
        }
    }

    fn rnode(id: &str, repo: Option<&str>) -> Node {
        Node {
            id: NodeId(id.into()),
            label: id.into(),
            file_type: FileType::Code,
            source_file: format!("{}/{id}.rs", repo.unwrap_or("x")),
            source_location: None,
            community: None,
            repo: repo.map(|r| r.into()),
            extra: Map::new(),
        }
    }
    fn redge(s: &str, t: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: format!("{s}.rs"),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn labeled(id: &str, label: &str, repo: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: format!("{repo}/{id}.rs"),
            source_location: None,
            community: None,
            repo: Some(repo.into()),
            extra: Map::new(),
        }
    }

    fn cnode(id: &str, repo: &str, community: u32) -> Node {
        Node {
            id: NodeId(id.into()),
            label: id.into(),
            file_type: FileType::Code,
            source_file: format!("{repo}/{id}.rs"),
            source_location: None,
            community: Some(community),
            repo: Some(repo.into()),
            extra: Map::new(),
        }
    }
    fn xedge(s: &str, t: &str, rel: &str, conf: Confidence, cross: bool) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: rel.into(),
            confidence: conf,
            source_file: format!("{s}.rs"),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: cross,
            extra: Map::new(),
        }
    }

    /// 2-repo graph with communities + a flagged cross-repo edge, for comparing a
    /// fan-out aggregate against the same tool over the unified graph.
    fn two_repo_gd() -> GraphData {
        GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                cnode("a", "billing", 1),
                cnode("b", "billing", 1),
                cnode("c", "web", 2),
            ],
            links: vec![
                xedge("a", "b", "calls", Confidence::Extracted, false), // intra billing
                xedge("b", "c", "calls", Confidence::Inferred, true),   // cross-repo
            ],
            hyperedges: vec![],
            built_at_commit: None,
        }
    }

    fn shard_provider_over(gd: &GraphData) -> GraphProvider {
        use synaptic_store::{migrate, ShardStore};
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let store_dir = dir.path().join("store");
        let mut store = ShardStore::open(&store_dir).unwrap();
        migrate::migrate_into(&mut store, gd).unwrap();
        GraphProvider::from_store(ShardStore::open(&store_dir).unwrap())
    }

    #[test]
    fn graph_stats_streaming_equals_union() {
        let gd = two_repo_gd();
        let unified = GraphProvider::single(gd.clone(), Prepared::default());
        let sharded = shard_provider_over(&gd);
        assert_eq!(
            sharded.stats(),
            unified.stats(),
            "fan-out graph_stats must equal the union (nodes/edges/communities/cross_repo)"
        );
    }

    #[test]
    fn god_nodes_streaming_equals_union() {
        let gd = two_repo_gd();
        let unified = GraphProvider::single(gd.clone(), Prepared::default());
        let sharded = shard_provider_over(&gd);
        assert!(
            !unified.god_nodes_all().is_empty(),
            "fixture must produce candidates or the equality proves nothing"
        );
        assert_eq!(
            sharded.god_nodes_all(),
            unified.god_nodes_all(),
            "fan-out god_nodes must equal the union (bridge edges count toward degree)"
        );
        // The cross-repo hub only outranks its peers when bridge edges count.
        assert_eq!(
            sharded.god_nodes_all()[0].id.0,
            "b",
            "bridge bump ranks b first"
        );
    }

    #[test]
    fn communities_streaming_equals_union() {
        let gd = two_repo_gd();
        let unified = GraphProvider::single(gd.clone(), Prepared::default());
        let sharded = shard_provider_over(&gd);
        assert!(
            !unified.communities().is_empty(),
            "fixture must produce communities or the equality proves nothing"
        );
        assert_eq!(
            sharded.communities(),
            unified.communities(),
            "fan-out communities must equal the union"
        );
    }

    #[test]
    fn query_streaming_equals_union() {
        // Real multi-token labels (two_repo_gd's one-char labels tokenize to
        // nothing); "payment" matches nodes in BOTH repos so the global df
        // (2, not each shard's 1) is what keeps scores identical.
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                labeled("b_pay", "PaymentService", "billing"),
                labeled("b_util", "format_invoice", "billing"),
                labeled("w_pay", "PaymentWidget", "web"),
            ],
            links: vec![
                xedge("b_pay", "b_util", "calls", Confidence::Extracted, false),
                xedge("b_pay", "w_pay", "calls", Confidence::Extracted, true),
            ],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let unified = GraphProvider::single(gd.clone(), Prepared::default());
        let sharded = shard_provider_over(&gd);
        let u = unified.query_with_recency("payment", 10, TraversalMode::Bfs, None);
        let s = sharded.query_with_recency("payment", 10, TraversalMode::Bfs, None);
        assert!(!u.nodes.is_empty(), "fixture must rank something");
        assert_eq!(s.seeds, u.seeds, "global df must make seeds identical");
        assert_eq!(s.nodes, u.nodes, "bridge adjacency must drive expansion");
        assert_eq!(s.scores, u.scores);
        assert_eq!(
            s.edges, u.edges,
            "in-shard + bridge result edges must match the union"
        );
        assert!(
            u.edges
                .iter()
                .any(|e| e.source.0 == "b_pay" && e.target.0 == "w_pay"),
            "fixture must surface the bridge edge or the equality is weak"
        );
    }

    #[test]
    fn structural_search_streaming_equals_union() {
        let gd = two_repo_gd();
        let unified = GraphProvider::single(gd.clone(), Prepared::default());
        let sharded = shard_provider_over(&gd);
        let q = Some("MATCH (n) RETURN n LIMIT 2");
        let u = unified.structural_search(q, None, None).unwrap();
        let s = sharded.structural_search(q, None, None).unwrap();
        assert!(!u.rows.is_empty(), "fixture must match something");
        assert_eq!(s, u, "row merge + deferred LIMIT must equal the union");

        let qa = Some("MATCH (n) RETURN n.community, count(n)");
        let ua = unified.structural_search(qa, None, None).unwrap();
        let sa = sharded.structural_search(qa, None, None).unwrap();
        assert_eq!(sa, ua, "aggregate counts must sum across shards");
    }

    #[test]
    fn repo_counts_streaming_equals_union() {
        let gd = two_repo_gd();
        let unified = GraphProvider::single(gd.clone(), Prepared::default());
        let sharded = shard_provider_over(&gd);
        let u = unified.repo_counts();
        let s = sharded.repo_counts();
        assert_eq!(
            u.get("billing"),
            Some(&(2, 2)),
            "billing: 2 nodes; intra edge + bridge edge both source here: {u:?}"
        );
        assert_eq!(
            s, u,
            "per-shard counts + bridge attribution must equal the union"
        );
    }

    #[test]
    fn lru_bounds_resident_shards() {
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                rnode("a", Some("r1")),
                rnode("b", Some("r2")),
                rnode("c", Some("r3")),
            ],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let p = shard_provider_over(&gd).with_lru_cap(1);
        for tag in p.tags() {
            let _ = p.shard(&tag).unwrap();
        }
        if let GraphProvider::Shard(sp) = &p {
            assert_eq!(
                sp.resident_count(),
                1,
                "the LRU must evict down to its cap as shards stream"
            );
        } else {
            panic!("expected shard provider");
        }
    }

    #[test]
    fn resolve_fans_out_across_shards() {
        use synaptic_store::{migrate, ShardStore};
        // "Widget" exists in both repos (ambiguous); "Solo" only in billing.
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                labeled("b_widget", "Widget", "billing"),
                labeled("b_solo", "Solo", "billing"),
                labeled("w_widget", "Widget", "web"),
            ],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let mut store = ShardStore::open(&store_dir).unwrap();
        migrate::migrate_into(&mut store, &gd).unwrap();
        let p = GraphProvider::from_store(ShardStore::open(&store_dir).unwrap());

        // unique to one shard -> Unique with that tag
        match p.resolve("Solo") {
            ScopedResolution::Unique(tag, _) => assert_eq!(tag, "billing"),
            other => panic!("expected Unique billing, got {other:?}"),
        }
        // present in both shards -> Ambiguous with both repos represented
        match p.resolve("Widget") {
            ScopedResolution::Ambiguous(hits) => {
                let tags: std::collections::HashSet<&str> =
                    hits.iter().map(|(t, _)| t.as_str()).collect();
                assert!(tags.contains("billing") && tags.contains("web"));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        // missing -> NotFound
        assert_eq!(p.resolve("Nonexistent"), ScopedResolution::NotFound);
    }

    #[test]
    fn shard_provider_lists_and_materializes_shards() {
        use synaptic_store::{migrate, ShardStore};
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                rnode("a", Some("billing")),
                rnode("b", Some("billing")),
                rnode("c", Some("web")),
            ],
            links: vec![redge("a", "b"), redge("b", "c")], // a->b intra, b->c cross-repo
            hyperedges: vec![],
            built_at_commit: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let mut store = ShardStore::open(&store_dir).unwrap();
        migrate::migrate_into(&mut store, &gd).unwrap();
        let store = ShardStore::open(&store_dir).unwrap();

        let p = GraphProvider::from_store(store);
        assert_eq!(p.tags(), vec!["billing".to_string(), "web".to_string()]);
        assert_eq!(p.shard("billing").unwrap().kg.node_count(), 2);
        assert_eq!(p.shard("web").unwrap().kg.node_count(), 1);

        let mut total = 0usize;
        p.for_each_shard(&mut |_t, sh| {
            total += sh.kg.node_count();
            Ok(())
        })
        .unwrap();
        assert_eq!(total, 3);

        assert_eq!(p.bridge().len(), 1, "the cross-repo edge is in the bridge");
    }

    #[test]
    fn single_graph_exposes_one_shard() {
        let p = GraphProvider::single(gd(), Prepared::default());
        assert_eq!(p.tags(), vec!["local".to_string()]);

        let s = p.shard("local").unwrap();
        assert_eq!(s.kg.node_count(), 2);

        // for_each_shard visits exactly the one shard
        let mut seen = 0usize;
        p.for_each_shard(&mut |_tag, sh| {
            seen += sh.kg.node_count();
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, 2);

        assert!(p.bridge().is_empty());
    }
}
