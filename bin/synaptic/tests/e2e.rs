//! End-to-end CLI test: extract a tiny Python corpus, then query it.

use std::fs;

use assert_cmd::Command;

fn write(root: &std::path::Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn extract_then_query_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/analysis.py",
        "def compute_score(data):\n    return sum(data)\n\n\ndef run_analysis(data):\n    return compute_score(data)\n",
    );
    write(root, "README.md", "# Demo\n\nA tiny project.\n");

    // extract
    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph_path = root.join("synaptic-out/graph.json");
    assert!(graph_path.exists(), "graph.json should exist");
    assert!(
        root.join("synaptic-out/graph.html").exists(),
        "graph.html should exist"
    );
    assert!(
        root.join("synaptic-out/GRAPH_REPORT.md").exists(),
        "GRAPH_REPORT.md should exist"
    );
    for f in [
        "graph.graphml",
        "graph.cypher",
        "graph.dot",
        "callflow.html",
        "tree.html",
        "graph.svg",
        "graph-3d.html",
    ] {
        assert!(
            root.join("synaptic-out").join(f).exists(),
            "{f} should be written by default"
        );
    }

    let graph: serde_json::Value = serde_json::from_slice(&fs::read(&graph_path).unwrap()).unwrap();
    let labels: Vec<String> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["label"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        labels.iter().any(|l| l == "run_analysis()"),
        "expected run_analysis() node, got {labels:?}"
    );
    assert!(labels.iter().any(|l| l == "compute_score()"));
    // Function signatures flow end-to-end into graph.json (Track A): the
    // compute_score(data) node carries its parameter.
    let compute = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["label"] == "compute_score()")
        .expect("compute_score node");
    let params = compute["signature"]["params"]
        .as_array()
        .expect("signature params present in graph.json");
    assert_eq!(
        params[0]["name"], "data",
        "captured parameter name reaches graph.json"
    );
    // The intra-file call edge must be present.
    let calls = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["relation"] == "calls");
    assert!(calls, "expected a calls edge");

    // Portability: ids must be root-relative, never embedding the absolute path.
    let ids: Vec<String> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        ids.iter().any(|i| i == "src_analysis_py"),
        "file-node id should be relative; got {ids:?}"
    );
    let tmp_marker = root.file_name().unwrap().to_string_lossy().to_lowercase();
    assert!(
        ids.iter().all(|i| !i.contains(&*tmp_marker)),
        "no id should embed the absolute temp-dir path"
    );

    // query against the produced graph
    let out = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["query", "analysis"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(stdout.contains("Seeds:"), "query output: {stdout}");

    // explain a node by label
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["explain", "run_analysis()"])
        .assert()
        .success();

    // affected: run_analysis() calls compute_score(), so changing compute_score
    // affects run_analysis() via a `calls` edge. Resolve by bare name.
    let aff = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["affected", "compute_score"])
        .assert()
        .success();
    let aff_out = String::from_utf8_lossy(&aff.get_output().stdout).into_owned();
    assert!(
        aff_out.contains("Affected nodes for compute_score()"),
        "affected header: {aff_out}"
    );
    assert!(
        aff_out.contains("run_analysis()") && aff_out.contains("[calls]"),
        "expected run_analysis() reached via calls: {aff_out}"
    );
}

