# Workspaces and Federation

Synaptic can turn a set of *member* sources into one **federated** graph.
A member is either a local package (a monorepo crate/package) or a separate
repository (multi-repo). Each member is extracted into its own subgraph, node
ids are namespaced as `tag::id`, the subgraphs are composed into one graph,
cross-repo edges are resolved against each member's published export surface,
and the merged graph is re-clustered at the workspace level.

Three layers cooperate:

- The `workspace` subcommands build and manage a federated graph driven by a
  `synaptic-workspace.toml` manifest (or auto-discovery).
- The `global` subcommands maintain a persistent cross-repo store under
  `~/.synaptic`.
- `merge-graphs` composes several existing `graph.json` files into one
  namespaced graph.

See also [Commands], [Querying], [Output-Formats], and [Configuration].

## The `workspace` subcommands

All `workspace` subcommands operate on the current working directory as the
workspace root.

### `workspace init`

Auto-discovers members and writes `synaptic-workspace.toml`. The workspace
name defaults to the root directory name, `default_branch` is `main`, and each
discovered member is recorded as a root-relative member glob (path separators
normalized to `/`).

```
synaptic workspace init
```

Optional sibling-repo discovery appends `[[repos]]` entries:

```
synaptic workspace init --scan-repos
synaptic workspace init --scan-repos ../ --depth 3 --max 50
```

- `--scan-repos [DIR]`: also scan a directory for sibling git repositories.
  Bare `--scan-repos` scans the parent of the current repo. Each discovered repo
  is added as a `[[repos]]` entry with a root-relative `path` (it is not cloned).
  The current repo is excluded. Repos already present (by name) are skipped.
- `--depth` (default 3): directory levels below the scan root to examine.
- `--max` (default 50): maximum repos to include; the rest are reported as
  skipped (`over --max cap`). Git repos without a recognized manifest are also
  reported as skipped (`no recognized manifest`).

### `workspace add`

Adds one member to the manifest (creating it if absent). The argument is
classified as a git URL or a local path: a value that does not exist on disk and
contains `://` (or an scp-style `user@host:path`) becomes a `[[repos]]` git
member; anything else (including an existing local path) is appended to the
`[workspace].members` list.

```
synaptic workspace add services/billing
synaptic workspace add https://github.com/acme/identity
```

For a git URL, the member name is the last path segment minus a trailing `.git`.

### `workspace discover`

Ephemeral federation of sibling git repos with no manifest written. Scans a
parent directory for sibling repos and immediately federates the ones with a
recognized manifest, writing the federated outputs.

```
synaptic workspace discover
synaptic workspace discover ../ --depth 3 --max 50
```

The scan root defaults to the parent of the current repo. The current repo is
excluded from the scan. Discovered repos are federated as local `path` members.

### `workspace build`

Builds every member and federates them into `synaptic-out/graph.json` plus the
standard outputs (see [Output-Formats]), and writes each member's export surface
to `synaptic-out/surfaces/<repo>.json`.

```
synaptic workspace build
synaptic workspace build --directed
synaptic workspace build --changed
```

- `--directed`: produce a directed federated graph.
- `--changed`: incremental. Skips the whole federation when no local member's
  source changed and no remote `[[repos]]` are declared and a federated
  `graph.json` already exists; otherwise does a full rebuild. After a rebuild it
  also reports which members' export *surface* changed (the dependents whose
  cross-repo edges may be affected).

A full build records workspace incremental state; the incremental path persists
state only after the artifacts are durably written. See
[Incremental-Updates].

