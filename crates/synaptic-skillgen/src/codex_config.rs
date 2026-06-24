//! Codex-native integration. Two modes, because the Codex CLI and the Codex
//! desktop app read configuration differently:
//!
//! - **Project (CLI)** [`install`]/[`uninstall`]: writes a repo's `.codex/config.toml`
//!   (`[mcp_servers.synaptic]`) + a `SessionStart` hook (`.codex/hooks.json` +
//!   helper script). The CLI loads these for trusted projects.
//! - **Global (app)** [`install_global_mcp`]/[`uninstall_global_mcp`]: writes a
//!   per-repo `[mcp_servers.synaptic-<repo>]` into the GLOBAL `~/.codex/config.toml`.
//!   The desktop app ignores a project's `.codex/config.toml` for MCP and reads
//!   only the global file, so app users need this. No hook in this mode (the app
//!   would not fire it); orientation rides on the always-on AGENTS.md block.
//!
//! Why a SessionStart hook (not PreToolUse like Claude): Codex does NOT honor
//! `additionalContext` on PreToolUse (it marks the hook run failed), and its
//! top-level `systemMessage` is UI-only and never reaches the model. SessionStart
//! `additionalContext` IS injected as model-visible developer context, so we
//! orient the agent once per session when a graph actually exists. The always-on
//! AGENTS.md block carries the same instruction persistently; the hook adds the
//! dynamic "a graph exists right now" signal that a static file can't.
//!
//! Everything here is idempotent and preserves foreign content: reinstalling
//! never duplicates our entries, and uninstall removes exactly ours (deleting a
//! file only once nothing of anyone's remains).

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use toml_edit::{value, Array, DocumentMut, Item, Table};

use crate::settings_hooks::{load_settings, write_settings};

/// Repo-relative path of the Codex MCP/config file.
const CONFIG_REL: &str = ".codex/config.toml";
/// Repo-relative path of the Codex hooks file.
const HOOKS_REL: &str = ".codex/hooks.json";
/// Repo-relative path of the hook's helper script.
const SCRIPT_REL: &str = ".codex/synaptic-hook.py";
/// The lifecycle event we hook (see the module docs for why not PreToolUse).
const HOOK_EVENT: &str = "SessionStart";

/// The hook body, as a self-contained Python script. Python (rather than a shell
/// snippet) keeps it identical across Codex's Unix shell and the Windows
/// `commandWindows` path, with no `case`/`[ -f ]` portability traps. Fails open:
/// any IO hiccup exits 0 so a session never stalls on the hook.
pub(crate) const HOOK_SCRIPT: &str = r#"#!/usr/bin/env python3
"""Synaptic SessionStart hook for Codex.

Inject model-visible context, once per session, telling the agent to consult the
Synaptic knowledge graph before grepping or reading files broadly. Only fires
when a graph exists in this repo. Fails open."""
import json
import os
import sys

# Drain the SessionStart payload on stdin; we only need the cwd (the project
# root), so the graph path stays relative.
try:
    sys.stdin.read()
except Exception:
    pass

if not os.path.isfile(os.path.join("synaptic-out", "graph.json")):
    sys.exit(0)

message = (
    "This repo has a Synaptic knowledge graph (synaptic-out/graph.json). Query it "
    "(synaptic query/explain/path, or the Synaptic MCP tools) before grepping or "
    "reading files broadly; the MCP server's instructions list the available tools."
)
print(json.dumps({
    "hookSpecificOutput": {
        "hookEventName": "SessionStart",
        "additionalContext": message,
    }
}))
"#;

/// Install the Codex MCP server registration + SessionStart hook under `.codex/`.
/// Idempotent. Returns the paths written.
pub fn install(repo_root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    written.push(install_mcp_server(repo_root)?);
    written.extend(install_hook(repo_root)?);
    Ok(written)
}

