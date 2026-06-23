//! Workspace build orchestration.
//!
//! [`build_workspace`] is the **co-located** mode: resolve members (declared
//! manifest or auto-discovery), build each local member with the full per-member
//! pipeline (one `synaptic_incremental::rebuild` call), publish each member's
//! export surface, then compose + cross-repo-resolve + re-cluster into one
//! federated graph. [`federate_artifacts`] is the **artifact** mode: compose the
//! same way from a directory of already-published `graph.json` +
//! `export-surface.json` files — members are never checked out together. Declared
//! `[[repos]]` members can also be `git`-cloned or consumed as a remote
//! `subgraph` (best-effort; the network path is untested offline).

use std::path::{Path, PathBuf};

use std::collections::BTreeMap;
use synaptic_core::{GraphData, NodeId};
use synaptic_graph::{
    apply_communities, cluster, link_dynamic_refs, mark_cross_repo_edges,
    resolve_parameterized_routes, resolve_route_handlers, resolve_sql_queries, ClusterOptions,
    KnowledgeGraph,
};
use synaptic_incremental::{rebuild, ChangeSet, RebuildOptions};

use crate::coordinate::{Coordinate, Ecosystem};
use crate::discover::{discover_members, members_from_globs, Member};
use crate::export_surface::{
    build_export_surface, load_surface, resolve_cross_repo, CrossRepoReport, ExportSurface,
};
use crate::federate::compose;
use crate::manifest::{load_manifest, RepoMember};
use crate::{load_graph, sanitize_tag, Result, WorkspaceError};

/// How a member's subgraph was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberSource {
    /// Built locally from source (monorepo / co-located).
    Local,
    /// Loaded from a published artifact (`graph.json` + `export-surface.json`).
    Artifact,
    /// Cloned from git or fetched as a remote subgraph.
    Remote,
}

/// Per-member summary for the build report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberReport {
    pub tag: String,
    pub coordinate: Option<Coordinate>,
    pub node_count: usize,
    pub edge_count: usize,
    pub source: MemberSource,
}

/// The result of a federated build.
pub struct WorkspaceBuild {
    /// The federated, cross-repo-resolved, re-clustered graph.
    pub federated: KnowledgeGraph,
    /// Workspace-level community assignment.
    pub communities: BTreeMap<u32, Vec<NodeId>>,
    /// Per-member summaries.
    pub members: Vec<MemberReport>,
    /// Each member's published export surface.
    pub surfaces: Vec<ExportSurface>,
    /// What cross-repo resolution did.
    pub cross_repo: CrossRepoReport,
}

/// Options for a workspace build.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceBuildOptions {
    pub directed: bool,
    pub force: bool,
}

/// Build one local member's subgraph via the full per-member pipeline.
fn build_member_graph(member: &Member, opts: &WorkspaceBuildOptions) -> Result<GraphData> {
    let outcome = rebuild(
        &RebuildOptions {
            root: member.path.clone(),
            directed: opts.directed,
            force: opts.force,
        },
        &ChangeSet::Full,
        None,
    )
    .map_err(|source| WorkspaceError::Member {
        member: member.tag.clone(),
        source,
    })?;
    Ok(outcome.kg.to_graph_data())
}

