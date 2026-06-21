//! MCP-config ingestion.
//!
//! Reads `.mcp.json` / `claude_desktop_config.json` / `mcp.json` /
//! `mcp_servers.json` and turns the `mcpServers` map into graph nodes/edges:
//! a `mcp_config_file` node → `mcp_server` nodes (stem-scoped) → globally-scoped
//! `mcp_command` / `mcp_package` / `env_var` nodes (so the same package or env
//! var across configs collapses to one node).
//!
//! **Security:** env var VALUES are never read — only NAMES become `env_var`
//! nodes. Positional `args` are never persisted (they can embed paths/secrets);
//! only a recognized package id is extracted from them. Labels go through
//! `sanitize_label`; the file is capped at 1 MiB.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use synaptic_core::make_id;

use crate::{file_stem, make_edge, make_node, Ingested};

const MAX_BYTES: usize = 1_048_576; // 1 MiB
const MAX_SERVERS: usize = 200;

/// Recognised MCP config basenames.
pub const MCP_CONFIG_FILENAMES: &[&str] = &[
    ".mcp.json",
    "claude_desktop_config.json",
    "mcp.json",
    "mcp_servers.json",
];

/// True when `path`'s basename is a recognised MCP config filename.
pub fn is_mcp_config_path(path: &Path) -> bool {
    path.file_name()
        .map(|n| MCP_CONFIG_FILENAMES.contains(&n.to_string_lossy().as_ref()))
        .unwrap_or(false)
}

static NPM_PKG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*(?:@[\w.\-+]+)?$")
        .expect("valid npm-package regex")
});
static PY_MCP_PKG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z0-9][a-z0-9._-]*-mcp(?:-[a-z0-9._-]+)?$|^mcp-[a-z0-9][a-z0-9._-]*$")
        .expect("valid py-mcp-package regex")
});
static ARG_FLAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-{1,2}\w").expect("valid arg-flag regex"));

/// First arg that looks like an npm or pypi package id (skipping flags/options),
/// else `None`.
fn detect_package_from_args(args: &[Value]) -> Option<String> {
    for raw in args {
        let Some(arg) = raw.as_str() else { continue };
        let arg = arg.trim();
        if arg.is_empty() || ARG_FLAG_RE.is_match(arg) {
            continue;
        }
        if NPM_PKG_RE.is_match(arg) {
            return Some(strip_version(arg));
        }
        if PY_MCP_PKG_RE.is_match(arg) {
            return Some(arg.to_string());
        }
    }
    None
}

/// Drop the `@version` suffix from an npm id, preserving any scope.
fn strip_version(pkg: &str) -> String {
    if let Some(rest) = pkg.strip_prefix('@') {
        match rest.find('@') {
            Some(i) => pkg[..i + 1].to_string(), // +1 for the leading '@'
            None => pkg.to_string(),
        }
    } else {
        match pkg.find('@') {
            Some(i) => pkg[..i].to_string(),
            None => pkg.to_string(),
        }
    }
}

struct Builder {
    out: Ingested,
    seen_nodes: HashSet<String>,
    seen_edges: HashSet<(String, String, String)>,
    source_file: String,
}

impl Builder {
    fn node(&mut self, id: &str, label: &str, kind: &str) {
        if id.is_empty() || !self.seen_nodes.insert(id.to_string()) {
            return;
        }
        self.out.nodes.push(make_node(
            id.to_string(),
            label,
            &self.source_file,
            1,
            Some(kind),
        ));
    }

    fn edge(&mut self, source: &str, target: &str, relation: &str, context: Option<&str>) {
        if source.is_empty() || target.is_empty() || source == target {
            return;
        }
        let key = (source.to_string(), target.to_string(), relation.to_string());
        if !self.seen_edges.insert(key) {
            return;
        }
        self.out.edges.push(make_edge(
            source.to_string(),
            target.to_string(),
            relation,
            &self.source_file,
            1,
            context,
        ));
    }
}

