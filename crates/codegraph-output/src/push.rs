//! Live database push (feature `push`). Streams the graph into a *running*
//! Neo4j or FalkorDB. Both use idempotent `MERGE` upserts (re-runnable,
//! never duplicates) built from the same escaped statements as [`crate::cypher`].
//!
//! Transport choices (deliberate, documented):
//! - **Neo4j → `cypher-shell`** (ships with Neo4j, speaks Bolt). We shell out and
//!   pipe the `;`-terminated script to stdin, so the user supplies a
//!   `bolt://…` URI — and we avoid pulling a heavy async
//!   Bolt crate (the "shell out to an installed tool" decision).
//! - **FalkorDB → the pure-Rust `redis` client** (FalkorDB is a Redis module;
//!   `GRAPH.QUERY` over RESP). Programmatic args sidestep shell-quoting hazards.
//!
//! The statement generation + URI parsing are pure and unit-tested; the actual
//! subprocess / socket round-trip is thin glue (it needs a live server, so it
//! can't be exercised offline — like the external integrations).

use std::io::{self, Write};
use std::process::{Command, Stdio};

use codegraph_graph::KnowledgeGraph;

use crate::cypher::cypher_statements;

/// Push the graph to a running Neo4j via `cypher-shell` (must be on PATH).
/// `uri` is a Bolt URL (e.g. `bolt://localhost:7687`). Returns the statement
/// count on success.
pub fn push_neo4j(kg: &KnowledgeGraph, uri: &str, user: &str, password: &str) -> io::Result<usize> {
    // Build the rich (props + community) statements once; `;`-terminate for the
    // cypher-shell script.
    let stmts = cypher_statements(kg, true);
    let count = stmts.len();
    let script = stmts
        .iter()
        .map(|s| format!("{s};"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut child = Command::new("cypher-shell")
        .args(["-a", uri, "-u", user, "-p", password, "--format", "plain"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("cypher-shell not available: {e}")))?;
    // Write stdin on a worker thread so a large script can't deadlock the pipe
    // (same fix as the claude-CLI backend).
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let writer = std::thread::spawn(move || stdin.write_all(script.as_bytes()));
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!("cypher-shell exited {status}")));
    }
    // Surface a genuine write failure (a `BrokenPipe` just means the shell
    // finished reading early, not an error).
    if let Ok(Err(e)) = writer.join() {
        if e.kind() != io::ErrorKind::BrokenPipe {
            return Err(e);
        }
    }
    Ok(count)
}

/// Push the graph to a running FalkorDB via the `redis` client. Runs each MERGE
/// through `GRAPH.QUERY <graph> <stmt>`. `uri` host/port are parsed (default
/// `localhost:6379`); auth is optional. Returns the statement count.
pub fn push_falkordb(
    kg: &KnowledgeGraph,
    uri: &str,
    graph_name: &str,
    password: Option<&str>,
) -> Result<usize, redis::RedisError> {
    let stmts = cypher_statements(kg, true);
    let client = redis::Client::open(falkordb_conn_info(uri, password))?;
    let mut con = client.get_connection()?;
    for stmt in &stmts {
        redis::cmd("GRAPH.QUERY")
            .arg(graph_name)
            .arg(stmt.as_str())
            .query::<redis::Value>(&mut con)?;
    }
    Ok(stmts.len())
}

/// Parse `host`/`port` out of a FalkorDB URI, accepting `falkordb://`,
/// `redis://`, `rediss://`, a bare `host:port`, or a bare `host` (default port
/// 6379). Any `user@`/path components are ignored (only host+port matter).
fn parse_host_port(uri: &str) -> (String, u16) {
    let after_scheme = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(after_scheme);
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    match hostport.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => (h.to_string(), p.parse().unwrap_or(6379)),
        _ => (hostport.to_string(), 6379),
    }
}

/// Build the `redis` `ConnectionInfo` programmatically — the password is set as
/// a field, never interpolated into a URL, so a password containing `@`/`:`/`/`/
/// `%` (which would break a `redis://…` URL) connects correctly.
fn falkordb_conn_info(uri: &str, password: Option<&str>) -> redis::ConnectionInfo {
    let (host, port) = parse_host_port(uri);
    redis::ConnectionInfo {
        addr: redis::ConnectionAddr::Tcp(host, port),
        redis: redis::RedisConnectionInfo {
            password: password.filter(|p| !p.is_empty()).map(str::to_string),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_port_handles_schemes_and_defaults() {
        assert_eq!(parse_host_port("falkordb://db:6380"), ("db".into(), 6380));
        assert_eq!(
            parse_host_port("redis://localhost:6379"),
            ("localhost".into(), 6379)
        );
        assert_eq!(parse_host_port("myhost"), ("myhost".into(), 6379));
        assert_eq!(parse_host_port("host:1234/0"), ("host".into(), 1234));
        // user@ is ignored (only host:port matter).
        assert_eq!(parse_host_port("redis://u@h:7000"), ("h".into(), 7000));
    }

    #[test]
    fn conn_info_sets_addr_and_password_without_url_escaping() {
        let ci = falkordb_conn_info("falkordb://h:6380", Some("p@ss/w:rd"));
        match ci.addr {
            redis::ConnectionAddr::Tcp(host, port) => {
                assert_eq!(host, "h");
                assert_eq!(port, 6380);
            }
            other => panic!("expected Tcp addr, got {other:?}"),
        }
        // The special-char password survives verbatim (no URL mangling).
        assert_eq!(ci.redis.password.as_deref(), Some("p@ss/w:rd"));
    }

    #[test]
    fn conn_info_omits_empty_password() {
        assert_eq!(falkordb_conn_info("h:6379", Some("")).redis.password, None);
        assert_eq!(falkordb_conn_info("h:6379", None).redis.password, None);
    }
}