/// Compose + cross-repo-resolve + re-cluster the collected subgraphs.
/// `member_roots` (`(tag, on-disk root)`) feeds import-map alias resolution; pass
/// `&[]` when members have no source on disk (artifact federation).
fn finalize(
    subgraphs: Vec<(String, GraphData)>,
    surfaces: &[ExportSurface],
    member_roots: &[(String, PathBuf)],
) -> (KnowledgeGraph, BTreeMap<u32, Vec<NodeId>>, CrossRepoReport) {
    let composed = compose(subgraphs);
    let aliases = crate::alias::collect_aliases(member_roots);
    let (mut resolved, report) = resolve_cross_repo(composed, surfaces, &aliases);
    // Cross-repo HTTP routes: resolve any named route handler that spans repos
    // (router in one member, handler in another); exact same-path route nodes were
    // already merged by label in `compose`; then match a concrete client path in
    // one repo to a parameterized server route in another (/users/7 -> /users/{id});
    // finally flag the cross-language edges that end up spanning repos.
    let (hn, he) = resolve_route_handlers(resolved.nodes, resolved.links);
    let (hn, he) = resolve_sql_queries(hn, he);
    let (rn, re) = resolve_parameterized_routes(hn, he);
    // Evidence-link reflection sites to their unique target across the whole
    // federation before cross-repo edges are flagged, so a cross-repo dynamic_ref
    // is marked like any other cross-repo coupling.
    let (rn, re) = link_dynamic_refs(rn, re);
    resolved.nodes = rn;
    resolved.links = mark_cross_repo_edges(&resolved.nodes, re);
    // Cross-language coupling that spans repos (HTTP/RPC/FFI/WebSocket) is flagged
    // on the edge, not by the import resolver, so count it for the summary here.
    let mut report = report;
    report.cross_language = resolved
        .links
        .iter()
        .filter(|e| e.cross_repo && e.relation != "imports_from" && e.relation != "re_exports")
        .count();
    let mut kg = KnowledgeGraph::from_graph_data(resolved);
    let communities = cluster(&kg, &ClusterOptions::default());
    apply_communities(&mut kg, &communities);
    (kg, communities, report)
}

/// Does this look like a fetchable URL (vs a local path)?
fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Validate a git remote URL: only https/ssh/git schemes, or scp-style
/// `user@host:path`. Rejects `file://` and anything else.
fn validate_git_url(url: &str) -> Result<()> {
    // Never let a URL be parsed as a git option (defense in depth; clone_repo also
    // passes `--`).
    if url.starts_with('-') {
        return Err(WorkspaceError::Remote {
            member: url.to_string(),
            reason: "git URL must not start with '-'".into(),
        });
    }
    let ok = url.starts_with("https://")
        || url.starts_with("ssh://")
        || url.starts_with("git://")
        || url.starts_with("file://")
        || (url.contains('@') && url.contains(':') && !url.contains("://"));
    if ok {
        Ok(())
    } else {
        Err(WorkspaceError::Remote {
            member: url.to_string(),
            reason: "unsupported git URL scheme (allowed: https/ssh/git/file, scp-style)".into(),
        })
    }
}

/// Clone a `[[repos]] git=…` member into the workspace cache.
///
/// A source that is an existing local directory (a repo on disk, incl. tests) is
/// cloned directly — fully offline; remote URLs go through the scheme allowlist.
/// An already-cloned member is reused as-is (`synaptic workspace sync` refreshes
/// it). The network path (real remote URLs) is accepted-untested-offline.
fn clone_repo(repo: &RepoMember, url: &str, cache: &Path) -> Result<PathBuf> {
    let dest = cache.join(sanitize_tag(&repo.name));
    if dest.is_dir() {
        return Ok(dest); // already cloned; `sync` pulls updates
    }
    let local = Path::new(url).is_dir();
    if !local {
        validate_git_url(url)?;
    }
    std::fs::create_dir_all(cache).map_err(|e| WorkspaceError::Io {
        context: format!("creating {}", cache.display()),
        source: e,
    })?;
    let mut cmd = std::process::Command::new("git");
    cmd.arg("clone");
    if !local {
        // `--depth 1` is ignored (and warns) on local clones; only shallow remotes.
        cmd.arg("--depth").arg("1");
    }
    if let Some(rev) = &repo.rev {
        cmd.arg("--branch").arg(rev);
    }
    // `--` then url/dest as plain args, never shell-interpolated. Strip the
    // Windows `\\?\` verbatim prefix from the destination: `git` rejects it
    // ("Invalid argument") even though it's a valid Rust path.
    let dest_arg = {
        let s = dest.to_string_lossy();
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
    };
    cmd.arg("--").arg(url).arg(&dest_arg);
    let status = cmd.status().map_err(|e| WorkspaceError::Remote {
        member: repo.name.clone(),
        reason: format!("git clone failed to start: {e}"),
    })?;
    if !status.success() {
        return Err(WorkspaceError::Remote {
            member: repo.name.clone(),
            reason: format!("git clone exited with {status}"),
        });
    }
    Ok(dest)
}

