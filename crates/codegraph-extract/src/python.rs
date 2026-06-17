#[cfg(feature = "lang-python")]
use crate::config::{ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-python")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-python")]
use crate::walker::extract_with_config;

/// Python built-in callables skipped as call targets. Only the Python-relevant
/// names are listed here, scoped to the Python extractor.
#[cfg(feature = "lang-python")]
pub const PYTHON_BUILTINS: &[&str] = &[
    "str",
    "int",
    "float",
    "bool",
    "list",
    "dict",
    "set",
    "tuple",
    "bytes",
    "len",
    "range",
    "enumerate",
    "zip",
    "map",
    "filter",
    "sum",
    "min",
    "max",
    "print",
    "open",
    "isinstance",
    "type",
    "super",
    "sorted",
    "reversed",
    "any",
    "all",
    "abs",
    "round",
    "next",
    "iter",
    "hash",
    "id",
    "repr",
    "callable",
    "getattr",
    "setattr",
    "hasattr",
    "delattr",
    "vars",
    "dir",
];

/// Typing containers / generics that are never emitted as type-reference nodes
/// themselves, though their nested arguments still count.
pub(crate) const PYTHON_TYPE_CONTAINERS: &[&str] = &[
    "list",
    "dict",
    "set",
    "tuple",
    "frozenset",
    "type",
    "List",
    "Dict",
    "Set",
    "Tuple",
    "FrozenSet",
    "Type",
    "Optional",
    "Union",
    "Sequence",
    "Iterable",
    "Mapping",
    "MutableMapping",
    "Iterator",
    "Callable",
    "Awaitable",
    "AsyncIterable",
    "AsyncIterator",
    "Coroutine",
    "Generator",
    "AsyncGenerator",
    "ContextManager",
    "AsyncContextManager",
    "Annotated",
    "ClassVar",
    "Final",
    "Literal",
    "Concatenate",
    "ParamSpec",
    "TypeVar",
    "None",
    "Ellipsis",
];

/// Scalar builtins and test-mock names that appear in annotations but carry no
/// useful semantic meaning as graph nodes.
pub(crate) const PYTHON_ANNOTATION_NOISE: &[&str] = &[
    "str",
    "int",
    "float",
    "bool",
    "bytes",
    "bytearray",
    "complex",
    "object",
    "True",
    "False",
    "MagicMock",
    "Mock",
    "AsyncMock",
    "NonCallableMock",
    "NonCallableMagicMock",
    "PropertyMock",
    "patch",
    "sentinel",
];

/// True if `name` is a typing container or annotation-noise name and so must not
/// be emitted as a type reference.
pub(crate) fn is_suppressed_type(name: &str) -> bool {
    PYTHON_TYPE_CONTAINERS.contains(&name) || PYTHON_ANNOTATION_NOISE.contains(&name)
}

/// The Python `LanguageConfig`.
#[cfg(feature = "lang-python")]
pub fn python_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_python::LANGUAGE.into(),
        class_types: &["class_definition"],
        function_types: &["function_definition"],
        call_types: &["call"],
        name_field: "name",
        body_field: "body",
        call_function_field: "function",
        call_accessor_node_types: &["attribute"],
        call_accessor_field: "attribute",
        function_boundary_types: &["function_definition"],
        superclasses_field: Some("superclasses"),
        decorated_types: &["decorated_definition"],
        builtins: PYTHON_BUILTINS,
        import_types: &["import_statement", "import_from_statement"],
        import_style: Some(ImportStyle::Python),
        type_ref_style: Some(TypeRefStyle::Python),
        heritage_style: None,
        constructor_call_type: None,
        body_kinds: &[],
    }
}

/// Extract a Python file's content already in memory. `path` is used for the
/// file-node id/label and scoping (it need not exist on disk — handy for tests).
#[cfg(feature = "lang-python")]
pub fn extract_python_source(path: &str, source: &[u8]) -> ExtractionResult {
    // Docstrings + comment-marker rationale are extracted inside the single
    // parse/walk in `extract_with_config` (Python is detected via its config), so
    // there is no second parse here.
    extract_with_config(path, source, &python_config())
}

/// Read and extract a Python file from disk.
#[cfg(feature = "lang-python")]
pub fn extract_python_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_python_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-python"))]
mod tests {
    use super::*;
    use codegraph_core::FileType;

    #[test]
    fn comment_markers_become_rationale_nodes() {
        let r = extract_python_source(
            "m.py",
            b"def f():\n    # NOTE: keep this fast for the hot path\n    return 1\n",
        );
        assert!(
            r.nodes
                .iter()
                .any(|n| n.file_type == FileType::Rationale && n.label.contains("NOTE")),
            "expected a NOTE rationale node, got: {:?}",
            r.nodes
                .iter()
                .map(|n| (n.label.clone(), n.file_type))
                .collect::<Vec<_>>()
        );
        assert!(
            r.edges.iter().any(|e| e.relation == "rationale_for"),
            "expected a rationale_for edge"
        );
    }

    #[test]
    fn docstrings_become_rationale_nodes_linked_to_their_symbol() {
        let r = extract_python_source(
            "m.py",
            b"def compute():\n    \"\"\"This computes the score for ranking purposes.\"\"\"\n    return 1\n",
        );
        assert!(
            r.nodes
                .iter()
                .any(|n| n.file_type == FileType::Rationale
                    && n.label.contains("computes the score")),
            "expected a docstring rationale node, got: {:?}",
            r.nodes
                .iter()
                .map(|n| (n.label.clone(), n.file_type))
                .collect::<Vec<_>>()
        );
        let compute_id = r
            .nodes
            .iter()
            .find(|n| n.label == "compute()")
            .map(|n| n.id.clone())
            .expect("compute() node");
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "rationale_for" && e.target == compute_id),
            "docstring should link to compute()"
        );
    }

    #[test]
    fn short_docstrings_are_ignored() {
        let r = extract_python_source("m.py", b"def f():\n    \"\"\"ok\"\"\"\n    return 1\n");
        assert!(
            !r.nodes.iter().any(|n| n.file_type == FileType::Rationale),
            "docstrings <= 20 chars are not rationale"
        );
    }

    #[test]
    fn captures_function_signature_with_params_and_return() {
        let r = extract_python_source(
            "m.py",
            b"def greet(name: str, count: int = 1) -> str:\n    return name\n",
        );
        let f = r.nodes.iter().find(|n| n.label == "greet()").unwrap();
        let sig = f.signature().expect("function node carries a signature");
        let names: Vec<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["name", "count"]);
        assert_eq!(sig.params[0].type_ref.as_deref(), Some("str"));
        assert_eq!(sig.params[1].type_ref.as_deref(), Some("int"));
        assert_eq!(sig.return_type.as_deref(), Some("str"));
        // raw is the verbatim header, always present.
        assert!(sig.raw.contains("greet(name: str"));
    }

    #[test]
    fn untyped_params_keep_names_without_types() {
        let r = extract_python_source("m.py", b"def add(a, b):\n    return a + b\n");
        let f = r.nodes.iter().find(|n| n.label == "add()").unwrap();
        let sig = f.signature().expect("signature present");
        let names: Vec<&str> = sig.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert!(sig.params.iter().all(|p| p.type_ref.is_none()));
        assert!(sig.return_type.is_none());
    }
}