/// Dynamic-dispatch awareness end-to-end: an event bus connects a cross-file
/// publisher/subscriber through one channel node; a literal-key reflection site is
/// evidence-linked to its target; an opaque site makes a 0-dependent symbol's
/// "affected" carry the honesty caveat; and the `hazards` CLI lists the sites.
#[test]
fn dynamic_dispatch_surfaces_in_graph_and_cli() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/bus.ts",
        "import { EventEmitter } from 'events';\nconst bus = new EventEmitter();\nexport function fire() { bus.emit('job:done', 1); }\nexport function wire() { bus.on('job:done', onDone); }\n",
    );
    write(
        root,
        "src/dispatch.ts",
        "const handlers: any = {};\nexport function onDone() { return 1; }\nexport function helper() { return 2; }\nexport function route(name: string) { handlers['onDone'](); return handlers[name](); }\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph_path = root.join("synaptic-out/graph.json");
    let graph: serde_json::Value = serde_json::from_slice(&fs::read(&graph_path).unwrap()).unwrap();
    let nodes = graph["nodes"].as_array().unwrap();

    // The event bus mints one channel node that both fire (emit) and wire (on) meet on.
    assert!(
        nodes.iter().any(|n| n["label"] == "event #job:done"),
        "expected an event #job:done channel node"
    );
    let chan = nodes
        .iter()
        .find(|n| n["label"] == "event #job:done")
        .unwrap();
    let chan_id = chan["id"].as_str().unwrap();
    let links = graph["links"].as_array().unwrap();
    assert!(
        links
            .iter()
            .any(|e| e["target"] == chan_id && e["relation"] == "calls_service"),
        "publisher calls_service the channel"
    );
    assert!(
        links
            .iter()
            .any(|e| e["source"] == chan_id && e["relation"] == "handled_by"),
        "subscriber handled_by the channel"
    );

    // The literal-key site handlers['onDone']() evidence-links to onDone().
    let on_done = nodes
        .iter()
        .find(|n| n["label"] == "onDone()")
        .expect("onDone node");
    assert_eq!(
        on_done["dynamically_referenced"],
        serde_json::json!(true),
        "onDone should be evidence-linked (dynamically_referenced)"
    );
    assert!(
        links
            .iter()
            .any(|e| e["relation"] == "dynamic_ref" && e["target"] == on_done["id"]),
        "a dynamic_ref edge should reach onDone"
    );

    // `hazards` lists the reflection sites in dispatch.ts (keyed + opaque).
    let haz = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .arg("hazards")
        .assert()
        .success();
    let haz_out = String::from_utf8_lossy(&haz.get_output().stdout).into_owned();
    assert!(
        haz_out.contains("dispatch.ts"),
        "hazards lists dispatch.ts sites: {haz_out}"
    );
    assert!(
        haz_out.contains("(opaque)") && haz_out.contains("\"onDone\""),
        "hazards shows both keyed and opaque sites: {haz_out}"
    );

    // `affected` on the orphan `helper` (0 static dependents, but its file has an
    // opaque dynamic site) carries the honesty caveat.
    let aff = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["affected", "helper"])
        .assert()
        .success();
    let aff_out = String::from_utf8_lossy(&aff.get_output().stdout).into_owned();
    assert!(
        aff_out.contains("not provably unused"),
        "affected on an orphan in a reflection file must carry the caveat: {aff_out}"
    );
}

/// .NET project files and Markdown structure flow through a full `extract` into
/// graph.json.
#[test]
fn extract_dotnet_and_markdown_structure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/App/App.csproj",
        "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup>\n  <ItemGroup>\n    <PackageReference Include=\"Serilog\" Version=\"3.1.1\" />\n    <ProjectReference Include=\"..\\Lib\\Lib.csproj\" />\n  </ItemGroup>\n</Project>\n",
    );
    write(
        root,
        "src/Lib/Lib.csproj",
        "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup>\n</Project>\n",
    );
    write(
        root,
        "docs/guide.md",
        "# Guide\n\nintro\n\n## Install\n\n```sh\n# not a heading\nnpm i\n```\n\n## Usage\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph_path = root.join("synaptic-out/graph.json");
    let graph: serde_json::Value = serde_json::from_slice(&fs::read(&graph_path).unwrap()).unwrap();
    let nodes = graph["nodes"].as_array().unwrap();
    let label_type = |label: &str| -> Option<String> {
        nodes
            .iter()
            .find(|n| n["label"] == label)
            .map(|n| n["file_type"].as_str().unwrap_or("").to_string())
    };

    // .NET: TargetFramework is a concept; NuGet package is a node.
    assert_eq!(
        label_type("net8.0").as_deref(),
        Some("concept"),
        "{nodes:?}"
    );
    assert!(
        nodes.iter().any(|n| n["label"] == "Serilog (3.1.1)"),
        "missing NuGet node"
    );
    // The ProjectReference target id equals Lib.csproj's own file-node id, so the
    // two projects are connected (cross-file via shared id).
    let imports_lib =
        graph["links"].as_array().unwrap().iter().any(|e| {
            e["relation"] == "imports" && e["target"].as_str() == Some("src_lib_lib_csproj")
        });
    assert!(
        imports_lib,
        "App.csproj should import Lib.csproj by file id"
    );

    // Markdown: heading nodes are documents; the fenced `# not a heading` is gone.
    assert_eq!(label_type("Guide").as_deref(), Some("document"));
    assert_eq!(label_type("Install").as_deref(), Some("document"));
    assert_eq!(label_type("Usage").as_deref(), Some("document"));
    assert!(
        !nodes.iter().any(|n| n["label"]
            .as_str()
            .is_some_and(|l| l.contains("not a heading"))),
        "fenced code # leaked as a heading"
    );
}

