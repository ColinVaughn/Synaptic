//! Tests for the walker extractor (split from walker.rs).

use crate::python::extract_python_source;
use codegraph_core::Confidence;

const SAMPLE: &[u8] = b"class Transformer:\n    def __init__(self, d_model):\n        self.d_model = d_model\n\n    def forward(self, x):\n        return x\n";

fn labels(src: &[u8]) -> Vec<String> {
    extract_python_source("fixtures/sample.py", src)
        .nodes
        .into_iter()
        .map(|n| n.label)
        .collect()
}

#[test]
fn finds_class() {
    assert!(labels(SAMPLE).contains(&"Transformer".to_string()));
}

#[test]
fn finds_methods() {
    let ls = labels(SAMPLE);
    assert!(ls.iter().any(|l| l.contains("forward")));
    assert!(ls.iter().any(|l| l.contains("__init__")));
}

#[test]
fn file_node_label_is_filename() {
    assert!(labels(SAMPLE).contains(&"sample.py".to_string()));
}

#[test]
fn method_labels_have_dot_prefix() {
    let ls = labels(SAMPLE);
    assert!(ls.contains(&".forward()".to_string()));
}

#[test]
fn no_dangling_edge_sources() {
    let r = extract_python_source("fixtures/sample.py", SAMPLE);
    let ids: std::collections::HashSet<_> = r.nodes.iter().map(|n| n.id.clone()).collect();
    for e in &r.edges {
        assert!(ids.contains(&e.source), "dangling source: {}", e.source);
    }
}

#[test]
fn structural_edges_are_extracted() {
    let r = extract_python_source("fixtures/sample.py", SAMPLE);
    for e in &r.edges {
        if matches!(e.relation.as_str(), "contains" | "method" | "inherits") {
            assert_eq!(e.confidence, Confidence::Extracted, "edge {e:?}");
        }
    }
}

#[test]
fn contains_and_method_edges_present() {
    let r = extract_python_source("fixtures/sample.py", SAMPLE);
    let rels: Vec<_> = r.edges.iter().map(|e| e.relation.as_str()).collect();
    assert!(rels.contains(&"contains")); // file -> class
    assert!(rels.contains(&"method")); // class -> method
}

#[test]
fn inherits_creates_external_base_and_edge() {
    let src = b"class Base:\n    pass\n\nclass Child(Base):\n    pass\n\nclass Orphan(Missing):\n    pass\n";
    let r = extract_python_source("fixtures/inh.py", src);
    let inherits: Vec<_> = r
        .edges
        .iter()
        .filter(|e| e.relation == "inherits")
        .collect();
    // Child -> Base (local) and Orphan -> Missing (external stub)
    assert_eq!(inherits.len(), 2);
    assert!(r.nodes.iter().any(|n| n.label == "Missing")); // external stub node exists
    for e in inherits {
        assert_eq!(e.confidence, Confidence::Extracted);
    }
}

const CALLS: &[u8] = b"def compute_score(data):\n    return sum(data)\n\n\ndef normalize(value):\n    return value / 100.0\n\n\ndef run_analysis(data):\n    score = compute_score(data)\n    return normalize(score)\n\n\nclass Analyzer:\n    def process(self, data):\n        return run_analysis(data)\n\n    def score(self, data):\n        return compute_score(data)\n\n    def full_pipeline(self, data):\n        raw = self.score(data)\n        return normalize(raw)\n";

fn call_pairs(src: &[u8]) -> std::collections::HashSet<(String, String)> {
    let r = extract_python_source("fixtures/sample_calls.py", src);
    let label = |id: &codegraph_core::NodeId| {
        r.nodes
            .iter()
            .find(|n| &n.id == id)
            .map(|n| n.label.clone())
            .unwrap()
    };
    r.edges
        .iter()
        .filter(|e| e.relation == "calls")
        .map(|e| (label(&e.source), label(&e.target)))
        .collect()
}

#[test]
fn call_edges_have_call_context() {
    let r = extract_python_source("fixtures/sample_calls.py", CALLS);
    let calls: Vec<_> = r.edges.iter().filter(|e| e.relation == "calls").collect();
    assert!(!calls.is_empty());
    assert!(calls.iter().all(|e| e.context.as_deref() == Some("call")));
    assert!(calls.iter().all(|e| e.confidence == Confidence::Extracted));
}

#[test]
fn run_analysis_calls_compute_score_and_normalize() {
    let pairs = call_pairs(CALLS);
    assert!(pairs.contains(&("run_analysis()".into(), "compute_score()".into())));
    assert!(pairs.contains(&("run_analysis()".into(), "normalize()".into())));
}

#[test]
fn method_calls_module_function() {
    let pairs = call_pairs(CALLS);
    // Analyzer.process() calls run_analysis(); Analyzer.score() calls compute_score()
    assert!(pairs.contains(&(".process()".into(), "run_analysis()".into())));
    assert!(pairs.contains(&(".score()".into(), "compute_score()".into())));
}