The build prints a summary: federated node/edge/community/member counts, the
cross-repo report (`extracted`, `inferred`, `cross-language`, `external_package`),
and per-member node/edge counts. `extracted`/`inferred` count import/coordinate
resolution (see [Cross-repo symbol resolution](#cross-repo-symbol-resolution));
`cross-language` counts the HTTP/RPC/FFI/WebSocket boundaries that span repos (see
[Cross-Language-Edges](Cross-Language-Edges)) — these are flagged on the edge, so
a graph with only WebSocket or route coupling no longer reads as "0 cross-repo
links".

### `workspace federate`

Artifact-mode federation: composes from a directory of already-published
per-member artifacts. Members are never checked out together; each member is a
subdirectory containing `graph.json` (required) and optionally
`export-surface.json`.

```
synaptic workspace federate ./artifacts
```

Layout:

```
artifacts/
  billing/
    graph.json
    export-surface.json
  identity/
    graph.json
    export-surface.json
```

The member tag is the subdirectory name. A surface's `repo` field is rewritten
to the tag so cross-repo targets line up.

### `workspace sync`

For each declared git `[[repos]]` member already cloned under
`synaptic-out/workspace-repos/<tag>`, runs `git pull`, then performs an
incremental update (same logic as `build --changed`) and writes the federated
outputs if anything rebuilt.

```
synaptic workspace sync
```

### `workspace status`

Shows each local member's change status (`changed` or `unchanged`) against the
saved workspace state, without building. If remote `[[repos]]` are present it
notes that `build --changed` forces a rebuild.

```
synaptic workspace status
```

### `workspace list`

Lists the resolved members (from the manifest when present, else auto-discovery)
with their tag, package coordinate, and path, plus any remote `[[repos]]` with
their git URL, subgraph URL, or path. When there is no workspace build-file but
projects were discovered by manifest presence, it notes the discovery.

```
synaptic workspace list
```

## The `synaptic-workspace.toml` manifest

The manifest declares the workspace and its members. When the file is absent, a
workspace is auto-discovered instead. The conventional filename is
`synaptic-workspace.toml` at the workspace root.

```toml
[workspace]
name = "acme-platform"
default_branch = "main"
members = ["services/*", "libs/*"]

[[repos]]
name = "billing"
git  = "https://github.com/acme/billing"
rev  = "main"

[[repos]]
name = "identity"
subgraph = "https://artifacts.acme.com/identity/latest/graph.json"

[[repos]]
name = "shared"
path = "../shared"
```

`[workspace]` fields:

- `name` (string, required).
- `default_branch` (string, default `main`).
- `members` (array of strings): local package-root globs, relative to the
  workspace root (like Cargo/pnpm workspace member globs). A declared glob that
  resolves outside the workspace root is a configuration error.

Each `[[repos]]` entry is a separate repository federated into the workspace.
Exactly one of `path`, `git`, or `subgraph` drives how it is built:

- `name` (string, required): the basis for the member tag.
- `path` (string, optional): a local, already-checked-out repo (relative to the
  root). Built locally.
- `git` (string, optional): a git URL to clone into
  `synaptic-out/workspace-repos/<tag>`, then built. Only `https`, `ssh`, `git`,
  `file` schemes and scp-style `user@host:path` are accepted. A directory that
  already exists on disk is cloned directly (offline); `workspace sync` pulls
  updates.
- `rev` (string, optional): branch/revision for the git clone (passed as
  `--branch`).
- `subgraph` (string, optional): artifact federation. A prebuilt subgraph,
  either a local path (relative to the root) or an `http(s)` URL, consumed
  directly instead of cloning and building.

## Member auto-discovery

When no `synaptic-workspace.toml` exists, members are auto-discovered from
build files at the workspace root:

- Cargo: the root crate (if the root `Cargo.toml` has `[package]`) plus each
  `[workspace].members` glob, minus `[workspace].exclude`. `default-members` is
  not used to narrow scope.
- npm/yarn: `package.json` `workspaces` (an array of globs, or
  `{ "packages": [...] }`).
- pnpm: `pnpm-workspace.yaml` `packages:` list.
- Go: `go.work` `use` directives (`use ./path` or a `use ( ... )` block).
- Python: `pyproject.toml` `[tool.uv.workspace].members` globs.
- Maven: `pom.xml` `<modules><module>` directories.
- Gradle: `settings.gradle(.kts)` `include` directives (`:a:b` maps to `a/b`).
- .NET: `.sln` `Project(...)` rows (member dir is the project file's parent).

Discovered members are deduplicated by resolved path, filtered to those inside
the root, sorted, and assigned a unique sanitized tag (the directory base name,
with collisions disambiguated by a `-2`, `-3`, ... suffix). Negation glob
patterns (a leading `!`) are dropped (negation is not applied). Nested
workspaces are expanded up to a bounded depth.

`!`-prefixed patterns are not treated as literal globs. A member directory must
contain a recognized manifest (`Cargo.toml`, `package.json`, `go.mod`,
`pyproject.toml`, `pom.xml`, `build.gradle(.kts)`, a `*.csproj`/`*.fsproj`/
`*.vbproj`, or a `*.sln`) to be kept. The `.sln` covers the standard .NET layout
(a solution at the repo root with its projects in subdirectories, and no
`.csproj` directly at the root) — without it such a repo would be skipped for
"no recognized manifest" and dropped from a multi-repo federation.

If no build file declares any member, a manifest-presence fallback scans the
root (gitignore- and noise-aware, bounded depth) for directories that contain a
recognized manifest and treats each as a member. Descent stops at the first
project root on each branch, and git submodules whose checkout is a project root
are surfaced as separate members.

### Package coordinate detection

A member's published **coordinate** is the name another member would import. It
is read from the member's package manifest, with precedence
Cargo to npm to Go to Python to Maven to Gradle to .NET:

| Ecosystem | Source | Coordinate |
| --- | --- | --- |
| `cargo` | `Cargo.toml` | `[package].name` |
| `npm` | `package.json` | `name` |
| `go` | `go.mod` | `module` path |
| `python` | `pyproject.toml` | `[project].name`, else `[tool.poetry].name` |
| `jvm` | `pom.xml` | `groupId:artifactId` (artifactId alone if no groupId) |
| `gradle` | `settings.gradle(.kts)` | `rootProject.name`, else the dir name |
| `dotnet` | `*.csproj`/`*.fsproj`/`*.vbproj`, else a root `*.sln` | `AssemblyName`, else `RootNamespace`, else the project-file stem. With only a `.sln` at the root, the first project it references supplies the name (falling back to the `.sln` stem) |

A member with no recognized manifest has no coordinate (and so contributes no
export surface in co-located mode).

## Cross-repo symbol resolution

### Export surfaces

Each member publishes an `export-surface.json`: its coordinate plus its public
symbols. Every code node defined in the member (non-empty `source_file`, i.e.
not an external stub) is exported, keyed by its label. The surface schema also
records a version (current schema version is `1`; a newer version is rejected on
load) and, for JVM/.NET members, a synthesized `namespace`: the longest dotted
prefix shared by at least half the qualified symbol ids (because JVM/.NET imports
spell a package namespace, not the build coordinate).

### Import-edge matching

Resolution runs on the composed (already `tag::`-prefixed) graph. Each external
stub that is the target of an `imports`/`imports_from` edge (or an edge whose
`context` is `import`) is resolved once, importer-independently:

1. **Alias match** (tried first): an import map, tsconfig `paths`, or
   module-federation remote that maps the specifier to a member tag. Resolves
   package-level to the member's anchor node (`INFERRED`, confidence 0.75).
2. **Coordinate match**: the imported path equals or is under a member's
   coordinate (`/`- or `.`-separated). The longest matching coordinate wins.
   A submodule import (`from billing.ledger import ...`,
   `import ".../billing/ledger"`) resolves module-exactly (`EXTRACTED`); a bare
   package import (`use billing::...`, `import billing`) resolves package-level
   to the member's anchor (`INFERRED`, confidence 0.75). Cargo coordinates are
   matched with `-` normalized to `_` (Rust `use` paths use the underscore lib
   name). JVM/.NET imports match the synthesized namespace.
3. **Single-owner symbol fallback**: when the imported stub names a symbol (e.g.
   Rust `use billing::Ledger` yields a stub labeled `Ledger`) and exactly one
   member exports that symbol, it resolves to that member (`EXTRACTED`). A symbol
   exported by two or more members is ambiguous and left unresolved.

Once a stub is resolved, *every* edge pointing at it (including non-import edges
like `references`/`inherits`) is rewired into the owning member and marked
`cross_repo`, except self-references (an importer importing its own repo). Import
edges adopt the resolution's confidence; other edges keep theirs. Colliding
rewired edges are deduped, keeping the highest-confidence one. Import targets
that match no member are retagged as third-party `external_package` nodes so
nothing dangles; orphaned rewired stubs are dropped.

The build reports counts of `extracted`, `inferred`, and `external_package`
resolutions, plus a `cross-language` count for the HTTP/RPC/FFI/WebSocket
boundaries that span repos (flagged on the edge by the cross-language pass, not by
this import resolver — see [Cross-Language-Edges](Cross-Language-Edges)).

### Aliases: tsconfig `paths`, module-federation `remotes`, import maps

Several JS/TS toolchains let one member reference another by an alias decoupled
from the target's `package.json name`. Synaptic collects all of them into one
alias map (`alias -> member tag`), built from a bounded walk over each member's
source tree (skipping `node_modules`, `.git`, `synaptic-out`). A self-alias (an
alias pointing at its own member) is dropped.

- **Import maps** (single-spa / SystemJS / native): the first `imports` object in
  a candidate file (`.json`, `.js`, `.mjs`, `.cjs`, `.ts`, `.ejs`, `.html`,
  `.htm`). The alias is **exact** (the import specifier must equal the alias).
  The member is identified by a path segment of the target value (e.g.
  `@PCMatic/Hub` to `${url}/hub/dist/...` resolves to member `hub`).
- **tsconfig `paths`**: `compilerOptions.paths` in `tsconfig*.json`. A `"@app/*"`
  key (trailing `/*`) is a **prefix** alias; a bare key is **exact**. The member
  is identified by a path segment of the target paths (e.g. `../hub/src/*`). A
  bare `"*"` catch-all is ignored.
- **module-federation `remotes`** (webpack `ModuleFederationPlugin`, Vite
  `@originjs/vite-plugin-federation`): the `remotes` object. Each remote is a
  **prefix** alias. The member is identified by a path segment of the remote
  value (the `name@url` form, or a URL path) or, as a fallback, the alias key
  itself.

Resolution tries an exact alias first, then the longest matching prefix alias
(with a path-segment boundary, so `@app` matches `@app` and `@app/Button` but
never `@apple`). On duplicate aliases, the first mapping wins.

## Composition and namespacing

Composition prefixes every member's subgraph with its tag (`id` becomes
`tag::id`), sets a `repo` attribute on each node, stashes the original id as
`local_id`, repo-prefixes `source_file`s, and unions the subgraphs. Shared
third-party **external** nodes (those with an empty `source_file`) that share a
label are collapsed to one node across repos (so the federated graph has one
`serde`/`requests` node, not one per repo). After cross-repo resolution the
merged graph is re-clustered at the workspace level.

## The global cross-repo store

The global store is a persistent, namespaced union of many repos' graphs kept
under a store directory: the default is `~/.synaptic`
(`%USERPROFILE%\.synaptic` on Windows, falling back to `.synaptic` in the CWD
if no home is set). It holds `global-graph.json` (the merged graph) and
`global-manifest.json` (per-repo bookkeeping: source path, node/edge counts, and
a source hash).

```
synaptic global add synaptic-out/graph.json
synaptic global add path/to/graph.json --as billing
synaptic global remove billing
synaptic global list
synaptic global path
```

- `global add <graph> [--as <tag>]`: add (or replace) a repo's `graph.json`
  under a tag. The default tag is the graph's grandparent directory name (so
  `<repo>/synaptic-out/graph.json` becomes `<repo>`). The tag is sanitized.
  `add` is idempotent: a source whose hash is unchanged is skipped. Re-adding a
  repo prunes its previous nodes first, then unions the new version in and
  collapses shared externals onto the existing global externals.
- `global remove <tag>`: remove a repo's nodes (and the edges/hyperedges
  touching them) from the store; prints the node count removed.