/// Remove the Codex MCP server registration + hook. No-op if absent; foreign
/// servers/hooks are preserved.
pub fn uninstall(repo_root: &Path) -> std::io::Result<()> {
    uninstall_mcp_server(repo_root)?;
    uninstall_hook(repo_root)?;
    Ok(())
}

// --- MCP server (.codex/config.toml) ----------------------------------------

/// Generous startup timeout: loading a large graph when `serve` starts can take
/// a few seconds, and Codex's default (10s) is tight. Matches the app convention.
const STARTUP_TIMEOUT_SEC: i64 = 120;

/// Project-mode server table: `synaptic serve` with no `--graph`, so it defaults
/// to `synaptic-out/graph.json` relative to the server's cwd (the project root).
fn synaptic_server_table() -> Table {
    let mut server = Table::new();
    server["command"] = value("synaptic");
    let mut args = Array::new();
    args.push("serve");
    server["args"] = value(args);
    server["startup_timeout_sec"] = value(STARTUP_TIMEOUT_SEC);
    server
}

/// Global-mode server table: `synaptic serve --graph <abs>`. The desktop app
/// gives a server no per-project cwd, so the graph path must be absolute.
fn global_server_table(graph_path: &Path) -> Table {
    let mut server = Table::new();
    server["command"] = value("synaptic");
    let mut args = Array::new();
    args.push("serve");
    args.push("--graph");
    args.push(graph_path.to_string_lossy().as_ref());
    server["args"] = value(args);
    server["startup_timeout_sec"] = value(STARTUP_TIMEOUT_SEC);
    server
}

/// Insert/replace `[mcp_servers.<name>]` in `config_path`, preserving every other
/// server and key (format-preserving via `toml_edit`). Creates parent dirs.
/// Idempotent: `Table::insert` overwrites a same-named key in place.
fn upsert_mcp_server(config_path: &Path, name: &str, table: Table) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(config_path).unwrap_or_default();
    let mut doc = existing.parse::<DocumentMut>().unwrap_or_default();

    // Ensure `mcp_servers` is a table holding sub-tables. When we create it, mark
    // it `implicit` so it emits `[mcp_servers.<name>]` rather than a bare
    // `[mcp_servers]` header. A pre-existing table is left as the user wrote it
    // (we only insert our sub-table) so any direct keys/headers they have survive.
    let servers = doc.entry("mcp_servers").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    match servers {
        Item::Table(t) => {
            t.insert(name, Item::Table(table));
        }
        // `mcp_servers` exists but isn't a plain table (e.g. an array/value): replace.
        other => {
            let mut t = Table::new();
            t.set_implicit(true);
            t.insert(name, Item::Table(table));
            *other = Item::Table(t);
        }
    }

    write_string(config_path, &doc.to_string())
}

/// Remove `[mcp_servers.<name>]`, dropping the `mcp_servers` table when it becomes
/// empty and the file when nothing remains. Foreign servers/keys are kept.
fn remove_mcp_server(config_path: &Path, name: &str) -> std::io::Result<()> {
    let Ok(existing) = std::fs::read_to_string(config_path) else {
        return Ok(());
    };
    let mut doc = existing.parse::<DocumentMut>().unwrap_or_default();
    if let Some(Item::Table(t)) = doc.get_mut("mcp_servers") {
        t.remove(name);
        if t.is_empty() {
            doc.remove("mcp_servers");
        }
    }
    let out = doc.to_string();
    if out.trim().is_empty() {
        std::fs::remove_file(config_path)?;
    } else {
        std::fs::write(config_path, out)?;
    }
    Ok(())
}

/// Insert/replace our project-scoped server (CLI; the app ignores this file).
fn install_mcp_server(repo_root: &Path) -> std::io::Result<PathBuf> {
    let path = repo_root.join(CONFIG_REL);
    upsert_mcp_server(&path, "synaptic", synaptic_server_table())?;
    Ok(path)
}

