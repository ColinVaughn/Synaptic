use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::confidence::Confidence;
use crate::id::NodeId;

const SITES_KEY: &str = "sites";

/// Semantic identity for an edge in a simple graph.
///
/// Context is intentionally part of the key: for context-bearing relations,
/// such as HTTP couplings, GET and POST are distinct connections even when
/// their endpoints and relation are otherwise identical.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct EdgeKey {
    pub source: NodeId,
    pub target: NodeId,
    pub relation: String,
    pub context: Option<String>,
}

impl EdgeKey {
    /// Build a key using directed or undirected endpoint semantics.
    pub fn new(edge: &Edge, directed: bool) -> Self {
        let (source, target) = if directed || edge.source <= edge.target {
            (edge.source.clone(), edge.target.clone())
        } else {
            (edge.target.clone(), edge.source.clone())
        };
        Self {
            source,
            target,
            relation: edge.relation.clone(),
            context: edge.context.clone(),
        }
    }
}

/// One concrete source location that produced an edge.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeSite {
    pub source_file: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_location: Option<String>,
}

/// Collects provenance for a group of semantically identical edges.
///
/// Each input site's JSON is parsed once, membership checks are expected O(1),
/// and the flattened `sites` array is serialized only when [`Self::apply_to`]
/// materializes the completed group.
#[derive(Debug, Clone, Default)]
pub struct EdgeSiteAccumulator {
    sites: Vec<EdgeSite>,
    seen: HashSet<EdgeSite>,
}

impl EdgeSiteAccumulator {
    /// Start an accumulator with every site already represented by `edge`.
    pub fn new(edge: &Edge) -> Self {
        let mut accumulator = Self::default();
        accumulator.include_edge(edge);
        accumulator
    }

    /// Add every previously unseen site represented by `edge`, preserving the
    /// deterministic first-seen order.
    pub fn include_edge(&mut self, edge: &Edge) {
        edge.visit_sites(|site| self.push(site));
    }

    /// Rewrite collected sites while retaining the first occurrence if the
    /// rewrite makes two locations identical.
    pub fn rewrite(&mut self, mut rewrite: impl FnMut(&mut EdgeSite)) {
        let old_sites = std::mem::take(&mut self.sites);
        self.seen.clear();
        self.sites.reserve(old_sites.len());
        for mut site in old_sites {
            rewrite(&mut site);
            self.push(site);
        }
    }

    /// Write the accumulated sites to `edge` once. The winner's typed source
    /// location remains primary; all other sites are stored in flattened
    /// metadata in deterministic first-seen order.
    pub fn apply_to(self, edge: &mut Edge) {
        let primary = edge.primary_site();
        let additional: Vec<Value> = self
            .sites
            .into_iter()
            .filter(|site| Some(site) != primary.as_ref())
            .filter_map(|site| serde_json::to_value(site).ok())
            .collect();
        if additional.is_empty() {
            edge.extra.remove(SITES_KEY);
        } else {
            edge.extra
                .insert(SITES_KEY.to_string(), Value::Array(additional));
        }
    }

    fn push(&mut self, site: EdgeSite) {
        if self.seen.insert(site.clone()) {
            self.sites.push(site);
        }
    }
}

/// A directed relationship between two nodes. The required fields are the ones
/// in `REQUIRED_EDGE_FIELDS`. `_src`/`_tgt` build-layer direction markers are
/// intentionally NOT typed here (they are a petgraph-build concern stripped on
/// export); if present on input they land in `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub source: NodeId,
    pub target: NodeId,
    pub relation: String,
    pub confidence: Confidence,
    pub source_file: String,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub confidence_score: Option<f32>,
    #[serde(default = "default_weight")]
    pub weight: f32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub context: Option<String>,
    /// True for federated cross-repo edges; omitted when false.
    #[serde(skip_serializing_if = "is_false", default)]
    pub cross_repo: bool,

    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Edge {
    fn primary_site(&self) -> Option<EdgeSite> {
        if self.source_file.is_empty() && self.source_location.is_none() {
            None
        } else {
            Some(EdgeSite {
                source_file: self.source_file.clone(),
                source_location: self.source_location.clone(),
            })
        }
    }

    /// All distinct extraction sites represented by this semantic edge.
    ///
    /// The primary site remains in the typed fields for compatibility; any
    /// additional sites created by deduplication live in flattened sites
    /// metadata and are returned here as one deterministic list.
    pub fn sites(&self) -> Vec<EdgeSite> {
        let mut sites = Vec::new();
        let mut seen = HashSet::new();
        self.visit_sites(|site| {
            if seen.insert(site.clone()) {
                sites.push(site);
            }
        });
        sites
    }

    /// Preserve source sites from another edge with the same semantic key.
    pub fn merge_sites_from(&mut self, other: &Edge) {
        let mut sites = EdgeSiteAccumulator::new(self);
        sites.include_edge(other);
        sites.apply_to(self);
    }

    fn visit_sites(&self, mut visit: impl FnMut(EdgeSite)) {
        if let Some(site) = self.primary_site() {
            visit(site);
        }
        if let Some(Value::Array(values)) = self.extra.get(SITES_KEY) {
            for value in values {
                if let Ok(site) = EdgeSite::deserialize(value) {
                    visit(site);
                }
            }
        }
    }
}

