# Assistant Integration

`synaptic install` wires the Synaptic skill into a host AI assistant so it
queries the graph before falling back to broad file exploration. It writes a
per-platform skill file (where the platform has one), injects an always-on
instructions section into the platform's instructions file, and (for Claude
Code) registers `PreToolUse` hooks in `.claude/settings.json`. For **Codex** it
also registers a native MCP server and a `SessionStart` hook, with a `--global`
mode for the Codex desktop app (see [Codex](#codex)). All writes are idempotent
and target the current working directory.

The companion `synaptic uninstall` reverses it, and `synaptic skill
check`/`bless` guard the generated artifacts against drift (a dev/CI tool).

## Install

```
synaptic install [platform] [--global]
synaptic install --refresh
```

`platform` defaults to `claude`. Files are written under the current directory
(the repo root). The command prints each path it wrote. `--global` applies only
to `codex` and targets the global `~/.codex/config.toml` instead of the project
(see [Codex](#codex)). `--refresh` re-renders every previously-installed skill to
the current version (see [Versioning and auto-refresh](#versioning-and-auto-refresh)).

```
synaptic install
synaptic install agents
synaptic install copilot
synaptic install codex            # Codex CLI (project .codex/)
synaptic install codex --global   # Codex desktop app (global ~/.codex)
```

### Supported platforms

| Argument(s) | Skill file | Always-on instructions file | Extra wiring |
|---|---|---|---|
| `claude` | `.claude/skills/synaptic/SKILL.md` | `CLAUDE.md` | `PreToolUse` hooks in `.claude/settings.json` |
| `codex` | none | `AGENTS.md` | MCP server + `SessionStart` hook in `.codex/` (project), or global MCP with `--global` |
| `agents`, `agent`, `opencode` | none | `AGENTS.md` | none |
| `gemini` | none | `GEMINI.md` | none |
| `cursor` | none | `.cursorrules` | none |
| `copilot`, `github-copilot` | none | `.github/copilot-instructions.md` | none |
| `kilo`, `kilocode` | none | `.kilocode/rules/synaptic.md` | none |

Platform names are case-insensitive. `codex` is its own platform (it reads
`AGENTS.md` but also gets native MCP/hook wiring, see [Codex](#codex)); `opencode`
maps onto the plain `agents` platform. Only Claude gets a dedicated `SKILL.md`;
the other platforms consume the always-on instructions file directly. Any needed
parent directories (for example `.github/`, `.kilocode/rules/`) are created.

### What gets written

1. **Skill file** (Claude only): `.claude/skills/synaptic/SKILL.md`. It carries
   frontmatter (`name: synaptic`) and instructs the assistant to query the graph
   before grepping or broad reading, listing the build/query CLI commands and the
   MCP tools (see [MCP-Server](MCP-Server)).

2. **Always-on section**: a marked block injected into the platform's
   instructions file:

   ```
   <!-- synaptic:start -->
   <!-- synaptic-skill v0.3.7 -->
   ## Synaptic

   This repo has a Synaptic knowledge graph (`synaptic-out/graph.json`). Query it
   before broad file exploration: `synaptic query "<question>"`, `synaptic affected
   <node>`, or run `synaptic serve` for the MCP tools. Rebuild with `synaptic
   extract .` / `synaptic update <files>`.
   <!-- synaptic:end -->
   ```

   The block is delimited by `<!-- synaptic:start -->` and
   `<!-- synaptic:end -->`. On reinstall it is replaced in place (never
   duplicated), and any prose around it is preserved. A new instructions file is
   created if none exists. The `<!-- synaptic-skill vX.Y.Z -->` line stamps the
   version that produced the block (see
   [Versioning and auto-refresh](#versioning-and-auto-refresh)); the Claude
   `SKILL.md` carries the same stamp just under its frontmatter.

3. **PreToolUse hooks** (Claude only): two entries merged into
   `.claude/settings.json` under `hooks.PreToolUse` (see below).

4. **MCP server + hook** (Codex only): an MCP server registration and a
   `SessionStart` hook, in the project `.codex/` or the global `~/.codex/`
   (see [Codex](#codex)).

### Idempotency

Install can be run repeatedly without piling up duplicates:

- The always-on block is matched by its marker and replaced in place. A truncated
  or hand-edited block (a dangling start marker) is repaired into a single clean
  block.
- The settings hooks are matched by matcher plus the literal `synaptic` in the
  body; any prior Synaptic hooks are removed before the current pair is appended,
  so a reinstall keeps exactly two. Foreign hooks and unrelated top-level
  settings keys are preserved. A corrupt `settings.json` is treated as empty and
  rewritten.

## Versioning and auto-refresh

Installed skills are versioned so they can be updated as Synaptic improves,
without you re-running `install` in every repo.

- **Version stamp.** Each installed artifact carries a `<!-- synaptic-skill
  vX.Y.Z -->` comment (under the `SKILL.md` frontmatter, and just inside the
  always-on block's start marker) recording the Synaptic version that produced it.
  The stamp is added at write time only — the build-time drift snapshots stay
  version-agnostic, so a version bump never churns them.
- **Install registry.** `synaptic install` records each `(repo, host)` it writes
  in `~/.synaptic/skills.toml` (`%USERPROFILE%\.synaptic\skills.toml` on Windows),
  with the version and a content hash of each artifact. `uninstall` removes the
  entry.
- **Auto-refresh on update.** After `synaptic self-update` replaces the binary, it
  re-renders every recorded skill to the new version automatically. (Because the
  running process is still the old binary, it spawns the freshly-installed one to
  do the rendering.) Run it on demand with `synaptic install --refresh`.
- **Hand-edits are preserved.** A refresh only rewrites an artifact that is
  byte-identical to what Synaptic last wrote (verified by the recorded hash). If
  you edited a skill, it is left untouched and reported — re-run `synaptic install
  <host>` to overwrite. The always-on block is rewritten in place, so surrounding
  prose is always preserved regardless. Entries whose files are gone are dropped.

Refresh covers the Markdown skill artifacts (the `SKILL.md` and the always-on
blocks). Codex MCP config / hooks and Claude `settings.json` hooks are not
auto-rewritten — re-run `synaptic install` to refresh those.

## Codex

Codex is wired differently from Claude, and the **CLI and desktop app read
configuration from different places**, so there are two modes.

### Project mode (Codex CLI): `synaptic install codex`

Writes, under the current repo:

- **`.codex/config.toml`** -- a `[mcp_servers.synaptic]` entry that launches
  `synaptic serve` (stdio MCP). No `--graph`, so it resolves
  `synaptic-out/graph.json` relative to the server's working directory.
- **`.codex/hooks.json` + `.codex/synaptic-hook.py`** -- a `SessionStart` hook
  that injects model-visible context (once per session, only when a graph
  exists) telling the agent to query the graph first.
- The always-on **`AGENTS.md`** block.

The Codex CLI loads a project's `.codex/` layer only for **trusted** projects, so
trust the folder in Codex for the server and hook to take effect.

Why `SessionStart` and not `PreToolUse` (Claude's choice): Codex does not honor
`additionalContext` on `PreToolUse` (it marks the hook run failed), and its
top-level `systemMessage` is UI-only and never reaches the model. `SessionStart`
`additionalContext` is injected as model-visible developer context, so that is
where the nudge lives.

### App mode (Codex desktop app): `synaptic install codex --global`

The **desktop app ignores a project's `.codex/config.toml` for MCP** and reads
servers only from the global `~/.codex/config.toml`. For app users, `--global`
registers a per-repo server there instead:

- **`~/.codex/config.toml`** gains `[mcp_servers.synaptic-<repo>]` (named after
  the repo dir, sanitized), launching `synaptic serve --graph <absolute path>`.
  The absolute `--graph` is required because the app gives a server no per-project
  working directory. Your other servers and the `[projects.*]` trust list are
  preserved.
- The always-on **`AGENTS.md`** block (the app reads project `AGENTS.md`).
- No hook is written (the app would not fire it); orientation rides on `AGENTS.md`.

After installing: build the graph with `synaptic extract .`, restart the Codex
app, and check **Settings > MCP servers** for the `synaptic-<repo>` entry.
Install for each repo you use; each appears as its own server. The Codex home
directory honors `CODEX_HOME` (otherwise `~/.codex`). `synaptic` must be on your
`PATH` (e.g. `cargo install --path bin/synaptic`) so the app can launch it.

## How the hooks nudge the assistant

For Claude, two `PreToolUse` hooks are written into `.claude/settings.json`. They
**nudge, never block** (they fail open, so a legitimate tool call always
proceeds), and they only fire when `synaptic-out/graph.json` exists. Each hook's
shell snippet parses the tool input with `python3`.

- **Bash matcher**: fires when a shell command looks like a search
  (`grep`, `rg`, `ripgrep`, `find`, `fd`, `ack`, `ag`). It injects additional
  context telling the assistant to run `synaptic query "<question>"` before
  grepping raw files.
- **Read|Glob matcher**: fires when a `Read`/`Glob` targets a source or doc file
  (by extension, for example `.py .js .ts .go .rs .java .md` and many others)
  outside `synaptic-out/`. It injects context telling the assistant to run
  `synaptic query` / `synaptic explain` / `synaptic path` first, and to carry
  the same rule into subagent prompts.

When a hook fires, Claude Code receives the `additionalContext` text as a
`PreToolUse` hook output, steering the assistant toward the graph before it reads
or greps. Because the hook is gated on `synaptic-out/graph.json`, nothing fires
until a graph has been built (see [Quickstart](Quickstart) and
[Extraction](Extraction)).

## Uninstall

```
synaptic uninstall [platform]
synaptic uninstall codex --global
synaptic uninstall --all
```

`platform` defaults to `claude`. Uninstall removes the dedicated skill file (if
any), tidies now-empty skill directories, and strips the always-on marker block
from the instructions file. If nothing else remains in that file, the file is
removed; otherwise the surrounding prose and its blank-line spacing are
preserved. For Claude it also removes exactly the Synaptic `PreToolUse` hooks,
leaving foreign hooks intact. For Codex it removes the project `.codex/` server,
hook, and script (or, with `--global`, this repo's `synaptic-<repo>` entry from
`~/.codex/config.toml`), preserving foreign servers. `--all` uninstalls from
every supported platform (project scope).

## Skill drift commands

The skill artifacts are generated by pure slot substitution over an embedded
template, with a committed golden snapshot tree (`expected/`) next to the
skillgen crate source. These commands are dev/CI tools run from a repo checkout:

```
synaptic skill check
synaptic skill bless
```

- `synaptic skill check` re-renders every artifact (one per platform plus the
  shared always-on section) and byte-diffs it against the committed snapshots
  (ignoring line-ending style). It prints `skill artifacts are in sync with
  expected/.` when clean, or lists each drift and exits non-zero. A `cargo test`
  run also fails on drift.
- `synaptic skill bless` rewrites the committed snapshot tree from the current
  render, printing the paths written. Run it after an intentional template
  change.

Note: because the snapshot tree is resolved relative to the crate source, these
commands are meaningful only from a repo checkout; an installed binary reports the
snapshots missing by design.

## See also

- [MCP-Server](MCP-Server) -- run `synaptic serve` and the tools the skill
  points an assistant at.
- [Quickstart](Quickstart) -- build a graph first so the hooks activate.
- [Configuration](Configuration) -- environment variables and settings.
- [Commands](Commands) -- the full CLI reference.
