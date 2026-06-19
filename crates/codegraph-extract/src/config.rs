/// Per-language import semantics. We use a dispatch tag the walker matches on
/// (only Python today — B2 adds `EcmaScript`, `Go`, … as the languages land).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStyle {
    /// `import X [as y]` / `from M import …` with relative-import path resolution.
    Python,
    /// `import { x } from 'm'` / `import X from 'm'` / `export { x } from 'm'`:
    /// `imports_from` / `re_exports` edges to a specifier-labeled module stub,
    /// plus named-import records (module stem = last path component) for B3.
    EcmaScript,
    /// Java `import a.b.C;` (and `import static a.b.C.m;`): an `imports` edge to
    /// the dotted name's tail (`C`) as an external stub. Wildcard `import a.b.*`
    /// imports the package's last segment.
    Java,
    /// C# `using A.B.C;` (and `using static …`, `using X = A.B;`): an `imports`
    /// edge to the dotted name's tail, like [`ImportStyle::Java`].
    CSharp,
    /// Kotlin `import a.b.C`: an `imports` edge to the `qualified_identifier`
    /// tail.
    Kotlin,
    /// Swift `import Foundation`: an `imports` edge to the module identifier.
    Swift,
    /// C/C++ `#include "x.h"` / `#include <x.h>`: an `imports_from` edge to the
    /// header's base name (extension stripped) as an external stub.
    CInclude,
    /// PHP `use A\B\C;`: an `imports` edge to the `\`-qualified name's tail.
    Php,
    /// Scala `import a.b.C`: an `imports` edge to the last path identifier.
    Scala,
}

/// Per-language type-annotation reference semantics (see [`ImportStyle`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeRefStyle {
    /// Python parameter/return annotations (`typed_parameter`, `return_type`).
    Python,
    /// TS parameter/return `type_annotation`s → `references` edges to the named
    /// types (primitives surface as `predefined_type` and are skipped).
    EcmaScript,
    /// Java method parameter/return types → `references` edges. Generic args are
    /// tagged `generic_arg`; primitive node types (`integral_type`,
    /// `floating_point_type`, `boolean_type`, `void_type`) are skipped.
    Java,
    /// C# method parameter/return types → `references` edges. Generic args
    /// (`type_argument_list`) are tagged `generic_arg`; `predefined_type`
    /// (`int`, `string`, …) is skipped.
    CSharp,
    /// Kotlin parameter (`function_value_parameters`) + return (the direct
    /// `user_type` child) types → `references`. `type_arguments` → `generic_arg`.
    Kotlin,
    /// Swift parameter + return types → `references`. Both arrive as a `name`
    /// field holding a `user_type` (the grammar reuses `name`). `type_arguments`
    /// → `generic_arg`.
    Swift,
    /// C/C++ parameter (via the `function_declarator`'s `parameter_list`) and
    /// return (the `function_definition`'s `type` field) types → `references`.
    /// `template_type` args → `generic_arg`; primitive node types are skipped.
    Cpp,
    /// PHP parameter (`simple_parameter.type`) + return (`return_type`) + property
    /// (`property_declaration.type`) types → `references` (`named_type` → its
    /// name; `primitive_type` skipped).
    Php,
    /// Scala parameter (`parameter.type`) + return (`return_type`) types →
    /// `references` (`type_identifier`; `generic_type` args → `generic_arg`).
    Scala,
}

/// Per-language class/interface heritage semantics. The walker matches on this
/// to emit `inherits` / `implements` edges from the grammar-specific heritage
/// nodes (Python uses `superclasses_field` instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeritageStyle {
    /// `class B extends A`, `class C implements I`, `interface X extends Y`:
    /// `class_heritage` → `extends_clause`/`implements_clause` (TS) or a direct
    /// base identifier (JS), plus `extends_type_clause` on interfaces.
    EcmaScript,
    /// Java `class B extends A` (`superclass`) → `inherits`; `class C implements
    /// I` (`super_interfaces`) → `implements`; `interface X extends Y`
    /// (`extends_interfaces`) → `inherits`. Generic args on a base are ignored.
    Java,
    /// C# `base_list` bases, classified `inherits` vs `implements` by a pre-scan
    /// of in-file `interface_declaration` names plus the `I`-prefix convention
    /// (`IFoo`). Generic args on a base are ignored.
    CSharp,
    /// Kotlin `delegation_specifiers`: a `constructor_invocation` base →
    /// `inherits` (superclass), a bare `user_type` base → `implements`.
    Kotlin,
    /// Swift `inheritance_specifier` bases, classified `inherits` vs `implements`
    /// by a pre-scan of in-file `protocol_declaration` names (protocols →
    /// `implements`, the one class base → `inherits`).
    Swift,
    /// C++ `base_class_clause` bases → `inherits` (C++ has no interfaces; a
    /// qualified base contributes its tail, a `template_type` its head).
    Cpp,
    /// PHP `base_clause` (`extends`) → `inherits`; `class_interface_clause`
    /// (`implements`) → `implements`. Names are taken by their `\`-tail.
    Php,
    /// Scala `extends A with B with C`: the `extends_clause`'s first type →
    /// `inherits`, the rest (mixed-in traits) → `mixes_in`.
    Scala,
}

/// Declarative description of one language's tree-sitter node vocabulary. Only
/// the fields needed by the current walker are modeled; more are additive.
pub struct LanguageConfig {
    /// Builds the tree-sitter language (e.g. `|| tree_sitter_python::LANGUAGE.into()`).
    pub language: fn() -> tree_sitter::Language,
    pub class_types: &'static [&'static str],
    pub function_types: &'static [&'static str],
    pub call_types: &'static [&'static str],
    pub name_field: &'static str,
    pub body_field: &'static str,
    /// Field on a `call` node naming the callee.
    pub call_function_field: &'static str,
    /// Member-access node types (e.g. `attribute`) whose accessor field names the method.
    pub call_accessor_node_types: &'static [&'static str],
    pub call_accessor_field: &'static str,
    /// Recursion stops at these types during the call pass (don't descend into nested fns).
    pub function_boundary_types: &'static [&'static str],
    /// Field naming the superclass list on a class node (`superclasses` for Python).
    pub superclasses_field: Option<&'static str>,
    /// Transparent wrapper node types (e.g. `decorated_definition`): the walker
    /// recurses into them preserving the parent-class scope, so decorated methods
    /// are not mis-scoped as module functions.
    pub decorated_types: &'static [&'static str],
    /// Callee names treated as language builtins and skipped.
    pub builtins: &'static [&'static str],
    /// Statement node types that declare imports (handled by `import_style`).
    pub import_types: &'static [&'static str],
    /// How to interpret `import_types` nodes (`None` = no import handling).
    pub import_style: Option<ImportStyle>,
    /// How to collect type-annotation references on functions (`None` = none).
    pub type_ref_style: Option<TypeRefStyle>,
    /// How to read class/interface heritage into `inherits`/`implements` edges
    /// (`None` = none; Python uses `superclasses_field` instead).
    pub heritage_style: Option<HeritageStyle>,
    /// Node type of a constructor call (`new_expression`) whose `constructor`
    /// field is the callee, resolved like a normal call (`None` = none).
    pub constructor_call_type: Option<&'static str>,
    /// Fallback body node kinds for grammars that attach the class/function body
    /// as a positional child rather than a named `body_field` (e.g. Kotlin's
    /// `class_body`/`function_body`). Tried only when `body_field` is absent;
    /// empty for grammars that expose a real body field.
    pub body_kinds: &'static [&'static str],
}
