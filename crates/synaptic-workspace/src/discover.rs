//! Workspace member **auto-discovery** — used when no manifest exists.
//! Detects Cargo workspaces, npm/yarn `workspaces` + `pnpm-workspace.yaml`,
//! `go.work`, Python `[tool.uv.workspace]`, Maven `<modules>`, Gradle
//! `settings.gradle(.kts)` `include`, and `.sln` projects; expands nested
//! workspaces; and, when no build-file declares members, falls back to a
//! manifest-presence scan ([`discover_project_roots`]) so polyglot repos still
//! discover their projects. Each detected package directory becomes a [`Member`].
//!
//! Bazel/Buck/Nx/Turbo/Lerna are intentionally out of scope (heavier, often
//! build-overlays on top of the above, and outside the supported languages).

use std::path::{Path, PathBuf};

use toml::Value as Toml;

use crate::coordinate::{package_coordinate, Coordinate};
use crate::sanitize_tag;

/// A discovered (or declared) workspace member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Federation tag — the basis for `tag::id` namespacing and `--repo` scoping.
    pub tag: String,
    /// Absolute path to the member's package root.
    pub path: PathBuf,
    /// Published coordinate (for cross-repo import matching), if a recognized
    /// package manifest is present.
    pub coordinate: Option<Coordinate>,
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// True if `dir` contains a recognized package/build manifest (the project-root
/// predicate shared by discovery validation and the manifest-presence fallback).
/// `Cargo.toml` counts whether it declares `[package]` or `[workspace]`.
pub fn has_recognized_manifest(dir: &Path) -> bool {
    const FILES: &[&str] = &[
        "Cargo.toml",
        "package.json",
        "go.mod",
        "pyproject.toml",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
    ];
    if FILES.iter().any(|f| dir.join(f).is_file()) {
        return true;
    }
    // .NET: a project file directly in `dir`, or a solution (`.sln`) at the repo
    // root with its projects in subdirectories (the standard layout).
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.filter_map(|e| e.ok()).any(|e| {
            matches!(
                e.path().extension().and_then(|x| x.to_str()),
                Some("csproj") | Some("fsproj") | Some("vbproj") | Some("sln")
            )
        })
    })
}

/// Expand `root`-relative glob `patterns` to existing **directories**.
/// `!`-prefixed exclusion patterns (pnpm/npm/yarn) are skipped — negation is not
/// supported, so they are dropped rather than treated as a literal glob (which
/// would otherwise spuriously match a dir literally named `!…`).
fn expand_dirs(root: &Path, patterns: impl IntoIterator<Item = String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for pat in patterns {
        if pat.starts_with('!') {
            continue;
        }
        let full = root.join(&pat);
        let Ok(matches) = glob::glob(&full.to_string_lossy()) else {
            continue;
        };
        for m in matches.flatten() {
            if m.is_dir() {
                out.push(m);
            }
        }
    }
    out
}

/// Cargo members: the root crate (when the root `Cargo.toml` has a `[package]`,
/// covering single-crate repos + root-crate workspaces) plus each
/// `[workspace].members` glob.
fn cargo_members(root: &Path) -> Vec<PathBuf> {
    let Some(data) = read(&root.join("Cargo.toml")).and_then(|t| toml::from_str::<Toml>(&t).ok())
    else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    if data.get("package").and_then(Toml::as_table).is_some() {
        dirs.push(root.to_path_buf());
    }
    if let Some(members) = data
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(Toml::as_array)
    {
        let patterns = members
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string));
        dirs.extend(expand_dirs(root, patterns));
    }
    // Subtract `[workspace].exclude`. `default-members` is deliberately not used to
    // narrow scope: graph the whole workspace, not just the default build set.
    if let Some(excludes) = data
        .get("workspace")
        .and_then(|w| w.get("exclude"))
        .and_then(Toml::as_array)
    {
        let patterns = excludes
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string));
        let excluded: std::collections::HashSet<PathBuf> = expand_dirs(root, patterns)
            .into_iter()
            .map(|p| p.canonicalize().unwrap_or(p))
            .collect();
        dirs.retain(|d| !excluded.contains(&d.canonicalize().unwrap_or_else(|_| d.clone())));
    }
    dirs
}

