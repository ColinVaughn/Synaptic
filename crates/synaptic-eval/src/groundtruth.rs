//! Hand-labeled ground truth for one corpus fixture, plus a resolver that maps
//! "relative/path::symbol" labels to the NodeId the extractor produced.
//!
//! Labels are written the way a human reads the code (`src/router.rs::route`).
//! The extractor flattens that into an opaque id (`src_router_route`) and a
//! display label (`route()`), so [`resolve_label`] bridges the two by matching
//! on the source file and the bare symbol name.

use serde::Deserialize;

use synaptic_core::{GraphData, NodeId};

/// A labeled caller -> callee call edge (the oracle includes cross-file calls,
/// so recall measured against it reflects the real call graph, not just the
/// subset the extractor is designed to resolve).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CallEdge {
    pub from: String,
    pub to: String,
}

/// A labeled test -> covered-code linkage.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TestLink {
    pub test: String,
    pub covers: Vec<String>,
}

/// A labeled blast-radius expectation: a seed change, its true transitive set,
/// and (optionally) distractor nodes that must NOT be reported as affected so
/// the metric penalizes an over-broad impact set, not just misses.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Blast {
    pub seed: String,
    pub affects: Vec<String>,
    #[serde(default)]
    pub not_affected: Vec<String>,
}

/// A labeled cross-language edge (only in cross-lang fixtures): a client-side
/// symbol that MUST connect to a server/native symbol.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CrossEdge {
    pub from: String,
    pub to: String,
}

/// All labels for one fixture.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct GroundTruth {
    #[serde(default, rename = "call_edge")]
    pub call_edges: Vec<CallEdge>,
    #[serde(default, rename = "test_link")]
    pub test_links: Vec<TestLink>,
    /// Test linkages that must NOT hold: changing `covers` must NOT select
    /// `test`. A selected non-link is a false positive for affected-test
    /// precision (selecting every test gives perfect recall but no speed-up).
    #[serde(default, rename = "test_nonlink")]
    pub test_nonlinks: Vec<TestLink>,
    #[serde(default, rename = "blast")]
    pub blasts: Vec<Blast>,
    #[serde(default, rename = "cross_edge")]
    pub cross_edges: Vec<CrossEdge>,
    /// Cross-language couplings that MUST NOT exist (distractors): a client call
    /// whose path/method has no matching server route, a look-alike route, etc.
    /// A connection here is a false positive for cross-language precision.
    #[serde(default, rename = "cross_nonedge")]
    pub cross_nonedges: Vec<CrossEdge>,
}

impl GroundTruth {
    pub fn parse(toml_src: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_src)
    }

    /// Every distinct `path::symbol` label this ground truth references, across
    /// all label kinds. Used by the preflight to assert every label resolves to
    /// a real node before any metric is computed -- so a dropped node becomes a
    /// loud failure, not a silently shrunken denominator.
    pub fn all_labels(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |s: &str| out.push(s.to_string());
        for c in &self.call_edges {
            push(&c.from);
            push(&c.to);
        }
        for t in self.test_links.iter().chain(&self.test_nonlinks) {
            push(&t.test);
            for c in &t.covers {
                push(c);
            }
        }
        for b in &self.blasts {
            push(&b.seed);
            for a in &b.affects {
                push(a);
            }
            for d in &b.not_affected {
                push(d);
            }
        }
        for c in &self.cross_edges {
            push(&c.from);
            push(&c.to);
        }
        for c in &self.cross_nonedges {
            push(&c.from);
            push(&c.to);
        }
        out.sort();
        out.dedup();
        out
    }
}

/// One fixture entry in the corpus manifest.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FixtureEntry {
    pub dir: String,
    pub family: String,
}

/// The corpus manifest: every fixture the harness scores.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Manifest {
    #[serde(default, rename = "fixture")]
    pub fixtures: Vec<FixtureEntry>,
}