/// `update` re-extracts markdown headings: the structural markdown pass runs in
/// the incremental rebuild, not just `extract`. (The same `rebuild` backs `watch`
/// and `workspace build`.)
#[test]
fn update_reextracts_markdown_headings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.py", "def f():\n    return 1\n");
    write(root, "docs/guide.md", "# Original\n");

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    // Change a heading, then incrementally update just that file.
    write(root, "docs/guide.md", "# Renamed\n\n## Added\n");
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["update", "docs/guide.md"])
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let labels: Vec<String> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["label"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(labels.contains(&"Renamed".to_string()), "{labels:?}");
    assert!(labels.contains(&"Added".to_string()), "{labels:?}");
    assert!(
        !labels.contains(&"Original".to_string()),
        "stale heading: {labels:?}"
    );
}

/// `export` re-emits formats from an existing graph.json without re-extracting
/// (here DOT + a Neo4j cypher script).
#[test]
fn export_reemits_without_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.py", "def f():\n    return 1\n");

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();
    assert!(root.join("synaptic-out/graph.json").exists());

    // Re-emit DOT to a custom path (no re-extraction).
    let dot = root.join("out.dot");
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["export", "dot", "--out", dot.to_str().unwrap()])
        .assert()
        .success();
    let dot_text = fs::read_to_string(&dot).unwrap();
    assert!(dot_text.contains("Synaptic {"), "dot: {dot_text}");

    // `export neo4j` without --push writes a cypher import script.
    let cyp = root.join("import.cypher");
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["export", "neo4j", "--out", cyp.to_str().unwrap()])
        .assert()
        .success();
    assert!(fs::read_to_string(&cyp).unwrap().contains("MERGE"));

    // `export report` regenerates GRAPH_REPORT.md (recomputes communities+analysis).
    let rep = root.join("report.md");
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["export", "report", "--out", rep.to_str().unwrap()])
        .assert()
        .success();
    assert!(
        fs::read_to_string(&rep).unwrap().contains("# "),
        "report has headings"
    );
}

#[test]
fn query_accepts_dfs_flag() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/analysis.py",
        "def compute_score(data):\n    return sum(data)\n\n\ndef run_analysis(data):\n    return compute_score(data)\n",
    );
    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    // The --dfs flag is accepted and produces a subgraph (depth-first traversal).
    let out = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["query", "analysis", "--dfs"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(stdout.contains("Seeds:"), "query --dfs output: {stdout}");
}

#[test]
fn update_incrementally_reflects_a_changed_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a.py", "def a():\n    return 1\n");
    write(root, "b.py", "def b():\n    return 2\n");

    // Initial full extract.
    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    // Change a.py (add c()); leave b.py untouched. Run incremental update.
    write(
        root,
        "a.py",
        "def a():\n    return 1\n\n\ndef c():\n    return 3\n",
    );
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["update", "a.py"])
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let labels: Vec<String> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["label"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        labels.iter().any(|l| l == "c()"),
        "new fn after update: {labels:?}"
    );
    assert!(labels.iter().any(|l| l == "a()"));
    assert!(
        labels.iter().any(|l| l == "b()"),
        "unchanged b.py preserved through incremental update: {labels:?}"
    );
}

#[test]
fn ingest_cargo_merges_crate_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(
        root,
        "crates/a/Cargo.toml",
        "[package]\nname = \"a\"\n[dependencies]\nb = { path = \"../b\" }\n",
    );
    write(root, "crates/a/src/lib.rs", "pub fn a() {}\n");
    write(root, "crates/b/Cargo.toml", "[package]\nname = \"b\"\n");
    write(root, "crates/b/src/lib.rs", "pub fn b() {}\n");

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["ingest", "cargo", "."])
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let ids: Vec<&str> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        ids.contains(&"crate:a") && ids.contains(&"crate:b"),
        "{ids:?}"
    );
    let dep = graph["links"].as_array().unwrap().iter().any(|e| {
        e["relation"] == "crate_depends_on" && e["source"] == "crate:a" && e["target"] == "crate:b"
    });
    assert!(dep, "expected crate:a --crate_depends_on--> crate:b");
}