#[test]
fn member_call_resolves_to_sibling_method() {
    let pairs = call_pairs(CALLS);
    // full_pipeline() calls self.score(...) -> .score(); and normalize(...)
    assert!(pairs.contains(&(".full_pipeline()".into(), ".score()".into())));
    assert!(pairs.contains(&(".full_pipeline()".into(), "normalize()".into())));
}

#[test]
fn builtins_do_not_create_calls() {
    let pairs = call_pairs(CALLS);
    // `sum(data)` in compute_score must not appear as a calls edge target.
    assert!(!pairs.iter().any(|(_, tgt)| tgt == "sum" || tgt == "sum()"));
}

#[test]
fn unresolved_call_goes_to_raw_calls() {
    let src = b"def f():\n    return external_thing()\n";
    let r = extract_python_source("fixtures/u.py", src);
    assert!(r.edges.iter().all(|e| e.relation != "calls"));
    assert!(r.raw_calls.iter().any(|rc| rc.callee == "external_thing"));
}

#[test]
fn decorated_methods_stay_methods() {
    // @property / @staticmethod methods are `decorated_definition` nodes; they
    // must be scoped as methods (`.name()` + `method` edge), not module functions.
    let src = b"class Widget:\n    @property\n    def value(self):\n        return 1\n\n    @staticmethod\n    def make():\n        return Widget()\n\n    def plain(self):\n        return 2\n";
    let r = extract_python_source("fixtures/w.py", src);
    let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
    assert!(
        labels.contains(&".value()"),
        "decorated @property method; got {labels:?}"
    );
    assert!(
        labels.contains(&".make()"),
        "decorated @staticmethod method; got {labels:?}"
    );
    assert!(labels.contains(&".plain()"));
    // All three are `method` edges from the class, none `contains` from the file.
    let methods = r.edges.iter().filter(|e| e.relation == "method").count();
    assert_eq!(methods, 3, "expected 3 method edges");
    // No function should be a module-level `contains` (the class itself is the only contains).
    let contains_targets: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.relation == "contains")
        .map(|e| e.target.as_str())
        .collect();
    assert!(contains_targets
        .iter()
        .all(|t| !t.ends_with("_value") && !t.ends_with("_make")));
}

#[test]
fn nodes_tagged_ast_origin() {
    let r = extract_python_source("fixtures/o.py", b"def f():\n    pass\n");
    assert!(r
        .nodes
        .iter()
        .all(|n| n.extra.get("_origin").and_then(|v| v.as_str()) == Some("ast")));
}

// imports
use codegraph_core::make_id;

fn import_edges(r: &crate::result::ExtractionResult) -> Vec<(String, String, String)> {
    r.edges
        .iter()
        .filter(|e| matches!(e.relation.as_str(), "imports" | "imports_from"))
        .map(|e| (e.source.0.clone(), e.relation.clone(), e.target.0.clone()))
        .collect()
}

#[test]
fn plain_import_creates_imports_edge_and_stub() {
    let r = extract_python_source("m.py", b"import os\n");
    let file = make_id(&["m.py"]);
    let os = make_id(&["os"]);
    assert!(import_edges(&r).contains(&(file, "imports".into(), os.clone())));
    // External stub node exists for the module, labeled `os`.
    assert!(r.nodes.iter().any(|n| n.id.0 == os && n.label == "os"));
    // import edges carry EXTRACTED confidence + the `import` context.
    let imp = r.edges.iter().find(|e| e.relation == "imports").unwrap();
    assert_eq!(imp.confidence, Confidence::Extracted);
    assert_eq!(imp.context.as_deref(), Some("import"));
}

#[test]
fn aliased_import_strips_alias() {
    let r = extract_python_source("m.py", b"import numpy as np\n");
    let file = make_id(&["m.py"]);
    let numpy = make_id(&["numpy"]);
    assert!(import_edges(&r).contains(&(file, "imports".into(), numpy)));
    // The alias `np` is not the target.
    assert!(!r.nodes.iter().any(|n| n.label == "np"));
}

#[test]
fn multiple_modules_in_one_import() {
    let r = extract_python_source("m.py", b"import os, sys\n");
    let edges = import_edges(&r);
    assert!(edges.iter().any(|(_, _, t)| t == &make_id(&["os"])));
    assert!(edges.iter().any(|(_, _, t)| t == &make_id(&["sys"])));
}

#[test]
fn relative_import_resolves_to_sibling_file_id() {
    // `from .helper import transform` in pkg/mod.py targets pkg/helper.py's id.
    let r = extract_python_source("pkg/mod.py", b"from .helper import transform\n");
    let helper_file = make_id(&["pkg/helper.py"]);
    assert!(import_edges(&r)
        .iter()
        .any(|(_, rel, t)| rel == "imports_from" && t == &helper_file));
    // Relative targets are NOT stubbed (they bind to the real file node).
    assert!(!r.nodes.iter().any(|n| n.id.0 == helper_file));
}

