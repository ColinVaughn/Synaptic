//! Synaptic graph assembly: build extraction output into a `KnowledgeGraph`.

pub mod analyze;
pub mod betweenness;
pub mod build;
pub mod cluster;
mod community;
pub mod cross_language;
pub mod dedup;
pub mod dynamic_link;
pub mod error;
pub mod graph;
pub mod ids;
mod minhash;
pub mod symbol_resolution;

pub use analyze::{
    AnalysisResult, GodNode, GraphDelta, GraphStats, ImportCycle, Question, Surprise, analyze,
    find_import_cycles, god_nodes, god_nodes_with_extra, graph_diff, graph_stats,
    strongly_connected_components, suggest_questions, surprising_connections,
};
pub use build::{BuildOptions, build_from_parts, guard_shrink};
pub use cluster::{
    Algorithm, ClusterOptions, apply_communities, cluster, cohesion_score,
    partition_cohesion_scores, remap_communities_to_previous,
};
pub use cross_language::{
    CROSS_LANGUAGE_RELATIONS, mark_cross_repo_edges, resolve_command_invocations,
    resolve_parameterized_routes, resolve_pyo3_imports, resolve_pyo3_modules,
    resolve_route_handlers, resolve_sql_queries,
};
pub use dedup::{
    ambiguous_concept_pairs, deduplicate_entities, deterministic_tiebreak,
    deterministic_tiebreak_candidates, merge_pairs,
};
pub use dynamic_link::link_dynamic_refs;
pub use error::GraphError;
pub use graph::{KnowledgeGraph, is_structural_edge, is_structural_node};
pub use ids::{norm_source_file, normalize_id};
pub use symbol_resolution::resolve_symbols;
