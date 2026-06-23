//! Synaptic extraction: tree-sitter walkers that turn source files into
//! `synaptic-core` nodes and edges. Languages live behind `lang-*` cargo
//! features so a build only compiles the grammars it needs.

use std::path::Path;

pub mod cache;
pub mod config;
#[cfg(feature = "cross-language")]
pub mod crosslang;
#[cfg(feature = "cross-language")]
pub mod dynamic;
pub mod paths;
pub mod python;
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
    feature = "lang-dotnet",
    feature = "lang-markdown",
    feature = "lang-apex",
    feature = "lang-pascal",
    feature = "lang-php"
))]
pub(crate) mod common;
#[cfg(feature = "lang-cpp")]
pub mod cpp;
#[cfg(feature = "lang-csharp")]
pub mod csharp;
#[cfg(feature = "lang-dart")]
pub mod dart;
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
#[cfg(feature = "lang-razor")]
pub mod razor;
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

pub use cache::{cached_extract_source, AST_CACHE_VERSION};
pub use config::{ImportStyle, LanguageConfig, TypeRefStyle};
pub use resolve::{resolve_imports, resolve_relative_imports, ResolveStats};
pub use result::{ExtractionResult, ImportRecord, RawCall};
#[cfg(feature = "lang-sql")]
pub use sql_semantic::{emit_sql_columns, set_emit_sql_columns};
pub use tsconfig::{load_alias_resolver, AliasResolver};
pub use walker::extract_with_config;

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
    let ext = Path::new(path).extension().and_then(|e| e.to_str())?;
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
        "c" | "h" => Some(c::extract_c_source(path, source)),
        #[cfg(feature = "lang-cpp")]
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some(cpp::extract_cpp_source(path, source)),
        #[cfg(feature = "lang-json")]
        "json" => Some(json::extract_json_source(path, source)),
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
        #[cfg(feature = "lang-verilog")]
        "v" | "sv" | "vh" | "svh" => Some(verilog::extract_verilog_source(path, source)),
        #[cfg(feature = "lang-vue")]
        "vue" => Some(webframework::extract_vue_source(path, source)),
        #[cfg(feature = "lang-svelte")]
        "svelte" => Some(webframework::extract_svelte_source(path, source)),
        #[cfg(feature = "lang-astro")]
        "astro" => Some(webframework::extract_astro_source(path, source)),
        #[cfg(feature = "lang-dotnet")]
        "csproj" | "fsproj" | "vbproj" | "sln" | "slnx" => {
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

    #[test]
    fn dispatch_ignores_unknown_extension() {
        assert!(extract_source("a/b.zzz", b"x").is_none());
        assert!(extract_source("noext", b"x").is_none());
    }
}
