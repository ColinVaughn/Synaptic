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

pub mod codex_config;
pub mod drift;
pub mod settings_hooks;

pub use drift::{bless, check_drift, render_all, RenderedArtifact};
pub use settings_hooks::{install_settings_hook, uninstall_settings_hook};

use std::path::{Path, PathBuf};

const SKILL_TEMPLATE: &str = r#"---
name: codegraph
description: Queries this repo's CodeGraph knowledge graph (symbols and their calls, imports, and inheritance) instead of grepping or reading files. Use when exploring an unfamiliar codebase, finding what calls or depends on a symbol, tracing how one part reaches another, reading a symbol's source, or judging the blast radius of a change.
---

# CodeGraph for @@HOST@@

This repository has a **CodeGraph** knowledge graph of its code. Before grepping
or reading files broadly, query the graph. It is faster and surfaces
relationships (calls, imports, inheritance, impact).

## Build / refresh
- `codegraph extract .`: build the graph into `codegraph-out/`.
- `codegraph update <changed files>`: incremental rebuild after edits.

## Query (CLI)
- `codegraph query "<question>"`: the relevant subgraph for a question.
- `codegraph explain <node>`: a node and its neighbours.
- `codegraph path <a> <b>`: shortest path between two nodes.
- `codegraph affected <node>`: what (transitively) depends on a node.
- `codegraph search "<cgql>"` / `--pattern <name>`: structural search (CGQL) by
  kind/visibility/loc/fan-in-out, variable-length paths, and `count(...)`
  aggregation + named patterns (singleton, factory, observer, service-locator,
  god-class). Not text search. `.name` is the bare symbol (no `()`); use `=~` for
  a regex/substring match. `--explain` shows the plan; `--save`/`--saved` store
  queries.
- `codegraph diff <rev1> [rev2]` (or `--since <date>`): how the graph changed
  between two git revisions (new/removed dependencies, removed APIs, drift, new
  cycles, hotspots); `--html` writes a report.
- `codegraph refactor rename <name> --to <new>` (also `move`/`extract`): a
  confidence-scored plan (plan.json + plan.md) for you to apply; CodeGraph never
  edits source. Then `codegraph refactor verify --plan <plan.json>` checks the
  graph after you edit.
- `codegraph predict [<files>...]` (or `--base <rev>`): forecast a change BEFORE
  you make it -- the graph nodes the changed files define, the reverse-impact
  blast radius that depends on them, which edited symbols are public API, the
  tests that exercise the code, new import cycles, and a verify checklist
  (forecast.json + forecast.md). Run it before editing a symbol other code
  depends on.
- `codegraph predict --edit "<kind>:<symbol>"`: analytic mode -- forecast a
  DESCRIBED edit (kind = delete, signature, or visibility) before writing any
  code. Reports the predicted graph delta (node and edges removed, public API
  removed) and which dependents will break vs need review.
- `codegraph speculate [<files>...]`: the ground-truth check -- apply your pending
  change in a throwaway git worktree and actually RUN the forecast's at-risk tests
  plus a build/type-check, reporting real pass/fail (report.json + report.md). It
  is disposable and never touches your working tree. Because it executes commands
  it is NOT part of the read-only MCP surface by default; it appears as the MCP
  `speculate` tool only when the server is started with `--allow-exec`, otherwise
  run it here on the CLI.

## MCP (preferred for @@HOST@@)
Use the **codegraph** MCP server's tools. Start with `query_graph`, then:
- `get_source` -- read a symbol's actual code (no need to open the file).
- `affected` -- the blast radius of changing a symbol; `working_changes_impact`
  does the same for your current git diff (no PR needed).
- `predict_impact` -- forecast a change before you make it: pass the files you
  are about to edit (or omit them for your current diff) to get the blast radius,
  public APIs at risk, the tests that exercise it, and a verify checklist. Reach
  for it before editing.
- `affected_tests` -- predictive test selection: the tests that exercise the code
  you are about to change. Run those before and after editing.
- `predict_edit` -- what breaks if you delete / change the signature of / make
  private a symbol (classified into "will break" vs "to review").
- `speculate` (present only when the server runs with `--allow-exec`) -- run the
  change for real in a throwaway worktree and report actual test/build pass/fail.
  Off by default because it executes commands; otherwise use the CLI `speculate`.