/// npm/yarn `package.json` `workspaces` — an array of globs, or `{ "packages": [...] }`.
fn npm_members(root: &Path) -> Vec<PathBuf> {
    let Some(data) = read(&root.join("package.json"))
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    else {
        return Vec::new();
    };
    let ws = data.get("workspaces");
    let arr = match ws {
        Some(serde_json::Value::Array(a)) => Some(a),
        Some(serde_json::Value::Object(o)) => o.get("packages").and_then(|p| p.as_array()),
        _ => None,
    };
    let Some(arr) = arr else { return Vec::new() };
    let patterns = arr.iter().filter_map(|v| v.as_str().map(str::to_string));
    expand_dirs(root, patterns)
}

/// pnpm `pnpm-workspace.yaml` `packages:` list. YAML is parsed line-wise (no yaml
/// dep): collect `- 'glob'` items under the `packages:` key.
fn pnpm_members(root: &Path) -> Vec<PathBuf> {
    let Some(text) = read(&root.join("pnpm-workspace.yaml")) else {
        return Vec::new();
    };
    let mut patterns: Vec<String> = Vec::new();
    let mut in_packages = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !in_packages {
            if trimmed.starts_with("packages:") {
                in_packages = true;
            }
            continue;
        }
        // A list item under `packages:` is indented and starts with `-`.
        if let Some(item) = trimmed.strip_prefix('-') {
            let pat = item.trim().trim_matches(['"', '\'']).to_string();
            if !pat.is_empty() {
                patterns.push(pat);
            }
        } else {
            // A non-indented / non-list line ends the `packages:` block.
            break;
        }
    }
    expand_dirs(root, patterns)
}

