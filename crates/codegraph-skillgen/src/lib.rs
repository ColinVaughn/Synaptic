//! The CodeGraph "skill" — the Markdown frontend that drives a host AI assistant
//! to query the graph. A build-time generator plus the runtime installer,
//! **CodeGraph-branded** (our own fragments).
//!
//! Generation is pure `@@SLOT@@` string substitution over an embedded template
//! (no template engine), so the render is deterministic and unit-testable. The
//! installer writes the per-platform skill file (where the platform has one) and
//! injects an always-on section into `CLAUDE.md`/`AGENTS.md`/`GEMINI.md` via a
//! marker block that's replaced in place on reinstall (idempotent upgrade).
//!
//! Scope: a focused platform set (Claude/Agents/Gemini) rather
//! than all ~20 integrations or monolith hosts (deferred). Installing the Claude
//! platform also registers `PreToolUse` `settings.json` hooks (see
//! [`settings_hooks`]). Git hooks are a separate command (`codegraph hook
//! install`, C1d). Drift-guarding the rendered artifacts lives in [`drift`].
#![forbid(unsafe_code)]

pub mod drift;
pub mod settings_hooks;

pub use drift::{bless, check_drift, render_all, RenderedArtifact};
pub use settings_hooks::{install_settings_hook, uninstall_settings_hook};

use std::path::{Path, PathBuf};

const SKILL_TEMPLATE: &str = r#"---
name: codegraph
description: Query this repo's CodeGraph knowledge graph before broad file exploration.
---

# CodeGraph — @@HOST@@

This repository has a **CodeGraph** knowledge graph of its code. Before grepping
or reading files broadly, query the graph — it is faster and surfaces
relationships (calls, imports, inheritance, impact).

## Build / refresh
- `codegraph extract .` — build the graph into `codegraph-out/`.
- `codegraph update <changed files>` — incremental rebuild after edits.

## Query
- `codegraph query "<question>"` — the relevant subgraph for a question.
- `codegraph explain <node>` — a node and its neighbours.
- `codegraph path <a> <b>` — shortest path between two nodes.
- `codegraph affected <node>` — what (transitively) depends on a node.

## MCP (preferred for @@HOST@@)
Run `codegraph serve` and use the MCP tools: `query_graph`, `get_node`,
`get_neighbors`, `get_community`, `god_nodes`, `graph_stats`, `shortest_path`,
plus the PR tools `list_prs` / `get_pr_impact` / `triage_prs`.

Reach for the graph on "what calls X", "what breaks if I change Y", and "how does
A reach B" — don't reconstruct those by reading files.
"#;

const MARK_START: &str = "<!-- codegraph:start -->";
const MARK_END: &str = "<!-- codegraph:end -->";

/// The always-on block injected into `CLAUDE.md`/`AGENTS.md`/`GEMINI.md`.
fn always_on_section() -> String {
    format!(
        "{MARK_START}\n\
## CodeGraph\n\
\n\
This repo has a CodeGraph knowledge graph (`codegraph-out/graph.json`). Query it\n\
before broad file exploration: `codegraph query \"<question>\"`, `codegraph affected\n\
<node>`, or run `codegraph serve` for the MCP tools. Rebuild with `codegraph\n\
extract .` / `codegraph update <files>`.\n\
{MARK_END}"
    )
}

/// A host assistant integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Claude,
    Agents,
    Gemini,
    Cursor,
    Copilot,
    Kilo,
}

impl Platform {
    /// Parse a platform name (case-insensitive). `codex`/`opencode` map onto the
    /// generic `Agents` platform — both read `AGENTS.md`, so they need no
    /// dedicated variant.
    pub fn parse(s: &str) -> Option<Platform> {
        match s.to_lowercase().as_str() {
            "claude" => Some(Platform::Claude),
            "agents" | "agent" | "codex" | "opencode" => Some(Platform::Agents),
            "gemini" => Some(Platform::Gemini),
            "cursor" => Some(Platform::Cursor),
            "copilot" | "github-copilot" => Some(Platform::Copilot),
            "kilo" | "kilocode" => Some(Platform::Kilo),
            _ => None,
        }
    }

    /// All platforms (for `uninstall --all`).
    pub fn all() -> [Platform; 6] {
        [
            Platform::Claude,
            Platform::Agents,
            Platform::Gemini,
            Platform::Cursor,
            Platform::Copilot,
            Platform::Kilo,
        ]
    }