- `find_callers` / `find_callees` -- who calls a symbol / what it calls.
- `get_neighbors`, `shortest_path`, `god_nodes`, `graph_stats`, `get_node`,
  `get_community` -- navigate and inspect the graph.
- `structural_search` -- CGQL or a named pattern (kind/loc/fan-in-out, not text).
  Structured results include each match's captured signature (params + return).
- `describe_node` -- a compact "takes X, returns Y, calls Z" summary of a symbol
  from its signature and outgoing calls; handy for writing a tool/function blurb.
- `time_travel_diff` -- how the graph changed between two git revisions.
- `plan_rename` -- a plan-only, confidence-scored rename plan (never edits;
  apply it, then `codegraph refactor verify` on the CLI).
- `list_prs` / `get_pr_impact` / `triage_prs` -- graph-aware PR review (need `gh`).

Reference them with your client's MCP prefix (Claude Code:
`mcp__codegraph__query_graph`). The server's `initialize` reply describes the
toolset, and each tool documents its parameters. If the server is not already
connected, start it with `codegraph serve`.

Reach for the graph on "what calls X", "what breaks if I change Y", "how does A
reach B", and to read a symbol's code. Don't reconstruct those by reading files.
Impact analysis crosses language boundaries: a change to a Rust function exported
to Python via PyO3, an HTTP or gRPC handler and the clients that call it, or a
binary a script invokes all surface as dependents, because those couplings are
graph edges too (subprocess `invokes`, FFI `binds_native`, service
`calls_service`/`handled_by`).
Before editing a symbol other code depends on, forecast the change with
`predict_impact` (or `codegraph predict`) and run the checks it lists.

## Verify a change before you commit (grounded review)
A change can look correct and still break callers or tests. Don't rely on
re-reading your own diff -- ground the judgment in two signals the graph gives you
for free:
1. Forecast it: `codegraph predict <files>` for the blast radius, the public APIs
   at risk, the at-risk tests, the risk score, and a verify checklist. For an edit
   you have only described (not yet written), use `codegraph predict --edit
   "<kind>:<symbol>"`.
2. Confirm it for real: `codegraph speculate <files>` applies the change in a
   disposable worktree and actually runs those at-risk tests plus a build/type-
   check, so you see real pass/fail instead of a guess.
3. Judge safety from three inputs only -- the diff, the forecast, and the speculate
   result -- not from re-reading the implementation. A fresh-context reviewer (a
   subagent that sees just those three) catches breakage that self-review misses,
   because its verdict is grounded in graph and sandbox evidence rather than in the
   same reasoning that produced the change.
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
before broad file exploration: `codegraph query \"<question>\"` / `codegraph affected\n\
<node>`, or the **codegraph** MCP tools if your assistant has them connected.\n\
Before editing a symbol other code depends on, forecast the blast radius with\n\
`codegraph predict <files>` (or the `predict_impact` MCP tool).\n\
Rebuild with `codegraph extract .` / `codegraph update <files>`.\n\
{MARK_END}"
    )
}

/// A host assistant integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Claude,
    Agents,
    Codex,
    Gemini,
    Cursor,
    Copilot,
    Kilo,
}

impl Platform {
    /// Parse a platform name (case-insensitive). `codex` is its own platform: it
    /// reads `AGENTS.md` like the generic `Agents`, but a full install also wires
    /// its native MCP server config and lifecycle hook (see [`crate::codex_config`]).
    /// `opencode` stays on the plain `Agents` variant (AGENTS.md only).
    pub fn parse(s: &str) -> Option<Platform> {
        match s.to_lowercase().as_str() {
            "claude" => Some(Platform::Claude),
            "agents" | "agent" | "opencode" => Some(Platform::Agents),
            "codex" => Some(Platform::Codex),
            "gemini" => Some(Platform::Gemini),
            "cursor" => Some(Platform::Cursor),
            "copilot" | "github-copilot" => Some(Platform::Copilot),
            "kilo" | "kilocode" => Some(Platform::Kilo),
            _ => None,
        }
    }

