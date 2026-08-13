//! C++ extractor — Bucket B (declarative `LanguageConfig` + the walker's
//! declarator-unwrap function-name fallback). Covers the C++ config, imports,
//! and type-reference collection.
//!
//! Classes/structs → bare name; inline-defined methods/free functions → `name()`
//! / `.name()` (name unwrapped from the declarator chain); `#include` →
//! `imports_from`; `base_class_clause` → `inherits`; parameter/return types →
//! `references`.

#[cfg(feature = "lang-cpp")]
use crate::config::{HeritageStyle, ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-cpp")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-cpp")]
use crate::walker::{extract_with_config, normalize_c_family_source};

#[cfg(feature = "lang-cpp")]
const CPP_MACRO_BUILTINS: &[&str] = &[
    "GENERATED_BODY",
    "GENERATED_UCLASS_BODY",
    "GENERATED_USTRUCT_BODY",
    "UCLASS",
    "UENUM",
    "UFUNCTION",
    "UINTERFACE",
    "UMETA",
    "UPROPERTY",
    "USTRUCT",
    "UE_LOG",
    "SLATE_BEGIN_ARGS",
    "SLATE_END_ARGS",
];

/// The C++ `LanguageConfig`. `class_specifier`/`struct_specifier` carry `name`
/// and `body` fields; inline methods are `function_definition` (their name is
/// unwrapped from the declarator). Method prototypes (`field_declaration` with a
/// `function_declarator`) become method nodes and data members become `field`
/// type references — handled by the walker's `class_members` pass.
#[cfg(feature = "lang-cpp")]
pub fn cpp_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_cpp::LANGUAGE.into(),
        class_types: &["class_specifier", "struct_specifier"],
        function_types: &["function_definition"],
        call_types: &["call_expression"],
        name_field: "name",
        body_field: "body",
        call_function_field: "function",
        call_accessor_node_types: &["field_expression"],
        call_accessor_field: "field",
        function_boundary_types: &["function_definition"],
        superclasses_field: None,
        decorated_types: &[
            "template_declaration",
            "preproc_if",
            "preproc_ifdef",
            "preproc_elif",
            "preproc_elifdef",
            "preproc_else",
        ],
        builtins: CPP_MACRO_BUILTINS,
        import_types: &["preproc_include"],
        import_style: Some(ImportStyle::CInclude),
        type_ref_style: Some(TypeRefStyle::Cpp),
        heritage_style: Some(HeritageStyle::Cpp),
        constructor_call_type: None,
        body_kinds: &[],
    }
}

/// Extract a C++ source file already in memory.
#[cfg(feature = "lang-cpp")]
pub fn extract_cpp_source(path: &str, source: &[u8]) -> ExtractionResult {
    let source = normalize_c_family_source(source, false);
    extract_with_config(path, &source, &cpp_config())
}

/// Read and extract a C++ file from disk.
#[cfg(feature = "lang-cpp")]
pub fn extract_cpp_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_cpp_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-cpp"))]
mod tests {
    use super::extract_cpp_source;
    use crate::result::ExtractionResult;
    use synaptic_core::Confidence;

    const SAMPLE: &[u8] = br#"
#include <vector>

class Animal {
public:
    void breathe() { idle(); }
    void idle() {}
};

class Dog : public Animal {
public:
    Result greet(Food food) { return makeSound(); }
    Result makeSound() { return Result(); }
};
"#;

