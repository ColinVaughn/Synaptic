# Languages

CodeGraph extracts structure from a broad set of languages and config formats.
Most languages are parsed with a tree-sitter grammar; a few that lack a usable
grammar use a regex extractor instead, and the single-file web component formats
extract their script block and delegate to the JavaScript/TypeScript extractor.

Each language is gated behind a `lang-*` Cargo feature so a build only compiles
the grammars it needs. All `lang-*` features are on by default; the per-feature
note at the end explains how to build a slimmer binary. See [Configuration].

## What gets extracted

Across languages, extraction produces a common vocabulary of graph elements.
Exact coverage varies per language (listed below), but the shared kinds are:

Node kinds:

- File nodes (one per source file).
- Class / type nodes (classes, interfaces, traits, structs, enums, protocols,
  modules, and similar), labelled by their bare name.
- Function / method nodes, labelled `name()` for free functions and `.name()`
  for methods.
- External stub nodes for imported packages and referenced symbols defined
  outside the corpus, so edges to them survive the build's dangling-edge drop.
- Concept nodes for things like config keys, route names, and framework targets.
- Document nodes for Markdown files and headings; rationale nodes for Python
  docstrings and marked comments (`NOTE:`, `TODO:`, `FIXME:`, and so on).

Edge relations:

- `contains` - file contains a class/function; class contains nested items.
- `method` - class to its methods.
- `calls` - resolved intra-file call (cross-file calls are resolved in a later
  pass; see [Querying] and [Analysis-and-Reports]).
- `imports` / `imports_from` - import or include statements.
- `inherits` / `implements` / `mixes_in` - class and interface heritage.
- `references` - type references from parameters, returns, and fields
  (with context tags like `parameter_type`, `return_type`, `field`,
  `generic_arg`, `attribute`).
- `re_exports` (JavaScript/TypeScript), `embeds` (Go), `depends_on`
  (YAML/HCL), `reads_from` and `triggers` (SQL), and the framework edges
  described below.

## Tree-sitter languages

### Python (`.py`)

Classes and functions/methods; `import` and `from ... import` with relative
import resolution; `inherits` from base classes; parameter and return type
references; intra-file `calls`. Module, class, and function docstrings and
marked comments become rationale nodes (`rationale_for` edges). Auto-generated
files (protobuf, Alembic, Django migrations) skip docstring rationale.

### JavaScript / TypeScript (`.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.mts`, `.cts`, `.tsx`)

Functions, classes, and (TypeScript) interfaces, enums, type aliases, and
abstract classes. `imports_from` for import and `require` specifiers;
`re_exports` for `export { x } from 'm'`; dynamic imports (`import()`,
`require()`, `System.import()`). `inherits` / `implements` heritage; TypeScript
parameter and return type references (built-in utility types and primitives are
skipped); `calls` including member calls and `new` constructor calls. Relative
imports bind to real file nodes; TypeScript path aliases (`@/...`) resolve via
`tsconfig.json` / `jsconfig.json`.

### Go (`.go`)

Functions, methods, and struct/interface types; `imports_from` for packages;
`calls`; type `references` in fields and generics; `embeds` for anonymous field
embedding. Package-scoped types share one node across files.

### Rust (`.rs`)

Functions, methods, and struct/enum/trait types; `imports_from` for `use`;
`calls`; type `references` in fields, parameters, and returns; `implements` for
`impl Trait for T`; `inherits` for supertraits.

### Java (`.java`)