    /// All platforms (for `uninstall --all`).
    pub fn all() -> [Platform; 7] {
        [
            Platform::Claude,
            Platform::Agents,
            Platform::Codex,
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
            Platform::Codex => "Codex",
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
            Platform::Codex => "codex",
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
            | Platform::Codex
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
            // Codex reads AGENTS.md too, like the generic Agents platform.
            Platform::Agents | Platform::Codex => "AGENTS.md",
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
    written.push(inject_always_on(platform, repo_root)?);
    // Claude Code also reads PreToolUse hooks from .claude/settings.json; install
    // them so the assistant is nudged to query the graph before broad exploration.
    if platform == Platform::Claude {
        written.push(settings_hooks::install_settings_hook(repo_root)?);
    }
    // Codex natively supports MCP servers and lifecycle hooks: register the MCP
    // server (so `serve` is wired without manual config) and a PreToolUse hook
    // (the same "query the graph first" nudge) under the repo's `.codex/`.
    if platform == Platform::Codex {
        written.extend(codex_config::install(repo_root)?);
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
    strip_always_on(platform, repo_root)?;
    if platform == Platform::Claude {
        settings_hooks::uninstall_settings_hook(repo_root)?;
    }
    if platform == Platform::Codex {
        codex_config::uninstall(repo_root)?;
    }
    Ok(())
}

/// Inject (or refresh) the always-on section into the platform's instructions
/// file, creating its parent dir (some live under `.github/`, `.kilocode/rules/`).
/// Idempotent. Returns the path written.
fn inject_always_on(platform: Platform, repo_root: &Path) -> std::io::Result<PathBuf> {
    let ao_path = repo_root.join(platform.always_on_file());
    if let Some(parent) = ao_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&ao_path).unwrap_or_default();
    let updated = replace_or_append_section(&existing, &always_on_section());
    std::fs::write(&ao_path, updated)?;
    Ok(ao_path)
}

/// Strip the always-on section from the platform's instructions file, removing
/// the file if nothing else remains. No-op if the file is absent.
fn strip_always_on(platform: Platform, repo_root: &Path) -> std::io::Result<()> {
    let ao_path = repo_root.join(platform.always_on_file());
    if let Ok(existing) = std::fs::read_to_string(&ao_path) {
        let stripped = strip_section(&existing);
        if stripped.trim().is_empty() {
            let _ = std::fs::remove_file(&ao_path);
        } else {
            std::fs::write(&ao_path, stripped)?;
        }
    }
    Ok(())
}

/// Install CodeGraph for the Codex **desktop app**: register the MCP server in the
/// GLOBAL `<codex_home>/config.toml` (the app ignores a project's `.codex/` for
/// MCP) and inject the always-on AGENTS.md block. No project hook is written (the
/// app would not fire it). Returns the paths written.
pub fn install_codex_global(repo_root: &Path, codex_home: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut written = vec![inject_always_on(Platform::Codex, repo_root)?];
    written.push(codex_config::install_global_mcp(codex_home, repo_root)?);
    Ok(written)
}

/// Reverse [`install_codex_global`]: strip the AGENTS.md block and remove this
/// repo's global MCP server entry.
pub fn uninstall_codex_global(repo_root: &Path, codex_home: &Path) -> std::io::Result<()> {
    strip_always_on(Platform::Codex, repo_root)?;
    codex_config::uninstall_global_mcp(codex_home, repo_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_fills_all_slots() {
        let s = render_skill(Platform::Claude);
        assert!(!s.contains("@@"), "no unfilled slots");
        assert!(s.contains("# CodeGraph for Claude Code"));
        assert!(s.contains("codegraph query"));
        assert!(s.contains("codegraph serve"));
    }

    #[test]
    fn skill_orients_with_triggers_and_qualified_tools() {
        let s = render_skill(Platform::Claude);
        // Finding #5: the description encodes WHEN to use the skill (triggers).
        assert!(
            s.contains("Use when"),
            "description needs trigger keywords: {s}"
        );
        // Findings #4/#5: reference MCP tools with the server-qualified prefix so
        // the agent does not hit "tool not found".
        assert!(s.contains("mcp__codegraph__"), "qualify MCP tools: {s}");
        // Finding #5: do not tell an already-connected assistant to launch serve.
        assert!(
            !s.contains("Run `codegraph serve` and use"),
            "serve redundancy: {s}"
        );
    }

    #[test]
    fn generated_artifacts_are_plain_ascii() {
        // The skill + always-on ship into users' repos; keep them free of em-dashes,
        // smart quotes, and arrows so the generated text does not read as machine
        // written. (Doc comments elsewhere are exempt; this guards the OUTPUT.)
        let tells = [
            '\u{2014}', '\u{2013}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2192}',
        ];
        let mut bodies: Vec<String> = Platform::all().iter().map(|p| render_skill(*p)).collect();
        bodies.push(always_on_section());
        for body in &bodies {
            for t in tells {
                assert!(
                    !body.contains(t),
                    "AI tell {t:?} in generated artifact: {body}"
                );
            }
        }
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
    fn parse_codex_is_its_own_platform() {
        // `codex` is distinct (it gets the full MCP + hook install); `opencode`
        // stays on the plain AGENTS.md-only `Agents` platform.
        assert_eq!(Platform::parse("codex"), Some(Platform::Codex));
        assert_eq!(Platform::parse("opencode"), Some(Platform::Agents));
        assert_ne!(Platform::parse("codex"), Platform::parse("opencode"));
        assert_eq!(Platform::parse("cursor"), Some(Platform::Cursor));
        assert_eq!(Platform::parse("kilocode"), Some(Platform::Kilo));
        assert_eq!(Platform::parse("nope"), None);
    }

    #[test]
    fn install_codex_is_a_full_install() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let written = install(Platform::Codex, root).unwrap();
        // AGENTS.md always-on block (like the generic Agents platform)...
        let agents = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(agents.contains("## CodeGraph"), "{agents}");
        // ...plus the Codex-native MCP server, hook, and helper script.
        assert!(root.join(".codex/config.toml").exists());
        assert!(root.join(".codex/hooks.json").exists());
        assert!(root.join(".codex/codegraph-hook.py").exists());
        assert!(
            written.iter().any(|p| p.ends_with("config.toml"))
                && written.iter().any(|p| p.ends_with("hooks.json")),
            "returns the paths it wrote: {written:?}"
        );
    }

    #[test]
    fn codex_global_install_writes_agents_and_global_mcp_no_hook() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codexhome");
        let repo = dir.path().join("myrepo");
        std::fs::create_dir_all(&repo).unwrap();
        let written = install_codex_global(&repo, &home).unwrap();
        // AGENTS.md block in the repo (the app reads project AGENTS.md)...
        let agents = std::fs::read_to_string(repo.join("AGENTS.md")).unwrap();
        assert!(agents.contains("## CodeGraph"), "{agents}");
        // ...plus the per-repo MCP server in the GLOBAL config...
        let toml = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(toml.contains("codegraph-myrepo"), "{toml}");
        assert!(written.iter().any(|p| p.ends_with("config.toml")));
        // ...and NO project .codex/ hook (the app would not fire it).
        assert!(!repo.join(".codex/hooks.json").exists());
        assert!(!repo.join(".codex/config.toml").exists());
    }

    #[test]
    fn codex_global_uninstall_reverts() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codexhome");
        let repo = dir.path().join("myrepo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "# Repo\n\nKeep this.\n").unwrap();
        install_codex_global(&repo, &home).unwrap();
        uninstall_codex_global(&repo, &home).unwrap();
        let agents = std::fs::read_to_string(repo.join("AGENTS.md")).unwrap();
        assert!(!agents.contains("## CodeGraph"), "block removed: {agents}");
        assert!(agents.contains("Keep this."), "foreign content survives");
        assert!(
            !home.join("config.toml").exists(),
            "global entry removed (file empty)"
        );
    }

    #[test]
    fn install_uninstall_round_trip_codex() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("AGENTS.md"), "# Repo\n\nKeep this.\n").unwrap();

        install(Platform::Codex, root).unwrap();
        uninstall(Platform::Codex, root).unwrap();

        // Foreign AGENTS.md prose survives; our block is gone.
        let after = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(!after.contains("## CodeGraph"), "block removed: {after}");
        assert!(after.contains("Keep this."), "foreign content survives");
        // All Codex-native artifacts are gone.
        assert!(!root.join(".codex/config.toml").exists());
        assert!(!root.join(".codex/hooks.json").exists());
        assert!(!root.join(".codex/codegraph-hook.py").exists());
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