#[test]
fn ingest_scip_merges_symbol_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/m.py", "def f():\n    return 1\n");
    // A simplified SCIP index: A references B (same file) and an external C.
    write(
        root,
        "index.scip.json",
        r#"{"documents":[{"relative_path":"src/m.py","symbols":[
            {"symbol":"m#A","display_name":"A","relationships":[
                {"symbol":"m#B","is_reference":true},
                {"symbol":"ext#C","is_implementation":true}]},
            {"symbol":"m#B","display_name":"B"}]}]}"#,
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["ingest", "scip", "index.scip.json"])
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let labels: Vec<&str> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["label"].as_str().unwrap_or(""))
        .collect();
    assert!(labels.contains(&"A") && labels.contains(&"B"), "{labels:?}");
    // The external relationship target is stubbed (label "C") so its edge survives.
    assert!(labels.contains(&"C"), "external stub expected: {labels:?}");
    let rels: Vec<&str> = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["relation"].as_str().unwrap_or(""))
        .collect();
    assert!(rels.contains(&"scip_ref"), "{rels:?}");
    assert!(rels.contains(&"scip_impl"), "{rels:?}");
}

#[test]
#[cfg(not(feature = "pg"))]
fn ingest_pg_without_feature_errors_clearly() {
    // Default builds omit the postgres client; the subcommand stays visible but
    // explains how to enable it instead of silently doing nothing.
    let dir = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(dir.path())
        .args(["ingest", "pg", "postgresql://localhost/db"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(stderr.contains("--features pg"), "hint expected: {stderr}");
}

#[test]
fn install_then_uninstall_skill() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["install", "claude"])
        .assert()
        .success();
    assert!(root.join(".claude/skills/synaptic/SKILL.md").exists());
    let claude = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert!(
        claude.contains("## Synaptic"),
        "always-on section: {claude}"
    );
    // Installing Claude also registers PreToolUse hooks in .claude/settings.json.
    let settings = fs::read_to_string(root.join(".claude/settings.json")).unwrap();
    assert!(
        settings.contains("PreToolUse") && settings.contains("synaptic-out/graph.json"),
        "settings hooks: {settings}"
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["uninstall", "claude"])
        .assert()
        .success();
    assert!(!root.join(".claude/skills/synaptic/SKILL.md").exists());
    // The PreToolUse hooks are removed too (settings.json may remain, hooks gone).
    if let Ok(after) = fs::read_to_string(root.join(".claude/settings.json")) {
        assert!(
            !after.contains("synaptic-out/graph.json"),
            "hooks removed: {after}"
        );
    }
}

#[test]
fn install_then_uninstall_codex() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["install", "codex"])
        .assert()
        .success();
    // AGENTS.md always-on block...
    let agents = fs::read_to_string(root.join("AGENTS.md")).unwrap();
    assert!(agents.contains("## Synaptic"), "always-on: {agents}");
    // ...plus the Codex-native MCP server, hook, and helper script.
    let config = fs::read_to_string(root.join(".codex/config.toml")).unwrap();
    assert!(
        config.contains("[mcp_servers.synaptic]") && config.contains("serve"),
        "mcp server: {config}"
    );
    let hooks = fs::read_to_string(root.join(".codex/hooks.json")).unwrap();
    assert!(
        hooks.contains("SessionStart") && hooks.contains("synaptic-hook.py"),
        "hook: {hooks}"
    );
    assert!(root.join(".codex/synaptic-hook.py").exists());

    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["uninstall", "codex"])
        .assert()
        .success();
    assert!(
        !root.join(".codex/config.toml").exists(),
        "mcp config removed"
    );
    assert!(!root.join(".codex/hooks.json").exists(), "hooks removed");
    assert!(
        !root.join(".codex/synaptic-hook.py").exists(),
        "script removed"
    );
}

#[test]
fn install_codex_global_writes_to_codex_home() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let codex_home = dir.path().join("codexhome");

    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(&root)
        .env("CODEX_HOME", &codex_home)
        .args(["install", "codex", "--global"])
        .assert()
        .success();

    // MCP server lands in the GLOBAL config (named per-repo), with an absolute --graph.
    let cfg = fs::read_to_string(codex_home.join("config.toml")).unwrap();
    assert!(
        cfg.contains("synaptic-repo") && cfg.contains("serve") && cfg.contains("--graph"),
        "global config: {cfg}"
    );
    // AGENTS.md block is written; no project .codex/ files in global mode.
    assert!(root.join("AGENTS.md").exists());
    assert!(
        !root.join(".codex/hooks.json").exists(),
        "no project hook in global mode"
    );
    assert!(
        !root.join(".codex/config.toml").exists(),
        "no project config in global mode"
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(&root)
        .env("CODEX_HOME", &codex_home)
        .args(["uninstall", "codex", "--global"])
        .assert()
        .success();
    assert!(
        !codex_home.join("config.toml").exists(),
        "global entry removed (file was only ours)"
    );
}

