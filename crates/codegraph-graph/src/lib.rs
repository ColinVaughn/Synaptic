//! CodeGraph graph assembly: build extraction output into a `KnowledgeGraph`.

pub mod analyze;
pub mod betweenness;
pub mod build;
pub mod cluster;
mod community;
pub mod dedup;
pub mod error;
pub mod graph;
pub mod ids;
mod minhash;
pub mod symbol_resolution;

pub use analyze::{
    analyze, find_import_cycles, god_nodes, graph_diff, graph_stats, suggest_questions,
    surprising_connections, AnalysisResult, GodNode, GraphDelta, GraphStats, ImportCycle, Question,
    Surprise,
};
pub use build::{build_from_parts, guard_shrink, BuildOptions};
pub use cluster::{
    apply_communities, cluster, cohesion_score, remap_communities_to_previous, Algorithm,
    ClusterOptions,
};
pub use dedup::{
    ambiguous_concept_pairs, deduplicate_entities, deterministic_tiebreak, merge_pairs,
};
pub use error::GraphError;
pub use graph::KnowledgeGraph;
pub use ids::{norm_source_file, normalize_id};
pub use symbol_resolution::resolve_symbols;
