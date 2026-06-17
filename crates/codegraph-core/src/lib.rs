//! CodeGraph core: the stable data contract shared by every crate.
//!
//! Owns the leaf types ([`NodeId`], [`FileType`], [`Confidence`], [`Node`],
//! [`Edge`], [`Hyperedge`]), the `graph.json` node-link DTO ([`GraphData`]),
//! [`make_id`] (stable ID construction), and
//! [`validate_extraction`] (extraction-schema validation).
#![forbid(unsafe_code)]

pub mod confidence;
pub mod edge;
pub mod error;
pub mod file_type;
pub mod graph_data;
pub mod hyperedge;
pub mod id;
pub mod node;
pub mod node_kind;
pub mod raw_call;
pub mod sanitize;
pub mod signature;
pub mod span;
pub mod test_path;
pub mod validate;

pub use confidence::Confidence;
pub use edge::Edge;
pub use error::{CoreError, Result};
pub use file_type::FileType;
pub use graph_data::GraphData;
pub use hyperedge::Hyperedge;
pub use id::{make_id, NodeId};
pub use node::Node;
pub use node_kind::{NodeKind, Visibility};
pub use raw_call::{ImportRecord, RawCall};
pub use sanitize::{sanitize_label, sanitize_metadata, sanitize_metadata_value};
pub use signature::{Param, Signature};
pub use span::Span;
pub use test_path::is_test_path;
pub use validate::{assert_valid, validate_extraction};