    fn extract() -> ExtractionResult {
        extract_cpp_source("src/app.cpp", SAMPLE)
    }

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let lbl = |id: &synaptic_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| (lbl(&e.source), lbl(&e.target)))
            .collect()
    }

    #[test]
    fn class_and_method_nodes() {
        let r = extract();
        let ls = labels(&r);
        assert!(ls.contains(&"Animal".to_string()), "{ls:?}");
        assert!(ls.contains(&"Dog".to_string()));
        assert!(ls.contains(&".greet()".to_string()));
        assert!(ls.contains(&".makeSound()".to_string()));
    }

    #[test]
    fn include_emits_header_base() {
        let r = extract();
        let imps = rels(&r, "imports_from");
        assert!(imps.iter().any(|(_, t)| t == "vector"), "imports: {imps:?}");
    }

    #[test]
    fn base_class_clause_inherits() {
        let r = extract();
        let inh = rels(&r, "inherits");
        assert!(
            inh.contains(&("Dog".to_string(), "Animal".to_string())),
            "inherits: {inh:?}"
        );
    }

    #[test]
    fn parameter_and_return_type_references() {
        let r = extract();
        let refs: Vec<(String, String)> = r
            .edges
            .iter()
            .filter(|e| e.relation == "references")
            .map(|e| {
                let tgt = r
                    .nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| e.target.0.clone());
                (tgt, e.context.clone().unwrap_or_default())
            })
            .collect();
        assert!(
            refs.contains(&("Food".to_string(), "parameter_type".to_string())),
            "refs: {refs:?}"
        );
        assert!(refs.contains(&("Result".to_string(), "return_type".to_string())));
    }

    #[test]
    fn intra_class_call_resolves() {
        let r = extract();
        let calls = rels(&r, "calls");
        // greet() calls makeSound(); breathe() calls idle()
        assert!(
            calls.contains(&(".greet()".to_string(), ".makeSound()".to_string())),
            "calls: {calls:?}"
        );
        assert!(calls.contains(&(".breathe()".to_string(), ".idle()".to_string())));
    }

    #[test]
    fn method_prototypes_and_data_members() {
        let r = extract_cpp_source(
            "F.cpp",
            b"class C {\n  Leash leash;\n  void walk(Dog d);\n  Result fetch();\n};\n",
        );
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        // prototypes become method nodes
        assert!(labels.contains(&".walk()".to_string()), "{labels:?}");
        assert!(labels.contains(&".fetch()".to_string()));
        let refs: Vec<(String, String)> = r
            .edges
            .iter()
            .filter(|e| e.relation == "references")
            .map(|e| {
                let tgt = r
                    .nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| e.target.0.clone());
                (tgt, e.context.clone().unwrap_or_default())
            })
            .collect();
        // data member type + prototype param/return
        assert!(
            refs.contains(&("Leash".to_string(), "field".to_string())),
            "{refs:?}"
        );
        assert!(refs.contains(&("Dog".to_string(), "parameter_type".to_string())));
        assert!(refs.contains(&("Result".to_string(), "return_type".to_string())));
    }

    #[test]
    fn template_class_inheritance_and_members() {
        // A class template that inherits from a templated base, with members
        // whose types are the template parameter `T`.
        let r = extract_cpp_source(
            "tpl.cpp",
            br#"
template <typename T>
class Container {
public:
    T get() { return value; }
    void set(T v) { value = v; }
private:
    T value;
};

template <typename T>
class Stack : public Container<T> {
public:
    void push(T v) { this->set(v); }
};

class IntStack : public Stack<int> {
public:
    int top() { return this->get(); }
};
"#,
        );
        // The class template and its specializing subclass are real nodes.
        let ls = labels(&r);
        assert!(ls.contains(&"Container".to_string()), "{ls:?}");
        assert!(ls.contains(&"Stack".to_string()), "{ls:?}");
        assert!(ls.contains(&"IntStack".to_string()), "{ls:?}");

        // Inheritance follows through the templated base.
        let inh = rels(&r, "inherits");
        assert!(
            inh.contains(&("Stack".to_string(), "Container".to_string())),
            "{inh:?}"
        );
        assert!(
            inh.contains(&("IntStack".to_string(), "Stack".to_string())),
            "{inh:?}"
        );

        // The template parameter `T` is a placeholder, not a type: it must not
        // become a node nor a `references`/`inherits` target.
        assert!(
            !ls.contains(&"T".to_string()),
            "spurious template-param node T: {ls:?}"
        );
        let refs_to_t = r.edges.iter().any(|e| {
            r.nodes
                .iter()
                .find(|n| n.id == e.target)
                .is_some_and(|n| n.label == "T")
        });
        assert!(!refs_to_t, "edges should not target template parameter T");
    }

    #[test]
    fn structural_edges_extracted_confidence() {
        let r = extract();
        for e in &r.edges {
            if matches!(e.relation.as_str(), "contains" | "method" | "inherits") {
                assert_eq!(e.confidence, Confidence::Extracted, "edge {e:?}");
            }
        }
    }

    #[test]
    fn unreal_export_macros_and_reflected_methods_parse() {
        let r = extract_cpp_source(
            "Source/Game/Hero.h",
            br#"
UCLASS(BlueprintType)
class GAME_API AHero : public AActor {
    GENERATED_BODY()
public:
    UFUNCTION(BlueprintCallable)
    void TakeDamage(float Amount, const FVector& HitPoint);
    UFUNCTION(BlueprintPure)
    const FVector& GetPoint(int Index) const;
};
"#,
        );
        let ls = labels(&r);
        assert!(ls.contains(&"AHero".to_string()), "{ls:?}");
        assert!(ls.contains(&".TakeDamage()".to_string()), "{ls:?}");
        assert!(ls.contains(&".GetPoint()".to_string()), "{ls:?}");
        assert!(
            rels(&r, "inherits").contains(&("AHero".into(), "AActor".into())),
            "{:?}",
            rels(&r, "inherits")
        );
        let method = r.nodes.iter().find(|n| n.label == ".TakeDamage()").unwrap();
        let sig = method.signature().expect("prototype signature");
        assert_eq!(
            sig.params
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Amount", "HitPoint"]
        );
        let get_point = r.nodes.iter().find(|n| n.label == ".GetPoint()").unwrap();
        assert_eq!(get_point.signature().unwrap().params[0].name, "Index");
    }

    #[test]
    fn unreal_log_macro_does_not_become_a_function() {
        let r = extract_cpp_source(
            "Crash.cpp",
            br#"
void cleanup() {}
void crash() {
    UE_LOG(LogGame, Error, TEXT("failed (%s)"), *Message);
    cleanup();
}
"#,
        );
        assert!(!labels(&r).contains(&"UE_LOG()".to_string()));
        assert!(r.raw_calls.iter().all(|call| call.callee != "UE_LOG"));
        assert!(rels(&r, "calls").contains(&("crash()".into(), "cleanup()".into())));
    }

    #[test]
    fn unreal_legacy_generated_bodies_keep_interface_and_struct() {
        let r = extract_cpp_source(
            "Source/Game/Types.h",
            br#"
UINTERFACE()
class GAME_API UDamageable : public UInterface {
    GENERATED_UINTERFACE_BODY()
};
USTRUCT()
struct FDamageData {
    GENERATED_USTRUCT_BODY()
    float Amount;
};
"#,
        );
        let ls = labels(&r);
        assert!(ls.contains(&"UDamageable".to_string()), "{ls:?}");
        assert!(ls.contains(&"FDamageData".to_string()), "{ls:?}");
    }

    #[test]
    fn forward_declaration_is_not_a_class_definition() {
        let r = extract_cpp_source("Types.h", b"class Forward; class Real { Forward* Value; };");
        let classes: Vec<_> = r
            .nodes
            .iter()
            .filter(|node| node.kind() == Some(synaptic_core::NodeKind::Class))
            .map(|node| node.label.as_str())
            .collect();
        assert_eq!(classes, ["Real"]);
    }

    #[test]
    fn unreal_metadata_and_slate_dsl_are_not_functions() {
        let r = extract_cpp_source(
            "Ui.h",
            br#"
enum Difficulty { Sane UMETA(DisplayName = "Sane") };
class Widget {
    SLATE_BEGIN_ARGS(Widget) : _Enabled(true) {}
        SLATE_ARGUMENT(bool, Enabled)
    SLATE_END_ARGS()
};
"#,
        );
        let ls = labels(&r);
        assert!(!ls.iter().any(|label| label.contains("UMETA")), "{ls:?}");
        assert!(!ls.iter().any(|label| label.contains("SLATE_")), "{ls:?}");
    }

    #[test]
    fn library_control_macros_preserve_classes_and_methods() {
        let r = extract_cpp_source(
            "fmt/base.h",
            br#"
FMT_BEGIN_NAMESPACE
FMT_BEGIN_EXPORT
template <typename T> class basic_view : public base<T> {
 public:
  template <typename U>
  FMT_CONSTEXPR explicit basic_view(const U& value) : value_(value) {}
  FMT_NODISCARD FMT_CONSTEXPR int size() const { return 0; }
 private:
  T value_;
};
FMT_END_EXPORT
class dynamic_arg_list {};
FMT_END_NAMESPACE
"#,
        );
        let ls = labels(&r);
        assert!(ls.contains(&"basic_view".to_string()), "{ls:?}");
        assert!(ls.contains(&"dynamic_arg_list".to_string()), "{ls:?}");
        assert!(ls.contains(&".basic_view()".to_string()), "{ls:?}");
        assert!(ls.contains(&".size()".to_string()), "{ls:?}");
        assert!(
            rels(&r, "inherits").contains(&("basic_view".into(), "base".into())),
            "{:?}",
            rels(&r, "inherits")
        );
    }

    #[test]
    fn macro_blocks_are_not_functions() {
        let r = extract_cpp_source(
            "test.cc",
            b"TEST(format_test, works) { helper(); }\nvoid real() {}\n",
        );
        let ls = labels(&r);
        assert!(!ls.contains(&"TEST()".to_string()), "{ls:?}");
        assert!(ls.contains(&"real()".to_string()), "{ls:?}");
    }

    #[test]
    fn punctuation_bearing_functions_have_distinct_ids() {
        let r = extract_cpp_source(
            "ops.hpp",
            br#"
class Value {
 public:
  Value() {}
  ~Value() {}
  bool operator==(const Value&) const { return true; }
  bool operator!=(const Value&) const { return false; }
};
"#,
        );
        let methods: Vec<_> = r
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.label.as_str(),
                    ".Value()" | ".~Value()" | ".operator==()" | ".operator!=()"
                )
            })
            .collect();
        assert_eq!(methods.len(), 4, "{:?}", labels(&r));
        let ids: std::collections::HashSet<_> = methods.iter().map(|node| &node.id).collect();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn conversion_operator_keeps_operator_name_and_member_pointer_is_a_field() {
        let r = extract_cpp_source(
            "conversion.hpp",
            br#"
template <typename T> class Wrapper {
 public:
  explicit operator T() const { return T{}; }
  explicit operator const char*() const { return nullptr; }
  void (Wrapper::*callback)();
};
"#,
        );
        let ls = labels(&r);
        assert!(ls.contains(&".operator T()".to_string()), "{ls:?}");
        assert!(
            ls.contains(&".operator const char*()".to_string()),
            "{ls:?}"
        );
        assert!(!ls.contains(&".Wrapper()".to_string()), "{ls:?}");
    }

    #[test]
    fn macro_definitions_do_not_emit_functions() {
        let r = extract_cpp_source(
            "macros.hpp",
            b"void DebugBreak();\n#define CATCH_BREAK_INTO_DEBUGGER() [] {}\n#define MAKE_TEST(name) void name() {}\nvoid real() {}\n",
        );
        let ls = labels(&r);
        assert_eq!(
            ls.iter().filter(|label| label.ends_with("()")).count(),
            1,
            "{ls:?}"
        );
        assert!(ls.contains(&"real()".to_string()));
    }

    #[test]
    fn top_level_conditionals_do_not_merge_neighboring_declarations() {
        let r = extract_cpp_source(
            "conditional.hpp",
            br#"
#ifndef HEADER_ONLY
extern template auto implementation<char>() -> char;
#endif
template <typename T> auto equal2(const T* lhs, const T* rhs) -> bool {
  return *lhs == *rhs;
}
"#,
        );
        assert!(labels(&r).contains(&"equal2()".to_string()));
    }

    #[test]
    fn conditionals_inside_classes_preserve_method_scope() {
        let r = extract_cpp_source(
            "conditional.hpp",
            br#"
class locale_ref {
#if USE_LOCALE
 public:
  locale_ref() {}
  explicit operator bool() const { return true; }
#endif
};
"#,
        );
        let ls = labels(&r);
        assert!(ls.contains(&".locale_ref()".to_string()), "{ls:?}");
        assert!(ls.contains(&".operator bool()".to_string()), "{ls:?}");
        assert!(!ls.contains(&"locale_ref()".to_string()), "{ls:?}");
    }

    #[test]
    fn overloads_have_distinct_nodes_and_calls_are_not_arbitrarily_bound() {
        let r = extract_cpp_source(
            "overloads.hpp",
            br#"
class Visitor {
 public:
  void operator()(int) {}
  void operator()(const char*) {}
  void run() { (*this)(1); }
};
"#,
        );
        let overloads: Vec<_> = r
            .nodes
            .iter()
            .filter(|node| node.label == ".operator()()")
            .collect();
        assert_eq!(overloads.len(), 2, "{:?}", labels(&r));
        assert_ne!(overloads[0].id, overloads[1].id);
        assert!(r.edges.iter().all(|edge| {
            edge.relation != "calls" || !overloads.iter().any(|node| node.id == edge.target)
        }));
    }

    #[test]
    fn anonymous_struct_methods_are_retained() {
        let r = extract_cpp_source(
            "anonymous.cpp",
            br#"
void parse() {
  struct {
    void operator()(int value) { consume(value); }
  } visitor;
}
"#,
        );
        let ls = labels(&r);
        assert!(
            ls.iter().any(|label| label.starts_with("anonymous@")),
            "{ls:?}"
        );
        assert!(ls.contains(&".operator()()".to_string()), "{ls:?}");
    }
}
