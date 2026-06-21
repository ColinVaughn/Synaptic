# PR Dashboard

`synaptic prs` is a graph-aware pull-request dashboard. It lists open PRs with their CI and review state, and overlays each PR's graph "blast radius" (how many graph nodes and communities its changed files touch) so you can see merge-order risk at a glance.

It is a deterministic CLI: it shells out to the GitHub CLI (`gh`) and to `git`, and reads `synaptic-out/graph.json` for blast radius. It makes no LLM calls. For LLM-ranked triage, use the MCP server's `triage_prs` tool instead (see [MCP-Server]).

Requires the `gh` CLI to be installed and authenticated (`gh auth login`). If `gh` is unavailable or unauthenticated, the dashboard reports an error.

See also: [Commands], [Analysis-and-Reports].

## Usage

```
synaptic prs [NUMBER] [--repo OWNER/NAME] [--base BRANCH] [--graph PATH] [--triage] [--conflicts]
```

- `NUMBER` shows the detail view for one PR; omit it for the dashboard.
- `--repo OWNER/NAME` targets another repository (default: the current directory's repo).
- `--base BRANCH` filters to a base branch (default: the repository's default branch, auto-detected via `gh repo view`, then `git symbolic-ref`, then `main`).
- `--graph PATH` points at a `graph.json` for blast radius (default: the standard `synaptic-out/graph.json`).
- `--triage` and `--conflicts` are dashboard-level views (described below). They take precedence over `NUMBER`.

## Dashboard (default)

Lists open PRs targeting the base branch, sorted by status then age. Each line shows the number, computed status, CI state, review decision, age in days, author, and blast radius (when a graph is present):

```
Open PRs targeting main: 4  (1 on wrong base, not shown)

#42 [CI-FAIL] CI=FAILURE review=none age=2d author=alice  blast_radius=37 nodes / 4 communities
  Fix auth token refresh

#39 [READY] CI=SUCCESS review=APPROVED age=1d author=bob
  Add rate limiter
```

PRs on a base other than the target are counted but not shown.

### Status classification

Each PR is classified by precedence (first match wins):

| Status | Condition |
| --- | --- |
| `WRONG-BASE` | base branch is not the expected base |
| `CI-FAIL` | CI rollup has any failing conclusion |
| `CHANGES-REQ` | review decision is `CHANGES_REQUESTED` |
| `DRAFT` | PR is a draft |
| `STALE` | not updated in 14+ days |
| `APPROVED` | review decision is `APPROVED` |
| `PENDING` | CI is in progress / queued |
| `READY` | none of the above |

CI state is rolled up from the PR's `statusCheckRollup`: any failing conclusion (`FAILURE`, `CANCELLED`, `TIMED_OUT`, `ACTION_REQUIRED`, `STARTUP_FAILURE`) is a failure; otherwise an in-progress/queued check is pending; otherwise a `SUCCESS` conclusion is success; otherwise none.

### Blast radius

When a `graph.json` is available, each PR's changed files (from `gh pr diff <n> --name-only`) are matched against the source files of graph nodes (path-boundary-safe matching), and the touched communities plus affected node count are attached. This is shown as, for example, `blast_radius=37 nodes / 4 communities`. With no graph, blast radius is omitted.

## Detail view (`synaptic prs NUMBER`)

Shows one PR with its branch, status, author, age, CI/review state, its git worktree path (if any), blast radius with the touched community ids, and up to 20 changed files:

```
PR #42 — Fix auth token refresh
  feat/auth-refresh → main
  status: CI-FAIL
  author: alice   age: 2d
  CI: FAILURE   review: none
  blast radius: 37 nodes / 4 communities
  communities: 1, 4, 7, 9
  files (5):
    src/auth/token.rs
    src/auth/refresh.rs
    ...
```

The single PR is fetched with `gh pr view`, so it works regardless of its base branch.

## `--triage`

```
synaptic prs --triage [--base BRANCH] [--repo OWNER/NAME]
```

Ranked, actionable PRs targeting the base, with blast radius. It selects PRs on the correct base, drops those classified `WRONG-BASE` or `STALE` (not worth acting on now), and sorts by triage rank then age:

```
Actionable PRs targeting main: 3 (ranked by review priority)

#42 [CI-FAIL] CI=FAILURE review=none age=2d author=alice  blast_radius=37 nodes / 4 communities
  Fix auth token refresh
```

This is deterministic, with no LLM. It mirrors the filter and sort of the MCP `triage_prs` tool; for an LLM-summarized ranking, run the MCP server and let the assistant rank `triage_prs` (see [MCP-Server]).

## `--conflicts`

```
synaptic prs --conflicts [--base BRANCH] [--repo OWNER/NAME]
```

Reports PRs that touch the same graph community, which signals merge-order risk. It considers only PRs targeting the base that have graph impact data, groups them by shared community (most-overlapping first), and lists the overlapping PRs:

```
Community conflicts (PRs sharing the same graph community):

Community 4  (2 PRs overlap)
  #42   CI-FAIL      Fix auth token refresh
  #51   READY        Refactor session store
```

Messages when there is nothing to report:

- No graph: `No graph impact data — run with a valid graph.json to detect conflicts.`
- Graph present, no overlap: `No community overlap between open PRs — safe to merge in any order.`

## CLI vs. the MCP `triage_prs` tool

The `prs` CLI is fully deterministic: fixed status precedence, sort order, and blast-radius computation, with no model involved. The MCP server exposes the same PR data (`list_prs`, `get_pr_impact`, `triage_prs`) so an AI assistant can read and reason over it; `triage_prs` lets the assistant produce a natural-language, model-ranked triage. Use the CLI for a stable, scriptable report; use the MCP tool when you want the assistant to summarize and prioritize. See [MCP-Server].