/// Parse an MCP config file into graph nodes/edges. Returns an empty result on
/// any read/parse error, oversize file, or missing `mcpServers` map (so it's
/// indistinguishable from "no MCP config here").
pub fn ingest_mcp_config(path: &Path) -> Ingested {
    let Ok(bytes) = std::fs::read(path) else {
        return Ingested::default();
    };
    if bytes.len() > MAX_BYTES {
        return Ingested::default();
    }
    let Ok(doc) = serde_json::from_slice::<Value>(&bytes) else {
        return Ingested::default();
    };
    let servers = doc
        .get("mcpServers")
        .and_then(Value::as_object)
        .or_else(|| {
            doc.get("mcp")
                .and_then(|m| m.get("servers"))
                .and_then(Value::as_object)
        });
    let Some(servers) = servers else {
        return Ingested::default();
    };

    let str_path = path.to_string_lossy().into_owned();
    let file_nid = make_id(&[str_path.as_str()]);
    let stem = file_stem(path);
    let fname = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut b = Builder {
        out: Ingested::default(),
        seen_nodes: HashSet::new(),
        seen_edges: HashSet::new(),
        source_file: str_path.clone(),
    };
    b.node(&file_nid, &fname, "mcp_config_file");

    for (name, spec) in servers.iter().take(MAX_SERVERS) {
        if name.is_empty() {
            continue;
        }
        let Some(spec) = spec.as_object() else {
            continue;
        };
        let server_nid = make_id(&[stem.as_str(), "mcp_server", name]);
        b.node(&server_nid, name, "mcp_server");
        b.edge(&file_nid, &server_nid, "contains", None);

        if let Some(cmd) = spec.get("command").and_then(Value::as_str) {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                let cmd_nid = make_id(&["mcp_command", cmd]);
                b.node(&cmd_nid, cmd, "mcp_command");
                b.edge(&server_nid, &cmd_nid, "references", Some("command"));
            }
        }
        if let Some(args) = spec.get("args").and_then(Value::as_array) {
            if let Some(pkg) = detect_package_from_args(args) {
                let pkg_nid = make_id(&["mcp_package", &pkg]);
                b.node(&pkg_nid, &pkg, "mcp_package");
                b.edge(&server_nid, &pkg_nid, "references", Some("package"));
            }
        }
        if let Some(env) = spec.get("env").and_then(Value::as_object) {
            for env_name in env.keys() {
                if env_name.is_empty() {
                    continue;
                }
                let env_nid = make_id(&["env_var", env_name]);
                b.node(&env_nid, env_name, "env_var");
                b.edge(&server_nid, &env_nid, "requires_env", None);
            }
        }
    }
    b.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) -> std::path::PathBuf {
        let p = dir.join(".mcp.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn detects_servers_command_package_env() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","@modelcontextprotocol/server-filesystem@1.2.3","/data"],"env":{"API_TOKEN":"sk-secret-123"}}}}"#,
        );
        let g = ingest_mcp_config(&p);
        let labels: Vec<&str> = g.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&".mcp.json"), "config file node");
        assert!(labels.contains(&"fs"), "server node");
        assert!(labels.contains(&"npx"), "command node");
        assert!(
            labels.contains(&"@modelcontextprotocol/server-filesystem"),
            "package (version stripped, scope kept): {labels:?}"
        );
        assert!(labels.contains(&"API_TOKEN"), "env var NAME node");
        let rels: HashSet<&str> = g.edges.iter().map(|e| e.relation.as_str()).collect();
        assert!(
            rels.contains("contains")
                && rels.contains("references")
                && rels.contains("requires_env")
        );
    }

    #[test]
    fn env_values_and_args_paths_are_never_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","@x/y","/tmp/workspace"],"env":{"API_TOKEN":"sk-secret-123"}}}}"#,
        );
        let g = ingest_mcp_config(&p);
        // The secret token value and the filesystem path arg must appear NOWHERE.
        for n in &g.nodes {
            assert!(
                !n.label.contains("sk-secret-123"),
                "secret leaked: {}",
                n.label
            );
            assert_ne!(n.label, "/tmp/workspace", "fs path arg persisted");
        }
    }

    #[test]
    fn missing_map_yields_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), r#"{"notMcp": 1}"#);
        assert_eq!(ingest_mcp_config(&p), Ingested::default());
    }

    #[test]
    fn strip_version_cases() {
        assert_eq!(strip_version("@scope/name@1.2.3"), "@scope/name");
        assert_eq!(strip_version("@scope/name"), "@scope/name");
        assert_eq!(strip_version("name@1.0"), "name");
        assert_eq!(strip_version("name"), "name");
    }
}
