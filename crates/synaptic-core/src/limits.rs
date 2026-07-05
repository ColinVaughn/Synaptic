//! Graph safety caps, shared by every loader that guards against oversized or
//! runaway `graph.json` inputs (the git merge driver, federation, the global
//! store, remote subgraph fetches).
//!
//! Defaults match the historical hard-coded caps (50 MiB / 100k nodes) but are
//! overridable per process: `SYNAPTIC_MAX_GRAPH_MB` sets the byte cap in
//! mebibytes and `SYNAPTIC_MAX_NODES` the node cap. `0` disables a cap
//! entirely; unset or unparseable values fall back to the default.

/// Default byte cap for a loaded `graph.json` / export surface: 50 MiB.
pub const DEFAULT_MAX_GRAPH_BYTES: u64 = 50 * 1024 * 1024;
/// Default node-count cap for a loaded or merged graph: 100k nodes.
pub const DEFAULT_MAX_NODES: usize = 100_000;
/// Env var overriding the byte cap, in MiB (`0` = no cap).
pub const MAX_GRAPH_MB_ENV: &str = "SYNAPTIC_MAX_GRAPH_MB";
/// Env var overriding the node cap (`0` = no cap).
pub const MAX_NODES_ENV: &str = "SYNAPTIC_MAX_NODES";

/// Effective byte cap for graph/surface files, honoring
/// [`MAX_GRAPH_MB_ENV`] (`0` disables the cap).
pub fn max_graph_bytes() -> u64 {
    parse_graph_bytes(std::env::var(MAX_GRAPH_MB_ENV).ok().as_deref())
}

/// Effective node-count cap for loaded/merged graphs, honoring
/// [`MAX_NODES_ENV`] (`0` disables the cap).
pub fn max_nodes() -> usize {
    parse_node_cap(std::env::var(MAX_NODES_ENV).ok().as_deref())
}

fn parse_graph_bytes(raw: Option<&str>) -> u64 {
    match raw.map(str::trim).and_then(|s| s.parse::<u64>().ok()) {
        Some(0) => u64::MAX,
        Some(mb) => mb.saturating_mul(1024 * 1024),
        None => DEFAULT_MAX_GRAPH_BYTES,
    }
}

fn parse_node_cap(raw: Option<&str>) -> usize {
    match raw.map(str::trim).and_then(|s| s.parse::<usize>().ok()) {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => DEFAULT_MAX_NODES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_cap_defaults_when_unset_or_garbage() {
        assert_eq!(parse_graph_bytes(None), DEFAULT_MAX_GRAPH_BYTES);
        assert_eq!(parse_graph_bytes(Some("")), DEFAULT_MAX_GRAPH_BYTES);
        assert_eq!(parse_graph_bytes(Some("abc")), DEFAULT_MAX_GRAPH_BYTES);
        assert_eq!(parse_graph_bytes(Some("-5")), DEFAULT_MAX_GRAPH_BYTES);
        assert_eq!(parse_graph_bytes(Some("50MB")), DEFAULT_MAX_GRAPH_BYTES);
    }

    #[test]
    fn byte_cap_is_mebibytes() {
        assert_eq!(parse_graph_bytes(Some("200")), 200 * 1024 * 1024);
        // Surrounding whitespace tolerated.
        assert_eq!(parse_graph_bytes(Some(" 10 ")), 10 * 1024 * 1024);
    }

    #[test]
    fn byte_cap_zero_disables() {
        assert_eq!(parse_graph_bytes(Some("0")), u64::MAX);
    }

    #[test]
    fn byte_cap_saturates_on_huge_values() {
        assert_eq!(parse_graph_bytes(Some("18446744073709551615")), u64::MAX);
    }

    #[test]
    fn node_cap_defaults_when_unset_or_garbage() {
        assert_eq!(parse_node_cap(None), DEFAULT_MAX_NODES);
        assert_eq!(parse_node_cap(Some("")), DEFAULT_MAX_NODES);
        assert_eq!(parse_node_cap(Some("lots")), DEFAULT_MAX_NODES);
        assert_eq!(parse_node_cap(Some("-1")), DEFAULT_MAX_NODES);
    }

    #[test]
    fn node_cap_parses_and_zero_disables() {
        assert_eq!(parse_node_cap(Some("250000")), 250_000);
        assert_eq!(parse_node_cap(Some(" 42 ")), 42);
        assert_eq!(parse_node_cap(Some("0")), usize::MAX);
    }
}