impl Manifest {
    pub fn parse(src: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(src)
    }
}

/// The bare symbol a node represents: its display label minus the `(...)` arg
/// hint the extractor appends to functions/methods and any leading `.` some
/// language extractors prefix onto methods. `handle_request()` ->
/// `handle_request`, `.route()` -> `route`, `Type::method()` -> `Type::method`.
fn node_symbol(label: &str) -> &str {
    label
        .split('(')
        .next()
        .unwrap_or(label)
        .trim()
        .trim_start_matches('.')
}

/// Resolve a "relative/path::symbol" label to the NodeId the extractor emitted.
///
/// The path part is everything before the FIRST `::` (file paths never contain
/// `::`); the symbol is the rest. A node matches when its source file ends with
/// that path and its bare symbol equals the label symbol or the label symbol's
/// last `::` segment (so `Type::method` matches a node labeled `method()`).
/// Returns the first match in node order (deterministic: nodes are sorted on
/// build), or None if nothing matches.
pub fn resolve_label(gd: &GraphData, label: &str) -> Option<NodeId> {
    let (path_part, symbol) = label.split_once("::")?;
    let symbol_last = symbol.rsplit("::").next().unwrap_or(symbol);
    gd.nodes
        .iter()
        .find(|n| {
            let file = n.source_file.replace('\\', "/");
            let sym = node_symbol(&n.label);
            path_matches(&file, path_part) && (sym == symbol || sym == symbol_last)
        })
        .map(|n| n.id.clone())
}

/// Does `file` end with `path_part` on a path-segment boundary? A plain
/// `ends_with` would let `top.py` match `test_top.py`; the path part must be the
/// whole file or be preceded by a `/`.
fn path_matches(file: &str, path_part: &str) -> bool {
    file == path_part
        || file
            .strip_suffix(path_part)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_label_kinds() {
        let src = r#"
[[call_edge]]
from = "src/lib.rs::handle_request"
to = "src/router.rs::route"

[[test_link]]
test = "tests/t.rs::routes"
covers = ["src/router.rs::route"]

[[blast]]
seed = "src/router.rs::route"
affects = ["src/lib.rs::handle_request"]
"#;
        let gt = GroundTruth::parse(src).unwrap();
        assert_eq!(gt.call_edges.len(), 1);
        assert_eq!(gt.call_edges[0].to, "src/router.rs::route");
        assert_eq!(gt.test_links[0].covers, vec!["src/router.rs::route"]);
        assert_eq!(gt.blasts[0].affects.len(), 1);
        assert!(gt.cross_edges.is_empty());
    }

    #[test]
    fn parses_manifest() {
        let src = r#"
[[fixture]]
dir = "systems-rust"
family = "systems-rust"
"#;
        let m = Manifest::parse(src).unwrap();
        assert_eq!(m.fixtures.len(), 1);
        assert_eq!(m.fixtures[0].family, "systems-rust");
    }
}

#[cfg(test)]
mod resolver_tests {
    use super::*;
    use std::path::PathBuf;
    use synaptic_incremental::{rebuild, ChangeSet, RebuildOptions};

    fn build() -> GraphData {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/systems-rust");
        let out = rebuild(
            &RebuildOptions {
                root,
                directed: true,
                force: true,
            },
            &ChangeSet::Full,
            None,
        )
        .unwrap();
        out.kg.to_graph_data()
    }

    #[test]
    fn resolves_function_label_to_node() {
        let gd = build();
        assert!(
            resolve_label(&gd, "src/router.rs::route").is_some(),
            "route must resolve"
        );
        assert!(
            resolve_label(&gd, "src/lib.rs::handle_request").is_some(),
            "handle_request must resolve"
        );
        assert!(
            resolve_label(&gd, "src/lib.rs::validate").is_some(),
            "validate must resolve"
        );
        assert!(resolve_label(&gd, "src/router.rs::nonexistent").is_none());
    }
}
