//! Acceptance tests (spec 7.4.7 + 7.6): plan a rename across several files with an
//! ambiguous name, then verify a correctly-applied plan passes and a broken one
//! fails on the references-preserved invariant.
//!
//! Uses the Python extractor (`synaptic-extract`'s `lang-python`, on by default).

use std::path::Path;

use synaptic_graph::KnowledgeGraph;
use synaptic_incremental::{rebuild, ChangeSet, RebuildOptions};
use synaptic_refactor::{
    plan_relocate, plan_rename, verify_plan, verify_relocate, RenameOptions, RenamePlan,
};

fn build(root: &Path) -> KnowledgeGraph {
    rebuild(
        &RebuildOptions {
            root: root.to_path_buf(),
            directed: true,
            force: true,
        },
        &ChangeSet::Full,
        None,
    )
    .expect("rebuild")
    .kg
}

/// models.py defines the target `User`; service.py imports + calls it; other.py
/// has an unrelated `class User` that makes the name ambiguous.
fn write_fixture(root: &Path) {
    std::fs::write(
        root.join("models.py"),
        b"class User:\n    def __init__(self):\n        self.name = 1\n",
    )
    .unwrap();
    std::fs::write(
        root.join("service.py"),
        b"from models import User\n\n\ndef make():\n    return User()\n",
    )
    .unwrap();
    std::fs::write(
        root.join("other.py"),
        b"class User:\n    def role(self):\n        return 2\n",
    )
    .unwrap();
}

#[test]
fn plan_enumerates_sites_and_routes_ambiguous_to_review() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fixture(root);
    let kg = build(root);

    let opts = RenameOptions {
        file: Some("models.py".into()),
        ..Default::default()
    };
    let plan = plan_rename(&kg, "User", "Account", root, &opts).expect("plan");

    // The name is ambiguous (two `class User`), disambiguated to models.py.
    assert!(plan.ambiguous_target, "two User definitions => ambiguous");
    assert!(plan.target.file.ends_with("models.py"));
    assert_eq!(plan.candidates.len(), 2);

    // Definition site is present and high-confidence (it lands in edits).
    let def = plan
        .edits
        .iter()
        .find(|s| s.reason == "definition")
        .expect("definition edit present");
    // Its span is backfilled to the precise name token: a single-line range, not
    // the coarse whole-definition block the node span carries.
    let sp = def.span.expect("definition has a span");
    assert_eq!(
        sp.start_line, sp.end_line,
        "definition span is the name token (single line), got {sp:?}"
    );

    // At least the definition + one reference were found.
    assert!(
        plan.blast_radius.edit_count >= 2,
        "expected >=2 sites, got {}",
        plan.blast_radius.edit_count
    );

    // Ambiguous-name call sites are routed to review (never silently in edits).
    assert!(
        plan.review.iter().any(|s| s.reason == "call site"),
        "ambiguous call site routed to review: review={:?}",
        plan.review
    );
}

#[test]
fn plan_rename_surfaces_module_level_importer() {
    // A symbol whose only reference from another file is a module-level
    // `imports_from` edge to a stub (the symbol-level walk misses it). plan_rename
    // must still surface that importer as a review site and in the blast radius,
    // not silently report zero. Graph-only (scan_text off) to isolate the path.
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};

    let mut imp = Edge {
        source: NodeId("testfile".into()),
        target: NodeId("stub".into()),
        relation: "imports_from".into(),
        confidence: Confidence::Extracted,
        source_file: "src/darkMode.test.ts".into(),
        source_location: Some("L1".into()),
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: Map::new(),
    };
    imp.extra
        .insert("imported".into(), serde_json::json!(["toggleDarkMode"]));
    let mk = |id: &str, label: &str, file: &str| Node {
        id: NodeId(id.into()),
        label: label.into(),
        file_type: FileType::Code,
        source_file: file.into(),
        source_location: Some("L1".into()),
        community: None,
        repo: None,
        extra: Map::new(),
    };
    let kg = KnowledgeGraph::from_graph_data(GraphData {
        directed: true,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![
            mk("toggle", "toggleDarkMode", "src/darkMode.ts"),
            mk("testfile", "darkMode.test.ts", "src/darkMode.test.ts"),
            mk("stub", "./darkMode", ""),
        ],
        links: vec![imp],
        hyperedges: vec![],
        built_at_commit: None,
    });

    let dir = tempfile::tempdir().unwrap();
    let opts = RenameOptions {
        scan_text: false,
        ..Default::default()
    };
    let plan = plan_rename(&kg, "toggleDarkMode", "toggleTheme", dir.path(), &opts).expect("plan");

    let all_sites = plan.edits.iter().chain(plan.review.iter());
    assert!(
        all_sites
            .clone()
            .any(|s| s.file.ends_with("darkMode.test.ts")),
        "the module-level test importer must be a site; edits={:?} review={:?}",
        plan.edits,
        plan.review
    );
    assert!(
        plan.blast_radius
            .affected_node_ids
            .iter()
            .any(|id| id == "testfile"),
        "importer must be in the blast radius: {:?}",
        plan.blast_radius.affected_node_ids
    );
}