#[test]
fn relative_import_climbs_parents() {
    // `from ..util import x` in a/b/mod.py resolves to a/util.py
    let r = extract_python_source("a/b/mod.py", b"from ..util import x\n");
    let util = make_id(&["a/util.py"]);
    assert!(import_edges(&r)
        .iter()
        .any(|(_, rel, t)| rel == "imports_from" && t == &util));
}

#[test]
fn absolute_from_import_records_alias_and_stem() {
    let r = extract_python_source("m.py", b"from lib.lower import Foo as F\n");
    let modid = make_id(&["lib.lower"]);
    // Edge to the module + stub node.
    assert!(import_edges(&r)
        .iter()
        .any(|(_, rel, t)| rel == "imports_from" && t == &modid));
    assert!(r.nodes.iter().any(|n| n.id.0 == modid));
    // Import record captures alias + module stem (final component).
    assert_eq!(r.imports.len(), 1);
    let rec = &r.imports[0];
    assert_eq!(rec.local_name, "F");
    assert_eq!(rec.imported_name, "Foo");
    assert_eq!(rec.module_stem, "lower");
    assert_eq!(rec.source_file, "m.py");
}

#[test]
fn from_import_multiple_names_records_each() {
    let r = extract_python_source("m.py", b"from pkg.helper import a, b as c\n");
    let mut recs: Vec<(String, String, String)> = r
        .imports
        .iter()
        .map(|i| {
            (
                i.local_name.clone(),
                i.imported_name.clone(),
                i.module_stem.clone(),
            )
        })
        .collect();
    recs.sort();
    assert_eq!(
        recs,
        vec![
            ("a".into(), "a".into(), "helper".into()),
            ("c".into(), "b".into(), "helper".into()),
        ]
    );
}

#[test]
fn bare_relative_and_wildcard_record_nothing() {
    // `from . import x` has an empty module stem, so no record (still an edge).
    let r = extract_python_source("pkg/m.py", b"from . import x\n");
    assert!(r.imports.is_empty());
    assert!(r.edges.iter().any(|e| e.relation == "imports_from"));
    // `from pkg import *` skips the wildcard.
    let r2 = extract_python_source("m.py", b"from pkg import *\n");
    assert!(r2.imports.is_empty());
}

// type references
fn refs(r: &crate::result::ExtractionResult) -> Vec<(String, String)> {
    // (target label, context) for each `references` edge.
    r.edges
        .iter()
        .filter(|e| e.relation == "references")
        .map(|e| {
            let label = r
                .nodes
                .iter()
                .find(|n| n.id == e.target)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| e.target.0.clone());
            (label, e.context.clone().unwrap_or_default())
        })
        .collect()
}

#[test]
fn param_and_return_types_link_to_in_file_classes() {
    let src = b"class Widget:\n    pass\n\nclass Result:\n    pass\n\ndef build(w: Widget) -> Result:\n    return Result()\n";
    let r = extract_python_source("t.py", src);
    let rs = refs(&r);
    assert!(
        rs.contains(&("Widget".to_string(), "parameter_type".to_string())),
        "got {rs:?}"
    );
    assert!(rs.contains(&("Result".to_string(), "return_type".to_string())));
    // The reference edge binds to the actual in-file class node id.
    let widget_id = make_id(&["t", "Widget"]);
    assert!(r
        .edges
        .iter()
        .any(|e| e.relation == "references" && e.target.0 == widget_id));
    // references edges are EXTRACTED.
    assert!(r
        .edges
        .iter()
        .filter(|e| e.relation == "references")
        .all(|e| e.confidence == Confidence::Extracted));
}

#[test]
fn generic_args_tagged_and_containers_filtered() {
    let src = b"def f(items: list[Thing]) -> None:\n    return None\n";
    let r = extract_python_source("t.py", src);
    let rs = refs(&r);
    // `Thing` emitted as generic_arg; `list` and `None` filtered out.
    assert!(rs.contains(&("Thing".to_string(), "generic_arg".to_string())));
    assert!(!rs.iter().any(|(l, _)| l == "list" || l == "None"));
    assert_eq!(rs.len(), 1, "only Thing should be referenced; got {rs:?}");
}

#[test]
fn unknown_param_type_creates_global_stub() {
    let src = b"def f(cfg: Settings):\n    return cfg\n";
    let r = extract_python_source("t.py", src);
    let settings = make_id(&["Settings"]);
    // Global stub node created + a references edge pointing at it.
    assert!(r
        .nodes
        .iter()
        .any(|n| n.id.0 == settings && n.label == "Settings"));
    assert!(r
        .edges
        .iter()
        .any(|e| e.relation == "references" && e.target.0 == settings));
}

#[test]
fn scalar_annotations_emit_no_references() {
    let src = b"def f(x: int, name: str) -> bool:\n    return True\n";
    let r = extract_python_source("t.py", src);
    assert!(r.edges.iter().all(|e| e.relation != "references"));
}
