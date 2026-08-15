//! Synaptic extraction: tree-sitter walkers that turn source files into
//! `synaptic-core` nodes and edges. Languages live behind `lang-*` cargo
//! features so a build only compiles the grammars it needs.

use std::path::Path;
use std::sync::LazyLock;

/// Parser walks are depth-bounded, but generated fixtures in very large
/// repositories can still require more than Rayon's small platform-default
/// worker stack. This mirrors the CLI's 64 MiB worker and, unlike
/// `RUST_MIN_STACK`, applies deterministically without requiring user setup.
const EXTRACTION_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Ceiling on `workers * EXTRACTION_STACK_BYTES`. Rayon defaults to one worker
/// per core, so the pool reserved 64 MiB x core-count of stack before parsing a
/// single file -- 1 GiB on a 16-core box, 4 GiB on a 64-core one. Reservation is
/// not resident memory, but it is charged against the Windows commit limit and
/// shows up in every memory profile, so the worker count is capped instead of
/// the per-worker stack (which large generated fixtures genuinely need).
const MAX_TOTAL_STACK_BYTES: usize = 1024 * 1024 * 1024;

/// Explicit worker-count override: `SYNAPTIC_EXTRACT_THREADS`. `None` for unset,
/// zero, or unparseable, so a bad value falls back to the computed default
/// rather than deadlocking a zero-thread pool.
fn parse_thread_override(raw: Option<String>) -> Option<usize> {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
}

/// Worker count for the extraction pool: one per core, capped so total stack
/// reservation stays under [`MAX_TOTAL_STACK_BYTES`], and always at least one.
fn extraction_threads() -> usize {
    if let Some(n) = parse_thread_override(std::env::var("SYNAPTIC_EXTRACT_THREADS").ok()) {
        return n;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let cap = (MAX_TOTAL_STACK_BYTES / EXTRACTION_STACK_BYTES).max(1);
    cores.min(cap).max(1)
}

static EXTRACTION_POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(extraction_threads())
        .stack_size(EXTRACTION_STACK_BYTES)
        .thread_name(|index| format!("synaptic-extract-{index}"))
        .build()
        .expect("build Synaptic extraction thread pool")
});

/// Run parallel file extraction on workers with parser-safe stacks.
pub fn with_extraction_pool<R: Send>(operation: impl FnOnce() -> R + Send) -> R {
    EXTRACTION_POOL.install(operation)
}

pub mod cache;
pub mod config;
#[cfg(feature = "cross-language")]
pub mod crosslang;
#[cfg(feature = "cross-language")]
pub mod dynamic;
pub mod paths;
pub mod python;
#[cfg(feature = "decl-recovery")]
mod recovery;
pub mod resolve;
pub mod result;
pub mod signature;
pub mod tsconfig;
pub mod walker;

