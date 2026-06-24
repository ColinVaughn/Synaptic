//! CLI argument definitions (clap) split from main.rs.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "synaptic",
    version,
    about = "Build and query a code knowledge graph"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Build the graph for a directory and write synaptic-out/.
    Extract {
        /// Root directory to scan (default: current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Produce a directed graph.
        #[arg(long)]
        directed: bool,
        /// Also write an Obsidian vault (one note per node) under synaptic-out/obsidian/.
        #[arg(long)]
        obsidian: bool,
        /// Also write a Markdown wiki under synaptic-out/wiki/.
        #[arg(long)]
        wiki: bool,
        /// Run the LLM semantic pass over documents/papers (requires an API key
        /// in the environment, e.g. OPENAI_API_KEY). Off by default — this makes
        /// paid API calls. Also enables the LLM dedup tiebreaker.
        #[arg(long)]
        semantic: bool,
        /// Skip SQL column and index nodes. Smaller graph.json on column-heavy
        /// schemas, at the cost of column-level SQL audit rules.
        #[arg(long)]
        no_columns: bool,
    },
    /// Re-emit an output format from an existing graph.json — no re-extraction —
    /// or push the graph live to a database. Formats: json, html, svg, graphml,
    /// cypher, dot, callflow, tree, 3d, obsidian, wiki, report, neo4j, falkordb.
    Export {
        /// Output format (see the command help for the full list).
        format: String,
        /// Source graph.json (default: synaptic-out/graph.json).
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Output file or directory (default: alongside the source graph.json).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Scope to one federated member (its `repo` tag) before exporting.
        #[arg(long)]
        repo: Option<String>,
        /// For neo4j/falkordb: push live to this URI (e.g. bolt://localhost:7687
        /// or falkordb://localhost:6379) instead of writing the cypher script.
        /// Requires building with `--features push`.
        #[arg(long)]
        push: Option<String>,
        /// Auth user for --push (Neo4j).
        #[arg(long, default_value = "neo4j")]
        user: String,
        /// Auth password for --push (or set NEO4J_PASSWORD / FALKORDB_PASSWORD).
        #[arg(long)]
        password: Option<String>,
    },
    /// Query the graph for a relevant subgraph.
    Query {
        /// Free-text query.
        text: String,
        #[arg(long)]
        graph: Option<PathBuf>,
        #[arg(long, default_value_t = 30)]
        max_nodes: usize,
        /// Scope to one federated member (its `repo` tag).
        #[arg(long)]
        repo: Option<String>,
        /// Expand the subgraph depth-first instead of breadth-first (favors deep
        /// call chains over broad neighbourhoods).
        #[arg(long)]
        dfs: bool,
        /// Boost nodes whose file changed on the current branch since this baseline:
        /// a git ref (main, HEAD~10), a date ("2 weeks ago"), or "auto" (detect the
        /// default branch). Scope is merge-base(SINCE, HEAD)..working-tree.
        #[arg(long)]
        since: Option<String>,
        /// With --since: also inject changed-file nodes as seeds, so the branch's
        /// changed surface appears even when the query matches little.
        #[arg(long)]
        seed_changed: bool,
    },
    /// Shortest path between two nodes (by id or label).
    Path {
        /// Start node: id, label, or bare name. If shared by several files, qualify as "name@file-substring".
        from: String,
        /// End node: id, label, or bare name. If shared by several files, qualify as "name@file-substring".
        to: String,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Scope to one federated member (its `repo` tag).
        #[arg(long)]
        repo: Option<String>,
    },
    /// Show a node and its neighbours.
    Explain {
        /// Node id or label. If the name is shared by several files, qualify it as "name@file-substring" (e.g. "announce@core/foo.ts").
        node: String,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Scope to one federated member (its `repo` tag).
        #[arg(long)]
        repo: Option<String>,
    },
    /// Incrementally rebuild the graph after files change (or fully with --full).
    Update {
        /// Changed file paths (repo-relative). Empty + no --full = full rebuild.
        paths: Vec<PathBuf>,
        /// Rebuild every code file from scratch (preserves semantic nodes).
        #[arg(long)]
        full: bool,
        /// Build directed when there's no existing graph to inherit from.
        #[arg(long)]
        directed: bool,
        /// Bypass the shrink guard.
        #[arg(long)]
        force: bool,
    },
    /// Watch the working tree and incrementally rebuild on change (debounced).
    Watch {
        /// Build directed when there's no existing graph to inherit from.
        #[arg(long)]
        directed: bool,
        /// Bypass the shrink guard on each rebuild.
        #[arg(long)]
        force: bool,
    },
    /// Nodes that (transitively) depend on a node (reverse-impact).
    Affected {
        /// Node id, label, bare name, source file, or unique label substring. If the name is shared by several files, qualify it as "name@file-substring".
        node: String,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Max hops to walk backward.
        #[arg(long, default_value_t = 2)]
        depth: usize,
        /// Restrict to these edge relations (repeatable); defaults to the
        /// structural impact relations.
        #[arg(long = "relation")]
        relations: Vec<String>,
        /// Max dependents listed before a "+N more" summary (a per-depth
        /// breakdown and the true total are always shown). Ignored with --verbose.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// List every dependent instead of the summarized top-N.
        #[arg(long)]
        verbose: bool,
    },
    /// Reflection / dynamic-dispatch sites recorded in the graph. A symbol reached
    /// only by dynamic dispatch has no static dependents, so "0 dependents" is not
    /// proof it is safe to change; this lists the sites behind that risk.
    Hazards {
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Restrict to one federated member repo (tag).
        #[arg(long)]
        repo: Option<String>,
        /// Restrict to one site kind (reflection, dynamic_import, eval).
        #[arg(long)]
        kind: Option<String>,
        /// Max sites listed before a "+N more" summary.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Git merge driver for graph.json — invoked by git as `%O %A %B`, not by
    /// users. Union-composes the two sides into `current` so graph.json never
    /// conflicts. (Registered by `hook install`.)
    #[command(hide = true)]
    MergeDriver {
        /// Common ancestor (%O) — accepted but unused.
        base: PathBuf,
        /// Current/ours (%A) — the union is written here.
        current: PathBuf,
        /// Other/theirs (%B).
        other: PathBuf,
    },
    /// Manage git hooks (post-commit/post-checkout) + the graph.json merge driver.
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
    /// Run the MCP server (read-only graph tools + PR tools for an AI assistant).
    /// Defaults to stdio; `--http <addr>` serves over HTTP instead.
    Serve {
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Serve over HTTP at this address (e.g. 127.0.0.1:8765) instead of stdio.
        #[arg(long)]
        http: Option<String>,
        /// Require this API key for HTTP requests (or set SYNAPTIC_API_KEY).
        #[arg(long)]
        api_key: Option<String>,
        /// Trusted root for resolving source files in code-reading tools
        /// (default: the directory above synaptic-out/, i.e. the repo root).
        #[arg(long)]
        source_root: Option<PathBuf>,
        /// Expose the command-running `speculate` tool (applies a change in a
        /// throwaway worktree and runs tests/build). OFF by default: this makes
        /// the server no longer read-only, so enable it only for trusted clients.
        #[arg(long)]
        allow_exec: bool,
        /// Token-lean output: lower the default list/budget sizes so tool results
        /// return less to the model (an explicit per-call argument still wins).
        /// Equivalent to setting SYNAPTIC_CONCISE=1.
        #[arg(long)]
        concise: bool,
    },
    /// Ingest an external source into the graph (cargo workspace, MCP config) or
    /// fetch a URL into synaptic-out/ingested/ for the next extract.
    Ingest {
        #[command(subcommand)]
        source: IngestSource,
    },
    /// Install the Synaptic skill for a host assistant
    /// (claude | agents | codex | opencode | gemini | cursor | copilot | kilo).
    Install {
        #[arg(default_value = "claude")]
        platform: String,
        /// Codex only: register the MCP server in the GLOBAL `~/.codex/config.toml`
        /// (per-repo named server) so the Codex desktop app picks it up, instead of
        /// the per-project `.codex/` the CLI reads.
        #[arg(long)]
        global: bool,
        /// Re-render every skill recorded in `~/.synaptic/skills.toml` to the
        /// current version (the platform arg is ignored). Hand-edited skills are
        /// left untouched. This is what `self-update` runs automatically.
        #[arg(long)]
        refresh: bool,
    },
    /// Remove the Synaptic skill for a platform (or `--all`).
    Uninstall {
        #[arg(default_value = "claude")]
        platform: String,
        /// Uninstall from every supported platform.
        #[arg(long)]
        all: bool,
        /// Codex only: remove this repo's server from the GLOBAL `~/.codex/config.toml`.
        #[arg(long)]
        global: bool,
    },
    /// Graph-aware PR dashboard (open PRs + CI/review state); a number shows one
    /// PR's detail with graph blast radius. Requires the `gh` CLI.
    Prs {
        /// PR number for a detailed view; omit for the dashboard.
        number: Option<u64>,
        /// Target repo `owner/name` (default: the current directory's repo).
        #[arg(long)]
        repo: Option<String>,
        /// Base branch to filter to (default: the repo's default branch).
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Ranked actionable PRs with blast radius (deterministic; no LLM — for
        /// LLM summarization use the MCP server's `triage_prs` tool).
        #[arg(long)]
        triage: bool,
        /// PRs that touch the same graph community (merge-order risk).
        #[arg(long)]
        conflicts: bool,
    },
    /// Maintain the generated skill artifacts (dev/CI): check for drift against
    /// the committed snapshots, or re-bless them after an intentional change.
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Multi-repo / monorepo federation: discover members, build a federated
    /// graph, and scope queries to a repo.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// The cross-repo global graph store (`~/.synaptic`).
    Global {
        #[command(subcommand)]
        action: GlobalAction,
    },
    /// Compose several `graph.json` files into one namespaced graph.
    MergeGraphs {
        /// The graph.json files to merge (tag = each file's grandparent dir).
        graphs: Vec<PathBuf>,
        /// Output path (default: synaptic-out/merged-graph.json).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Maintain the on-disk extraction cache (`synaptic-out/cache`).
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Diff the code graph between two git revisions: added/removed dependencies,
    /// removed APIs, architectural drift, new cycles, and hotspots of change.
    /// Each revision is built in a throwaway git worktree; built graphs are cached
    /// per commit under synaptic-out/history/.
    Diff {
        /// The base revision (e.g. HEAD~10, a branch, or a SHA). Omit when using --since.
        rev1: Option<String>,
        /// The target revision (default: the current working tree).
        rev2: Option<String>,
        /// Resolve the base revision from a date (e.g. 2026-01-01); the base is the
        /// latest commit on HEAD at or before it. Mutually exclusive with rev1.
        #[arg(long)]
        since: Option<String>,
        /// Repo root (default: current directory).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Produce a directed graph for each revision.
        #[arg(long)]
        directed: bool,
        /// Limit reports to source files under this repo-relative prefix.
        #[arg(long)]
        scope: Option<String>,
        /// Max rows per ranked section.
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Path-component depth defining a "module" (e.g. 2 => crates/foo).
        #[arg(long, default_value_t = 2)]
        module_depth: usize,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
        /// Also write a Markdown report to this path.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Also write a self-contained HTML report to this path.
        #[arg(long)]
        html: Option<PathBuf>,
        /// Always rebuild; skip the per-SHA snapshot store.
        #[arg(long)]
        no_cache: bool,
    },
    /// Structural search over the graph with SYNQL, or a named architectural
    /// pattern. Not text search: matches on kind/visibility/loc/fan-in/out/etc.
    /// `.name` is the bare symbol (no `()`); use `=~` for a regex/substring match.
    /// Example: synaptic search "MATCH (c:class) WHERE c.loc > 500 RETURN c"
    /// Example: synaptic search "MATCH (f:function) WHERE f.name =~ \"announce\" RETURN f"
    Search {
        /// A SYNQL query. Omit when using --pattern or --list-patterns.
        query: Option<String>,
        /// Run a built-in pattern instead (singleton|factory|observer|service-locator|god-class).
        #[arg(long)]
        pattern: Option<String>,
        /// List the built-in patterns and exit.
        #[arg(long)]
        list_patterns: bool,
        /// Print the query plan instead of running it.
        #[arg(long)]
        explain: bool,
        /// Save the given query under this name (synaptic-out/synql/<name>.synql).
        #[arg(long)]
        save: Option<String>,
        /// Run a previously saved query by name.
        #[arg(long)]
        saved: Option<String>,
        /// List saved query names and exit.
        #[arg(long)]
        list_saved: bool,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Scope to one federated member (its `repo` tag).
        #[arg(long)]
        repo: Option<String>,
        /// Emit results as JSON.
        #[arg(long)]
        json: bool,
        /// Max rows to display.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Plan and verify a safe rename. Synaptic does not edit source: `rename`
    /// emits a confidence-scored plan (plan.json + plan.md) for an AI agent, and
    /// `verify` checks graph invariants after the agent applies it.
    Refactor {
        #[command(subcommand)]
        action: RefactorAction,
    },
    /// Forecast the consequences of a change before applying it: the graph nodes
    /// it touches, the reverse-impact blast radius, public APIs at risk, and (vs a
    /// base revision) new import cycles, removed APIs, and dependency deltas.
    /// Synaptic does not edit source; the forecast is data an agent reads first.
    Predict {
        /// Changed files (repo-relative). Empty = `git diff --name-only <base>`.
        paths: Vec<PathBuf>,
        /// Base revision to measure the change against (used for the changed-file
        /// diff and the time-travel diff). Default: the detected default branch.
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Repo root for the time-travel diff (default: current directory).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Reverse-impact hop bound.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Cap on blast-radius dependents reported.
        #[arg(long, default_value_t = 200)]
        max_hits: usize,
        /// Skip the git/worktree time-travel diff (faster; no cycle / removed-API detection).
        #[arg(long)]
        no_diff: bool,
        /// Exit non-zero if the change introduces a new import cycle or removes a
        /// public API (a pre-commit / CI quality gate). Forces the time-travel diff.
        #[arg(long)]
        gate: bool,
        /// Analytic mode: forecast a DESCRIBED edit instead of a file diff, before
        /// any code is written. Format "<kind>:<symbol>" with kind one of
        /// delete|signature|visibility (e.g. --edit "delete:Service"). If the name
        /// is shared by several files, qualify it as "<kind>:<name>@<file-substring>"
        /// (e.g. "delete:announce@core/foo.ts"). Reports the predicted graph delta
        /// and which dependents break. Ignores --base/--gate.
        #[arg(long)]
        edit: Option<String>,
        /// Output directory for forecast.json + forecast.md (default: synaptic-out/predict).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Scope to one federated member (its `repo` tag).
        #[arg(long)]
        repo: Option<String>,
        /// Emit the forecast as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Speculatively execute a proposed change for real: apply it in a throwaway
    /// git worktree, run the forecast's at-risk tests plus a build/type-check,
    /// and report the actual pass/fail outcome. Disposable and opt-in -- it never
    /// touches your working tree and is never exposed as an MCP tool (it runs
    /// commands, which would break the server's read-only invariant).
    Speculate {
        /// Changed files (repo-relative). Empty = derive from the patch, else
        /// from `git diff --name-only <base>`. Explicit paths also scope the
        /// applied working-tree diff to those files.
        paths: Vec<PathBuf>,
        /// Base revision to apply onto and diff against. Default: HEAD with
        /// --patch, else the detected default branch.
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Repo root (default: current directory).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Apply this unified-diff file instead of the current working-tree changes.
        #[arg(long)]
        patch: Option<PathBuf>,
        /// Test command template; `{files}` expands to the at-risk test files
        /// (run per file). With no placeholder it runs once as a whole suite.
        #[arg(long)]
        test_cmd: Option<String>,
        /// Build / type-check command, run once before the tests.
        #[arg(long)]
        check_cmd: Option<String>,
        /// Do not auto-detect commands from project markers (Cargo.toml, go.mod,
        /// pyproject.toml, package.json).
        #[arg(long)]
        no_detect: bool,
        /// Reverse-impact hop bound for selecting at-risk tests.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Per-command wall-clock budget in seconds.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Cap on the number of at-risk test files run.
        #[arg(long, default_value_t = 20)]
        max_tests: usize,
        /// Stop after the first failing test.
        #[arg(long)]
        fail_fast: bool,
        /// Output directory for report.json + report.md (default: synaptic-out/speculate).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Scope to one federated member (its `repo` tag).
        #[arg(long)]
        repo: Option<String>,
        /// Emit the report as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Evaluate forecast quality by replaying history: re-predict each commit
    /// from its parent-state graph and score the prediction against git ground
    /// truth (the tests actually edited, the public APIs actually removed).
    Eval {
        #[command(subcommand)]
        action: EvalAction,
    },
    /// Audit SQL for performance + security issues over the SQL-aware graph
    /// (RLS coverage, grants, injection, missing indexes, anti-patterns), or
    /// advise on a single candidate query before it is written.
    Sql {
        #[command(subcommand)]
        action: SqlAction,
    },
    /// Update the Synaptic binary to the latest GitHub release (opt-in).
    /// Bare: check and, if newer, prompt to download + replace. `--enable` /
    /// `--disable` toggle the background "update available" notice (off by
    /// default; persisted to ~/.synaptic/update.toml). `--check` reports
    /// availability without downloading.
    SelfUpdate {
        /// Enable the background update-available notice and exit.
        #[arg(long, conflicts_with_all = ["disable", "check", "yes"])]
        enable: bool,
        /// Disable the background update-available notice and exit.
        #[arg(long, conflicts_with_all = ["enable", "check", "yes"])]
        disable: bool,
        /// Report whether an update is available, then exit (no download).
        #[arg(long)]
        check: bool,
        /// Skip the confirmation prompt before downloading and replacing.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum SqlAction {
    /// Audit the SQL in the graph and report findings (findings.json + audit.md).
    Audit {
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Repo root, enabling source-reading rules like N+1 (default: .).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Only report findings at least this severe (critical|high|medium|low|info).
        #[arg(long)]
        severity: Option<String>,
        /// Scope to one federated member (its `repo` tag).
        #[arg(long)]
        repo: Option<String>,
        /// Output directory (default: synaptic-out/sql).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Run EXPLAIN against a live database to corroborate perf findings.
        /// Requires building with `--features live-explain`.
        #[arg(long)]
        explain: bool,
        /// Database URL for --explain (postgres://, mysql://, sqlite://).
        #[arg(long)]
        db_url: Option<String>,
        /// Emit the report as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Critique a single candidate query before writing it: perf + security
    /// findings, cross-referenced against the graph's tables/indexes/RLS.
    Advise {
        /// The SQL query to critique.
        #[arg(long)]
        query: String,
        /// SQL dialect hint (postgres|mysql|mssql|sqlite); advisory.
        #[arg(long)]
        dialect: Option<String>,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Scope to one federated member (its `repo` tag).
        #[arg(long)]
        repo: Option<String>,
        /// Emit the report as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum EvalAction {
    /// Replay <from>..HEAD: report predictive-test-selection recall/precision,
    /// removed-API recall, and blast-radius selectivity. Builds a graph per
    /// revision in a throwaway worktree (cached per commit), so it is slow on a
    /// cold repo.
    Replay {
        /// Replay the commits after this revision (e.g. HEAD~20, a branch, a SHA).
        #[arg(default_value = "HEAD~10")]
        from: String,
        /// Repo root (default: current directory).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Reverse-impact hop bound for each forecast.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Cap on the number of commits replayed.
        #[arg(long, default_value_t = 50)]
        max_commits: usize,
        /// Build directed graphs for each revision.
        #[arg(long)]
        directed: bool,
        /// CI gate: exit non-zero if predictive-test-selection recall is below
        /// this percentage.
        #[arg(long)]
        min_test_recall: Option<u8>,
        /// Output directory for report.json + report.md (default: synaptic-out/eval).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Emit the report as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Calibrate the cross-language edge layer (FFI/subprocess/HTTP/gRPC/pyo3)
    /// over a built graph: per-relation counts plus two precision proxies
    /// (service-boundary connectivity and subprocess-invocation resolution).
    CrossLanguage {
        /// Path to graph.json (default: synaptic-out/graph.json).
        #[arg(long, default_value = "synaptic-out/graph.json")]
        graph: PathBuf,
        /// Emit the report as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Score Synaptic against the hand-labeled accuracy corpus: call-edge
    /// precision/recall/F1, affected-test recall, blast-radius false-negative
    /// rate, and cross-language relationship accuracy, per fixture and pooled.
    Corpus {
        /// Corpus root holding manifest.toml (default: the in-tree corpus).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Output directory for report.json + report.md (default: synaptic-out/eval/corpus).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Emit the report as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Calibrate co-change prediction confidence over recent history: bin each
    /// prediction's confidence against whether it actually co-changed, and report
    /// a reliability table plus a Brier score (0 = perfect, lower is better).
    Calibrate {
        /// Repo root (default: current directory).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// How many of the most recent commits to evaluate.
        #[arg(long, default_value_t = 200)]
        max_commits: usize,
        /// Number of reliability bins over [0,1].
        #[arg(long, default_value_t = 10)]
        bins: usize,
        /// Output directory for report.json + report.md (default: synaptic-out/eval/calibrate).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Emit the report as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Measure extraction throughput across pinned external repositories spanning
    /// size tiers and language families. Clones each repo at its pinned SHA and
    /// times a cold and a warm (AST-cache-hot) build. Network + git required.
    Scale {
        /// Manifest of pinned repos (default: the in-tree scale-corpus.toml).
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Restrict to one tier (small|medium|large).
        #[arg(long)]
        tier: Option<String>,
        /// Repetitions per repo (median + p95 are reported over these).
        #[arg(long, default_value_t = 3)]
        reps: usize,
        /// Clone cache directory (default: synaptic-out/bench).
        #[arg(long)]
        cache: Option<PathBuf>,
        /// Output directory for report.json + report.md (default: synaptic-out/eval/scale).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Emit the report as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum RefactorAction {
    /// Plan renaming a symbol; writes plan.json + plan.md (no edits made).
    Rename {
        /// The symbol to rename (its name, or a node id).
        name: String,
        /// The new name.
        #[arg(long)]
        to: String,
        /// Disambiguate by node id when the name matches several definitions.
        #[arg(long)]
        id: Option<String>,
        /// Disambiguate by file-path substring.
        #[arg(long)]
        file: Option<String>,
        /// Repo root (used to read referencing files for column-accurate sites).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Output directory (default: synaptic-out/refactor).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Minimum per-site confidence score [0,1] to land in edits vs review.
        #[arg(long, default_value_t = 0.8)]
        min_confidence: f32,
        /// Skip the whole-word textual scan for references the graph does not
        /// record as edges (type uses, enum-variant paths).
        #[arg(long)]
        no_text_scan: bool,
        /// Cap on textual occurrences enumerated by the scan.
        #[arg(long, default_value_t = 200)]
        max_text_sites: usize,
        /// Emit the plan as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Plan moving a symbol's definition to an existing file (imports updated by the agent).
    Move {
        /// The symbol to move.
        name: String,
        /// Destination file (repo-relative).
        #[arg(long)]
        to: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        file: Option<String>,
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        graph: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Plan extracting a symbol's definition into a new file.
    Extract {
        /// The symbol to extract.
        name: String,
        /// New destination file (repo-relative).
        #[arg(long)]
        to: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        file: Option<String>,
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        graph: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Verify the graph after an agent applied a plan's edits.
    Verify {
        /// The plan.json produced by `rename`, `move`, or `extract`.
        #[arg(long)]
        plan: PathBuf,
        /// Repo root to rebuild the post-edit graph from.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// The plan is a move/extract relocation (not a rename).
        #[arg(long)]
        relocate: bool,
        /// Emit the verify report as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum CacheAction {
    /// Delete the regenerable extraction cache. The AST cache normally
    /// self-invalidates on extractor changes; use this for a guaranteed cold
    /// start or suspected corruption.
    Clear {
        /// Repo/workspace root whose `synaptic-out/cache` to remove (default: .).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Also remove every `synaptic-out/cache` found beneath PATH (federated
        /// member caches), via a bounded, noise-pruned walk.
        #[arg(long)]
        recursive: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum HookAction {
    /// Install the hooks and register the merge driver (idempotent).
    Install,
    /// Remove the hooks (and our blocks from any shared hook files).
    Uninstall,
    /// Show which hooks are currently installed.
    Status,
}

#[derive(Subcommand)]
pub(crate) enum WorkspaceAction {
    /// Write synaptic-workspace.toml, auto-discovering members.
    Init {
        /// Also scan a parent dir for sibling git repos → `[[repos]]` entries.
        /// Bare `--scan-repos` scans the parent of the current repo.
        #[arg(long, num_args = 0..=1, value_name = "DIR")]
        scan_repos: Option<Option<PathBuf>>,
        /// Max directory levels below the scan root (with --scan-repos).
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Max repos to include (with --scan-repos).
        #[arg(long, default_value_t = 50)]
        max: usize,
    },
    /// Add a member: a local path (→ members) or a git URL (→ [[repos]]).
    Add { target: String },
    /// Scan a parent dir for sibling git repos and federate them (no manifest).
    Discover {
        /// Directory to scan (default: parent of the current repo).
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 3)]
        depth: usize,
        #[arg(long, default_value_t = 50)]
        max: usize,
    },
    /// Build all members + federate into synaptic-out/graph.json.
    Build {
        /// Only rebuild when a member changed (incremental).
        #[arg(long)]
        changed: bool,
        /// Produce a directed federated graph.
        #[arg(long)]
        directed: bool,
    },
    /// Compose from a directory of published <member>/graph.json artifacts.
    Federate { dir: PathBuf },
    /// Pull remote git members, then rebuild deltas.
    Sync,
    /// Show each member's change status (no build).
    Status,
    /// List the workspace members.
    List,
}

#[derive(Subcommand)]
pub(crate) enum GlobalAction {
    /// Add (or update) a repo's graph.json under a tag.
    Add {
        graph: PathBuf,
        /// Repo tag (default: the graph's grandparent dir name).
        #[arg(long = "as")]
        tag: Option<String>,
    },
    /// Remove a repo's nodes from the global store.
    Remove { tag: String },
    /// List the repos in the global store.
    List,
    /// Print the global graph path.
    Path,
}

#[derive(Subcommand)]
pub(crate) enum SkillAction {
    /// Re-render the skill artifacts and fail if they differ from the committed
    /// `expected/` snapshots (CI anti-drift guard).
    Check,
    /// Rewrite the committed `expected/` snapshots from the current render.
    Bless,
}

#[derive(Subcommand)]
pub(crate) enum IngestSource {
    /// A Cargo workspace root — adds crate nodes + internal-dependency edges.
    Cargo { path: PathBuf },
    /// An MCP config file (.mcp.json, claude_desktop_config.json, …).
    Mcp { file: PathBuf },
    /// A SCIP-index JSON file (simplified shape) — adds symbol nodes + edges.
    Scip { file: PathBuf },
    /// A live PostgreSQL database (DSN, or empty for PG* env vars) — adds
    /// table/view/function nodes + foreign-key edges. Requires a build with
    /// `--features pg`.
    Pg {
        /// libpq DSN (e.g. `postgresql://user@host/db`); empty = PG* env vars.
        #[arg(default_value = "")]
        dsn: String,
    },
    /// A URL — fetched (SSRF-guarded) into synaptic-out/ingested/.
    Url { url: String },
    /// An office spreadsheet (.xlsx/.xls/.ods) — converted to markdown in
    /// synaptic-out/ingested/. Requires a build with `--features office`.
    Office { file: PathBuf },
    /// A Google-Workspace pointer (.gdoc/.gsheet/.gslides) — exported to markdown
    /// via the `gws` CLI. Requires a build with `--features gws`.
    Gws { file: PathBuf },
    /// A local audio/video file — transcribed to markdown via a transcription
    /// CLI. Requires a build with `--features media`.
    Media { file: PathBuf },
}