/// `go.work` `use` directives — either `use ./path` or a `use ( ... )` block.
fn go_work_members(root: &Path) -> Vec<PathBuf> {
    let Some(text) = read(&root.join("go.work")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut in_block = false;
    let push = |dirs: &mut Vec<PathBuf>, p: &str| {
        let p = p.trim().trim_matches('"');
        if !p.is_empty() {
            dirs.push(root.join(p));
        }
    };
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or(raw).trim();
        if line.is_empty() {
            continue;
        }
        if in_block {
            if line.starts_with(')') {
                in_block = false;
            } else {
                push(&mut dirs, line);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("use") {
            let rest = rest.trim();
            if rest.starts_with('(') {
                in_block = true;
                // `use ( ./a` on one line is legal-ish; ignore trailing here.
            } else {
                push(&mut dirs, rest);
            }
        }
    }
    dirs.retain(|d| d.is_dir());
    dirs
}

/// Python `[tool.uv.workspace].members` globs.
fn python_members(root: &Path) -> Vec<PathBuf> {
    let Some(data) =
        read(&root.join("pyproject.toml")).and_then(|t| toml::from_str::<Toml>(&t).ok())
    else {
        return Vec::new();
    };
    let Some(members) = data
        .get("tool")
        .and_then(|t| t.get("uv"))
        .and_then(|u| u.get("workspace"))
        .and_then(|w| w.get("members"))
        .and_then(Toml::as_array)
    else {
        return Vec::new();
    };
    let patterns = members
        .iter()
        .filter_map(|m| m.as_str().map(str::to_string));
    expand_dirs(root, patterns)
}

/// Maven `pom.xml` `<modules><module>…` directories (one aggregator level).
fn maven_members(root: &Path) -> Vec<PathBuf> {
    let Some(text) = read(&root.join("pom.xml")) else {
        return Vec::new();
    };
    let Ok(doc) = roxmltree::Document::parse(&text) else {
        return Vec::new();
    };
    doc.descendants()
        .filter(|n| n.has_tag_name("module"))
        .filter_map(|n| n.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|m| root.join(m))
        .filter(|p| p.is_dir())
        .collect()
}

/// Gradle `settings.gradle(.kts)` `include` directives. Gradle path notation
/// `:a:b` maps to the directory `a/b` (leading `:` stripped, `:`→`/`).
fn gradle_members(root: &Path) -> Vec<PathBuf> {
    let mut text = None;
    for f in ["settings.gradle", "settings.gradle.kts"] {
        if let Some(t) = read(&root.join(f)) {
            text = Some(t);
            break;
        }
    }
    let Some(text) = text else { return Vec::new() };
    let mut dirs = Vec::new();
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or(raw).trim();
        let Some(rest) = line.strip_prefix("include") else {
            continue;
        };
        // `include 'a', ":b:c"` (Groovy) or `include("a", ":b:c")` (Kotlin).
        let rest = rest.trim().trim_start_matches('(').trim_end_matches(')');
        for item in rest.split(',') {
            let proj = item.trim().trim_matches(['"', '\'']);
            if proj.is_empty() {
                continue;
            }
            let rel = proj.trim_start_matches(':').replace(':', "/");
            let p = root.join(&rel);
            if p.is_dir() {
                dirs.push(p);
            }
        }
    }
    dirs
}

/// .NET `.sln` members: the parent directory of each project file the solution
/// references (shared `.sln` parser in [`crate::coordinate`]).
fn dotnet_members(root: &Path) -> Vec<PathBuf> {
    let Some(sln) = crate::coordinate::first_sln(root) else {
        return Vec::new();
    };
    crate::coordinate::sln_project_files(root, &sln)
        .into_iter()
        .filter_map(|proj| proj.parent().map(Path::to_path_buf))
        .filter(|parent| parent.is_dir())
        .collect()
}

/// Lexically-normalize `p` (resolve `.`/`..` without touching the filesystem).
fn normalize_lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// True when `candidate` is `root` or strictly inside it. Uses the real path when
/// both exist (canonicalize), falling back to a lexical comparison.
fn within_root(root: &Path, candidate: &Path) -> bool {
    match (root.canonicalize(), candidate.canonicalize()) {
        (Ok(r), Ok(c)) => c.starts_with(&r),
        _ => normalize_lexical(candidate).starts_with(normalize_lexical(root)),
    }
}

/// Turn a list of candidate directories into [`Member`]s: keep real dirs inside
/// `root`, dedup by canonical path, sort, and assign unique sanitized tags +
/// coordinates. Shared by [`discover_members`] and [`members_from_globs`].
fn dirs_to_members(root: &Path, dirs: Vec<PathBuf>) -> Vec<Member> {
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<PathBuf> = Vec::new();
    for d in dirs {
        if !d.is_dir() || !within_root(root, &d) || !has_recognized_manifest(&d) {
            continue;
        }
        let key = d.canonicalize().unwrap_or_else(|_| normalize_lexical(&d));
        if seen.insert(key.clone()) {
            kept.push(key);
        }
    }
    kept.sort();

    // Assign a unique tag per member. Disambiguate against all already-assigned
    // tags (not a per-base counter) so an auto-suffixed `x-2` can't collide with a
    // directory literally named `x-2`.
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    kept.into_iter()
        .map(|path| {
            let base = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".into());
            let base_tag = sanitize_tag(&base);
            let mut tag = base_tag.clone();
            let mut n = 2;
            while used.contains(&tag) {
                tag = format!("{base_tag}-{n}");
                n += 1;
            }
            used.insert(tag.clone());
            let coordinate = package_coordinate(&path);
            Member {
                tag,
                path,
                coordinate,
            }
        })
        .collect()
}

/// Max nesting for workspace-in-workspace expansion.
const MAX_NEST_DEPTH: usize = 3;

/// Run every build-file detector at `root` (no fallback, no recursion).
fn detect_member_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    dirs.extend(cargo_members(root));
    dirs.extend(npm_members(root));
    dirs.extend(pnpm_members(root));
    dirs.extend(go_work_members(root));
    dirs.extend(python_members(root));
    dirs.extend(maven_members(root));
    dirs.extend(gradle_members(root));
    dirs.extend(dotnet_members(root));
    dirs
}