#[test]
fn install_global_rejects_non_codex() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(dir.path())
        .args(["install", "gemini", "--global"])
        .assert()
        .failure();
}

#[test]
fn uninstall_all_with_global_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(dir.path())
        .args(["uninstall", "--all", "--global"])
        .assert()
        .failure();
}

#[test]
fn serve_answers_mcp_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/analysis.py",
        "def compute_score(data):\n    return sum(data)\n\n\ndef run_analysis(data):\n    return compute_score(data)\n",
    );
    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    // Drive the MCP server over stdio with two JSON-RPC requests; it reads to
    // EOF then exits.
    let stdin = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"graph_stats","arguments":{}}}"#,
        "\n",
    );
    let out = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .arg("serve")
        .write_stdin(stdin)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(
        stdout.contains("\"serverInfo\""),
        "initialize reply: {stdout}"
    );
    assert!(stdout.contains("query_graph"), "tools/list reply: {stdout}");
    assert!(stdout.contains("nodes"), "graph_stats reply: {stdout}");
}

#[test]
fn cgo_binds_native_edge_reaches_graph_json() {
    // Cross-language (FFI) edge: a cgo `C.sqrt()` call must survive the build into
    // graph.json as a `binds_native` edge to a native target stub.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "main.go",
        "package main\n\n// #include <math.h>\nimport \"C\"\n\nfunc Compute() float64 {\n\treturn float64(C.sqrt(4))\n}\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    assert!(
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["label"] == "C.sqrt"),
        "native target node missing: {:?}",
        graph["nodes"]
    );
    let has = graph["links"].as_array().unwrap().iter().any(|e| {
        e["relation"] == "binds_native" && e["confidence"] == "INFERRED" && e["context"] == "cgo"
    });
    assert!(
        has,
        "expected a binds_native edge in graph.json; links: {:?}",
        graph["links"]
    );
}

#[test]
fn http_client_connects_to_route_handler() {
    // A client call and a server handler in different files meet at a shared,
    // path-keyed route node, so impact traverses the HTTP boundary.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "server.py",
        "from flask import Flask\napp = Flask(__name__)\n\n@app.get(\"/api/users\")\ndef list_users():\n    return []\n",
    );
    write(
        root,
        "client.py",
        "import requests\n\ndef load():\n    return requests.get(\"http://svc/api/users\").json()\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let id_of = |label: &str| -> String {
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["label"] == label)
            .map(|n| n["id"].as_str().unwrap().to_string())
            .unwrap_or_default()
    };
    let route = id_of("/api/users");
    assert!(!route.is_empty(), "shared route node present");
    let links = graph["links"].as_array().unwrap();
    // route -> handler (handled_by) and client -> route (calls_service): the
    // dependency chain client -> route -> handler.
    let handled_by = links.iter().any(|e| {
        e["relation"] == "handled_by"
            && e["source"] == serde_json::json!(route)
            && e["target"] == serde_json::json!(id_of("list_users()"))
    });
    let calls = links.iter().any(|e| {
        e["relation"] == "calls_service"
            && e["source"] == serde_json::json!(id_of("load()"))
            && e["target"] == serde_json::json!(route)
    });
    assert!(handled_by, "route -> handler edge; links: {links:?}");
    assert!(calls, "client -> route edge; links: {links:?}");

    // Reverse-impact crosses the HTTP boundary: changing the handler reaches the
    // client through route -> handler + client -> route.
    let aff = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["affected", "list_users"])
        .assert()
        .success();
    let aff_out = String::from_utf8_lossy(&aff.get_output().stdout).into_owned();
    assert!(
        aff_out.contains("load()"),
        "affected(handler) should reach the HTTP client load(): {aff_out}"
    );
}