#[test]
fn move_plan_lists_def_and_import_update_then_verifies() {
    // helpers.py defines `helper`; app.py imports + calls it. Move helper to core.py.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("helpers.py"), b"def helper():\n    return 1\n").unwrap();
    std::fs::write(
        root.join("app.py"),
        b"from helpers import helper\n\n\ndef run():\n    return helper()\n",
    )
    .unwrap();
    std::fs::write(root.join("core.py"), b"# core module\n").unwrap();
    let before = build(root);

    let opts = RenameOptions::default();
    let plan = plan_relocate(&before, "helper", "core.py", "move", root, &opts).expect("plan");
    assert_eq!(plan.dest_file, "core.py");
    assert!(plan.dest_exists);
    assert!(plan.def_span.is_some(), "definition span captured");
    // app.py imports helper, so it needs an import update.
    assert!(
        plan.import_updates
            .iter()
            .any(|s| s.file.ends_with("app.py")),
        "import update for app.py: {:?}",
        plan.import_updates
    );

    // Apply the move: cut the def from helpers.py into core.py, update app's import.
    std::fs::write(root.join("helpers.py"), b"# moved out\n").unwrap();
    std::fs::write(root.join("core.py"), b"def helper():\n    return 1\n").unwrap();
    std::fs::write(
        root.join("app.py"),
        b"from core import helper\n\n\ndef run():\n    return helper()\n",
    )
    .unwrap();
    let report = verify_relocate(&plan, &before, root).expect("verify");
    assert!(
        report.passed,
        "correct move should verify; checks={:?}",
        report.checks
    );

    // Broken apply: never moved it (still in helpers.py) -> definition-relocated fails.
    std::fs::write(root.join("helpers.py"), b"def helper():\n    return 1\n").unwrap();
    std::fs::write(root.join("core.py"), b"# core module\n").unwrap();
    let report2 = verify_relocate(&plan, &before, root).expect("verify");
    assert!(!report2.passed);
    assert!(report2
        .checks
        .iter()
        .any(|c| c.name == "definition-relocated" && !c.passed));
}

#[test]
fn same_file_calls_are_recovered_as_references() {
    // The extractor resolves same-file calls into edges with no RawCall, so
    // recover_sites must still surface them. helpers.py: `caller` calls `helper`
    // in the same file; a rename/relocate must see that reference.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("helpers.py"),
        b"def helper():\n    return 1\n\n\ndef caller():\n    return helper()\n",
    )
    .unwrap();
    let before = build(root);

    // Rename: the same-file call site must appear (as a call site, line-only).
    let plan = plan_rename(
        &before,
        "helper",
        "helper2",
        root,
        &RenameOptions {
            scan_text: false, // prove it comes from edge recovery, not the text scan
            ..Default::default()
        },
    )
    .expect("plan");
    // The same-file direct call is recovered AND promoted into `edits` with a
    // precise column (it was previously line-only and stuck in `review`).
    let call = plan
        .edits
        .iter()
        .find(|s| s.reason == "call site" && s.file.ends_with("helpers.py") && s.line == Some(6))
        .unwrap_or_else(|| {
            panic!(
                "same-file call on line 6 promoted to edits; edits={:?} review={:?}",
                plan.edits, plan.review
            )
        });
    assert!(
        call.span.is_some(),
        "promoted same-file call carries a recovered column: {call:?}"
    );

    // Move: the def's own file (still using the symbol) gets an import update.
    let mplan = plan_relocate(
        &before,
        "helper",
        "core.py",
        "move",
        root,
        &Default::default(),
    )
    .expect("plan");
    assert!(
        mplan
            .import_updates
            .iter()
            .any(|s| s.file.ends_with("helpers.py")),
        "def's own file gets an import update when it still uses the symbol: {:?}",
        mplan.import_updates
    );
}

#[test]
fn cross_repo_reference_sites_carry_repo_tag() {
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, GraphData, Node, NodeId, NodeKind, Span};

    let mut def = Node {
        id: NodeId("lib::User".into()),
        label: "User".into(),
        file_type: synaptic_core::FileType::Code,
        source_file: "lib/models.py".into(),
        source_location: Some("L1".into()),
        community: None,
        repo: Some("lib".into()),
        extra: Map::new(),
    };
    def.set_kind(NodeKind::Class);
    def.set_span(Span {
        start_line: 1,
        start_col: 1,
        end_line: 3,
        end_col: 2,
    });
    let caller = Node {
        id: NodeId("app::main".into()),
        label: "main()".into(),
        file_type: synaptic_core::FileType::Code,
        source_file: "app/main.py".into(),
        source_location: Some("L9".into()),
        community: None,
        repo: Some("app".into()),
        extra: Map::new(),
    };
    // A cross-repo reference (non-call) so the site is emitted straight from the edge.
    let edge = Edge {
        source: NodeId("app::main".into()),
        target: NodeId("lib::User".into()),
        relation: "references".into(),
        confidence: Confidence::Inferred,
        source_file: "app/main.py".into(),
        source_location: Some("L9".into()),
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: true,
        extra: Map::new(),
    };
    let kg = KnowledgeGraph::from_graph_data(GraphData {
        directed: true,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![def, caller],
        links: vec![edge],
        hyperedges: vec![],
        built_at_commit: None,
    });

    let dir = tempfile::tempdir().unwrap();
    let opts = RenameOptions {
        scan_text: false, // no files on disk
        ..Default::default()
    };
    let plan = plan_rename(&kg, "User", "Account", dir.path(), &opts).expect("plan");
    // The definition site is tagged with the lib repo.
    assert!(plan
        .edits
        .iter()
        .chain(plan.review.iter())
        .any(|s| s.reason == "definition" && s.repo.as_deref() == Some("lib")));
    // The cross-repo reference site is tagged with the app repo.
    assert!(
        plan.edits
            .iter()
            .chain(plan.review.iter())
            .any(|s| s.repo.as_deref() == Some("app")),
        "a site carries the app repo tag: {:?}",
        plan.review
    );
}

