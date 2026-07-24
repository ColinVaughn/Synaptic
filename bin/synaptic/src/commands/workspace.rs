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

/// Write a federated build's outputs + per-member export surfaces.
///
/// `artifacts` selects the full visual/export suite (HTML, SVG, GraphML, ...);
/// with it off only `graph.json` is written. The watch loop re-federates on
/// every save and nobody reads the visual suite mid-edit, so it opts out —
/// matching why `synaptic update`/`watch` gate the same suite behind
/// `--artifacts`.
pub(crate) fn write_federated(
    build: &synaptic_workspace::workspace_build::WorkspaceBuild,
    root: &Path,
    artifacts: bool,
) -> Result<()> {
    let out_dir = root.join("synaptic-out");
    let extras = if artifacts {
        let analysis = analyze(&build.federated, &build.communities, &BTreeMap::new());
        write_outputs(
            &build.federated,
            &analysis,
            &build.communities,
            &BTreeMap::new(),
            &out_dir,
            false,
            false,
        )?
    } else {
        fs::create_dir_all(&out_dir).context("creating synaptic-out/")?;
        let graph_path = out_dir.join("graph.json");
        synaptic_output::to_json(&build.federated, &graph_path).context("writing graph.json")?;
        crate::commands::common::warn_if_over_caps(&graph_path, build.federated.node_count());
        String::from("graph.json")
    };
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

/// Unless `--no-store` opted out, build the sharded redb store from the federated graph
/// so read commands can use it without a separate `synaptic migrate` step.
fn maybe_write_store(
    store: bool,
    build: &synaptic_workspace::workspace_build::WorkspaceBuild,
    root: &Path,
) -> Result<()> {
    if !store {
        return Ok(());
    }
    let out_dir = root.join("synaptic-out");
    let report = crate::commands::common::write_store(
        &build.federated.to_graph_data(),
        &out_dir.join("store"),
    )?;
    println!(
        "Wrote {}/store ({} shard(s){})",
        out_dir.display(),
        report.shard_tags.len(),
        if report.bridge_edges > 0 {
            format!(", {} bridge edge(s)", report.bridge_edges)
        } else {
            String::new()
        }
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
        "Cross-repo links: {} extracted, {} inferred, {} cross-language · {} external package(s)",
        cr.extracted, cr.inferred, cr.cross_language, cr.external_packages
    );
    for m in &build.members {
        println!(
            "  {} — {} nodes, {} edges",
            m.tag, m.node_count, m.edge_count
        );
    }
}

/// One incremental federated rebuild: skip when no member changed, else build,
/// write artifacts + surfaces (+ store), then persist state.
///
/// The write order is the durability contract `update_workspace` documents —
/// state lands **after** the artifacts, so an interrupted cycle leaves the
/// workspace reading as "changed" and the next run redoes it, rather than
/// stamping "up to date" over outputs that never landed.
///
/// Returns `true` when a rebuild happened.
fn workspace_update_cycle(
    root: &Path,
    opts: &synaptic_workspace::workspace_build::WorkspaceBuildOptions,
    store: bool,
    artifacts: bool,
) -> Result<bool> {
    let outcome = synaptic_workspace::state::update_workspace(root, opts)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(build) = outcome.build else {
        println!("No member changes — federated graph is up to date.");
        return Ok(false);
    };
    if build.members.is_empty() {
        println!("No members found — run `synaptic workspace init` first.");
        return Ok(false);
    }
    write_federated(&build, root, artifacts)?;
    maybe_write_store(store, &build, root)?;
    // Persist state ONLY after artifacts are durably written.
    if let Some(state) = &outcome.new_state {
        synaptic_workspace::state::save_state(root, state).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    print_build_summary(&build);
    if !outcome.surface_changed.is_empty() {
        println!("Surfaces changed: {}", outcome.surface_changed.join(", "));
    }
    Ok(true)
}

/// How long a watch cycle waits for a competing rebuild (a `synaptic update`, a
/// git hook) to release the per-repo lock before giving up on this cycle.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run one watch cycle under the per-repo rebuild lock, so a concurrent
/// `synaptic update` in the same workspace cannot interleave its write with
/// ours. Skipping is safe: `update_workspace` derives what to rebuild from the
/// persisted member hashes, not from our event batch, so a skipped cycle leaves
/// the members still reading as changed and the next cycle covers them.
fn locked_update_cycle(
    root: &Path,
    opts: &synaptic_workspace::workspace_build::WorkspaceBuildOptions,
    store: bool,
    artifacts: bool,
) -> Result<bool> {
    let out_dir = root.join("synaptic-out");
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match synaptic_incremental::try_acquire_lock(&out_dir).context("acquiring rebuild lock")? {
            Some(_guard) => return workspace_update_cycle(root, opts, store, artifacts),
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            None => {
                println!(
                    "Another rebuild holds the lock; skipping this cycle (members stay marked changed and the next save retries)."
                );
                return Ok(false);
            }
        }
    }
}

/// Watch every member repository of a workspace and re-federate on change.
///
/// Registers one recursive watcher per *minimal* watch root — the workspace tree
/// plus each member checked out outside it — so a multi-repo workspace whose
/// members are sibling checkouts is fully covered. Events are filtered by
/// `synaptic_workspace::watch::classify` (detect's own noise rules + the
/// extractable-file set, so writing `synaptic-out/graph.json` cannot
/// self-trigger), batched over a settle window, and drained into one
/// [`workspace_update_cycle`]. Editing `synaptic-workspace.toml` re-resolves the
/// member set and re-registers the watchers, so adding or removing a repository
/// takes effect without a restart.
fn run_workspace_watch(
    root: &Path,
    opts: &synaptic_workspace::workspace_build::WorkspaceBuildOptions,
    store: bool,
    artifacts: bool,
    debounce_ms: Option<u64>,
) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::channel;
    use std::time::Duration;
    use synaptic_incremental::ChangeBatch;
    use synaptic_workspace::watch::{classify, member_for_path, resolve_watch_targets, WatchEvent};

    let debounce = debounce_ms
        .or_else(|| {
            std::env::var("SYNAPTIC_WATCH_DEBOUNCE_MS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(synaptic_incremental::DEBOUNCE_MS);
    // Filesystem notification backends can drop events without emitting a
    // rescan marker (observed with inotify under CI load). Reconcile from the
    // content hashes occasionally while idle so correctness never depends on
    // receiving every OS event. The interval is intentionally much longer than
    // the debounce window to keep idle scanning cheap.
    let reconcile_interval = std::env::var("SYNAPTIC_WATCH_RECONCILE_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(30));

    // Each pass is one watcher generation; a manifest edit ends the inner loop
    // so the member set (and therefore the watch roots) is re-resolved.
    loop {
        let targets = resolve_watch_targets(root).map_err(|e| anyhow::anyhow!("{e}"))?;
        if targets.members.is_empty() {
            println!("No members found — run `synaptic workspace init` first.");
            return Ok(());
        }
        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .context("creating filesystem watcher")?;
        // One unwatchable root (a permission-denied or racing-removed member
        // checkout) must not take the whole watcher down: warn and carry on with
        // the rest. That member still rebuilds, just not event-driven.
        let mut watched: Vec<&PathBuf> = Vec::new();
        for r in &targets.roots {
            match watcher.watch(r, RecursiveMode::Recursive) {
                Ok(()) => watched.push(r),
                Err(e) => eprintln!("warning: not watching {} ({e})", r.display()),
            }
        }
        if watched.is_empty() {
            anyhow::bail!("no watchable member root; nothing to watch");
        }

        // Register first, then catch up. Publishing the initial graph before
        // registration leaves a race where a save made immediately after
        // startup can occur in the gap and never produce an event. Repeating
        // the catch-up for each generation also covers members added by a
        // manifest edit without opening the same gap while re-resolving.
        if let Err(e) = locked_update_cycle(root, opts, store, artifacts) {
            eprintln!("watch catch-up failed: {e}");
        }

        println!(
            "Watching {} member(s) across {} root(s) (debounce {debounce}ms; Ctrl-C to stop):",
            targets.members.len(),
            watched.len()
        );
        for r in &watched {
            println!("  {}", r.display());
        }

        // Block until the first change, then drain a quiet window to batch a
        // burst. An idle timeout reconciles content hashes as a safety net for
        // dropped OS events. Ends when the watcher is dropped (channel closed)
        // or the manifest changes (re-resolve).
        let mut manifest_changed = false;
        loop {
            let first = match rx.recv_timeout(reconcile_interval) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    match resolve_watch_targets(root) {
                        Ok(refreshed) if refreshed != targets => {
                            println!("\nWorkspace membership changed → re-resolving members…");
                            manifest_changed = true;
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("watch reconciliation failed: {e}"),
                    }
                    if let Err(e) = locked_update_cycle(root, opts, store, artifacts) {
                        eprintln!("reconciliation rebuild failed: {e}");
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let mut batch = ChangeBatch::new();
            let mut touched: std::collections::BTreeSet<String> = Default::default();
            let mut rescan = false;
            let record = |res: notify::Result<notify::Event>,
                          batch: &mut ChangeBatch,
                          touched: &mut std::collections::BTreeSet<String>,
                          rescan: &mut bool,
                          manifest_changed: &mut bool| {
                match res {
                    Ok(ev) => {
                        // A rescan notice means events were dropped (buffer
                        // overflow on a huge change): fall back to a full
                        // member-hash comparison rather than trusting the batch.
                        if ev.need_rescan() {
                            *rescan = true;
                            return;
                        }
                        for p in ev.paths {
                            // Classify against the watch root the path lies
                            // under, so ignore rules apply to member-relative
                            // paths (a checkout under /build/app stays watched).
                            let Some(wr) = targets.roots.iter().find(|r| p.starts_with(r)) else {
                                continue;
                            };
                            match classify(&p, wr) {
                                Some(WatchEvent::Manifest) => *manifest_changed = true,
                                Some(WatchEvent::Source(rel)) => {
                                    if let Some(tag) = member_for_path(&p, &targets.members) {
                                        touched.insert(tag.to_string());
                                    }
                                    batch.record(rel);
                                }
                                None => {}
                            }
                        }
                    }
                    Err(_) => *rescan = true,
                }
            };
            record(
                first,
                &mut batch,
                &mut touched,
                &mut rescan,
                &mut manifest_changed,
            );
            while let Ok(ev) = rx.recv_timeout(Duration::from_millis(debounce)) {
                record(
                    ev,
                    &mut batch,
                    &mut touched,
                    &mut rescan,
                    &mut manifest_changed,
                );
            }

            if manifest_changed {
                println!("\nWorkspace manifest changed → re-resolving members…");
                break;
            }
            if rescan {
                println!("\nWatcher lost events → re-checking every member…");
            } else if batch.is_empty() {
                continue; // burst was all ignored / non-extractable files
            } else {
                let who = if touched.is_empty() {
                    "unattributed".to_string()
                } else {
                    touched.iter().cloned().collect::<Vec<_>>().join(", ")
                };
                println!(
                    "\n{} changed file(s) in {} → re-federating…",
                    batch.len(),
                    who
                );
            }
            // One member failing must not stop the watcher: `update_workspace`
            // names the member in its error, so report and keep watching.
            if let Err(e) = locked_update_cycle(root, opts, store, artifacts) {
                eprintln!("rebuild failed: {e}");
            }
        }
        if !manifest_changed {
            // The channel closed (watcher dropped): nothing left to wait on.
            println!("Watcher stopped.");
            return Ok(());
        }
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
        WorkspaceAction::Build {
            changed,
            watch,
            debounce_ms,
            artifacts,
            directed,
            store: _,
            no_store,
        } => {
            let store = !no_store;
            let opts = WorkspaceBuildOptions {
                directed,
                force: false,
            };
            if watch {
                // --watch implies --changed: every cycle is an incremental
                // federated update.
                return run_workspace_watch(&root, &opts, store, artifacts, debounce_ms);
            }
            if changed {
                workspace_update_cycle(&root, &opts, store, true)?;
            } else {
                let build = build_workspace(&root, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;
                if build.members.is_empty() {
                    println!("No members found — run `synaptic workspace init` first.");
                    return Ok(());
                }
                write_federated(&build, &root, true)?;
                maybe_write_store(store, &build, &root)?;
                synaptic_workspace::state::record_state(&root, &build)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                print_build_summary(&build);
            }
            Ok(())
        }
        WorkspaceAction::Federate { dir } => {
            let build = federate_artifacts(&dir).map_err(|e| anyhow::anyhow!("{e}"))?;
            write_federated(&build, &root, true)?;
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
                    write_federated(&build, &root, true)?;
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
            write_federated(&build, &root, true)?;
            print_build_summary(&build);
            Ok(())
        }
    }
}
