use serde::{Deserialize, Serialize};
use synaptic_core::{Edge, Node};

// The extraction-fact contract types live in `synaptic-core` so the graph crate
// can consume them without depending on `extract`. Re-exported here for callers
// that reach for `synaptic_extract::{RawCall, ImportRecord}`.
pub use synaptic_core::{ImportRecord, RawCall};

/// The result of extracting a single file: graph fragments plus unresolved
/// calls and import evidence. `nodes`/`edges` feed `synaptic-graph::build`;
/// `raw_calls` + `imports` feed the later cross-file resolution pass (B3).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub raw_calls: Vec<RawCall>,
    pub imports: Vec<ImportRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let r = ExtractionResult::default();
        assert!(r.nodes.is_empty() && r.edges.is_empty() && r.raw_calls.is_empty());
    }

    #[test]
    fn raw_call_roundtrips() {
        use synaptic_core::NodeId;
        let rc = RawCall {
            caller: NodeId("a".into()),
            callee: "foo".into(),
            is_member_call: false,
            source_file: "a.py".into(),
            source_location: Some("L3".into()),
            span: None,
        };
        let json = serde_json::to_string(&rc).unwrap();
        let back: RawCall = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rc);
    }
}