/// Cheap pre-filter for [`expand_nested`]: could `dir` *itself* be a workspace
/// root (i.e. declare members beyond itself)? Only such dirs are worth the full
/// [`detect_member_dirs`] pass. Existence-only checks first; content checks read a
/// manifest only when it exists and match a cheap substring (false positives just
/// cost an unnecessary — but correct — recursion). This keeps a flat workspace's
/// leaf packages off the expensive per-member detector path.
fn could_be_nested_workspace(dir: &Path) -> bool {
    if dir.join("pnpm-workspace.yaml").is_file()
        || dir.join("go.work").is_file()
        || dir.join("settings.gradle").is_file()
        || dir.join("settings.gradle.kts").is_file()
    {
        return true;
    }
    let has = |file: &str, needle: &str| read(&dir.join(file)).is_some_and(|t| t.contains(needle));
    has("Cargo.toml", "[workspace]")
        || has("package.json", "\"workspaces\"")
        || has("pyproject.toml", "uv.workspace")
        || has("pom.xml", "<modules>")
}

/// Expand nested workspaces: any member dir that is itself a workspace root
/// (declares members *other* than itself) contributes its own members too.
/// Bounded depth + a visited-path cycle guard. A single-package dir (whose only
/// "member" is itself, e.g. a Cargo `[package]`) is treated as a leaf — and is
/// skipped by the cheap [`could_be_nested_workspace`] pre-check before the
/// expensive full detector pass.
fn expand_nested(
    dirs: Vec<PathBuf>,
    depth: usize,
    seen: &mut std::collections::HashSet<PathBuf>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let key = d.canonicalize().unwrap_or_else(|_| normalize_lexical(&d));
        if !seen.insert(key.clone()) {
            continue;
        }
        let nested: Vec<PathBuf> = if depth >= MAX_NEST_DEPTH || !could_be_nested_workspace(&d) {
            Vec::new()
        } else {
            detect_member_dirs(&d)
                .into_iter()
                .filter(|n| n.canonicalize().unwrap_or_else(|_| normalize_lexical(n)) != key)
                .collect()
        };
        out.push(d);
        if !nested.is_empty() {
            out.extend(expand_nested(nested, depth + 1, seen));
        }
    }
    out
}

/// Discover workspace members under `root` from build files. Members are
/// deduplicated by resolved path, filtered to those inside `root`, sorted by path
/// for determinism, and tagged with a unique sanitized name. Nested workspaces
/// are expanded; when no build-file declares any member, falls back to a
/// manifest-presence scan ([`discover_project_roots`]).
pub fn discover_members(root: &Path) -> Vec<Member> {
    let top = detect_member_dirs(root);
    if top.is_empty() {
        return discover_project_roots(root);
    }
    let mut seen = std::collections::HashSet::new();
    let dirs = expand_nested(top, 0, &mut seen);
    dirs_to_members(root, dirs)
}

/// Expand explicit `synaptic-workspace.toml` member globs (relative to `root`)
/// into [`Member`]s — the declared-manifest counterpart of [`discover_members`].
/// Unlike auto-discovery (which silently drops out-of-root matches), a *declared*
/// glob that resolves outside the workspace root is a config error and surfaces.
pub fn members_from_globs(root: &Path, patterns: &[String]) -> crate::Result<Vec<Member>> {
    let dirs = expand_dirs(root, patterns.iter().cloned());
    for d in &dirs {
        if d.is_dir() && !within_root(root, d) {
            return Err(crate::WorkspaceError::OutsideRoot {
                member: d.display().to_string(),
            });
        }
    }
    Ok(dirs_to_members(root, dirs))
}

/// Default depth for the manifest-presence fallback scan.
const FALLBACK_MAX_DEPTH: usize = 6;

