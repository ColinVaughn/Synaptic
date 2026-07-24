//! `synaptic workspace build --watch`: the workspace-scale watcher.
//!
//! The regression these guard is that the single-repo `synaptic watch` is the
//! WRONG tool at a workspace root — it rebuilds the tree as one flat repo and
//! writes an un-namespaced graph over the federated one. The watcher here must
//! keep every rebuild federated: `tag::`-namespaced node ids, per-member
//! surfaces, and the resolved cross-repo edges.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A two-member monorepo where `app` uses a type from `lib` (a cross-repo edge).
fn make_workspace(root: &Path) {
    write(
        root,
        "synaptic-workspace.toml",
        "[workspace]\nname = \"demo\"\nmembers = [\"pkgs/*\"]\n",
    );
    write(root, "pkgs/lib/Cargo.toml", "[package]\nname = \"lib\"\n");
    write(
        root,
        "pkgs/lib/src/lib.rs",
        "pub struct Ledger;\nimpl Ledger { pub fn new() -> Ledger { Ledger } }\n",
    );
    write(root, "pkgs/app/Cargo.toml", "[package]\nname = \"app\"\n");
    write(
        root,
        "pkgs/app/src/lib.rs",
        "use lib::Ledger;\npub fn run() { let _ = Ledger::new(); }\n",
    );
}

fn read_graph(root: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(root.join("synaptic-out").join("graph.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Poll `graph.json` until `pred` holds or the deadline passes. Filesystem event
/// latency varies a lot across platforms, so this waits rather than sleeping a
/// fixed amount.
fn wait_for(root: &Path, secs: u64, pred: impl Fn(&serde_json::Value) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if let Some(g) = read_graph(root) {
            if pred(&g) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn labels(g: &serde_json::Value) -> Vec<String> {
    g["nodes"]
        .as_array()
        .map(|ns| {
            ns.iter()
                .filter_map(|n| n["label"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Every node carries a repo tag and a `tag::`-namespaced id — the property a
/// flat single-repo rebuild destroys.
fn fully_federated(g: &serde_json::Value) -> bool {
    let ns = g["nodes"].as_array().cloned().unwrap_or_default();
    !ns.is_empty()
        && ns.iter().all(|n| {
            n["id"].as_str().is_some_and(|i| i.contains("::")) && n["repo"].as_str().is_some()
        })
}

struct Watcher(Child);

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_watch(root: &Path) -> Watcher {
    Watcher(
        Command::new(env!("CARGO_BIN_EXE_synaptic"))
            .current_dir(root)
            .args([
                "workspace",
                "build",
                "--watch",
                "--no-store",
                "--debounce-ms",
                "300",
            ])
            // Exercise the dropped-event reconciliation path quickly in CI.
            .env("SYNAPTIC_WATCH_RECONCILE_SECS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning the workspace watcher"),
    )
}

#[test]
fn watch_federates_on_startup_then_rebuilds_on_a_member_edit() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    make_workspace(root);

    let _watcher = spawn_watch(root);

    // Startup catch-up federates before any event arrives.
    assert!(
        wait_for(root, 90, |g| {
            fully_federated(g) && labels(g).iter().any(|l| l == "run()")
        }),
        "startup catch-up must write a federated graph"
    );
    // The cross-repo edge app -> lib is resolved.
    let g = read_graph(root).unwrap();
    assert!(
        g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["cross_repo"] == true),
        "federated build resolves the app -> lib cross-repo edge"
    );

    // Edit a member: the watcher must re-federate, not flat-rebuild.
    write(
        root,
        "pkgs/app/src/lib.rs",
        "use lib::Ledger;\npub fn run() { let _ = Ledger::new(); }\npub fn extra() {}\n",
    );
    assert!(
        wait_for(root, 90, |g| labels(g).iter().any(|l| l == "extra()")),
        "an edit inside a member triggers a rebuild"
    );

    let g = read_graph(root).unwrap();
    assert!(
        fully_federated(&g),
        "every node stays repo-tagged and tag::-namespaced after a watch rebuild: {:?}",
        g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].clone())
            .collect::<Vec<_>>()
    );
    assert!(
        g["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["cross_repo"] == true),
        "the cross-repo edge survives a watch rebuild"
    );
    // Per-member surfaces are refreshed alongside the graph.
    assert!(
        root.join("synaptic-out/surfaces/app.json").is_file()
            && root.join("synaptic-out/surfaces/lib.json").is_file(),
        "member export surfaces are written on each cycle"
    );
}

#[test]
fn watch_skips_the_visual_artifact_suite() {
    // A workspace watcher re-federates on every save; regenerating the SVG/3D/
    // HTML suite each time would dominate the cost, so it is opt-in (mirroring
    // `synaptic update`/`watch`).
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    make_workspace(root);

    let _watcher = spawn_watch(root);
    assert!(
        wait_for(root, 90, fully_federated),
        "startup catch-up federates"
    );
    assert!(
        !root.join("synaptic-out/graph.svg").exists()
            && !root.join("synaptic-out/graph-3d.html").exists(),
        "the visual suite is not regenerated without --artifacts"
    );
}

#[test]
fn watch_only_flags_require_watch() {
    for flag in [["--debounce-ms", "500"], ["--artifacts", ""]] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_synaptic"));
        cmd.args(["workspace", "build", flag[0]]);
        if !flag[1].is_empty() {
            cmd.arg(flag[1]);
        }
        let out = cmd.output().unwrap();
        assert!(
            !out.status.success(),
            "{} without --watch must be rejected",
            flag[0]
        );
    }
}

#[test]
fn workspace_build_help_documents_watch() {
    let out = Command::new(env!("CARGO_BIN_EXE_synaptic"))
        .args(["workspace", "build", "--help"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for expected in ["--watch", "--debounce-ms", "--artifacts"] {
        assert!(text.contains(expected), "help mentions {expected}: {text}");
    }
}