/// Fetch a remote subgraph through the SSRF-guarded fetcher. **Untested offline.**
fn fetch_subgraph(name: &str, url: &str) -> Result<GraphData> {
    let bytes = synaptic_ingest::safe_fetch(url, crate::MAX_GRAPH_BYTES).map_err(|e| {
        WorkspaceError::Remote {
            member: name.to_string(),
            reason: format!("fetching {url}: {e}"),
        }
    })?;
    serde_json::from_slice(&bytes).map_err(|source| WorkspaceError::Json {
        path: url.to_string(),
        source,
    })
}

/// A declared `[[repos]]` member resolved into a subgraph (+ optional surface).
struct LoadedRepo {
    tag: String,
    graph: GraphData,
    surface: Option<ExportSurface>,
    source: MemberSource,
    coordinate: Option<Coordinate>,
    /// On-disk source root (path/git members); `None` for subgraph artifacts.
    /// Feeds import-map alias resolution.
    root: Option<PathBuf>,
}

/// Resolve one declared `[[repos]]` member into a subgraph + optional surface.
fn load_repo_member(
    root: &Path,
    repo: &RepoMember,
    opts: &WorkspaceBuildOptions,
    cache: &Path,
) -> Result<LoadedRepo> {
    let tag = sanitize_tag(&repo.name);

    // Artifact federation: consume a prebuilt subgraph (local path or URL).
    if let Some(sub) = &repo.subgraph {
        let graph = if is_url(sub) {
            fetch_subgraph(&repo.name, sub)?
        } else {
            load_graph(&root.join(sub))?
        };
        let coord = Coordinate {
            ecosystem: Ecosystem::Other,
            name: repo.name.clone(),
        };
        let surface = Some(build_export_surface(&tag, coord.clone(), &graph));
        return Ok(LoadedRepo {
            tag,
            graph,
            surface,
            source: MemberSource::Remote,
            coordinate: Some(coord),
            root: None,
        });
    }

    // A local already-checked-out path, or a git clone (best-effort, untested).
    let (path, source) = if let Some(p) = &repo.path {
        (root.join(p), MemberSource::Local)
    } else if let Some(url) = &repo.git {
        (clone_repo(repo, url, cache)?, MemberSource::Remote)
    } else {
        return Err(WorkspaceError::Remote {
            member: repo.name.clone(),
            reason: "no `path`, `git`, or `subgraph` specified".into(),
        });
    };

    let coordinate = crate::coordinate::package_coordinate(&path);
    let root = path.clone();
    let member = Member {
        tag: tag.clone(),
        path,
        coordinate: coordinate.clone(),
    };
    let graph = build_member_graph(&member, opts)?;
    let surface = coordinate
        .clone()
        .map(|c| build_export_surface(&tag, c, &graph));
    Ok(LoadedRepo {
        tag,
        graph,
        surface,
        source,
        coordinate,
        root: Some(root),
    })
}

/// Per-repo load output: namespaced subgraphs, their surfaces, member reports, and
/// the on-disk source roots (for import-map alias resolution).
type LoadedRepos = (
    Vec<(String, GraphData)>,
    Vec<ExportSurface>,
    Vec<MemberReport>,
    Vec<(String, PathBuf)>,
);

/// Load each declared `[[repos]]` member into a subgraph (+ surface + report + root).
/// Shared by [`build_workspace`] and [`federate_repos`].
fn load_repos(
    root: &Path,
    repos: &[RepoMember],
    opts: &WorkspaceBuildOptions,
    cache: &Path,
) -> Result<LoadedRepos> {
    let mut subgraphs = Vec::new();
    let mut surfaces = Vec::new();
    let mut reports = Vec::new();
    let mut roots = Vec::new();
    for repo in repos {
        let loaded = load_repo_member(root, repo, opts, cache)?;
        if let Some(s) = loaded.surface {
            surfaces.push(s);
        }
        if let Some(r) = &loaded.root {
            roots.push((loaded.tag.clone(), r.clone()));
        }
        reports.push(MemberReport {
            tag: loaded.tag.clone(),
            coordinate: loaded.coordinate,
            node_count: loaded.graph.nodes.len(),
            edge_count: loaded.graph.links.len(),
            source: loaded.source,
        });
        subgraphs.push((loaded.tag, loaded.graph));
    }
    Ok((subgraphs, surfaces, reports, roots))
}