#[cfg(feature = "lang-apex")]
pub mod apex;
#[cfg(feature = "lang-asp")]
pub mod asp;
#[cfg(feature = "lang-bash")]
pub mod bash;
#[cfg(feature = "lang-c")]
pub mod c;
#[cfg(any(
    feature = "lang-go",
    feature = "lang-rust",
    feature = "lang-json",
    feature = "lang-yaml",
    feature = "lang-hcl",
    feature = "lang-sql",
    feature = "lang-ruby",
    feature = "lang-lua",
    feature = "lang-bash",
    feature = "lang-powershell",
    feature = "lang-dart",
    feature = "lang-elixir",
    feature = "lang-julia",
    feature = "lang-zig",
    feature = "lang-asp",
    feature = "lang-objc",
    feature = "lang-verilog",
    feature = "lang-fortran",
    feature = "lang-ql",
    feature = "lang-dotnet",
    feature = "lang-markdown",
    feature = "lang-apex",
    feature = "lang-pascal",
    feature = "lang-php",
    // Not a language: `recovery` builds its nodes through the same `Builder`, and
    // it is reachable from languages (groovy) that are not otherwise listed here.
    feature = "decl-recovery"
))]
pub(crate) mod common;
#[cfg(feature = "lang-cpp")]
pub mod cpp;
#[cfg(feature = "lang-csharp")]
pub mod csharp;
#[cfg(feature = "lang-dart")]
pub mod dart;
#[cfg(feature = "lang-sql")]
mod dbt;
#[cfg(feature = "lang-dotnet")]
pub mod dotnet;
#[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
pub mod ecmascript;
#[cfg(feature = "lang-elixir")]
pub mod elixir;
#[cfg(feature = "lang-fortran")]
pub mod fortran;
#[cfg(feature = "lang-go")]
pub mod go;
#[cfg(feature = "lang-groovy")]
pub mod groovy;
#[cfg(feature = "lang-hcl")]
pub mod hcl;
#[cfg(feature = "lang-java")]
pub mod java;
#[cfg(feature = "lang-json")]
pub mod json;
#[cfg(feature = "lang-julia")]
pub mod julia;
#[cfg(feature = "lang-kotlin")]
pub mod kotlin;
#[cfg(feature = "lang-lua")]
pub mod lua;
#[cfg(feature = "lang-markdown")]
pub mod markdown;
#[cfg(feature = "lang-objc")]
pub mod objc;
#[cfg(feature = "lang-pascal")]
pub mod pascal;
#[cfg(feature = "lang-php")]
pub mod php;
#[cfg(feature = "lang-powershell")]
pub mod powershell;
#[cfg(feature = "lang-ql")]
pub mod ql;
#[cfg(feature = "lang-razor")]
pub mod razor;
#[cfg(feature = "lang-json")]
pub mod resource;
#[cfg(feature = "lang-ruby")]
pub mod ruby;
#[cfg(feature = "lang-rust")]
pub mod rust;
#[cfg(feature = "lang-scala")]
pub mod scala;
#[cfg(feature = "lang-sql")]
pub mod sql;
#[cfg(feature = "lang-sql")]
mod sql_semantic;
#[cfg(feature = "lang-swift")]
pub mod swift;
#[cfg(feature = "lang-verilog")]
pub mod verilog;
#[cfg(any(feature = "lang-vue", feature = "lang-svelte", feature = "lang-astro"))]
pub mod webframework;
#[cfg(feature = "lang-yaml")]
pub mod yaml;
#[cfg(feature = "lang-zig")]
pub mod zig;

pub use cache::{AST_CACHE_VERSION, cached_extract_source};
pub use config::{ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "cross-language")]
pub use crosslang::prune_local_sdk_candidates;
pub use resolve::{ResolveStats, attach_cpp_methods, resolve_imports, resolve_relative_imports};
#[cfg(feature = "lang-json")]
pub use resource::{
    ResourceResolveStats, emit_resources, extract_resource_source, resolve_resource_refs,
    set_emit_resources,
};
pub use result::{ExtractionResult, ImportRecord, RawCall};
#[cfg(feature = "lang-sql")]
pub use sql_semantic::{emit_sql_columns, set_emit_sql_columns};
pub use tsconfig::{AliasResolver, load_alias_resolver};
pub use walker::extract_with_config;

/// `.h` is shared by C and C++. Prefer C++ only when the source contains a
/// declaration form C cannot express; plain C headers keep the C grammar.
#[cfg(feature = "lang-cpp")]
/// True when a `.h` is an Objective-C header. Checked before the C/C++ sniff,
/// which only ever chose between C and C++ — an Obj-C header parses as neither, so
/// the public interface of an Obj-C project (its `@interface`/`@protocol`
/// declarations) yielded no symbols at all. Keyed on `@`-directives that have no
/// meaning in C or C++, so plain headers are unaffected.
#[cfg(feature = "lang-objc")]
fn looks_like_objc_header(source: &[u8]) -> bool {
    String::from_utf8_lossy(source).lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("@interface")
            || line.starts_with("@protocol")
            || line.starts_with("@implementation")
            || line.starts_with("@end")
    })
}

fn looks_like_cpp_header(source: &[u8]) -> bool {
    String::from_utf8_lossy(source).lines().any(|line| {
        let line = line.trim_start();
        if line.starts_with("//") || line.starts_with("/*") || line.starts_with('*') {
            return false;
        }
        line.starts_with("class ")
            || line.starts_with("struct ")
            || line.starts_with("namespace ")
            || line.starts_with("template<")
            || line.starts_with("template <")
            || line.starts_with("using ")
            || line.contains("enum class ")
            || line.contains("::")
            || matches!(line, "public:" | "protected:" | "private:")
            || [
                "UCLASS(",
                "UENUM(",
                "UFUNCTION(",
                "UINTERFACE(",
                "UPROPERTY(",
                "USTRUCT(",
                "GENERATED_BODY(",
            ]
            .iter()
            .any(|signal| line.contains(signal))
    })
}