fn default_weight() -> f32 {
    1.0
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Edge {
        Edge {
            source: NodeId("a".into()),
            target: NodeId("b".into()),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: "src/a.py".into(),
            source_location: Some("L10".into()),
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn required_keys_and_relation_string() {
        let json = serde_json::to_value(sample()).unwrap();
        let obj = json.as_object().unwrap();
        for k in ["source", "target", "relation", "confidence", "source_file"] {
            assert!(obj.contains_key(k), "missing {k}");
        }
        assert_eq!(obj["relation"], serde_json::json!("calls"));
        assert_eq!(obj["confidence"], serde_json::json!("EXTRACTED"));
        assert_eq!(obj["weight"], serde_json::json!(1.0));
    }

    #[test]
    fn omits_false_cross_repo_and_unset_options() {
        let obj = serde_json::to_value(sample()).unwrap();
        let obj = obj.as_object().unwrap().clone();
        assert!(!obj.contains_key("cross_repo")); // false -> omitted
        assert!(!obj.contains_key("confidence_score"));
        assert!(!obj.contains_key("context"));
    }

    #[test]
    fn weight_defaults_to_one_when_absent() {
        let raw = serde_json::json!({
            "source": "a", "target": "b", "relation": "imports",
            "confidence": "INFERRED", "source_file": "src/a.py",
            "confidence_score": 0.8
        });
        let e: Edge = serde_json::from_value(raw).unwrap();
        assert_eq!(e.weight, 1.0);
        assert_eq!(e.confidence, Confidence::Inferred);
        assert_eq!(e.confidence_score, Some(0.8));
    }

    #[test]
    fn direction_markers_land_in_extra() {
        let raw = serde_json::json!({
            "source": "a", "target": "b", "relation": "calls",
            "confidence": "EXTRACTED", "source_file": "src/a.py",
            "_src": "a", "_tgt": "b"
        });
        let e: Edge = serde_json::from_value(raw).unwrap();
        assert_eq!(e.extra.get("_src").unwrap(), "a");
        assert_eq!(e.extra.get("_tgt").unwrap(), "b");
    }

    #[test]
    fn edge_key_keeps_context_and_canonicalizes_only_undirected_endpoints() {
        let mut get = sample();
        get.context = Some("GET".into());
        let mut post = get.clone();
        post.context = Some("POST".into());
        assert_ne!(EdgeKey::new(&get, true), EdgeKey::new(&post, true));

        let mut reverse = get.clone();
        std::mem::swap(&mut reverse.source, &mut reverse.target);
        assert_eq!(EdgeKey::new(&get, false), EdgeKey::new(&reverse, false));
        assert_ne!(EdgeKey::new(&get, true), EdgeKey::new(&reverse, true));
    }

    #[test]
    fn merging_duplicate_edges_preserves_distinct_sites() {
        let mut first = sample();
        let mut second = sample();
        second.source_location = Some("L20".into());

        first.merge_sites_from(&second);

        assert_eq!(
            first.sites(),
            vec![
                EdgeSite {
                    source_file: "src/a.py".into(),
                    source_location: Some("L10".into()),
                },
                EdgeSite {
                    source_file: "src/a.py".into(),
                    source_location: Some("L20".into()),
                },
            ]
        );

        let roundtrip: Edge = serde_json::from_value(serde_json::to_value(first).unwrap()).unwrap();
        assert_eq!(roundtrip.sites().len(), 2);
    }

    #[test]
    fn accumulator_preserves_first_seen_site_order() {
        let mut first = sample();
        let mut second = sample();
        second.source_location = Some("L20".into());
        let mut third = sample();
        third.source_file = "src/b.py".into();
        third.source_location = Some("L30".into());

        let mut sites = EdgeSiteAccumulator::new(&first);
        sites.include_edge(&second);
        sites.include_edge(&first);
        sites.include_edge(&third);
        sites.apply_to(&mut first);

        assert_eq!(
            first.sites(),
            vec![
                EdgeSite {
                    source_file: "src/a.py".into(),
                    source_location: Some("L10".into()),
                },
                EdgeSite {
                    source_file: "src/a.py".into(),
                    source_location: Some("L20".into()),
                },
                EdgeSite {
                    source_file: "src/b.py".into(),
                    source_location: Some("L30".into()),
                },
            ]
        );
    }
}
