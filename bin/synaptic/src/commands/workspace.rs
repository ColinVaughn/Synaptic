//! `workspace` command(s) split from main.rs.

use crate::cli::WorkspaceAction;
use crate::commands::extract::write_outputs;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use synaptic_graph::analyze;

/// Last path segment of a git URL, minus a trailing `.git`, as a repo name.
pub(crate) fn derive_repo_name(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(url);
    last.trim_end_matches(".git").to_string()
}

/// Write a federated build's standard outputs + per-member export surfaces.
pub(crate) fn write_federated(
    build: &synaptic_workspace::workspace_build::WorkspaceBuild,
    root: &Path,
) -> Result<()> {
    let out_dir = root.join("synaptic-out");
    let analysis = analyze(&build.federated, &build.communities, &BTreeMap::new());
    let extras = write_outputs(
        &build.federated,
        &analysis,
        &build.communities,
        &BTreeMap::new(),
        &out_dir,
        false,
        false,
    )?;
    let surf_dir = out_dir.join("surfaces");
    fs::create_dir_all(&surf_dir).context("creating surfaces/")?;
    for s in &build.surfaces {
        synaptic_workspace::export_surface::save_surface(
            &surf_dir.join(format!("{}.json", s.repo)),
            s,
        )
        .map_err(|e| anyhow::anyhow!("writing surface: {e}"))?;
    }
    println!(
        "Wrote {}/{{{}}} + surfaces/ ({} member surface(s))",
        out_dir.display(),
        extras,
        build.surfaces.len()
    );
    Ok(())
}

pub(crate) fn print_build_summary(build: &synaptic_workspace::workspace_build::WorkspaceBuild) {
    println!(
        "Federated graph: {} nodes · {} edges · {} communities · {} member(s)",
        build.federated.node_count(),
        build.federated.edge_count(),
        build.communities.len(),
        build.members.len()
    );
    let cr = &build.cross_repo;
    println!(
        "Cross-repo links: {} extracted, {} inferred · {} external package(s)",
        cr.extracted, cr.inferred, cr.external_packages
    );
    for m in &build.members {
        println!(
            "  {} — {} nodes, {} edges",
            m.tag, m.node_count, m.edge_count
        );
    }
}

