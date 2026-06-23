//! Claude Code `PreToolUse` hooks written into `.claude/settings.json`,
//! Synaptic-branded.
//!
//! Two hooks nudge (never block) the assistant to query the graph before broad
//! file exploration, only when `synaptic-out/graph.json` exists:
//!   - a **Bash** hook that fires on `grep`/`rg`/`find`/… command strings, and
//!   - a **Read|Glob** hook that fires on reading a source/doc file outside
//!     `synaptic-out/`.
//!
//! Both shell snippets parse the tool input with `python3` (a near-universal dev
//! dependency) and fail open, so a legitimate tool call always proceeds. Merge
//! is idempotent: any prior Synaptic hook (matched by matcher + the literal
//! `synaptic` in its body) is removed before the current pair is appended, so
//! reinstalling never duplicates and uninstall removes exactly our entries.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// Matchers a Synaptic PreToolUse hook may use (current + legacy), used to
/// recognise our own entries for idempotent replace / clean removal.
const HOOK_MATCHERS: &[&str] = &["Bash", "Read|Glob", "Glob|Grep"];

/// The Bash-search hook: nudge before a grep/find-style shell command.
const BASH_HOOK_COMMAND: &str = r#"CMD=$(python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('tool_input',d).get('command',''))" 2>/dev/null || true); case "$CMD" in *grep*|*rg\ *|*ripgrep*|*find\ *|*fd\ *|*ack\ *|*ag\ *)   [ -f synaptic-out/graph.json ] &&   echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"MANDATORY: synaptic-out/graph.json exists. You MUST run `synaptic query \"<question>\"` before grepping raw files. Only grep after synaptic has oriented you, or to modify/debug specific lines."}}'   || true ;; esac"#;

/// The Read|Glob hook: nudge before reading a source/doc file outside the graph dir.
const READ_HOOK_COMMAND: &str = r#"HIT=$(python3 -c "import json,sys;d=json.load(sys.stdin);t=d.get('tool_input',d);s=(str(t.get('file_path') or '')+' '+str(t.get('pattern') or '')+' '+str(t.get('path') or '')).lower().replace(chr(92),'/');exts=('.py','.js','.ts','.tsx','.jsx','.go','.rs','.java','.rb','.c','.h','.cpp','.hpp','.cc','.cs','.kt','.swift','.php','.scala','.lua','.sh','.md','.rst','.txt','.mdx');sys.stdout.write('1' if 'synaptic-out/' not in s and any(e in s for e in exts) else '')" 2>/dev/null || true); if [ "$HIT" = 1 ] && [ -f synaptic-out/graph.json ]; then echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"MANDATORY: synaptic-out/graph.json exists. You MUST run synaptic before reading source files. Use: `synaptic query \"<question>\"` (scoped subgraph), `synaptic explain \"<concept>\"`, or `synaptic path \"<A>\" \"<B>\"`. Only read raw files after synaptic has oriented you, or to modify/debug specific lines. This rule applies to subagents too -- include it in every subagent prompt involving code exploration."}}'; fi || true"#;

fn hook_entry(matcher: &str, command: &str) -> Value {
    json!({
        "matcher": matcher,
        "hooks": [ { "type": "command", "command": command } ]
    })
}

/// Our two PreToolUse entries.
pub(crate) fn synaptic_hooks() -> Vec<Value> {
    vec![
        hook_entry("Bash", BASH_HOOK_COMMAND),
        hook_entry("Read|Glob", READ_HOOK_COMMAND),
    ]
}

/// True if `hook` looks like one of ours: a recognised matcher and the literal
/// `synaptic` somewhere in its serialized body.
fn is_synaptic_hook(hook: &Value) -> bool {
    let matcher_ok = hook
        .get("matcher")
        .and_then(Value::as_str)
        .map(|m| HOOK_MATCHERS.contains(&m))
        .unwrap_or(false);
    matcher_ok && hook.to_string().contains("synaptic")
}

/// Path to the platform settings file (`.claude/settings.json`).
fn settings_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".claude").join("settings.json")
}