- `global list`: list the stored repos (tag, node/edge counts, source path),
  sorted by tag.
- `global path`: print the path to `global-graph.json`.

Serving the global graph (`synaptic serve --graph ~/.synaptic/global-graph.json`)
reads `global-manifest.json` next to it and registers each member's own source
root (the grandparent of its recorded `source_path`), so `get_source` reads a
federated node from its real repo even though the members live in sibling
directories outside any single `--source-root`. A co-located **workspace build**
(members are subdirectories of one root, the common case) needs none of this: the
single `--source-root` already resolves the `tag/...` paths. When source still
cannot be read, `get_source` names the configured root and says whether the path
was missing or outside it, rather than a bare "not available".

## `merge-graphs`

Composes several existing `graph.json` files into one namespaced graph. Unlike
the global store, inputs are composed verbatim with **no** external dedup.

```
synaptic merge-graphs a/synaptic-out/graph.json b/synaptic-out/graph.json
synaptic merge-graphs g1.json g2.json --out merged.json
```

Each input's repo tag is derived from its grandparent directory name
(`<repo>/synaptic-out/graph.json` becomes `<repo>`), falling back to the file
stem; repeated tags are disambiguated with a numeric suffix. The default output
is `synaptic-out/merged-graph.json`. The command prints the merged node/edge
counts and the tags used.

## Repo scoping

The federated `graph.json` carries a `repo` tag on every node, so a federated
graph can be sliced to a single member with no extra index.

The `--repo <tag>` flag scopes a query or export to one federated member before
running. It keeps only that repo's nodes and the edges/hyperedges fully inside
it (cross-repo edges, whose other end is in another repo, are dropped).
`--repo` is available on:

```
synaptic query "billing ledger" --repo billing
synaptic path NodeA NodeB --repo billing
synaptic explain Ledger --repo billing
synaptic export svg --repo billing
```

See [Querying] for query/path/explain and [Output-Formats] for export.