#[test]
fn text_scan_enumerates_type_references_the_graph_misses() {
    // A type annotation `cfg: Settings` is not recorded as a graph edge, but the
    // textual scan must surface it (flagged for review), while the resolved call
    // site is not duplicated.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("conf.py"),
        b"class Settings:\n    def load(self):\n        return 1\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app.py"),
        b"from conf import Settings\n\n\ndef start(cfg: Settings):\n    return Settings()\n",
    )
    .unwrap();
    let kg = build(root);

    // `Settings` resolves to the class + an import-created node, so disambiguate.
    let opts = RenameOptions {
        file: Some("conf.py".into()),
        ..Default::default() // scan_text on by default
    };
    let plan = plan_rename(&kg, "Settings", "Config", root, &opts).expect("plan");

    // The `cfg: Settings` annotation occurrence is enumerated as a textual reference.
    let textual: Vec<_> = plan
        .review
        .iter()
        .filter(|s| s.reason == "textual reference" && s.file.ends_with("app.py"))
        .collect();
    assert!(
        textual
            .iter()
            .any(|s| s.span.map(|sp| sp.start_line) == Some(4)),
        "type-annotation occurrence on line 4 enumerated: {:?}",
        plan.review
    );

    // A textual site never duplicates a resolved site's (file,line,col): the
    // resolved call site at `return Settings()` is not re-emitted by the scan.
    use std::collections::BTreeSet;
    let resolved_keys: BTreeSet<(String, Option<u32>, Option<u32>)> = plan
        .edits
        .iter()
        .chain(plan.review.iter())
        .filter(|s| s.reason != "textual reference")
        .map(|s| (s.file.clone(), s.line, s.span.map(|sp| sp.start_col)))
        .collect();
    for t in plan
        .edits
        .iter()
        .chain(plan.review.iter())
        .filter(|s| s.reason == "textual reference")
    {
        let key = (t.file.clone(), t.line, t.span.map(|sp| sp.start_col));
        assert!(
            !resolved_keys.contains(&key),
            "textual site duplicates a resolved site: {key:?}"
        );
    }

    // Disabling the scan drops the textual sites.
    let plan2 = plan_rename(
        &kg,
        "Settings",
        "Config",
        root,
        &RenameOptions {
            file: Some("conf.py".into()),
            scan_text: false,
            ..Default::default()
        },
    )
    .expect("plan");
    assert!(plan2.review.iter().all(|s| s.reason != "textual reference"));
}

/// Apply every edit/review site's rename to its file (whole-word `User`->`Account`
/// on the recorded lines is sufficient for this fixture).
fn apply_full_rename(root: &Path) {
    for f in ["models.py", "service.py"] {
        let p = root.join(f);
        let src = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, src.replace("User", "Account")).unwrap();
    }
}

#[test]
fn verify_passes_on_correct_edit_and_fails_when_a_reference_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fixture(root);
    let before = build(root);

    let opts = RenameOptions {
        file: Some("models.py".into()),
        ..Default::default()
    };
    let plan: RenamePlan = plan_rename(&before, "User", "Account", root, &opts).expect("plan");

    // Correct application: rename the definition and its call site.
    apply_full_rename(root);
    let report = verify_plan(&plan, &before, root).expect("verify");
    assert!(
        report.passed,
        "correct rename should verify; checks={:?}",
        report.checks
    );

    // Now drop the reference: remove the call in service.py entirely.
    std::fs::write(root.join("service.py"), b"def make():\n    return 0\n").unwrap();
    let report2 = verify_plan(&plan, &before, root).expect("verify");
    assert!(!report2.passed, "dropping a reference must fail verify");
    let refs = report2
        .checks
        .iter()
        .find(|c| c.name == "references-preserved")
        .expect("references-preserved check present");
    assert!(!refs.passed, "references-preserved should fail");
    assert!(
        refs.detail.contains("references lost"),
        "detail names the lost references: {}",
        refs.detail
    );
}