pub(crate) fn run_workspace(action: WorkspaceAction) -> Result<()> {
    use synaptic_workspace::workspace_build::{
        build_workspace, federate_artifacts, resolve_members, WorkspaceBuildOptions,
    };
    let root = std::env::current_dir().context("resolving current directory")?;
    let root = root.canonicalize().unwrap_or(root);
    match action {
        WorkspaceAction::Init {
            scan_repos,
            depth,
            max,
        } => {
            use synaptic_workspace::discover::discover_members;
            use synaptic_workspace::manifest::{
                load_manifest, write_manifest, RepoMember, WorkspaceManifest, WorkspaceMeta,
                MANIFEST_NAME,
            };
            let members = discover_members(&root);
            let member_globs: Vec<String> = members
                .iter()
                .filter_map(|m| {
                    m.path
                        .strip_prefix(&root)
                        .ok()
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                })
                .collect();
            let name = root
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".into());
            let manifest = WorkspaceManifest {
                workspace: WorkspaceMeta {
                    name,
                    default_branch: "main".into(),
                    members: member_globs.clone(),
                },
                repos: vec![],
            };
            write_manifest(&root, &manifest).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "Wrote {MANIFEST_NAME} with {} member(s):",
                member_globs.len()
            );
            for g in &member_globs {
                println!("  - {g}");
            }
            // Optional: scan for sibling git repos and append [[repos]] entries.
            if let Some(scan_arg) = scan_repos {
                use synaptic_workspace::scan::{
                    discover_sibling_repos, relative_path, ScanOptions, SkipReason,
                };
                let scan_root = scan_arg.map(|p| root.join(p)).unwrap_or_else(|| {
                    root.parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| root.clone())
                });
                let scan_root = scan_root.canonicalize().unwrap_or(scan_root);
                let res =
                    discover_sibling_repos(&scan_root, &ScanOptions { depth, max }, Some(&root));
                let mut manifest = load_manifest(&root)
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .unwrap_or_default();
                let existing: std::collections::HashSet<String> =
                    manifest.repos.iter().map(|r| r.name.clone()).collect();
                let mut added = 0usize;
                for c in &res.repos {
                    if existing.contains(&c.name) {
                        continue;
                    }
                    manifest.repos.push(RepoMember {
                        name: c.name.clone(),
                        git: None,
                        rev: None,
                        subgraph: None,
                        path: Some(relative_path(&root, &c.path)),
                    });
                    added += 1;
                }
                write_manifest(&root, &manifest).map_err(|e| anyhow::anyhow!("{e}"))?;
                println!(
                    "Discovered {added} sibling repo(s) under {}:",
                    scan_root.display()
                );
                for c in &res.repos {
                    println!("  {} -> {}", c.name, relative_path(&root, &c.path));
                }
                for (p, reason) in &res.skipped {
                    let why = match reason {
                        SkipReason::NoManifest => "no recognized manifest",
                        SkipReason::OverCap => "over --max cap",
                    };
                    println!("  skipped {} ({why})", p.display());
                }
            }
            Ok(())
        }
        WorkspaceAction::Add { target } => {
            use synaptic_workspace::manifest::{load_manifest, write_manifest, RepoMember};
            let mut manifest = load_manifest(&root)
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .unwrap_or_default();
            // An existing local path is always a path member (so a Windows path
            // like C:\Users\bob@corp\repo isn't mistaken for an scp git URL).
            let looks_git = !Path::new(&target).exists()
                && (target.contains("://")
                    || (target.contains('@') && target.contains(':') && !target.contains(' ')));
            if looks_git {
                let name = derive_repo_name(&target);
                println!("Added git member '{name}' ({target}).");
                manifest.repos.push(RepoMember {
                    name,
                    git: Some(target),
                    rev: None,
                    subgraph: None,
                    path: None,
                });
            } else {
                println!("Added member path '{target}'.");
                manifest.workspace.members.push(target);
            }
            write_manifest(&root, &manifest).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(())
        }
        WorkspaceAction::Build { changed, directed } => {
            let opts = WorkspaceBuildOptions {
                directed,
                force: false,
            };
            if changed {
                let outcome = synaptic_workspace::state::update_workspace(&root, &opts)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let Some(build) = outcome.build else {
                    println!("No member changes — federated graph is up to date.");
                    return Ok(());
                };
                if build.members.is_empty() {
                    println!("No members found — run `synaptic workspace init` first.");
                    return Ok(());
                }
                write_federated(&build, &root)?;
                // Persist state ONLY after artifacts are durably written.
                if let Some(state) = &outcome.new_state {
                    synaptic_workspace::state::save_state(&root, state)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                print_build_summary(&build);
                if !outcome.surface_changed.is_empty() {
                    println!("Surfaces changed: {}", outcome.surface_changed.join(", "));
                }
            } else {
                let build = build_workspace(&root, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;
                if build.members.is_empty() {
                    println!("No members found — run `synaptic workspace init` first.");
                    return Ok(());
                }
                write_federated(&build, &root)?;
                synaptic_workspace::state::record_state(&root, &build)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                print_build_summary(&build);
            }
            Ok(())
        }
        WorkspaceAction::Federate { dir } => {
            let build = federate_artifacts(&dir).map_err(|e| anyhow::anyhow!("{e}"))?;
            write_federated(&build, &root)?;
            print_build_summary(&build);
            Ok(())
        }
        WorkspaceAction::Sync => {
            use synaptic_workspace::manifest::load_manifest;
            if let Some(m) = load_manifest(&root).map_err(|e| anyhow::anyhow!("{e}"))? {
                let cache = root.join("synaptic-out").join("workspace-repos");
                for repo in &m.repos {
                    if repo.git.is_some() {
                        let clone = cache.join(synaptic_workspace::sanitize_tag(&repo.name));
                        if clone.is_dir() {
                            println!("Pulling {}...", repo.name);
                            let _ = std::process::Command::new("git")
                                .arg("-C")
                                .arg(&clone)
                                .arg("pull")
                                .status();
                        }
                    }
                }
            }
            let outcome = synaptic_workspace::state::update_workspace(
                &root,
                &WorkspaceBuildOptions::default(),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            match outcome.build {
                Some(build) => {
                    write_federated(&build, &root)?;
                    if let Some(state) = &outcome.new_state {
                        synaptic_workspace::state::save_state(&root, state)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                    }
                    print_build_summary(&build);
                }
                None => println!("No changes — federated graph is up to date."),
            }
            Ok(())
        }
        WorkspaceAction::Status => {
            let (statuses, has_remote) = synaptic_workspace::state::workspace_status(&root)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if statuses.is_empty() {
                println!("No members found (run `synaptic workspace init`?).");
            }
            for s in &statuses {
                println!(
                    "  {} — {}",
                    s.tag,
                    if s.changed { "changed" } else { "unchanged" }
                );
            }
            if has_remote {
                println!("note: remote [[repos]] present — `build --changed` forces a rebuild.");
            }
            Ok(())
        }
        WorkspaceAction::List => {
            let (members, repos) = resolve_members(&root).map_err(|e| anyhow::anyhow!("{e}"))?;
            let manifest_present = synaptic_workspace::manifest::load_manifest(&root)
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .is_some();
            if !manifest_present && !members.is_empty() {
                println!(
                    "(no workspace build-file — discovered {} project root(s) by manifest)",
                    members.len()
                );
            }
            println!("Local members ({}):", members.len());
            for m in &members {
                let coord = m
                    .coordinate
                    .as_ref()
                    .map(|c| c.name.as_str())
                    .unwrap_or("-");
                println!("  {} [{}] {}", m.tag, coord, m.path.display());
            }
            if !repos.is_empty() {
                println!("Remote repos ({}):", repos.len());
                for r in &repos {
                    let loc = r
                        .git
                        .as_deref()
                        .or(r.subgraph.as_deref())
                        .or(r.path.as_deref())
                        .unwrap_or("-");
                    println!("  {} {}", r.name, loc);
                }
            }
            Ok(())
        }
        WorkspaceAction::Discover { path, depth, max } => {
            use synaptic_workspace::scan::{discover_sibling_repos, ScanOptions, SkipReason};
            use synaptic_workspace::workspace_build::federate_repos;
            let scan_root = path.map(|p| root.join(p)).unwrap_or_else(|| {
                root.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.clone())
            });
            let scan_root = scan_root.canonicalize().unwrap_or(scan_root);
            let res = discover_sibling_repos(&scan_root, &ScanOptions { depth, max }, Some(&root));
            let print_skipped = |skipped: &[(PathBuf, SkipReason)]| {
                for (p, reason) in skipped {
                    let why = match reason {
                        SkipReason::NoManifest => "no recognized manifest",
                        SkipReason::OverCap => "over --max cap",
                    };
                    println!("  skipped {} ({why})", p.display());
                }
            };
            if res.repos.is_empty() {
                println!("No sibling repos found under {}.", scan_root.display());
                print_skipped(&res.skipped);
                return Ok(());
            }
            let repos: Vec<synaptic_workspace::manifest::RepoMember> = res
                .repos
                .iter()
                .map(|c| synaptic_workspace::manifest::RepoMember {
                    name: c.name.clone(),
                    git: None,
                    rev: None,
                    subgraph: None,
                    path: Some(c.path.to_string_lossy().into_owned()),
                })
                .collect();
            println!(
                "Federating {} discovered repo(s) under {}:",
                repos.len(),
                scan_root.display()
            );
            for c in &res.repos {
                println!("  {} -> {}", c.name, c.path.display());
            }
            print_skipped(&res.skipped);
            let build = federate_repos(&root, &repos, &WorkspaceBuildOptions::default())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            write_federated(&build, &root)?;
            print_build_summary(&build);
            Ok(())
        }
    }
}