#[test]
fn eval_cross_language_calibrates_a_built_graph() {
    // A two-sided HTTP route (server + client) should calibrate as one fully
    // connected service boundary.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "server.py",
        "from flask import Flask\napp = Flask(__name__)\n\n@app.get(\"/api/users\")\ndef list_users():\n    return []\n",
    );
    write(
        root,
        "client.py",
        "import requests\n\ndef load():\n    return requests.get(\"http://svc/api/users\").json()\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let out = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["eval", "cross-language", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert!(
        report["total_edges"].as_u64().unwrap() >= 2,
        "report: {report}"
    );
    assert_eq!(report["service_boundaries"], 1, "report: {report}");
    assert_eq!(
        report["service_two_sided"], 1,
        "the /api/users route is two-sided: {report}"
    );
}

#[test]
fn axum_handler_resolved_across_files() {
    // The router in app.rs references a handler by a qualified name; the handler is
    // defined in handlers.rs. The cross-file resolver links the route to the
    // handler, so a client calling the path reaches it across the file boundary.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/handlers.rs",
        "pub async fn serve() -> String {\n    String::new()\n}\n",
    );
    write(
        root,
        "src/app.rs",
        "use axum::routing::get;\nmod handlers;\nfn app() -> Router {\n    Router::new().route(\"/api/x\", get(handlers::serve))\n}\n",
    );
    write(
        root,
        "client.py",
        "import requests\n\ndef call():\n    return requests.get(\"http://svc/api/x\").json()\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let id_of = |label: &str| -> String {
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["label"] == label)
            .map(|n| n["id"].as_str().unwrap().to_string())
            .unwrap_or_default()
    };
    let route = id_of("/api/x");
    let serve = id_of("serve()");
    assert!(
        !route.is_empty() && !serve.is_empty(),
        "route + handler nodes"
    );
    let linked = graph["links"].as_array().unwrap().iter().any(|e| {
        e["relation"] == "handled_by"
            && e["source"] == serde_json::json!(route)
            && e["target"] == serde_json::json!(serve)
    });
    assert!(
        linked,
        "route -> cross-file handler edge; links: {:?}",
        graph["links"]
    );

    // Impact crosses both boundaries: serve <- route <- client call().
    let aff = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["affected", "serve"])
        .assert()
        .success();
    let aff_out = String::from_utf8_lossy(&aff.get_output().stdout).into_owned();
    assert!(
        aff_out.contains("call()"),
        "affected(serve) reaches the client across files: {aff_out}"
    );
}

#[test]
fn pyo3_cross_file_module_connects_to_python_importer() {
    // The #[pyfunction] lives in ops.rs; the #[pymodule] that registers it (by a
    // qualified `wrap_pyfunction!(ops::add, ..)`) lives in lib.rs; a Python file
    // imports the module. The graph-level stitch links the module boundary to the
    // cross-file function, so impact crosses from the Rust impl all the way to the
    // Python importer.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/ops.rs",
        "use pyo3::prelude::*;\n\n#[pyfunction]\npub fn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n",
    );
    write(
        root,
        "src/lib.rs",
        "use pyo3::prelude::*;\nmod ops;\n\n#[pymodule]\nfn mathmod(_py: Python<'_>, m: &PyModule) -> PyResult<()> {\n    m.add_function(wrap_pyfunction!(ops::add, m)?)?;\n    Ok(())\n}\n",
    );
    write(
        root,
        "app.py",
        "import mathmod\n\ndef run():\n    return mathmod.add(1, 2)\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    assert!(
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["label"] == "pyo3:mathmod"),
        "module boundary present"
    );

    let aff = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["affected", "add"])
        .assert()
        .success();
    let aff_out = String::from_utf8_lossy(&aff.get_output().stdout).into_owned();
    assert!(
        aff_out.contains("app.py"),
        "affected(add) reaches the Python importer across files: {aff_out}"
    );
}

#[test]
fn pyo3_export_connects_to_python_importer() {
    // A Rust #[pymodule]/#[pyfunction] and a Python file importing that module
    // connect at graph build, so impact crosses from the Rust impl to the Python
    // file that depends on it.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "use pyo3::prelude::*;\n\n#[pyfunction]\nfn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n\n#[pymodule]\nfn mathmod(_py: Python<'_>, m: &PyModule) -> PyResult<()> {\n    m.add_function(wrap_pyfunction!(add, m)?)?;\n    Ok(())\n}\n",
    );
    write(
        root,
        "app.py",
        "import mathmod\n\ndef run():\n    return mathmod.add(1, 2)\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    assert!(
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["label"] == "pyo3:mathmod"),
        "pyo3 module boundary present"
    );

    // Reverse-impact from the Rust export reaches the importing Python file:
    // boundary handled_by add, importer calls_service boundary.
    let aff = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["affected", "add"])
        .assert()
        .success();
    let aff_out = String::from_utf8_lossy(&aff.get_output().stdout).into_owned();
    assert!(
        aff_out.contains("app.py"),
        "affected(add) should reach the Python importer app.py: {aff_out}"
    );
}