/// Resolve a workspace's members: from `synaptic-workspace.toml` (member globs +
/// `[[repos]]`) when present, else auto-discovery. `root` should already be
/// canonical.
pub fn resolve_members(root: &Path) -> Result<(Vec<Member>, Vec<RepoMember>)> {
    Ok(match load_manifest(root)? {
        Some(m) => (members_from_globs(root, &m.workspace.members)?, m.repos),
        None => (discover_members(root), Vec::new()),
    })
}

/// Co-located build: members from the manifest (or auto-discovery) + any declared
/// `[[repos]]`, federated into one graph.
pub fn build_workspace(root: &Path, opts: &WorkspaceBuildOptions) -> Result<WorkspaceBuild> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let (members, repos) = resolve_members(&root)?;

    let cache = root.join("synaptic-out").join("workspace-repos");
    let mut subgraphs: Vec<(String, GraphData)> = Vec::new();
    let mut surfaces: Vec<ExportSurface> = Vec::new();
    let mut reports: Vec<MemberReport> = Vec::new();

    for member in &members {
        let gd = build_member_graph(member, opts)?;
        if let Some(coord) = &member.coordinate {
            surfaces.push(build_export_surface(&member.tag, coord.clone(), &gd));
        }
        reports.push(MemberReport {
            tag: member.tag.clone(),
            coordinate: member.coordinate.clone(),
            node_count: gd.nodes.len(),
            edge_count: gd.links.len(),
            source: MemberSource::Local,
        });
        subgraphs.push((member.tag.clone(), gd));
    }

    // Local member roots feed import-map alias resolution (a root file's import
    // map can alias a sibling member).
    let mut member_roots: Vec<(String, PathBuf)> = members
        .iter()
        .map(|m| (m.tag.clone(), m.path.clone()))
        .collect();

    let (repo_subgraphs, repo_surfaces, repo_reports, repo_roots) =
        load_repos(&root, &repos, opts, &cache)?;
    subgraphs.extend(repo_subgraphs);
    surfaces.extend(repo_surfaces);
    reports.extend(repo_reports);
    member_roots.extend(repo_roots);

    let (federated, communities, cross_repo) = finalize(subgraphs, &surfaces, &member_roots);
    Ok(WorkspaceBuild {
        federated,
        communities,
        members: reports,
        surfaces,
        cross_repo,
    })
}

/// Ephemeral federation of an explicit repo list (no local members, no manifest):
/// load each repo, then compose → cross-repo-resolve → re-cluster. Used by
/// `synaptic workspace discover`.
pub fn federate_repos(
    root: &Path,
    repos: &[RepoMember],
    opts: &WorkspaceBuildOptions,
) -> Result<WorkspaceBuild> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let cache = root.join("synaptic-out").join("workspace-repos");
    let (subgraphs, surfaces, reports, member_roots) = load_repos(&root, repos, opts, &cache)?;
    let (federated, communities, cross_repo) = finalize(subgraphs, &surfaces, &member_roots);
    Ok(WorkspaceBuild {
        federated,
        communities,
        members: reports,
        surfaces,
        cross_repo,
    })
}

