//! Graph safety caps, shared by every loader that guards against oversized or
//! runaway `graph.json` inputs (the git merge driver, federation, the global
//! store, remote subgraph fetches).
//!
//! Defaults match the historical hard-coded caps (50 MiB / 100k nodes) but are
//! overridable per process: `SYNAPTIC_MAX_GRAPH_MB` sets the byte cap in
//! mebibytes and `SYNAPTIC_MAX_NODES` the node cap. `0` disables a cap
//! entirely; unset or unparseable values fall back to the default.
//!
//! The serve/query read path is separate. Those caps guard input arriving from
//! somewhere else; a served graph is the operator's own extraction and is
//! routinely hundreds of megabytes, so `SYNAPTIC_MAX_SERVE_MB` defaults to no
//! cap and the guard is a diagnostic first: see [`serve_guard`].

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

/// Default per-shard byte ceiling for the redb store: 2 GiB. A DoS guard on one
/// shard, not an aggregate cap (shards are materialized one at a time).
pub const DEFAULT_MAX_SHARD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Default per-shard node ceiling for the redb store: 5M nodes.
pub const DEFAULT_MAX_SHARD_NODES: u64 = 5_000_000;
/// Env var overriding the per-shard byte cap, in MiB (`0` = no cap).
pub const MAX_SHARD_MB_ENV: &str = "SYNAPTIC_MAX_SHARD_MB";
/// Env var overriding the per-shard node cap (`0` = no cap).
pub const MAX_SHARD_NODES_ENV: &str = "SYNAPTIC_MAX_SHARD_NODES";

/// Effective per-shard byte cap, honoring [`MAX_SHARD_MB_ENV`] (`0` disables).
pub fn max_shard_bytes() -> u64 {
    parse_shard_bytes(std::env::var(MAX_SHARD_MB_ENV).ok().as_deref())
}

/// Effective per-shard node cap, honoring [`MAX_SHARD_NODES_ENV`] (`0` disables).
pub fn max_shard_nodes() -> u64 {
    parse_shard_nodes(std::env::var(MAX_SHARD_NODES_ENV).ok().as_deref())
}

/// Env var capping the `graph.json` a *serve/query* load will accept, in MiB
/// (`0` or unset = no cap).
pub const MAX_SERVE_MB_ENV: &str = "SYNAPTIC_MAX_SERVE_MB";

/// Peak resident bytes a serve load costs, as a multiple of the `graph.json` it
/// reads. Measured at 2.95x on a 742 MiB compact graph (569k nodes, 1.57M edges)
/// that peaked at 2,193 MiB. Rounded up to 3, which is conservative for a
/// pretty-printed file: the same graph written pretty is larger on disk but
/// expands to the same structures, so its ratio is lower.
pub const SERVE_PEAK_RATIO: u64 = 3;

/// Threshold above which a load reports its projected cost even when no process
/// memory limit is detectable. Below this an OOM is not a plausible outcome and
/// a note would just be noise.
const SERVE_REPORT_FLOOR: u64 = 1024 * 1024 * 1024;

/// Effective byte cap for a serve/query load, honoring [`MAX_SERVE_MB_ENV`].
///
/// Unlike [`max_graph_bytes`] this defaults to **no cap**. That cap guards
/// untrusted input (the merge driver, federation, remote fetches) at 50 MiB; a
/// served graph is the operator's own extraction and is routinely much larger,
/// so a default here would reject working deployments.
pub fn max_serve_bytes() -> u64 {
    parse_serve_bytes(std::env::var(MAX_SERVE_MB_ENV).ok().as_deref())
}

/// Roughly what loading a `graph.json` of `file_bytes` will cost at peak.
pub fn projected_peak_bytes(file_bytes: u64) -> u64 {
    file_bytes.saturating_mul(SERVE_PEAK_RATIO)
}

/// What a serve load should do about memory before it allocates anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeGuard {
    /// Nothing worth saying; load normally.
    Proceed,
    /// Load, but tell the operator what it will cost. Emitted when the process
    /// has a known memory limit the projection does not fit inside, or when the
    /// graph is large enough that the cost is worth stating regardless.
    Warn(String),
    /// Refuse: the configured [`MAX_SERVE_MB_ENV`] cap is exceeded. Failing here
    /// is deterministic and explains itself, where an OOM kill does neither.
    Refuse(String),
}