Classes and interfaces, methods and constructors; `imports` (tail of a dotted
name, wildcard imports use the package's last segment); `inherits` (extends),
`implements`; type `references` for parameters, returns, fields, generic args,
and `@`-annotations; `calls`.

### C# (`.cs`)

Classes, interfaces, structs, records; methods and constructors; `using`
imports; `inherits` vs `implements` classified by an in-file interface pre-scan
plus the `IFoo` naming convention; type `references` including `[Attr]`
attributes; `calls`.

### Kotlin (`.kt`, `.kts`)

Classes and objects, functions and methods; dotted-name imports; `inherits`
(constructor-invocation base) vs `implements` (bare type) from delegation
specifiers; parameter, return, and field type references; `calls` including
member calls.

### Swift (`.swift`)

Classes and protocols; functions, plus `init`, `deinit`, and `subscript`
members; module imports; `inherits` (class base) vs `implements` (protocols)
classified by an in-file protocol pre-scan; parameter, return, and field type
references; `calls`.

### Scala (`.scala`, `.sc`)

Classes, objects, and traits; methods; imports (last path component); `inherits`
(first superclass) and `mixes_in` (additional `with` traits); parameter and
return type references; `calls`.

### Groovy (`.groovy`, `.gradle`)

Classes, interfaces, and enums; methods; imports; `inherits`, `implements`;
`calls`. Reuses the Java extraction configuration (Groovy's grammar is
Java-shaped).

### C (`.c`, `.h`)

Functions; `#include` directives become `imports_from` to the header base name;
non-primitive parameter and return type `references` (primitives like `int` are
skipped); `calls`.

### C++ (`.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`)

Classes and structs; methods including out-of-line prototypes and data members;
`#include` becomes `imports_from`; `inherits` from base classes; parameter,
return, and field type `references`; `calls`.

### Ruby (`.rb`)

Classes and modules; methods; `inherits` from a superclass; `mixes_in` for
`include` / `extend` / `prepend`; `imports_from` for `require`,
`require_relative`, and `load`; `calls`.

### PHP (`.php`)

Classes, interfaces, traits, enums; methods and functions; `use A\B\C` imports
(tail); `inherits` / `implements`; property, parameter, and return type
references; `calls` including `$this->method()`. A second Laravel-aware pass adds
framework edges (see below).

### Lua (`.lua`)

Free functions, table/struct definitions, and table methods; `imports_from` for
`require()`; `calls`.

### Bash (`.sh`, `.bash`)

Functions; sourced scripts (`source` / `.`) become `imports_from`; `calls`.

### PowerShell (`.ps1`, `.psm1`)

Functions (for example `Get-Thing()`); `imports_from` for `Import-Module` and
`using`; `calls`.

### Elixir (`.ex`, `.exs`)

Modules and functions; `imports_from` for `alias`, `import`, `require`, and
`use`; `calls`.

### Julia (`.jl`)

Modules, struct and abstract types, and functions; `imports_from` for `using`
and `import`; `calls`.

### Zig (`.zig`)

Functions, types (struct/enum/union/opaque bound to a const), and methods;
`imports_from` for `@import()`; parameter and return type `references`; `calls`.

### Objective-C (`.m`, `.mm`)

Classes (unified from `@interface` and `@implementation`) and methods;
`imports_from` for imported headers; `inherits` from a superclass; intra-class
`calls` from message sends.

### Verilog / SystemVerilog (`.v`, `.sv`, `.vh`, `.svh`)

Modules, interfaces, and programs as container nodes; functions and tasks as
procedure nodes; `contains` and `method` edges. No call graph.

### Fortran (`.f90`, `.f95`, `.f03`, `.f08`, `.f`, `.for`)

Modules, programs, and submodules; subroutines and functions; `imports_from` for
`use`; `calls` (intrinsics like `print` / `write` / `read` are filtered).

## Config and data formats (tree-sitter)

### JSON (`.json`)

Recognizes known manifests (`package.json`, `tsconfig.json`, `jsconfig.json`,
`.eslintrc.json`, `composer.json`, `deno.json`) and any JSON with config keys.
Produces package/dependency nodes, config-key concept nodes (top level plus one
nested level, capped per file), and external reference nodes. Edges: `imports`
(dependencies), `extends` (extended configs), `references` (`$ref`), `contains`
(config keys). Returns empty for arbitrary data JSON.

### YAML (`.yaml`, `.yml`)

Recognizes CI, Compose, and Kubernetes config. Produces GitHub Actions job
nodes, Docker Compose service nodes, and Kubernetes resource nodes. Edges:
`contains` and `depends_on` (CI `needs:`, Compose `depends_on:`). Returns empty
for non-config YAML.

### HCL / Terraform (`.tf`, `.tfvars`, `.hcl`)

Block-address nodes for `resource`, `variable`, `output`, `module`, `data`,
`provider`, and `locals`. Edges: `contains`, `references` (interpolated
attribute values to blocks), and `depends_on` (explicit `depends_on`). Ids are
directory-scoped for cross-file resolution within a module.

### SQL (`.sql`)

Hybrid tree-sitter plus regex recovery. Nodes for `CREATE TABLE` / `VIEW` /
`FUNCTION` / `PROCEDURE` / `TRIGGER`. Edges: `contains`, `references` (foreign
keys via `REFERENCES`), `reads_from` (`FROM` / `JOIN`), and `triggers` (trigger
to its table). The regex pass recovers procedures and triggers the grammar fails
to parse.

## Regex-based languages

These have no usable tree-sitter grammar and are extracted with regular
expressions.

### Classic ASP / VBScript (`.asp`, `.asa`)

`Function` and `Sub` definitions, `Class` definitions; `contains` edges;
`imports_from` for `<!--#include-->`; `calls` between defined routines. No type
references (dynamically typed).

### Salesforce Apex (`.cls`, `.trigger`)

Classes, interfaces, enums, methods, and triggers; SObjects as concept nodes.
Edges: `contains`, `method` (class to method, nearest enclosing class), and
`triggers` (trigger to its SObject). Calls are intentionally not emitted (too
noisy over regex). The tree-sitter Apex grammar pins an incompatible tree-sitter
version, so the regex extractor is used.

### Pascal / Delphi (`.pas`, `.pp`, `.dpr`, `.dpk`, `.lpr`)

Type definitions (`class`, `record`, `interface`, `object`), functions and
procedures including qualified method implementations (`TFoo.Bar`). Edges:
`imports_from` for `uses` clauses and `contains` for declarations. Comments are
stripped before scanning.

### Markdown (`.md`, `.mdx`, `.qmd`)

Structural extraction only: the file node and a heading node per `#`...`######`
heading, with `contains` edges nesting each heading under the nearest shallower
one. Fenced code blocks are skipped. This pass runs unconditionally, separate
from the optional LLM concept pass over the same documents (see
[Semantic-Analysis]). Markdown files are classified as documents, not code.

## XML and solution files

### .NET projects and solutions (`.csproj`, `.fsproj`, `.vbproj`, `.sln`, `.slnx`)

Project files and `.slnx` are parsed as XML (with entity-expansion screening);
the legacy `.sln` format is parsed with regex. Nodes: the project file, NuGet
package nodes (`<PackageReference>`), SDK and target-framework concept nodes, and
solution project nodes. Edges: `imports` (package / project references),
`references` (target framework / SDK), and `contains` (solution to projects).
This is project metadata, not a source language.

## Web component formats (delegating)

### Vue / Svelte / Astro (`.vue`, `.svelte`, `.astro`)

These extract the component's script: the first `<script>` block for Vue and
Svelte (using TypeScript when `lang="ts"` is present, otherwise JavaScript), or
the `---` frontmatter for Astro (always TypeScript). The extracted script is
newline-padded to preserve line numbers and handed to the JavaScript/TypeScript
extractor, so the resulting nodes and edges are exactly what that extractor
produces. A component with no script block yields nothing.

### Razor / Blazor (`.razor`, `.cshtml`)

The `@code` / `@functions` block is extracted, wrapped in a synthetic class named
after the file, and handed to the C# extractor, producing a component class node
with its member methods and properties.

## Framework-aware edges

Two extractors add framework-specific edges on top of the structural graph.

PHP / Laravel:

- `config('x.y')` to a config-key concept node via `uses_config`.
- `$app->bind(A::class, B::class)` as `A bound_to B`.
- `protected $listen = [Event::class => [Listener::class]]` as
  `Event listened_by Listener`.
- `Foo::$bar` via `uses_static_prop`; `Foo::BAR` via `references_constant`.

Dart / Flutter (regex heuristics, attached to the innermost enclosing
method or class):

- Navigation: string routes and route objects via `navigates` (route concept
  nodes).
- Riverpod `ref.watch/read/listen(...)` and Bloc widget bindings via
  `references`.
- Bloc event and state flow (`on<Event>`, `emit(State)`, `bloc.add(Event)`)
  via `calls`; type lookups (`context.read<Bloc>()`) via `references`.

Beyond these single-language framework edges, a separate post-pass adds
**cross-language** edges (`invokes`, `binds_native`, `calls_service`,
`handled_by`) for subprocess calls, FFI bindings, and HTTP/gRPC routes that span
languages. See [Cross-Language-Edges](Cross-Language-Edges).

## Per-language feature flags

Every language is behind a `lang-*` Cargo feature, all enabled by default. The
feature names follow the language: `lang-python`, `lang-typescript`,
`lang-rust`, `lang-go`, `lang-java`, `lang-csharp`, `lang-kotlin`,
`lang-swift`, `lang-c`, `lang-cpp`, `lang-ruby`, `lang-php`, `lang-lua`,
`lang-bash`, `lang-powershell`, `lang-scala`, `lang-elixir`, `lang-julia`,
`lang-zig`, `lang-dart`, `lang-objc`, `lang-verilog`, `lang-fortran`, `lang-groovy`,
`lang-json`, `lang-yaml`, `lang-hcl`, `lang-sql`, `lang-asp`, `lang-apex`,
`lang-pascal`, `lang-markdown`, `lang-dotnet`, `lang-razor`, `lang-vue`,
`lang-svelte`, `lang-astro`, and the JavaScript pair `lang-javascript` /
`lang-typescript`.

The Vue, Svelte, and Astro features pull in `lang-javascript` and
`lang-typescript`; `lang-razor` pulls in `lang-csharp`. A file whose language
feature is disabled at build time is simply not extracted (its extension routes
to nothing). To build a smaller binary with only the languages you need, disable
default features and select a subset:

```
cargo build --release --no-default-features \
  --features lang-python,lang-typescript,lang-rust
```

See [Configuration] for more on build-time options and [Extraction] for how
discovered files are routed to these extractors.
