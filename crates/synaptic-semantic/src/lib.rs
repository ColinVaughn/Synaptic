//! The semantic pass: turn detected documents/papers into knowledge-graph
//! concept nodes via `synaptic-llm`, convert the LLM output to typed
//! `synaptic-core` nodes/edges, and (optionally) resolve ambiguous concept
//! duplicates with an LLM tiebreaker. This is the integration layer that wires
//! the LLM backend into the build pipeline.
#![forbid(unsafe_code)]

pub mod convert;
pub mod label;
pub mod pass;
pub mod tiebreak;

pub use convert::fragment_to_graph;
pub use label::label_communities;
pub use pass::{run_semantic_pass, SemanticOutcome, MAX_RETRY_DEPTH, TOKEN_BUDGET};
pub use tiebreak::llm_tiebreak;