    fn display(self) -> &'static str {
        match self {
            Platform::Claude => "Claude Code",
            Platform::Agents => "your AI agent",
            Platform::Gemini => "Gemini",
            Platform::Cursor => "Cursor",
            Platform::Copilot => "GitHub Copilot",
            Platform::Kilo => "Kilo Code",
        }
    }

    /// Stable, filesystem-safe key (used for drift snapshot filenames).
    pub fn key(self) -> &'static str {
        match self {
            Platform::Claude => "claude",
            Platform::Agents => "agents",
            Platform::Gemini => "gemini",
            Platform::Cursor => "cursor",
            Platform::Copilot => "copilot",
            Platform::Kilo => "kilo",
        }
    }

    /// Repo-relative path of the dedicated skill file, if the platform uses one.
    fn skill_dest(self) -> Option<&'static str> {
        match self {
            Platform::Claude => Some(".claude/skills/codegraph/SKILL.md"),
            // The rest consume a single always-on instructions file directly.
            Platform::Agents
            | Platform::Gemini
            | Platform::Cursor
            | Platform::Copilot
            | Platform::Kilo => None,
        }
    }

    /// The always-on instructions file this platform reads.
    fn always_on_file(self) -> &'static str {
        match self {
            Platform::Claude => "CLAUDE.md",
            Platform::Agents => "AGENTS.md",
            Platform::Gemini => "GEMINI.md",
            Platform::Cursor => ".cursorrules",
            Platform::Copilot => ".github/copilot-instructions.md",
            Platform::Kilo => ".kilocode/rules/codegraph.md",
        }
    }
}

/// Render the skill markdown for a platform (fills `@@SLOT@@`s).
pub fn render_skill(platform: Platform) -> String {
    let out = SKILL_TEMPLATE.replace("@@HOST@@", platform.display());
    debug_assert!(
        !out.contains("@@"),
        "unfilled slot remains in skill template"
    );
    out
}

/// Insert (or replace) the marker block in `existing`, returning the new content
/// (the idempotent-upgrade primitive).
pub fn replace_or_append_section(existing: &str, section: &str) -> String {
    if let Some(s) = existing.find(MARK_START) {
        // Replace our block in place. End at MARK_END if it follows the start,
        // else replace to EOF: a dangling MARK_START (a truncated/hand-edited
        // block) must be replaced, not duplicated.
        let tail = match existing[s..].find(MARK_END) {
            Some(rel) => &existing[s + rel + MARK_END.len()..],
            None => "",
        };
        format!("{}{}{}", &existing[..s], section, tail)
    } else if existing.trim().is_empty() {
        format!("{section}\n")
    } else {
        let sep = if existing.ends_with("\n\n") {
            ""
        } else if existing.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        format!("{existing}{sep}{section}\n")
    }
}

/// Remove our marker block from `body`, collapsing only the seam it leaves
/// behind (NOT blank lines elsewhere — uninstall must not reformat foreign
/// prose) and normalizing to a single trailing newline.
fn strip_section(body: &str) -> String {
    let (Some(s), Some(e)) = (body.find(MARK_START), body.find(MARK_END)) else {
        return body.to_string();
    };
    let end = e + MARK_END.len();
    // Trim only at the cut points: trailing whitespace of the content before the
    // block, and blank lines immediately after it.
    let before = body[..s].trim_end();
    let after = body[end..].trim_start_matches(['\n', '\r']);
    let joined = match (before.is_empty(), after.is_empty()) {
        (true, _) => after.to_string(),
        (false, true) => before.to_string(),
        (false, false) => format!("{before}\n\n{after}"),
    };
    let trimmed = joined.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

/// Install the skill for `platform` under `repo_root`: write the skill file (if
/// any) and inject the always-on section. Idempotent. Returns the paths written.
pub fn install(platform: Platform, repo_root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    if let Some(dest) = platform.skill_dest() {
        let path = repo_root.join(dest);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, render_skill(platform))?;
        written.push(path);
    }
    let ao_path = repo_root.join(platform.always_on_file());
    // Some always-on files live in a subdir (.github/, .kilocode/rules/).
    if let Some(parent) = ao_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&ao_path).unwrap_or_default();
    let updated = replace_or_append_section(&existing, &always_on_section());
    std::fs::write(&ao_path, updated)?;
    written.push(ao_path);
    // Claude Code also reads PreToolUse hooks from .claude/settings.json; install
    // them so the assistant is nudged to query the graph before broad exploration.
    if platform == Platform::Claude {
        written.push(settings_hooks::install_settings_hook(repo_root)?);
    }
    Ok(written)
}