#[test]
fn parameterized_route_connects_concrete_client_call() {
    // A server route template /users/<int:uid> and a client call to the concrete
    // /users/42 are merged at graph build, so impact crosses the HTTP boundary
    // despite the path-parameter mismatch.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "server.py",
        "from flask import Flask\napp = Flask(__name__)\n\n@app.get(\"/users/<int:uid>\")\ndef get_user(uid):\n    return {}\n",
    );
    write(
        root,
        "client.py",
        "import requests\n\ndef load():\n    return requests.get(\"http://svc/users/42\").json()\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let nodes = graph["nodes"].as_array().unwrap();
    assert!(
        nodes.iter().any(|n| n["label"] == "/users/<int:uid>"),
        "template route present"
    );
    assert!(
        !nodes.iter().any(|n| n["label"] == "/users/42"),
        "concrete client route merged into the template"
    );

    // Reverse-impact crosses the boundary: the concrete client call was retargeted
    // to the template route, which is handled_by the parameterized handler.
    let aff = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["affected", "get_user"])
        .assert()
        .success();
    let aff_out = String::from_utf8_lossy(&aff.get_output().stdout).into_owned();
    assert!(
        aff_out.contains("load()"),
        "affected(handler) should reach the client via the merged route: {aff_out}"
    );
}

#[test]
fn update_resolves_subprocess_command_incrementally() {
    // The command-resolution pass must run on the incremental `update` path too,
    // not only one-shot `extract`, so the headline edge does not degrade.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "deploy.py", "def deploy():\n    return 1\n");
    write(root, "src/bin/tool.rs", "fn main() {}\n");

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    // Add a subprocess call, then update just that file.
    write(
        root,
        "deploy.py",
        "import subprocess\n\ndef deploy():\n    subprocess.run([\"tool\"])\n",
    );
    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["update", "deploy.py"])
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let tool_id = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["label"] == "tool.rs")
        .map(|n| n["id"].as_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(!tool_id.is_empty(), "rust binary file node present");
    let resolved = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["relation"] == "invokes" && e["target"].as_str() == Some(tool_id.as_str()));
    assert!(
        resolved,
        "after `update`, subprocess command should resolve to tool.rs; links: {:?}",
        graph["links"]
    );
}

#[test]
fn python_subprocess_resolves_to_rust_binary() {
    // The headline cross-language case: a Python script invoking a Rust binary by
    // name resolves to that binary's source file via an `invokes` edge.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "deploy.py",
        "import subprocess\n\ndef deploy():\n    subprocess.run([\"mybinary\", \"--release\"])\n",
    );
    write(
        root,
        "src/bin/mybinary.rs",
        "fn main() {\n    println!(\"hi\");\n}\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let id_of = |label: &str| -> String {
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["label"] == label)
            .map(|n| n["id"].as_str().unwrap().to_string())
            .unwrap_or_default()
    };
    let bin_id = id_of("mybinary.rs");
    assert!(!bin_id.is_empty(), "rust binary file node present");
    // The command stub was resolved away to the real binary source file.
    let resolved = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["relation"] == "invokes" && e["target"].as_str() == Some(bin_id.as_str()));
    assert!(
        resolved,
        "expected a cross-language invokes edge to mybinary.rs; links: {:?}",
        graph["links"]
    );
}

#[test]
fn cross_file_calls_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // import-guided: main.py imports transform from helper and calls it.
    write(root, "helper.py", "def transform(x):\n    return x * 2\n");
    write(
        root,
        "main.py",
        "from helper import transform\n\n\ndef run(d):\n    return transform(d)\n",
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let id_of = |label: &str| -> String {
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["label"] == label)
            .map(|n| n["id"].as_str().unwrap().to_string())
            .unwrap_or_default()
    };
    let run_id = id_of("run()");
    let transform_id = id_of("transform()");
    // A cross-file `calls` edge run() -> transform() must exist (import-guided,
    // EXTRACTED); the two live in different files so only resolution links them.
    let resolved = graph["links"].as_array().unwrap().iter().any(|e| {
        e["relation"] == "calls"
            && e["source"] == serde_json::json!(run_id)
            && e["target"] == serde_json::json!(transform_id)
            && e["confidence"] == "EXTRACTED"
            && e["context"] == "import_guided_call"
    });
    assert!(
        resolved,
        "expected import-guided cross-file calls edge; links: {:?}",
        graph["links"]
    );
}