/// Artifact mode: federate from a directory of `<member>/graph.json`
/// (+ optional `<member>/export-surface.json`). Members are never checked out
/// together — exactly what a per-repo-CI pipeline publishes.
pub fn federate_artifacts(dir: &Path) -> Result<WorkspaceBuild> {
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| WorkspaceError::Io {
            context: format!("reading {}", dir.display()),
            source: e,
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();

    let mut subgraphs: Vec<(String, GraphData)> = Vec::new();
    let mut surfaces: Vec<ExportSurface> = Vec::new();
    let mut reports: Vec<MemberReport> = Vec::new();

    for sub in subdirs {
        let gpath = sub.join("graph.json");
        if !gpath.is_file() {
            continue;
        }
        let tag = sanitize_tag(&sub.file_name().unwrap_or_default().to_string_lossy());
        let gd = load_graph(&gpath)?;

        let spath = sub.join("export-surface.json");
        let mut coordinate = None;
        if spath.is_file() {
            let mut s = load_surface(&spath)?;
            // Surface's repo must match the tag we prefix with, so cross-repo
            // targets (`tag::id`) line up.
            s.repo = tag.clone();
            coordinate = Some(s.coordinate.clone());
            surfaces.push(s);
        }
        reports.push(MemberReport {
            tag: tag.clone(),
            coordinate,
            node_count: gd.nodes.len(),
            edge_count: gd.links.len(),
            source: MemberSource::Artifact,
        });
        subgraphs.push((tag, gd));
    }

    // Artifact members have no source on disk, so no import maps to resolve.
    let (federated, communities, cross_repo) = finalize(subgraphs, &surfaces, &[]);
    Ok(WorkspaceBuild {
        federated,
        communities,
        members: reports,
        surfaces,
        cross_repo,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// A minimal Cargo monorepo: crate `app` (in pkgs/app) `use`s a type from
    /// crate `lib` (in pkgs/lib).
    fn make_monorepo(root: &Path) {
        touch(
            root,
            "synaptic-workspace.toml",
            "[workspace]\nname = \"demo\"\nmembers = [\"pkgs/*\"]\n",
        );
        touch(root, "pkgs/lib/Cargo.toml", "[package]\nname = \"lib\"\n");
        touch(
            root,
            "pkgs/lib/src/lib.rs",
            "pub struct Ledger;\nimpl Ledger { pub fn new() -> Ledger { Ledger } }\n",
        );
        touch(root, "pkgs/app/Cargo.toml", "[package]\nname = \"app\"\n");
        touch(
            root,
            "pkgs/app/src/lib.rs",
            "use lib::Ledger;\npub fn run() { let _ = Ledger::new(); }\n",
        );
    }

    #[test]
    fn co_located_build_federates_two_members_with_repo_tags() {
        let d = tempfile::tempdir().unwrap();
        make_monorepo(d.path());
        let build = build_workspace(d.path(), &WorkspaceBuildOptions::default()).unwrap();

        // Both members present, namespaced.
        let tags: std::collections::BTreeSet<&str> =
            build.members.iter().map(|m| m.tag.as_str()).collect();
        assert!(tags.contains("app") && tags.contains("lib"), "{tags:?}");
        let repos: std::collections::BTreeSet<Option<&str>> =
            build.federated.nodes().map(|n| n.repo.as_deref()).collect();
        assert!(repos.contains(&Some("app")) && repos.contains(&Some("lib")));

        // Two members each published a surface.
        assert_eq!(build.surfaces.len(), 2);
    }

    #[test]
    fn co_located_build_resolves_a_cross_repo_edge() {
        let d = tempfile::tempdir().unwrap();
        make_monorepo(d.path());
        let build = build_workspace(d.path(), &WorkspaceBuildOptions::default()).unwrap();
        // app `use lib::Ledger` produces a cross-repo edge into lib.
        let has_cross = build.federated.edges().any(|e| {
            e.cross_repo && e.source.0.starts_with("app::") && e.target.0.starts_with("lib::")
        });
        assert!(
            has_cross,
            "expected a cross_repo edge app→lib; report={:?}",
            build.cross_repo
        );
    }

    #[test]
    fn artifact_federation_matches_co_located() {
        let d = tempfile::tempdir().unwrap();
        make_monorepo(d.path());
        // Build co-located, then publish each member's graph + surface to an
        // artifact dir and federate from that.
        let co = build_workspace(d.path(), &WorkspaceBuildOptions::default()).unwrap();

        let art = d.path().join("artifacts");
        for member in &["app", "lib"] {
            let mdir = art.join(member);
            std::fs::create_dir_all(&mdir).unwrap();
            // Re-extract the single member to get its un-prefixed graph + surface.
            let m = Member {
                tag: member.to_string(),
                path: d.path().join("pkgs").join(member),
                coordinate: crate::coordinate::package_coordinate(
                    &d.path().join("pkgs").join(member),
                ),
            };
            let gd = build_member_graph(&m, &WorkspaceBuildOptions::default()).unwrap();
            crate::write_graph(&mdir.join("graph.json"), &gd).unwrap();
            if let Some(c) = &m.coordinate {
                let s = build_export_surface(member, c.clone(), &gd);
                crate::export_surface::save_surface(&mdir.join("export-surface.json"), &s).unwrap();
            }
        }

        let fed = federate_artifacts(&art).unwrap();
        assert_eq!(fed.members.len(), 2);
        assert_eq!(
            fed.cross_repo.extracted + fed.cross_repo.inferred,
            co.cross_repo.extracted + co.cross_repo.inferred,
            "artifact federation resolves the same cross-repo links"
        );
        assert!(fed
            .members
            .iter()
            .all(|m| m.source == MemberSource::Artifact));
    }

    #[test]
    fn build_resolves_import_map_alias_cross_repo() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        touch(
            r,
            "synaptic-workspace.toml",
            "[workspace]\nname=\"w\"\nmembers=[\"pkgs/*\"]\n",
        );
        // root app dynamically imports an alias; hub is the aliased member.
        touch(r, "pkgs/root/package.json", "{\"name\":\"root\"}");
        touch(
            r,
            "pkgs/root/src/index.js",
            "const importMaps = { imports: { \"@x/Hub\": `/pkgs/hub/dist/i.js` } };\nfunction boot(){ return System.import('@x/Hub'); }\n",
        );
        touch(r, "pkgs/hub/package.json", "{\"name\":\"hub\"}");
        touch(r, "pkgs/hub/src/index.js", "export function widget(){}\n");
        let build = build_workspace(r, &WorkspaceBuildOptions::default()).unwrap();
        assert!(
            build.cross_repo.extracted + build.cross_repo.inferred >= 1,
            "expected an import-map cross-repo link; report={:?}",
            build.cross_repo
        );
    }

    #[test]
    fn federate_repos_builds_path_members() {
        let d = tempfile::tempdir().unwrap();
        // Two standalone repos (no workspace manifest), referenced by path.
        touch(
            d.path(),
            "alpha/Cargo.toml",
            "[package]\nname = \"alpha\"\n",
        );
        touch(d.path(), "alpha/src/lib.rs", "pub fn a() {}\n");
        touch(d.path(), "beta/Cargo.toml", "[package]\nname = \"beta\"\n");
        touch(d.path(), "beta/src/lib.rs", "pub fn b() {}\n");
        let repos = vec![
            crate::manifest::RepoMember {
                name: "alpha".into(),
                git: None,
                rev: None,
                subgraph: None,
                path: Some("alpha".into()),
            },
            crate::manifest::RepoMember {
                name: "beta".into(),
                git: None,
                rev: None,
                subgraph: None,
                path: Some("beta".into()),
            },
        ];
        let build = federate_repos(d.path(), &repos, &WorkspaceBuildOptions::default()).unwrap();
        let tags: std::collections::BTreeSet<&str> =
            build.members.iter().map(|m| m.tag.as_str()).collect();
        assert!(tags.contains("alpha") && tags.contains("beta"), "{tags:?}");
        assert!(build
            .federated
            .nodes()
            .any(|n| n.repo.as_deref() == Some("alpha")));
    }

    #[test]
    fn rejects_bad_git_urls() {
        assert!(validate_git_url("https://github.com/a/b").is_ok());
        assert!(validate_git_url("git@github.com:a/b.git").is_ok());
        assert!(validate_git_url("ssh://git@host/a/b").is_ok());
        // `file://` is allowed (local repo clones; the manifest is the trust
        // boundary, same as `path=` members).
        assert!(validate_git_url("file:///srv/repo.git").is_ok());
        // Other schemes and option-injection attempts are rejected.
        assert!(validate_git_url("ftp://x/y").is_err());
        assert!(validate_git_url("--upload-pack=evil").is_err());
    }

    #[test]
    fn missing_repo_config_errors() {
        let d = tempfile::tempdir().unwrap();
        let repo = RepoMember {
            name: "ext".into(),
            git: None,
            rev: None,
            subgraph: None,
            path: None,
        };
        let err = load_repo_member(
            d.path(),
            &repo,
            &WorkspaceBuildOptions::default(),
            &d.path().join("cache"),
        )
        .err()
        .unwrap();
        assert!(matches!(err, WorkspaceError::Remote { .. }));
    }
}