/// Remove the skill for `platform`: delete the skill file (if any) and strip the
/// always-on section (removing the file if nothing else remains).
pub fn uninstall(platform: Platform, repo_root: &Path) -> std::io::Result<()> {
    if let Some(dest) = platform.skill_dest() {
        let _ = std::fs::remove_file(repo_root.join(dest));
        // Tidy now-empty skill dirs.
        if let Some(parent) = repo_root.join(dest).parent() {
            let _ = std::fs::remove_dir(parent);
            if let Some(grand) = parent.parent() {
                let _ = std::fs::remove_dir(grand);
            }
        }
    }
    let ao_path = repo_root.join(platform.always_on_file());
    if let Ok(existing) = std::fs::read_to_string(&ao_path) {
        let stripped = strip_section(&existing);
        if stripped.trim().is_empty() {
            let _ = std::fs::remove_file(&ao_path);
        } else {
            std::fs::write(&ao_path, stripped)?;
        }
    }
    if platform == Platform::Claude {
        settings_hooks::uninstall_settings_hook(repo_root)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_fills_all_slots() {
        let s = render_skill(Platform::Claude);
        assert!(!s.contains("@@"), "no unfilled slots");
        assert!(s.contains("# CodeGraph — Claude Code"));
        assert!(s.contains("codegraph query"));
        assert!(s.contains("codegraph serve"));
    }

    #[test]
    fn section_appends_then_replaces_in_place() {
        let doc = "# My Project\n\nSome notes.\n";
        let once = replace_or_append_section(doc, &always_on_section());
        assert!(once.contains("# My Project"), "existing content kept");
        assert!(once.contains("## CodeGraph"));
        assert_eq!(once.matches(MARK_START).count(), 1);

        // Reinstall (e.g. an updated section) replaces in place, still one block.
        let twice = replace_or_append_section(&once, &always_on_section());
        assert_eq!(twice.matches(MARK_START).count(), 1, "idempotent");
        assert!(twice.contains("# My Project"));
    }

    #[test]
    fn dangling_start_marker_is_replaced_not_duplicated() {
        // A truncated block (MARK_START but no MARK_END) must not yield two blocks.
        let broken = format!("# Doc\n\n{MARK_START}\n## CodeGraph\n(partial...");
        let fixed = replace_or_append_section(&broken, &always_on_section());
        assert_eq!(
            fixed.matches(MARK_START).count(),
            1,
            "no duplicate block: {fixed}"
        );
        assert_eq!(fixed.matches(MARK_END).count(), 1);
        assert!(fixed.contains("# Doc"));
    }

    #[test]
    fn strip_section_preserves_foreign_blank_lines() {
        // Uninstall must remove only our block, not collapse the user's spacing.
        let doc = format!(
            "# Doc\n\n\nIntentional triple-newline above.\n\n{MARK_START}\nx\n{MARK_END}\n"
        );
        let stripped = strip_section(&doc);
        assert!(!stripped.contains(MARK_START), "block removed");
        assert!(
            stripped.contains("# Doc\n\n\nIntentional"),
            "foreign blank lines preserved: {stripped:?}"
        );
        assert!(
            stripped.ends_with('\n') && !stripped.ends_with("\n\n"),
            "single trailing nl"
        );
    }

    #[test]
    fn install_uninstall_round_trip_claude() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("CLAUDE.md"), "# Repo\n\nKeep this.\n").unwrap();

        let written = install(Platform::Claude, root).unwrap();
        assert!(written.iter().any(|p| p.ends_with("SKILL.md")));
        assert!(root.join(".claude/skills/codegraph/SKILL.md").exists());
        let claude_md = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(claude_md.contains("## CodeGraph") && claude_md.contains("Keep this."));

        uninstall(Platform::Claude, root).unwrap();
        assert!(!root.join(".claude/skills/codegraph/SKILL.md").exists());
        let after = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(!after.contains("## CodeGraph"), "section removed");
        assert!(after.contains("Keep this."), "foreign content survives");
    }

    #[test]
    fn install_into_agents_md_has_no_skill_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let written = install(Platform::Agents, root).unwrap();
        assert_eq!(written.len(), 1, "AGENTS.md only, no skill file");
        assert!(root.join("AGENTS.md").exists());
        // Uninstall removes the whole file (we created it, nothing else inside).
        uninstall(Platform::Agents, root).unwrap();
        assert!(!root.join("AGENTS.md").exists());
    }

    #[test]
    fn parse_aliases_codex_and_opencode_to_agents() {
        assert_eq!(Platform::parse("codex"), Some(Platform::Agents));
        assert_eq!(Platform::parse("opencode"), Some(Platform::Agents));
        assert_eq!(Platform::parse("cursor"), Some(Platform::Cursor));
        assert_eq!(Platform::parse("kilocode"), Some(Platform::Kilo));
        assert_eq!(Platform::parse("nope"), None);
    }

    #[test]
    fn install_copilot_creates_nested_instructions_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The always-on file lives under .github/, so install must create the dir.
        let written = install(Platform::Copilot, root).unwrap();
        assert_eq!(written.len(), 1, "instructions file only");
        let ao = root.join(".github/copilot-instructions.md");
        assert!(ao.exists(), "{written:?}");
        assert!(std::fs::read_to_string(&ao)
            .unwrap()
            .contains("## CodeGraph"));
        uninstall(Platform::Copilot, root).unwrap();
        assert!(!ao.exists(), "empty instructions file removed on uninstall");
    }

    #[test]
    fn install_kilo_creates_rules_file_in_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        install(Platform::Kilo, root).unwrap();
        let rules = root.join(".kilocode/rules/codegraph.md");
        assert!(rules.exists());
        assert!(std::fs::read_to_string(&rules)
            .unwrap()
            .contains("CodeGraph"));
    }
}