#[test]
fn extract_is_deterministic_and_caches_asts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a.py", "def a():\n    return b()\n");
    write(root, "pkg/b.py", "def b():\n    return 1\n");
    write(
        root,
        "app.ts",
        "class C { run() { return helper(); } }\nfunction helper() { return 1; }\n",
    );

    let run = || {
        Command::cargo_bin("synaptic")
            .unwrap()
            .arg("extract")
            .arg(root)
            .assert()
            .success();
        fs::read(root.join("synaptic-out/graph.json")).unwrap()
    };

    let first = run();
    // The AST cache was populated under synaptic-out/cache/ast/v{version}/
    // (namespaced by crate version so extractor changes auto-invalidate).
    let cache_ver = root.join(format!(
        "synaptic-out/cache/ast/v{}",
        synaptic_extract::AST_CACHE_VERSION
    ));
    assert!(cache_ver.is_dir(), "AST cache dir should exist");
    let cached = fs::read_dir(&cache_ver).unwrap().count();
    assert!(cached >= 3, "expected >= 3 cached ASTs, got {cached}");

    // Re-running (now hitting the cache, parallel) yields a byte-identical graph.
    let second = run();
    assert_eq!(
        first, second,
        "graph.json must be deterministic across runs (cache + parallel extraction)"
    );
}

#[test]
fn workspace_build_federates_a_cargo_monorepo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A Cargo monorepo: crate `app` `use`s a type published by crate `lib`.
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

    let out = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["workspace", "build"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(stdout.contains("Federated graph:"), "{stdout}");

    let graph: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("synaptic-out/graph.json")).unwrap()).unwrap();
    let nodes = graph["nodes"].as_array().unwrap();
    // Nodes are namespaced + repo-tagged.
    let repos: std::collections::BTreeSet<&str> =
        nodes.iter().filter_map(|n| n["repo"].as_str()).collect();
    assert!(repos.contains("app") && repos.contains("lib"), "{repos:?}");
    assert!(nodes
        .iter()
        .any(|n| n["id"].as_str().unwrap_or("").starts_with("lib::")));
    // A cross-repo edge from app into lib exists.
    let cross = graph["links"].as_array().unwrap().iter().any(|e| {
        e["cross_repo"].as_bool().unwrap_or(false)
            && e["source"].as_str().unwrap_or("").starts_with("app::")
            && e["target"].as_str().unwrap_or("").starts_with("lib::")
    });
    assert!(cross, "expected an app→lib cross_repo edge");

    // Per-member export surfaces were published.
    assert!(root.join("synaptic-out/surfaces/lib.json").exists());

    // `query --repo lib` scopes to the lib member only.
    let q = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["query", "Ledger", "--repo", "lib"])
        .assert()
        .success();
    let qout = String::from_utf8_lossy(&q.get_output().stdout).into_owned();
    assert!(
        qout.contains("Seeds:") || qout.contains("No matches"),
        "{qout}"
    );

    // `workspace status` after a build reports members unchanged.
    let st = Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args(["workspace", "status"])
        .assert()
        .success();
    let stout = String::from_utf8_lossy(&st.get_output().stdout).into_owned();
    assert!(stout.contains("unchanged"), "status after build: {stout}");
}

#[test]
fn merge_graphs_namespaces_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Two single-repo graphs in <repo>/synaptic-out/graph.json layout.
    let g = |id: &str| {
        serde_json::json!({
            "nodes": [{"id": id, "label": id, "file_type": "code", "source_file": format!("{id}.rs")}],
            "links": [], "hyperedges": []
        })
    };
    write(
        root,
        "billing/synaptic-out/graph.json",
        &g("main").to_string(),
    );
    write(
        root,
        "identity/synaptic-out/graph.json",
        &g("main").to_string(),
    );

    Command::cargo_bin("synaptic")
        .unwrap()
        .current_dir(root)
        .args([
            "merge-graphs",
            "billing/synaptic-out/graph.json",
            "identity/synaptic-out/graph.json",
            "--out",
            "merged.json",
        ])
        .assert()
        .success();

    let merged: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("merged.json")).unwrap()).unwrap();
    let ids: Vec<&str> = merged["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"billing::main") && ids.contains(&"identity::main"),
        "{ids:?}"
    );
}