/// Decide how to handle a serve load of `file_bytes`, given a byte `cap`
/// (`u64::MAX` for none) and the process memory `limit` if one is known.
///
/// Pure, so the policy is testable without touching the environment or cgroups.
pub fn serve_guard(file_bytes: u64, cap: u64, limit: Option<u64>) -> ServeGuard {
    if file_bytes > cap {
        return ServeGuard::Refuse(format!(
            "graph.json is {}, over the {} serve cap ({MAX_SERVE_MB_ENV}); \
             raise it or set it to 0 to load without a cap",
            human_bytes(file_bytes),
            human_bytes(cap)
        ));
    }
    let projected = projected_peak_bytes(file_bytes);
    match limit {
        Some(limit) if projected > limit => ServeGuard::Warn(format!(
            "loading a {} graph.json needs roughly {} at peak, but this process \
             is limited to {} and is likely to be killed; serve a shard store, \
             raise the limit, or set {MAX_SERVE_MB_ENV} to fail fast instead",
            human_bytes(file_bytes),
            human_bytes(projected),
            human_bytes(limit)
        )),
        _ if projected >= SERVE_REPORT_FLOOR => ServeGuard::Warn(format!(
            "loading a {} graph.json needs roughly {} at peak",
            human_bytes(file_bytes),
            human_bytes(projected)
        )),
        _ => ServeGuard::Proceed,
    }
}

/// [`serve_guard`] wired to the environment and the process's own memory limit.
pub fn serve_guard_for(file_bytes: u64) -> ServeGuard {
    serve_guard(file_bytes, max_serve_bytes(), process_memory_limit_bytes())
}

/// The memory ceiling this process runs under, if one is discoverable.
///
/// Reads the cgroup limit, which is what a containerized MCP server is actually
/// killed by. Returns `None` off Linux, outside a limited cgroup, or when the
/// files are unreadable, in which case callers simply lose the comparison.
pub fn process_memory_limit_bytes() -> Option<u64> {
    for path in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ] {
        if let Ok(raw) = std::fs::read_to_string(path)
            && let Some(limit) = parse_cgroup_limit(&raw)
        {
            return Some(limit);
        }
    }
    None
}

/// A cgroup memory limit file's contents. cgroup v2 writes `max` for "no limit";
/// v1 writes a near-`u64::MAX` sentinel. Both mean unlimited.
fn parse_cgroup_limit(raw: &str) -> Option<u64> {
    let value: u64 = raw.trim().parse().ok()?;
    // v1's unlimited sentinel is page-aligned u64::MAX; anything within a
    // petabyte of the top is not a real container limit.
    (value < u64::MAX / 8192).then_some(value)
}

fn human_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.0} MiB", bytes as f64 / MIB as f64)
    }
}

fn parse_serve_bytes(raw: Option<&str>) -> u64 {
    match raw.map(str::trim).and_then(|s| s.parse::<u64>().ok()) {
        Some(0) | None => u64::MAX,
        Some(mb) => mb.saturating_mul(1024 * 1024),
    }
}

fn parse_shard_bytes(raw: Option<&str>) -> u64 {
    match raw.map(str::trim).and_then(|s| s.parse::<u64>().ok()) {
        Some(0) => u64::MAX,
        Some(mb) => mb.saturating_mul(1024 * 1024),
        None => DEFAULT_MAX_SHARD_BYTES,
    }
}