#[cfg(feature = "lang-python")]
pub use python::{extract_python_file, extract_python_source};

/// Extract in-memory source by file extension, dispatching to the matching
/// language extractor. Returns `None` for unsupported (or feature-disabled)
/// extensions.
#[cfg_attr(
    not(any(
        feature = "lang-python",
        feature = "lang-javascript",
        feature = "lang-typescript"
    )),
    allow(unused_variables)
)]
pub fn extract_source(path: &str, source: &[u8]) -> Option<ExtractionResult> {
    // Case-folded: `.F90` (preprocessed Fortran), `.PAS`, `.CLS` and friends are
    // ordinary spellings, and a case-sensitive match routes them to no extractor at
    // all — the file is counted as code and then silently yields nothing. The Unix
    // `.C`-means-C++ convention is deliberately not honoured here; folding it to the
    // C extractor beats dropping the file, and ambiguous `.h` already content-sniffs.
    let ext_owned = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    let ext = ext_owned.as_str();
    #[allow(unused_mut)]
    let mut result = (match ext {
        #[cfg(feature = "lang-python")]
        "py" => Some(python::extract_python_source(path, source)),
        #[cfg(feature = "lang-javascript")]
        "js" | "jsx" | "mjs" | "cjs" => Some(ecmascript::extract_js_source(path, source)),
        #[cfg(feature = "lang-typescript")]
        "ts" | "mts" | "cts" => Some(ecmascript::extract_ts_source(path, source)),
        #[cfg(feature = "lang-typescript")]
        "tsx" => Some(ecmascript::extract_tsx_source(path, source)),
        #[cfg(feature = "lang-go")]
        "go" => Some(go::extract_go_source(path, source)),
        #[cfg(feature = "lang-rust")]
        "rs" => Some(rust::extract_rust_source(path, source)),
        #[cfg(feature = "lang-java")]
        "java" => Some(java::extract_java_source(path, source)),
        #[cfg(feature = "lang-csharp")]
        "cs" => Some(csharp::extract_csharp_source(path, source)),
        #[cfg(feature = "lang-kotlin")]
        "kt" | "kts" => Some(kotlin::extract_kotlin_source(path, source)),
        #[cfg(feature = "lang-swift")]
        "swift" => Some(swift::extract_swift_source(path, source)),
        #[cfg(feature = "lang-c")]
        "c" => Some(c::extract_c_source(path, source)),
        #[cfg(all(feature = "lang-c", feature = "lang-cpp", feature = "lang-objc"))]
        "h" => Some(if looks_like_objc_header(source) {
            objc::extract_objc_source(path, source)
        } else if looks_like_cpp_header(source) {
            cpp::extract_cpp_source(path, source)
        } else {
            c::extract_c_source(path, source)
        }),
        #[cfg(all(feature = "lang-c", feature = "lang-cpp", not(feature = "lang-objc")))]
        "h" => Some(if looks_like_cpp_header(source) {
            cpp::extract_cpp_source(path, source)
        } else {
            c::extract_c_source(path, source)
        }),
        #[cfg(all(feature = "lang-c", not(feature = "lang-cpp")))]
        "h" => Some(c::extract_c_source(path, source)),
        #[cfg(all(feature = "lang-cpp", not(feature = "lang-c")))]
        "h" => Some(cpp::extract_cpp_source(path, source)),
        #[cfg(feature = "lang-cpp")]
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some(cpp::extract_cpp_source(path, source)),
        #[cfg(feature = "lang-json")]
        "json" | "uproject" | "uplugin" => Some(json::extract_json_source(path, source)),
        #[cfg(feature = "lang-json")]
        // ponytail: Unreal packages stay opaque; add a real package parser when
        // Blueprint symbol/bytecode coverage is required.
        "uasset" | "umap" => crate::resource::emit_resources()
            .then(|| crate::resource::extract_resource_source(path, b"")),
        // `.mcmeta` is JSON-syntax resource metadata, never config -> resource node
        // (gated by the resource toggle, like data JSON).
        #[cfg(feature = "lang-json")]
        "mcmeta" => {
            resource::emit_resources().then(|| resource::extract_resource_source(path, source))
        }
        #[cfg(feature = "lang-yaml")]
        "yaml" | "yml" => Some(yaml::extract_yaml_source(path, source)),
        #[cfg(feature = "lang-hcl")]
        "tf" | "tfvars" | "hcl" => Some(hcl::extract_hcl_source(path, source)),
        #[cfg(feature = "lang-sql")]
        "sql" => Some(sql::extract_sql_source(path, source)),
        #[cfg(feature = "lang-bash")]
        "sh" | "bash" => Some(bash::extract_bash_source(path, source)),
        #[cfg(feature = "lang-lua")]
        "lua" => Some(lua::extract_lua_source(path, source)),
        #[cfg(feature = "lang-ruby")]
        "rb" => Some(ruby::extract_ruby_source(path, source)),
        #[cfg(feature = "lang-powershell")]
        "ps1" | "psm1" => Some(powershell::extract_powershell_source(path, source)),
        #[cfg(feature = "lang-php")]
        "php" => Some(php::extract_php_source(path, source)),
        #[cfg(feature = "lang-scala")]
        "scala" | "sc" => Some(scala::extract_scala_source(path, source)),
        #[cfg(feature = "lang-dart")]
        "dart" => Some(dart::extract_dart_source(path, source)),
        #[cfg(feature = "lang-elixir")]
        "ex" | "exs" => Some(elixir::extract_elixir_source(path, source)),
        #[cfg(feature = "lang-julia")]
        "jl" => Some(julia::extract_julia_source(path, source)),
        #[cfg(feature = "lang-zig")]
        "zig" => Some(zig::extract_zig_source(path, source)),
        #[cfg(feature = "lang-asp")]
        "asp" | "asa" => Some(asp::extract_asp_source(path, source)),
        #[cfg(feature = "lang-groovy")]
        "groovy" | "gradle" => Some(groovy::extract_groovy_source(path, source)),
        #[cfg(feature = "lang-objc")]
        "m" | "mm" => Some(objc::extract_objc_source(path, source)),
        #[cfg(feature = "lang-fortran")]
        "f90" | "f95" | "f03" | "f08" | "f" | "for" => {
            Some(fortran::extract_fortran_source(path, source))
        }
        #[cfg(feature = "lang-ql")]
        "ql" | "qll" => Some(ql::extract_ql_source(path, source)),
        #[cfg(feature = "lang-verilog")]
        "v" | "sv" | "vh" | "svh" => Some(verilog::extract_verilog_source(path, source)),
        #[cfg(feature = "lang-vue")]
        "vue" => Some(webframework::extract_vue_source(path, source)),
        #[cfg(feature = "lang-svelte")]
        "svelte" => Some(webframework::extract_svelte_source(path, source)),
        #[cfg(feature = "lang-astro")]
        "astro" => Some(webframework::extract_astro_source(path, source)),
        #[cfg(feature = "lang-dotnet")]
        "csproj" | "fsproj" | "vbproj" | "sln" | "slnx" | "xaml" => {
            Some(dotnet::extract_dotnet_source(path, source))
        }
        #[cfg(feature = "lang-markdown")]
        "md" | "mdx" | "qmd" => Some(markdown::extract_markdown_source(path, source)),
        #[cfg(feature = "lang-apex")]
        "cls" | "trigger" => Some(apex::extract_apex_source(path, source)),
        #[cfg(feature = "lang-pascal")]
        "pas" | "pp" | "dpr" | "dpk" | "lpr" => Some(pascal::extract_pascal_source(path, source)),
        #[cfg(feature = "lang-razor")]
        "razor" | "cshtml" => Some(razor::extract_razor_source(path, source)),
        _ => None,
    })?;
    // Stamp incomplete extraction onto the file node. The console summary scrolls
    // away; the graph is what queries and MCP tools read, and without this a file
    // the grammar could not understand is indistinguishable from a file that simply
    // declares nothing. Only stamped when true, so clean graphs are unchanged.
    if result.parse_error {
        // Bounded recovery first: when the grammar errored and not one symbol came
        // out, scan for declaration-shaped lines so the file is not a silent hole.
        // No-op unless the file was a total loss (see `recovery::apply`).
        #[cfg(feature = "decl-recovery")]
        recovery::apply(path, ext, source, &mut result);
        let fid = paths::file_node_id(path);
        if let Some(file) = result.nodes.iter_mut().find(|n| n.id == fid) {
            file.extra
                .insert("parse_error".into(), serde_json::Value::Bool(true));
        }
    }
    #[cfg(feature = "cross-language")]
    crosslang::augment(path, source, &mut result);
    Some(result)
}