/// Parse the existing settings, treating a missing or corrupt file as empty.
/// Crate-visible so the Codex `hooks.json` writer can reuse it (same schema).
pub(crate) fn load_settings(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// The current `PreToolUse` array with any Synaptic entries removed.
fn pretooluse_without_ours(settings: &Map<String, Value>) -> Vec<Value> {
    settings
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|h| !is_synaptic_hook(h))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn write_settings(path: &Path, settings: &Map<String, Value>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Pretty-print (2-space), with a trailing newline.
    let mut text = serde_json::to_string_pretty(&Value::Object(settings.clone()))?;
    text.push('\n');
    std::fs::write(path, text)
}

/// Set `settings.hooks.PreToolUse` to `hooks`, creating the nested objects as
/// needed and preserving everything else.
fn set_pretooluse(settings: &mut Map<String, Value>, hooks: Vec<Value>) {
    let hooks_obj = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks_obj.is_object() {
        *hooks_obj = Value::Object(Map::new());
    }
    if let Some(map) = hooks_obj.as_object_mut() {
        map.insert("PreToolUse".to_string(), Value::Array(hooks));
    }
}

/// Install (or refresh) the Synaptic PreToolUse hooks in `.claude/settings.json`,
/// preserving foreign content. Idempotent. Returns the path written.
pub fn install_settings_hook(repo_root: &Path) -> std::io::Result<PathBuf> {
    let path = settings_path(repo_root);
    let mut settings = load_settings(&path);
    let mut pre = pretooluse_without_ours(&settings);
    pre.extend(synaptic_hooks());
    set_pretooluse(&mut settings, pre);
    write_settings(&path, &settings)?;
    Ok(path)
}

/// Remove the Synaptic PreToolUse hooks. No-op if the file is missing or holds
/// none of ours; foreign hooks are preserved.
pub fn uninstall_settings_hook(repo_root: &Path) -> std::io::Result<()> {
    let path = settings_path(repo_root);
    if !path.exists() {
        return Ok(());
    }
    let mut settings = load_settings(&path);
    let original_len = settings
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let pre = pretooluse_without_ours(&settings);
    if pre.len() == original_len {
        return Ok(()); // nothing of ours present
    }
    set_pretooluse(&mut settings, pre);
    write_settings(&path, &settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_pre(root: &Path) -> Vec<Value> {
        let settings = load_settings(&settings_path(root));
        settings["hooks"]["PreToolUse"].as_array().unwrap().clone()
    }

    #[test]
    fn install_writes_two_hooks_preserving_foreign_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Pre-existing settings with an unrelated hook + an unrelated top-level key.
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            settings_path(root),
            r#"{"model":"sonnet","hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"echo keep-me"}]}]}}"#,
        )
        .unwrap();

        install_settings_hook(root).unwrap();
        let pre = read_pre(root);
        // foreign Write hook kept + our two appended.
        assert_eq!(pre.len(), 3, "{pre:#?}");
        assert!(pre.iter().any(|h| h["matcher"] == "Write"));
        assert!(pre
            .iter()
            .any(|h| h["matcher"] == "Bash" && is_synaptic_hook(h)));
        assert!(pre
            .iter()
            .any(|h| h["matcher"] == "Read|Glob" && is_synaptic_hook(h)));
        // Unrelated top-level key survives.
        let settings = load_settings(&settings_path(root));
        assert_eq!(settings["model"], json!("sonnet"));
    }

    #[test]
    fn install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        install_settings_hook(root).unwrap();
        install_settings_hook(root).unwrap();
        let pre = read_pre(root);
        let ours = pre.iter().filter(|h| is_synaptic_hook(h)).count();
        assert_eq!(ours, 2, "reinstall must not duplicate: {pre:#?}");
    }

    #[test]
    fn uninstall_removes_ours_keeps_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            settings_path(root),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"echo keep-me"}]}]}}"#,
        )
        .unwrap();
        install_settings_hook(root).unwrap();
        uninstall_settings_hook(root).unwrap();
        let pre = read_pre(root);
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["matcher"], json!("Write"));
    }

    #[test]
    fn corrupt_settings_is_reset_not_crash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(settings_path(root), "{ not json").unwrap();
        install_settings_hook(root).unwrap();
        let pre = read_pre(root);
        assert_eq!(pre.iter().filter(|h| is_synaptic_hook(h)).count(), 2);
    }

    #[test]
    fn uninstall_on_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        // No .claude/settings.json at all.
        assert!(uninstall_settings_hook(dir.path()).is_ok());
    }

    #[test]
    fn hook_commands_are_synaptic_branded() {
        // Guard against shipping wrong branding / wrong graph dir.
        assert!(BASH_HOOK_COMMAND.contains("synaptic-out/graph.json"));
        assert!(BASH_HOOK_COMMAND.contains("synaptic query"));
        assert!(READ_HOOK_COMMAND.contains("synaptic explain"));
    }
}