fn parse_shard_nodes(raw: Option<&str>) -> u64 {
    match raw.map(str::trim).and_then(|s| s.parse::<u64>().ok()) {
        Some(0) => u64::MAX,
        Some(n) => n,
        None => DEFAULT_MAX_SHARD_NODES,
    }
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

    /// The serve path must stay uncapped unless an operator opts in. The 50 MiB
    /// `SYNAPTIC_MAX_GRAPH_MB` default guards untrusted merge/federation input;
    /// a served graph is the operator's own extraction and is routinely far
    /// larger, so inheriting that default would reject working deployments.
    #[test]
    fn serve_cap_is_off_unless_configured() {
        assert_eq!(parse_serve_bytes(None), u64::MAX);
        assert_eq!(parse_serve_bytes(Some("")), u64::MAX);
        assert_eq!(parse_serve_bytes(Some("nonsense")), u64::MAX);
        assert_eq!(parse_serve_bytes(Some("0")), u64::MAX);
    }

    #[test]
    fn serve_cap_parses_mebibytes() {
        assert_eq!(parse_serve_bytes(Some("2048")), 2048 * 1024 * 1024);
        assert_eq!(parse_serve_bytes(Some(" 512 ")), 512 * 1024 * 1024);
        assert_eq!(parse_serve_bytes(Some("18446744073709551615")), u64::MAX);
    }

    #[test]
    fn projected_peak_scales_with_the_file() {
        assert_eq!(projected_peak_bytes(0), 0);
        assert_eq!(
            projected_peak_bytes(1024 * 1024 * 1024),
            SERVE_PEAK_RATIO * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn cgroup_limit_reads_a_number_and_ignores_unlimited() {
        assert_eq!(
            parse_cgroup_limit("4294967296\n"),
            Some(4 * 1024 * 1024 * 1024)
        );
        assert_eq!(
            parse_cgroup_limit(" 2147483648 "),
            Some(2 * 1024 * 1024 * 1024)
        );
        // cgroup v2 spells "no limit" as `max`; v1 uses a near-u64::MAX sentinel.
        assert_eq!(parse_cgroup_limit("max\n"), None);
        assert_eq!(parse_cgroup_limit("9223372036854771712"), None);
        assert_eq!(parse_cgroup_limit(""), None);
        assert_eq!(parse_cgroup_limit("garbage"), None);
    }

    #[test]
    fn guard_proceeds_for_an_ordinary_graph() {
        assert!(matches!(
            serve_guard(20 * 1024 * 1024, u64::MAX, None),
            ServeGuard::Proceed
        ));
    }

    #[test]
    fn guard_refuses_over_the_configured_cap() {
        let cap = 512 * 1024 * 1024;
        let ServeGuard::Refuse(msg) = serve_guard(965 * 1024 * 1024, cap, None) else {
            panic!("a graph over the cap must be refused");
        };
        assert!(msg.contains("SYNAPTIC_MAX_SERVE_MB"), "{msg}");
        assert!(msg.contains("965"), "{msg}");
        assert!(msg.contains("512"), "{msg}");
    }

    /// A cap of exactly the file size is not exceeded.
    #[test]
    fn guard_allows_a_graph_at_the_cap() {
        let cap = 965 * 1024 * 1024;
        assert!(!matches!(
            serve_guard(cap, cap, None),
            ServeGuard::Refuse(_)
        ));
    }

    #[test]
    fn guard_warns_when_the_projection_exceeds_the_process_limit() {
        let limit = 2 * 1024 * 1024 * 1024;
        let ServeGuard::Warn(msg) = serve_guard(965 * 1024 * 1024, u64::MAX, Some(limit)) else {
            panic!("a projection over the process limit must warn");
        };
        assert!(msg.contains("limited to"), "{msg}");
        assert!(msg.contains("SYNAPTIC_MAX_SERVE_MB"), "{msg}");
    }

    /// A limit the graph comfortably fits inside stays silent.
    #[test]
    fn guard_is_quiet_when_the_graph_fits_the_process_limit() {
        let limit = 16 * 1024 * 1024 * 1024;
        assert!(matches!(
            serve_guard(200 * 1024 * 1024, u64::MAX, Some(limit)),
            ServeGuard::Proceed
        ));
    }

    /// With no limit to compare against, a large graph still reports what it
    /// will cost, so an OOM kill is not the first diagnostic anyone sees.
    #[test]
    fn guard_reports_the_projection_for_a_large_graph() {
        let ServeGuard::Warn(msg) = serve_guard(2 * 1024 * 1024 * 1024, u64::MAX, None) else {
            panic!("a large graph must report its projected cost");
        };
        assert!(msg.contains("GiB"), "{msg}");
    }

    #[test]
    fn shard_caps_default_and_override() {
        assert_eq!(parse_shard_bytes(None), DEFAULT_MAX_SHARD_BYTES);
        assert_eq!(parse_shard_bytes(Some("4096")), 4096 * 1024 * 1024);
        assert_eq!(parse_shard_bytes(Some("0")), u64::MAX);
        assert_eq!(parse_shard_bytes(Some("junk")), DEFAULT_MAX_SHARD_BYTES);
        assert_eq!(parse_shard_nodes(None), DEFAULT_MAX_SHARD_NODES);
        assert_eq!(parse_shard_nodes(Some("junk")), DEFAULT_MAX_SHARD_NODES);
        assert_eq!(parse_shard_nodes(Some("9000000")), 9_000_000);
        assert_eq!(parse_shard_nodes(Some("0")), u64::MAX);
    }
}