/// Extract a file from disk by extension. `Ok(None)` for unsupported extensions.
pub fn extract_file(path: &Path) -> std::io::Result<Option<ExtractionResult>> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_source(&path_str, &source))
}

#[cfg(test)]
mod fuzz_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every extension `synaptic-detect` classifies as `Code` must have an
    /// `extract_source` arm — otherwise those files are counted into corpus stats
    /// and then silently produce zero nodes (the detect/extract "drift" bug).
    /// Requires the default feature set (all `lang-*` on), so it is skipped in
    /// the per-language `--no-default-features --features lang-X` CI builds where
    /// only one extractor is compiled in.
    #[cfg(feature = "default")]
    #[test]
    fn every_detected_code_extension_has_an_extractor() {
        let orphans: Vec<&str> = synaptic_detect::file_type::CODE_EXTENSIONS
            .iter()
            .copied()
            .filter(|ext| extract_source(&format!("probe.{ext}"), b"\n").is_none())
            .collect();
        assert!(
            orphans.is_empty(),
            "extensions classified as Code but with no extractor (silent drop): {orphans:?}"
        );
    }

    /// `.F90` is the conventional spelling for preprocessed Fortran, and
    /// `.PAS`/`.CLS` turn up in Delphi and Salesforce trees exported from
    /// case-insensitive filesystems. Extension dispatch must not be case-sensitive
    /// or those files route to no extractor and vanish without a diagnostic.
    #[cfg(feature = "lang-fortran")]
    #[test]
    fn dispatch_routes_uppercase_fortran_extension() {
        let src = b"module m\ncontains\nsubroutine go()\nend subroutine go\nend module m\n";
        let upper = extract_source("src/thing.F90", src).expect("uppercase .F90 must extract");
        let lower = extract_source("src/thing.f90", src).expect("lowercase .f90 must extract");
        assert!(
            upper.nodes.iter().any(|n| n.label == ".go()"),
            "{:?}",
            upper.nodes.iter().map(|n| &n.label).collect::<Vec<_>>()
        );
        assert_eq!(upper.nodes.len(), lower.nodes.len());
    }

    #[cfg(feature = "default")]
    #[test]
    fn dispatch_is_case_insensitive_for_every_code_extension() {
        let orphans: Vec<String> = synaptic_detect::file_type::CODE_EXTENSIONS
            .iter()
            .map(|ext| ext.to_uppercase())
            .filter(|ext| extract_source(&format!("probe.{ext}"), b"\n").is_none())
            .collect();
        assert!(
            orphans.is_empty(),
            "uppercase extensions with no extractor (silent drop): {orphans:?}"
        );
    }

    /// `.h` sniffing only ever chose between C and C++, so Objective-C headers —
    /// where the public interface of an Obj-C project actually lives — parsed as
    /// neither and yielded no symbols at all.
    #[cfg(all(feature = "lang-objc", feature = "lang-c"))]
    #[test]
    fn objc_header_routes_to_the_objc_extractor() {
        let src = b"#import <Foundation/Foundation.h>\n\n@interface AFSecurityPolicy : NSObject\n- (BOOL)evaluateServerTrust:(SecTrustRef)trust;\n@end\n";
        let r = extract_source("AFNetworking/AFSecurityPolicy.h", src).expect("header extracts");
        let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(
            labels.contains(&"AFSecurityPolicy"),
            "expected the Obj-C class, got {labels:?}"
        );
        assert!(!r.parse_error, "Obj-C header should parse cleanly");
    }

    /// The Obj-C sniff must not steal plain C or C++ headers.
    #[cfg(all(feature = "lang-c", feature = "lang-cpp"))]
    #[test]
    fn objc_sniff_leaves_c_and_cpp_headers_alone() {
        let cpp = extract_source("g/Hero.h", b"class Hero {\npublic:\n  void Run();\n};\n")
            .expect("cpp header");
        assert!(cpp.nodes.iter().any(|n| n.label == "Hero"));
        let c = extract_source("g/util.h", b"int add(int a, int b) { return a + b; }\n")
            .expect("c header");
        assert!(
            c.nodes.iter().any(|n| n.label.contains("add")),
            "{:?}",
            c.nodes.iter().map(|n| &n.label).collect::<Vec<_>>()
        );
    }

    /// A console line scrolls away; the graph is what downstream tools read. A file
    /// whose extraction is incomplete must say so on its own file node, so a query
    /// can tell "no symbols here" from "no symbols understood here".
    #[cfg(feature = "lang-objc")]
    #[test]
    fn incomplete_extraction_is_stamped_on_the_file_node() {
        let broken = extract_source(
            "src/Broken.m",
            b"@interface Broken : NSObject\n@property (nonatomic) *** ;;; @@@\n@end\n",
        )
        .expect("extracts");
        let file = broken
            .nodes
            .iter()
            .find(|n| n.label == "Broken.m")
            .expect("file node");
        assert_eq!(
            file.extra.get("parse_error").and_then(|v| v.as_bool()),
            Some(true),
            "file node extra: {:?}",
            file.extra
        );

        let clean = extract_source(
            "src/Ok.m",
            b"@interface Ok : NSObject\n@end\n@implementation Ok\n- (void)go { }\n@end\n",
        )
        .expect("extracts");
        let file = clean
            .nodes
            .iter()
            .find(|n| n.label == "Ok.m")
            .expect("file node");
        assert!(
            !file.extra.contains_key("parse_error"),
            "clean parse must not be stamped: {:?}",
            file.extra
        );
    }

    /// A `generate`-style `if … begin : label` whose body is a macro invocation
    /// defeats tree-sitter-verilog outright — the real pattern behind 156 total
    /// losses across pulp-platform/axi, Cores-VeeR-EL2 and cva6. The module must
    /// still reach the graph, marked as recovered rather than parsed.
    #[cfg(feature = "lang-verilog")]
    #[test]
    fn a_module_the_grammar_cannot_parse_is_recovered() {
        let src = b"module m_gen #(\n  parameter int unsigned NoMstPorts = 32'd0\n) (\n  input logic clk\n);\n  if (NoMstPorts == 32'h1) begin : gen_no_demux\n    `AXI_ASSIGN_REQ_STRUCT(a, b)\n  end else begin\n    logic x;\n  end\nendmodule\n";
        let r = extract_source("src/m_gen.sv", src).expect("extracts");
        assert!(r.parse_error, "fixture must actually defeat the grammar");
        let m = r
            .nodes
            .iter()
            .find(|n| n.label == "m_gen")
            .unwrap_or_else(|| {
                panic!(
                    "module lost: {:?}",
                    r.nodes.iter().map(|n| &n.label).collect::<Vec<_>>()
                )
            });
        assert_eq!(m.source_location, Some("L1".to_string()));
        assert_eq!(
            m.extra.get("recovered").and_then(|v| v.as_bool()),
            Some(true),
            "recovered symbols must be marked"
        );
    }

    /// Recovery must never second-guess a grammar that produced something. A file
    /// that parsed keeps exactly the structure the walk found.
    #[cfg(feature = "lang-verilog")]
    #[test]
    fn a_module_that_parses_is_not_touched_by_recovery() {
        let r = extract_source(
            "src/ok.sv",
            b"module ok (input logic clk);\nendmodule\n\nmodule second (input logic c);\nendmodule\n",
        )
        .expect("extracts");
        assert!(!r.parse_error);
        assert!(
            r.nodes.iter().all(|n| !n.extra.contains_key("recovered")),
            "clean parse must have no recovered nodes"
        );
        assert_eq!(r.nodes.iter().filter(|n| n.label == "ok").count(), 1);
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn dispatch_routes_py_extension() {
        let r = extract_source("a/b.py", b"def f():\n    return 1\n").unwrap();
        assert!(r.nodes.iter().any(|n| n.label == "f()"));
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn dispatch_routes_js_extension() {
        let r = extract_source("a/b.js", b"function f() { return 1; }\n").unwrap();
        assert!(r.nodes.iter().any(|n| n.label == "f()"));
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn dispatch_routes_ts_and_tsx_extensions() {
        assert!(extract_source("a/b.ts", b"function f(): number { return 1; }\n").is_some());
        assert!(extract_source("a/b.tsx", b"function C() { return null; }\n").is_some());
    }

    #[cfg(all(feature = "lang-c", feature = "lang-cpp"))]
    #[test]
    fn h_dispatch_sniffs_c_vs_cpp() {
        let cpp = extract_source(
            "Source/Game/Hero.h",
            b"UCLASS()\nclass GAME_API AHero : public AActor { GENERATED_BODY() };\n",
        )
        .unwrap();
        assert!(cpp.nodes.iter().any(|node| node.label == "AHero"));

        let c = extract_source("include/math.h", b"int add(int a, int b);\n").unwrap();
        assert!(c.nodes.iter().all(|node| node.label != "AHero"));
    }

    #[cfg(feature = "lang-json")]
    #[test]
    fn unreal_extensions_are_dispatched() {
        let project = extract_source(
            "Game.uproject",
            br#"{"FileVersion":3,"Modules":[{"Name":"Game","Type":"Runtime"}]}"#,
        )
        .unwrap();
        assert!(project.nodes.iter().any(|node| node.label == "Game"));

        let _guard = resource::RESOURCE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        resource::set_emit_resources(true);
        let asset = extract_source("Content/BP_Hero.uasset", b"binary").unwrap();
        assert_eq!(asset.nodes.len(), 1);
        assert_eq!(asset.nodes[0].label, "Content/BP_Hero.uasset");
    }

    #[cfg(feature = "lang-dotnet")]
    #[test]
    fn xaml_links_code_behind_resources_and_event_handlers() {
        let result = extract_source(
            "Views/MainWindow.xaml",
            br#"<Window xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
                       x:Class="ConsumerVpn.MainWindow" Loaded="OnLoaded">
                    <ResourceDictionary Source="Themes/Colors.xaml" />
                    <Button Click="Connect_Click" />
                </Window>"#,
        )
        .expect("XAML extraction");

        assert!(result.edges.iter().any(|edge| {
            edge.relation == "references" && edge.context.as_deref() == Some("xaml_code_behind")
        }));
        assert!(result.edges.iter().any(|edge| {
            edge.relation == "imports" && edge.context.as_deref() == Some("xaml_resource")
        }));
        assert_eq!(
            result
                .raw_calls
                .iter()
                .map(|call| call.callee.as_str())
                .collect::<Vec<_>>(),
            vec!["MainWindow.OnLoaded", "MainWindow.Connect_Click"]
        );
    }

    #[test]
    fn dispatch_ignores_unknown_extension() {
        assert!(extract_source("a/b.zzz", b"x").is_none());
        assert!(extract_source("noext", b"x").is_none());
    }

    #[test]
    fn extraction_pool_executes_work() {
        assert_eq!(with_extraction_pool(|| 6 * 7), 42);
    }

    /// 64 MiB per worker times one worker per core reserved 4 GiB on a 64-core
    /// machine. Total reservation must stay bounded regardless of core count.
    #[test]
    fn extraction_pool_bounds_total_stack_reservation() {
        let threads = extraction_threads();
        assert!(threads >= 1, "at least one worker");
        assert!(
            threads * EXTRACTION_STACK_BYTES <= MAX_TOTAL_STACK_BYTES,
            "total reserved stack {} exceeds the {} cap ({threads} workers)",
            threads * EXTRACTION_STACK_BYTES,
            MAX_TOTAL_STACK_BYTES
        );
    }

    /// A bad override must fall back to the computed default, never to a
    /// zero-thread pool.
    #[test]
    fn extraction_thread_override_rejects_zero_and_garbage() {
        assert_eq!(parse_thread_override(Some("4".into())), Some(4));
        assert_eq!(parse_thread_override(Some("  8 ".into())), Some(8));
        assert_eq!(parse_thread_override(Some("0".into())), None);
        assert_eq!(parse_thread_override(Some("nonsense".into())), None);
        assert_eq!(parse_thread_override(Some(String::new())), None);
        assert_eq!(parse_thread_override(None), None);
    }

    #[cfg(feature = "lang-json")]
    #[test]
    fn dispatch_routes_mcmeta_as_resource() {
        let _g = crate::resource::RESOURCE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::resource::set_emit_resources(true);
        let r = extract_source("pack.mcmeta", br#"{"pack":{"pack_format":15}}"#)
            .expect("mcmeta dispatched when resources on");
        assert!(
            r.nodes
                .iter()
                .any(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("resource"))
        );
        crate::resource::set_emit_resources(false);
        assert!(
            extract_source("pack.mcmeta", b"{}").is_none(),
            "mcmeta not dispatched when resources off"
        );
        crate::resource::set_emit_resources(true);
    }
}