/// Remove our project-scoped server.
fn uninstall_mcp_server(repo_root: &Path) -> std::io::Result<()> {
    remove_mcp_server(&repo_root.join(CONFIG_REL), "synaptic")
}

// --- MCP server: global (app) registration in <codex_home>/config.toml ------

/// Sanitized per-repo server name, e.g. `synaptic-webapp`. The Codex desktop
/// app only reads the global config (it ignores a project's `.codex/config.toml`
/// for MCP), so each repo is registered as its own named server.
fn global_server_name(repo_root: &Path) -> String {
    let base = repo_root
        .file_name()
        .map(|n| sanitize(&n.to_string_lossy()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "repo".to_string());
    format!("synaptic-{base}")
}

/// Lowercase and keep only `[a-z0-9_-]`, mapping any other char to `-`, so the
/// result is a valid TOML bare key for the `[mcp_servers.<name>]` header.
fn sanitize(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Register `[mcp_servers.synaptic-<repo>]` in the global `<codex_home>/config.toml`,
/// pointing `synaptic serve` at this repo's graph with an absolute `--graph`
/// (the app gives a server no per-project cwd). Idempotent. Returns the config path.
pub fn install_global_mcp(codex_home: &Path, repo_root: &Path) -> std::io::Result<PathBuf> {
    let path = codex_home.join("config.toml");
    let graph = repo_root.join("synaptic-out").join("graph.json");
    upsert_mcp_server(
        &path,
        &global_server_name(repo_root),
        global_server_table(&graph),
    )?;
    Ok(path)
}

/// Remove this repo's `[mcp_servers.synaptic-<repo>]` from the global config,
/// deleting the file only if nothing else remains. Foreign servers/keys survive.
pub fn uninstall_global_mcp(codex_home: &Path, repo_root: &Path) -> std::io::Result<()> {
    remove_mcp_server(
        &codex_home.join("config.toml"),
        &global_server_name(repo_root),
    )
}

// --- Lifecycle hook (.codex/hooks.json + helper script) ---------------------

/// Our hook entry: run the Python helper. No `matcher` (fire on every session
/// source). `commandWindows` uses `python` (the usual Windows launcher) since
/// Codex runs the Windows override through a different shell; `timeout` bounds a
/// stuck interpreter.
fn synaptic_hook_entry() -> Value {
    json!({
        "hooks": [{
            "type": "command",
            "command": "python3 .codex/synaptic-hook.py",
            "commandWindows": "python .codex/synaptic-hook.py",
            "timeout": 30
        }]
    })
}

/// True if `entry` is one of ours: its command runs our helper script. Matching
/// on the unique script path is unambiguous (no reliance on a matcher field,
/// which SessionStart entries omit).
fn is_synaptic_hook(entry: &Value) -> bool {
    entry.to_string().contains("synaptic-hook.py")
}

/// The current `hooks.<HOOK_EVENT>` array (empty if absent/misshaped).
fn event_entries(settings: &Map<String, Value>) -> Vec<Value> {
    settings
        .get("hooks")
        .and_then(|h| h.get(HOOK_EVENT))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Set `settings.hooks.<HOOK_EVENT>` to `entries`, creating the nested objects as
/// needed and preserving every other key.
fn set_event_entries(settings: &mut Map<String, Value>, entries: Vec<Value>) {
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    if let Some(obj) = hooks.as_object_mut() {
        obj.insert(HOOK_EVENT.to_string(), Value::Array(entries));
    }
}

/// Write the hook into `.codex/hooks.json` (foreign hooks survive, reinstall
/// never duplicates) plus the helper script. Returns both paths.
fn install_hook(repo_root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let hooks_path = repo_root.join(HOOKS_REL);
    let mut settings = load_settings(&hooks_path);
    let mut entries: Vec<Value> = event_entries(&settings)
        .into_iter()
        .filter(|e| !is_synaptic_hook(e))
        .collect();
    entries.push(synaptic_hook_entry());
    set_event_entries(&mut settings, entries);
    write_settings(&hooks_path, &settings)?;

    let script_path = repo_root.join(SCRIPT_REL);
    write_string(&script_path, HOOK_SCRIPT)?;
    Ok(vec![hooks_path, script_path])
}

/// Remove our hook from `.codex/hooks.json` (dropping the now-empty event/`hooks`
/// keys and the file when nothing else remains) and delete the helper script.
/// Foreign hooks are preserved.
fn uninstall_hook(repo_root: &Path) -> std::io::Result<()> {
    let hooks_path = repo_root.join(HOOKS_REL);
    if hooks_path.exists() {
        let mut settings = load_settings(&hooks_path);
        let remaining: Vec<Value> = event_entries(&settings)
            .into_iter()
            .filter(|e| !is_synaptic_hook(e))
            .collect();
        if let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) {
            if remaining.is_empty() {
                hooks.remove(HOOK_EVENT);
            } else {
                hooks.insert(HOOK_EVENT.to_string(), Value::Array(remaining));
            }
            if hooks.is_empty() {
                settings.remove("hooks");
            }
        }
        if settings.is_empty() {
            std::fs::remove_file(&hooks_path)?;
        } else {
            write_settings(&hooks_path, &settings)?;
        }
    }
    let _ = std::fs::remove_file(repo_root.join(SCRIPT_REL));
    Ok(())
}

/// Write `contents` to `path`, creating the parent directory (`.codex/`) first.
fn write_string(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn read(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join(rel)).unwrap()
    }

    #[test]
    fn install_writes_mcp_server_block() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        install(root).unwrap();
        let toml = read(root, CONFIG_REL);
        assert!(toml.contains("[mcp_servers.synaptic]"), "{toml}");
        assert!(toml.contains("command = \"synaptic\""), "{toml}");
        assert!(toml.contains("serve"), "{toml}");
        // Round-trips as valid TOML with the expected command.
        let parsed: DocumentMut = toml.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["synaptic"]["command"].as_str(),
            Some("synaptic")
        );
    }

    #[test]
    fn install_preserves_a_foreign_mcp_server() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".codex")).unwrap();
        fs::write(
            root.join(CONFIG_REL),
            "[mcp_servers.other]\ncommand = \"thing\"\nargs = [\"x\"]\n",
        )
        .unwrap();
        install(root).unwrap();
        let toml = read(root, CONFIG_REL);
        assert!(toml.contains("[mcp_servers.other]"), "foreign kept: {toml}");
        assert!(
            toml.contains("[mcp_servers.synaptic]"),
            "ours added: {toml}"
        );
        let parsed: DocumentMut = toml.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"].as_str(),
            Some("thing")
        );
    }

    #[test]
    fn install_preserves_explicit_mcp_servers_header_with_direct_keys() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".codex")).unwrap();
        // A user who wrote an explicit [mcp_servers] header with a direct key
        // alongside a sub-table server. Both must survive next to ours.
        fs::write(
            root.join(CONFIG_REL),
            "[mcp_servers]\nenabled = true\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();
        install(root).unwrap();
        let toml = read(root, CONFIG_REL);
        let parsed: DocumentMut = toml.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["enabled"].as_bool(),
            Some(true),
            "direct key survives: {toml}"
        );
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"].as_str(),
            Some("x"),
            "foreign server survives: {toml}"
        );
        assert_eq!(
            parsed["mcp_servers"]["synaptic"]["command"].as_str(),
            Some("synaptic"),
            "ours added: {toml}"
        );
    }

    #[test]
    fn install_preserves_unrelated_top_level_config() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".codex")).unwrap();
        fs::write(
            root.join(CONFIG_REL),
            "model = \"gpt-5\"\n\n[history]\npersistence = \"save-all\"\n",
        )
        .unwrap();
        install(root).unwrap();
        let toml = read(root, CONFIG_REL);
        assert!(toml.contains("model = \"gpt-5\""), "{toml}");
        assert!(toml.contains("[history]"), "{toml}");
        assert!(toml.contains("[mcp_servers.synaptic]"), "{toml}");
    }

    #[test]
    fn install_writes_sessionstart_hook_and_script() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        install(root).unwrap();
        let hooks = read(root, HOOKS_REL);
        // The hook is registered under SessionStart (NOT PreToolUse, which Codex
        // does not honor additionalContext on), with a Windows override.
        let v: Value = serde_json::from_str(&hooks).unwrap();
        let entry = &v["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(entry["type"], json!("command"));
        assert!(
            entry["command"]
                .as_str()
                .is_some_and(|c| c.contains("synaptic-hook.py")),
            "{hooks}"
        );
        assert!(
            entry.get("commandWindows").is_some(),
            "win override: {hooks}"
        );
        // The script exists and uses the model-visible additionalContext channel.
        assert!(root.join(SCRIPT_REL).exists());
        let script = read(root, SCRIPT_REL);
        assert!(script.contains("graph.json"), "{script}");
        assert!(script.contains("additionalContext"), "{script}");
        assert!(script.contains("SessionStart"), "{script}");
    }

    #[test]
    fn install_preserves_a_foreign_hook() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".codex")).unwrap();
        // A foreign hook on a different event must survive when we add ours.
        fs::write(
            root.join(HOOKS_REL),
            r#"{"hooks":{"PreToolUse":[{"matcher":"apply_patch","hooks":[{"type":"command","command":"echo keep-me"}]}]}}"#,
        )
        .unwrap();
        install(root).unwrap();
        let hooks = read(root, HOOKS_REL);
        assert!(hooks.contains("keep-me"), "foreign kept: {hooks}");
        assert!(hooks.contains("synaptic-hook.py"), "ours added: {hooks}");
        let v: Value = serde_json::from_str(&hooks).unwrap();
        assert!(v["hooks"]["PreToolUse"].is_array(), "foreign event kept");
        assert!(v["hooks"]["SessionStart"].is_array(), "our event added");
    }

    #[test]
    fn install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        install(root).unwrap();
        install(root).unwrap();
        let toml = read(root, CONFIG_REL);
        assert_eq!(
            toml.matches("[mcp_servers.synaptic]").count(),
            1,
            "no duplicate server: {toml}"
        );
        let hooks = read(root, HOOKS_REL);
        let v: Value = serde_json::from_str(&hooks).unwrap();
        let entries = v["hooks"]["SessionStart"].as_array().unwrap();
        let ours = entries.iter().filter(|e| is_synaptic_hook(e)).count();
        assert_eq!(ours, 1, "no duplicate hook: {hooks}");
    }

    #[test]
    fn uninstall_removes_ours_and_keeps_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".codex")).unwrap();
        fs::write(
            root.join(CONFIG_REL),
            "[mcp_servers.other]\ncommand = \"thing\"\n",
        )
        .unwrap();
        fs::write(
            root.join(HOOKS_REL),
            r#"{"hooks":{"PreToolUse":[{"matcher":"apply_patch","hooks":[{"type":"command","command":"echo keep-me"}]}]}}"#,
        )
        .unwrap();
        install(root).unwrap();
        uninstall(root).unwrap();

        let toml = read(root, CONFIG_REL);
        assert!(!toml.contains("synaptic"), "our server gone: {toml}");
        assert!(toml.contains("[mcp_servers.other]"), "foreign kept: {toml}");

        let hooks = read(root, HOOKS_REL);
        assert!(!hooks.contains("synaptic"), "our hook gone: {hooks}");
        assert!(hooks.contains("keep-me"), "foreign kept: {hooks}");

        assert!(!root.join(SCRIPT_REL).exists(), "hook script removed");
    }

    #[test]
    fn uninstall_removes_now_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        install(root).unwrap();
        uninstall(root).unwrap();
        assert!(
            !root.join(CONFIG_REL).exists(),
            "config with only our server is removed"
        );
        assert!(
            !root.join(HOOKS_REL).exists(),
            "hooks with only our entry is removed"
        );
        assert!(!root.join(SCRIPT_REL).exists(), "script removed");
    }

    #[test]
    fn uninstall_on_missing_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        assert!(uninstall(dir.path()).is_ok());
    }

    // --- global (app) MCP registration ---

    #[test]
    fn global_install_writes_named_server_with_absolute_graph() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codexhome");
        let repo = dir.path().join("webapp");
        fs::create_dir_all(&repo).unwrap();
        let written = install_global_mcp(&home, &repo).unwrap();
        assert!(written.ends_with("config.toml"), "{written:?}");
        let toml = fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed: DocumentMut = toml.parse().unwrap();
        let s = &parsed["mcp_servers"]["synaptic-webapp"];
        assert_eq!(s["command"].as_str(), Some("synaptic"), "{toml}");
        let args: Vec<String> = s["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(args.first().map(String::as_str), Some("serve"), "{args:?}");
        assert!(args.iter().any(|a| a == "--graph"), "{args:?}");
        assert!(
            args.iter().any(|a| a
                .replace('\\', "/")
                .ends_with("webapp/synaptic-out/graph.json")),
            "absolute graph path expected: {args:?}"
        );
    }

    #[test]
    fn global_install_preserves_foreign_servers_and_projects() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codexhome");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            "[projects.'C:\\x']\ntrust_level = \"trusted\"\n\n[mcp_servers.sonarqube]\ncommand = \"docker\"\n",
        )
        .unwrap();
        let repo = dir.path().join("vpn");
        fs::create_dir_all(&repo).unwrap();
        install_global_mcp(&home, &repo).unwrap();
        let toml = fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed: DocumentMut = toml.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["sonarqube"]["command"].as_str(),
            Some("docker"),
            "foreign server kept: {toml}"
        );
        assert!(toml.contains("[projects."), "projects table kept: {toml}");
        assert!(
            parsed["mcp_servers"]["synaptic-vpn"]["command"].is_str(),
            "ours added: {toml}"
        );
    }

    #[test]
    fn global_install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codexhome");
        let repo = dir.path().join("hub");
        fs::create_dir_all(&repo).unwrap();
        install_global_mcp(&home, &repo).unwrap();
        install_global_mcp(&home, &repo).unwrap();
        // A duplicate table would make this fail to parse (TOML forbids it).
        let toml = fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed: DocumentMut = toml.parse().unwrap();
        assert!(
            parsed["mcp_servers"]["synaptic-hub"]["command"].is_str(),
            "{toml}"
        );
    }

    #[test]
    fn global_uninstall_removes_ours_keeps_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codexhome");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            "[mcp_servers.sonarqube]\ncommand = \"docker\"\n",
        )
        .unwrap();
        let repo = dir.path().join("login");
        fs::create_dir_all(&repo).unwrap();
        install_global_mcp(&home, &repo).unwrap();
        uninstall_global_mcp(&home, &repo).unwrap();
        let toml = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(!toml.contains("synaptic-login"), "ours gone: {toml}");
        assert!(
            toml.contains("[mcp_servers.sonarqube]"),
            "foreign kept: {toml}"
        );
    }

    #[test]
    fn global_uninstall_removes_now_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codexhome");
        let repo = dir.path().join("wrapper");
        fs::create_dir_all(&repo).unwrap();
        install_global_mcp(&home, &repo).unwrap();
        uninstall_global_mcp(&home, &repo).unwrap();
        assert!(
            !home.join("config.toml").exists(),
            "global config holding only our server is removed"
        );
    }

    #[test]
    fn global_server_name_is_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("New Project 2");
        fs::create_dir_all(&repo).unwrap();
        assert_eq!(global_server_name(&repo), "synaptic-new-project-2");
    }
}