/// Manifest-presence fallback: when no workspace build-file declares members,
/// scan `root` (gitignore + noise aware) for directories that contain a
/// recognized manifest and treat each as a member. Descent stops at the first
/// project root on each branch (a root's subdirs are its own files), so a project
/// with manifest-less subdirs yields exactly one member.
pub fn discover_project_roots(root: &Path) -> Vec<Member> {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut roots: Vec<PathBuf> = Vec::new();
    // walk_dirs returns dirs sorted, so a parent always precedes its children.
    for dir in synaptic_detect::walk_dirs(&canon, Some(FALLBACK_MAX_DEPTH)) {
        // Skip anything already inside an accepted project root.
        if roots.iter().any(|r| dir.starts_with(r) && dir != *r) {
            continue;
        }
        if has_recognized_manifest(&dir) {
            roots.push(dir);
        }
    }
    // Git submodules are separate codebases: surface any whose checkout is a
    // project root and isn't already covered by a discovered root.
    for sub in synaptic_detect::submodule_paths(&canon) {
        let sub = sub.canonicalize().unwrap_or(sub);
        if sub.starts_with(&canon)
            && has_recognized_manifest(&sub)
            && !roots
                .iter()
                .any(|r| sub.starts_with(r) || r.starts_with(&sub))
        {
            roots.push(sub);
        }
    }
    dirs_to_members(&canon, roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinate::Ecosystem;

    fn touch(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn discovers_cargo_workspace_members_with_coordinates() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(r, "Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        touch(
            r,
            "crates/billing/Cargo.toml",
            "[package]\nname = \"billing\"\n",
        );
        touch(
            r,
            "crates/identity/Cargo.toml",
            "[package]\nname = \"identity\"\n",
        );

        let members = discover_members(r);
        let tags: Vec<&str> = members.iter().map(|m| m.tag.as_str()).collect();
        assert!(
            tags.contains(&"billing") && tags.contains(&"identity"),
            "{tags:?}"
        );
        let billing = members.iter().find(|m| m.tag == "billing").unwrap();
        assert_eq!(
            billing.coordinate,
            Some(Coordinate {
                ecosystem: Ecosystem::Cargo,
                name: "billing".into()
            })
        );
    }

    #[test]
    fn discovers_npm_array_and_object_workspaces() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "package.json",
            "{\"name\":\"root\",\"workspaces\":[\"packages/*\"]}",
        );
        touch(r, "packages/ui/package.json", "{\"name\":\"@acme/ui\"}");
        let members = discover_members(r);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].tag, "ui");
        assert_eq!(members[0].coordinate.as_ref().unwrap().name, "@acme/ui");

        let d2 = tempfile::tempdir().unwrap();
        let r2 = d2.path();
        touch(
            r2,
            "package.json",
            "{\"workspaces\":{\"packages\":[\"libs/*\"]}}",
        );
        touch(r2, "libs/core/package.json", "{\"name\":\"core\"}");
        let m2 = discover_members(r2);
        assert_eq!(m2.len(), 1);
        assert_eq!(m2[0].tag, "core");
    }

    #[test]
    fn discovers_pnpm_yaml_packages() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "pnpm-workspace.yaml",
            "packages:\n  - 'apps/*'\n  - \"libs/*\"\n# a comment\n",
        );
        touch(r, "apps/web/package.json", "{\"name\":\"web\"}");
        touch(r, "libs/util/package.json", "{\"name\":\"util\"}");
        let members = discover_members(r);
        let tags: Vec<&str> = members.iter().map(|m| m.tag.as_str()).collect();
        assert!(tags.contains(&"web") && tags.contains(&"util"), "{tags:?}");
    }

    #[test]
    fn discovers_go_work_use_block_and_single() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "go.work",
            "go 1.22\n\nuse (\n    ./billing\n    ./identity\n)\nuse ./gateway\n",
        );
        touch(r, "billing/go.mod", "module github.com/acme/billing\n");
        touch(r, "identity/go.mod", "module github.com/acme/identity\n");
        touch(r, "gateway/go.mod", "module github.com/acme/gateway\n");
        let members = discover_members(r);
        let tags: Vec<&str> = members.iter().map(|m| m.tag.as_str()).collect();
        assert!(
            tags.contains(&"billing") && tags.contains(&"identity") && tags.contains(&"gateway"),
            "{tags:?}"
        );
        let billing = members.iter().find(|m| m.tag == "billing").unwrap();
        assert_eq!(
            billing.coordinate.as_ref().unwrap().name,
            "github.com/acme/billing"
        );
    }

    #[test]
    fn discovers_uv_python_workspace() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "pyproject.toml",
            "[project]\nname = \"root\"\n[tool.uv.workspace]\nmembers = [\"pkgs/*\"]\n",
        );
        touch(r, "pkgs/svc/pyproject.toml", "[project]\nname = \"svc\"\n");
        let members = discover_members(r);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].tag, "svc");
        assert_eq!(members[0].coordinate.as_ref().unwrap().name, "svc");
    }

    #[test]
    fn dedupes_overlapping_detectors_and_is_sorted() {
        // A dir that is both a Cargo member and an npm package is listed once.
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(r, "Cargo.toml", "[workspace]\nmembers = [\"pkgs/*\"]\n");
        touch(r, "package.json", "{\"workspaces\":[\"pkgs/*\"]}");
        touch(r, "pkgs/a/Cargo.toml", "[package]\nname=\"a\"\n");
        touch(r, "pkgs/a/package.json", "{\"name\":\"a\"}");
        touch(r, "pkgs/b/Cargo.toml", "[package]\nname=\"b\"\n");
        let members = discover_members(r);
        let tags: Vec<&str> = members.iter().map(|m| m.tag.as_str()).collect();
        assert_eq!(tags, vec!["a", "b"], "deduped + sorted");
    }

    #[test]
    fn single_crate_repo_is_one_member() {
        // A root Cargo.toml with [package] and no [workspace] yields the root itself.
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(r, "Cargo.toml", "[package]\nname = \"solo\"\n");
        touch(r, "src/lib.rs", "pub fn f() {}\n");
        let members = discover_members(r);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].coordinate.as_ref().unwrap().name, "solo");
    }

    #[test]
    fn excluded_patterns_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "pnpm-workspace.yaml",
            "packages:\n  - 'pkgs/*'\n  - '!pkgs/ignored'\n",
        );
        touch(r, "pkgs/keep/package.json", "{\"name\":\"keep\"}");
        touch(r, "pkgs/ignored/package.json", "{\"name\":\"ignored\"}");
        let tags: Vec<String> = discover_members(r).into_iter().map(|m| m.tag).collect();
        // The `!`-exclusion line is dropped, not treated as a literal glob. We
        // don't apply negation, so pkgs/* members still appear; the point is the
        // `!` entry never spuriously matches a `!...`-named dir.
        assert!(tags.iter().any(|t| t == "keep"), "{tags:?}");
    }

    #[test]
    fn tags_are_unique_even_against_a_literal_suffix_name() {
        // Two dirs named `x` plus a real `x-2` must yield three distinct tags.
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "Cargo.toml",
            "[workspace]\nmembers = [\"a/x\", \"b/x\", \"x-2\"]\n",
        );
        touch(r, "a/x/Cargo.toml", "[package]\nname = \"ax\"\n");
        touch(r, "b/x/Cargo.toml", "[package]\nname = \"bx\"\n");
        touch(r, "x-2/Cargo.toml", "[package]\nname = \"x2\"\n");
        let tags: std::collections::HashSet<String> =
            discover_members(r).into_iter().map(|m| m.tag).collect();
        assert_eq!(tags.len(), 3, "all tags unique: {tags:?}");
    }

    #[test]
    fn empty_root_discovers_nothing() {
        let d = tempfile::tempdir().unwrap();
        assert!(discover_members(d.path()).is_empty());
    }

    #[test]
    fn discovers_maven_modules() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "pom.xml",
            r#"<project><modules><module>billing</module><module>identity</module></modules></project>"#,
        );
        touch(
            r,
            "billing/pom.xml",
            "<project><artifactId>billing</artifactId></project>",
        );
        touch(
            r,
            "identity/pom.xml",
            "<project><artifactId>identity</artifactId></project>",
        );
        let tags: Vec<String> = discover_members(r).into_iter().map(|m| m.tag).collect();
        assert!(
            tags.contains(&"billing".to_string()) && tags.contains(&"identity".to_string()),
            "{tags:?}"
        );
    }

    #[test]
    fn discovers_gradle_includes() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "settings.gradle",
            "rootProject.name = 'root'\ninclude 'billing', ':svc:identity'\n",
        );
        touch(r, "billing/build.gradle", "plugins {}\n");
        touch(r, "svc/identity/build.gradle", "plugins {}\n");
        let tags: Vec<String> = discover_members(r).into_iter().map(|m| m.tag).collect();
        assert!(
            tags.contains(&"billing".to_string()) && tags.contains(&"identity".to_string()),
            "{tags:?}"
        );
    }

    #[test]
    fn discovers_dotnet_sln_projects() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "App.sln",
            "Project(\"{GUID}\") = \"Billing\", \"billing\\Billing.csproj\", \"{P1}\"\nEndProject\nProject(\"{GUID}\") = \"Identity\", \"identity\\Identity.csproj\", \"{P2}\"\nEndProject\n",
        );
        touch(r, "billing/Billing.csproj", "<Project/>");
        touch(r, "identity/Identity.csproj", "<Project/>");
        let tags: Vec<String> = discover_members(r).into_iter().map(|m| m.tag).collect();
        assert!(
            tags.contains(&"billing".to_string()) && tags.contains(&"identity".to_string()),
            "{tags:?}"
        );
    }

    #[test]
    fn declared_glob_escaping_root_errors() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        let root = d.path().join("root");
        touch(&outside, "Cargo.toml", "[package]\nname=\"x\"\n");
        touch(&root, "Cargo.toml", "[package]\nname=\"r\"\n");
        let err = members_from_globs(&root, &["../outside".to_string()]).unwrap_err();
        assert!(
            matches!(err, crate::WorkspaceError::OutsideRoot { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn cargo_exclude_is_subtracted() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/skip\"]\n",
        );
        touch(r, "crates/keep/Cargo.toml", "[package]\nname=\"keep\"\n");
        touch(r, "crates/skip/Cargo.toml", "[package]\nname=\"skip\"\n");
        let tags: Vec<String> = discover_members(r).into_iter().map(|m| m.tag).collect();
        assert_eq!(tags, vec!["keep"], "{tags:?}");
    }

    #[test]
    fn glob_match_without_manifest_is_dropped() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(r, "Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        touch(r, "crates/real/Cargo.toml", "[package]\nname=\"real\"\n");
        touch(r, "crates/fixtures/data.txt", "not a crate");
        let tags: Vec<String> = discover_members(r).into_iter().map(|m| m.tag).collect();
        assert_eq!(tags, vec!["real"], "manifest-less dir dropped: {tags:?}");
    }

    #[test]
    fn recognizes_all_manifest_kinds() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for (rel, body) in [
            ("rust/Cargo.toml", "[package]\nname=\"a\"\n"),
            ("rustws/Cargo.toml", "[workspace]\nmembers=[]\n"),
            ("js/package.json", "{}"),
            ("go/go.mod", "module x\n"),
            ("py/pyproject.toml", "[project]\nname=\"p\"\n"),
            ("mvn/pom.xml", "<project/>"),
            ("gr/build.gradle", "x"),
            ("net/Svc.csproj", "<Project/>"),
            ("netvb/Svc.vbproj", "<Project/>"),
            (
                "netsln/App.sln",
                "Project(\"{G}\") = \"x\", \"x\\x.csproj\", \"{P}\"\n",
            ),
        ] {
            touch(r, rel, body);
        }
        for dir in [
            "rust", "rustws", "js", "go", "py", "mvn", "gr", "net", "netvb", "netsln",
        ] {
            assert!(has_recognized_manifest(&r.join(dir)), "{dir}");
        }
        touch(r, "plain/readme.txt", "hi");
        assert!(!has_recognized_manifest(&r.join("plain")));
    }

    #[test]
    fn solution_repo_with_subdir_projects_is_one_member_with_coordinate() {
        // The standard .NET repo layout (solution at root, .csproj in subdirs, no
        // root .csproj) must federate as a single member carrying a coordinate,
        // not be dropped for lacking a root manifest.
        use crate::coordinate::Ecosystem;
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("desktop-app");
        touch(
            &repo,
            "Acme.sln",
            "Project(\"{G}\") = \"Acme.App\", \"App\\Acme.App.csproj\", \"{P}\"\nEndProject\n",
        );
        touch(
            &repo,
            "App/Acme.App.csproj",
            r#"<Project><PropertyGroup><AssemblyName>Acme.App</AssemblyName></PropertyGroup></Project>"#,
        );
        let members = discover_project_roots(d.path());
        let m = members
            .iter()
            .find(|m| m.tag == "desktop-app")
            .unwrap_or_else(|| panic!("repo not discovered: {members:?}"));
        let coord = m.coordinate.as_ref().expect("repo has a coordinate");
        assert_eq!(coord.ecosystem, Ecosystem::DotNet);
        assert_eq!(coord.name, "Acme.App");
    }
}
