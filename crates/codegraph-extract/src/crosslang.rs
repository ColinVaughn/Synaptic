//! Cross-language linkage post-passes (gated on the `cross-language` feature).
//!
//! After a file is extracted, these scan its source for coupling the per-language
//! AST walk does not model -- FFI bindings, subprocess invocations, HTTP/RPC calls
//! -- and add INFERRED edges from the enclosing function to a target stub node, so
//! impact analysis can traverse cross-language boundaries. Detection is regex-
//! driven over source (pragmatic and imprecise by design: these are best-effort,
//! low-confidence links, never EXTRACTED facts).

use std::path::Path;
use std::sync::OnceLock;

use codegraph_core::{make_id, Confidence, Edge, FileType, Node, NodeId, NodeKind};
use regex::Regex;
use serde_json::{json, Map};

use crate::paths::file_node_id;
use crate::result::ExtractionResult;

/// Augment `result` with cross-language edges detected in `source`.
pub fn augment(path: &str, source: &[u8], result: &mut ExtractionResult) {
    let Ok(text) = std::str::from_utf8(source) else {
        return;
    };
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    // Blank comments + docstrings (preserving string literals and byte offsets) so
    // a commented-out or documented call is not detected as a real edge.
    let masked = mask_code(ext, text);
    let text = masked.as_str();
    scan_ctypes(ext, path, text, result);
    scan_subprocess(ext, path, text, result);
    scan_ffi_bindings(ext, path, text, result);
    scan_http(ext, path, text, result);
    scan_grpc(ext, path, text, result);
    scan_sql(ext, path, text, result);
}

// --- comment / docstring masking ---
//
// The detectors are regex-driven over source, so they would otherwise fire inside
// comments and documentation. `mask_code` returns the source with comment and
// docstring regions replaced by spaces, while PRESERVING string-literal contents
// (the detectors extract paths/commands from them) and byte offsets + newlines (so
// line attribution is unchanged). It is string/raw-string/char aware so a `//` or
// `#` inside a string is not mistaken for a comment.

/// Per-language masking configuration.
struct MaskCfg {
    /// Line-comment starters (e.g. `//`, `#`).
    line: &'static [&'static str],
    /// Whether `/* */` block comments apply.
    block: bool,
    /// Rust raw strings `r#"..."#` (skipped, preserved).
    rust_raw: bool,
    /// Backtick strings: Go raw / JS template (skipped, preserved).
    backtick: bool,
    /// `'...'` is a string (JS/PHP/Python/Ruby) vs a char literal (Rust/Go/Java/C).
    single_quote_string: bool,
    /// Python triple-quoted strings `'''`/`"""` (blanked as docstrings).
    triple: bool,
}

fn mask_code(ext: &str, text: &str) -> String {
    let cfg = match ext {
        "rs" => MaskCfg {
            line: &["//"],
            block: true,
            rust_raw: true,
            backtick: false,
            single_quote_string: false,
            triple: false,
        },
        "go" => MaskCfg {
            line: &["//"],
            block: true,
            rust_raw: false,
            backtick: true,
            single_quote_string: false,
            triple: false,
        },
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => MaskCfg {
            line: &["//"],
            block: true,
            rust_raw: false,
            backtick: true,
            single_quote_string: true,
            triple: false,
        },
        "java" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cs" | "kt" | "swift"
        | "scala" | "dart" => MaskCfg {
            line: &["//"],
            block: true,
            rust_raw: false,
            backtick: false,
            single_quote_string: false,
            triple: false,
        },
        "php" => MaskCfg {
            line: &["//", "#"],
            block: true,
            rust_raw: false,
            backtick: false,
            single_quote_string: true,
            triple: false,
        },
        "py" => MaskCfg {
            line: &["#"],
            block: false,
            rust_raw: false,
            backtick: false,
            single_quote_string: true,
            triple: true,
        },
        "rb" => MaskCfg {
            line: &["#"],
            block: false,
            rust_raw: false,
            backtick: false,
            single_quote_string: true,
            triple: false,
        },
        _ => return text.to_string(),
    };
    mask_with(text, &cfg)
}

fn mask_with(text: &str, cfg: &MaskCfg) -> String {
    let b = text.as_bytes();
    let n = b.len();
    let mut out = b.to_vec();
    let mut i = 0;
    while i < n {
        // Triple-quoted docstrings (blanked).
        if cfg.triple && (region_is(b, i, b"\"\"\"") || region_is(b, i, b"'''")) {
            let end = skip_triple(b, i);
            blank(&mut out, i, end);
            i = end;
            continue;
        }
        // Rust raw string: blanked. The detectors only read `["']`-delimited
        // arguments, never raw strings, so blanking loses nothing -- and it keeps
        // raw-string braces out of the gRPC/pyo3 brace matcher. Checked before the
        // `'`/char and comment handling so `r"a // b"` is not mis-masked.
        if cfg.rust_raw && b[i] == b'r' && is_rust_raw_start(b, i) {
            let end = skip_rust_raw(b, i);
            blank(&mut out, i, end);
            i = end;
            continue;
        }
        match b[i] {
            b'"' => {
                i = skip_quoted(b, i, b'"');
                continue;
            }
            b'`' if cfg.backtick => {
                i = skip_quoted(b, i, b'`');
                continue;
            }
            b'\'' => {
                i = if cfg.single_quote_string {
                    skip_quoted(b, i, b'\'')
                } else {
                    skip_char(b, i)
                };
                continue;
            }
            _ => {}
        }
        if cfg.block && region_is(b, i, b"/*") {
            let end = block_comment_end(b, i);
            blank(&mut out, i, end);
            i = end;
            continue;
        }
        if let Some(lc) = cfg.line.iter().find(|lc| region_is(b, i, lc.as_bytes())) {
            // `#` line comments only apply outside the string/char cases handled
            // above, so a `#` here is a real comment.
            let _ = lc;
            let end = line_comment_end(b, i);
            blank(&mut out, i, end);
            i = end;
            continue;
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

/// `b[i..]` starts with `pat`.
fn region_is(b: &[u8], i: usize, pat: &[u8]) -> bool {
    b[i..].starts_with(pat)
}

/// Replace `out[start..end]` with spaces, keeping newlines (so line numbers and
/// byte offsets are unchanged).
fn blank(out: &mut [u8], start: usize, end: usize) {
    let end = end.min(out.len());
    for byte in &mut out[start..end] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

/// Index just past a `"`/`'`/backtick-quoted string starting at `start`, honoring
/// `\` escapes. Runs to EOF if unterminated.
fn skip_quoted(b: &[u8], start: usize, quote: u8) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// Index just past a `"""`/`'''` triple-quoted string starting at `start`.
fn skip_triple(b: &[u8], start: usize) -> usize {
    let quote = b[start];
    let close = [quote, quote, quote];
    let mut i = start + 3;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i..].starts_with(&close) {
            return i + 3;
        }
        i += 1;
    }
    b.len()
}

/// Whether `b[i]` (an `r`) begins a Rust raw string `r#*"`. Requires the `r` to
/// not be glued to a preceding identifier char (so `myr"x"` is not misread).
fn is_rust_raw_start(b: &[u8], i: usize) -> bool {
    if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        return false;
    }
    let mut j = i + 1;
    while j < b.len() && b[j] == b'#' {
        j += 1;
    }
    b.get(j) == Some(&b'"')
}

/// Index just past a Rust raw string `r#*"..."#*` starting at `start`.
fn skip_rust_raw(b: &[u8], start: usize) -> usize {
    let mut hashes = 0;
    let mut j = start + 1;
    while j < b.len() && b[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    // j is at the opening `"`.
    let mut i = j + 1;
    while i < b.len() {
        if b[i] == b'"'
            && b[i + 1..]
                .iter()
                .take(hashes)
                .filter(|&&c| c == b'#')
                .count()
                == hashes
        {
            return i + 1 + hashes;
        }
        i += 1;
    }
    b.len()
}

/// Index of the newline ending the line comment at `start` (the `\n` itself is
/// not included, so it is preserved).
fn line_comment_end(b: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < b.len() && b[i] != b'\n' {
        i += 1;
    }
    i
}

/// Index just past the `*/` closing the block comment at `start`.
fn block_comment_end(b: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    while i < b.len() {
        if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            return i + 2;
        }
        i += 1;
    }
    b.len()
}

// --- FFI: Python ctypes / cffi native-library loads ---

fn ctypes_dll_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?:ctypes\s*\.\s*)?(?:CDLL|WinDLL|OleDLL|PyDLL)\s*\(\s*["']([^"']+)["']"#)
            .expect("valid regex")
    })
}

fn ctypes_loadlibrary_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:ctypes\s*\.\s*)?(?:cdll|windll|oledll)\s*\.\s*LoadLibrary\s*\(\s*["']([^"']+)["']"#,
        )
        .expect("valid regex")
    })
}

/// Python `ctypes.CDLL("libfoo.so")` / `cdll.LoadLibrary(...)` loads a native
/// library: link the enclosing function to a native-library target.
fn scan_ctypes(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    if ext != "py" {
        return;
    }
    for re in [ctypes_dll_re(), ctypes_loadlibrary_re()] {
        for caps in re.captures_iter(text) {
            let whole = caps.get(0).expect("group 0");
            let lib = native_lib_name(&caps[1]);
            if lib.is_empty() {
                continue;
            }
            let line = line_of(text, whole.start());
            let target = NodeId(make_id(&["native", "lib", &lib]));
            ensure_target(result, &target, &lib, "native_library");
            link(result, path, line, target, "binds_native", "ctypes");
        }
    }
}

// --- subprocess / CLI invocations ---

fn py_subprocess_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:subprocess\s*\.\s*(?:run|call|check_call|check_output|Popen)|os\s*\.\s*(?:system|popen)|os\s*\.\s*exec\w+)\s*\(\s*\[?\s*["']([^"']+)["']"#,
        )
        .expect("valid regex")
    })
}

fn js_childproc_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Qualified child_process.X for any method; bare distinctive names only
        // (a bare `exec(` is too noisy to attribute).
        Regex::new(
            r#"(?:child_process\s*\.\s*(?:exec|execSync|spawn|spawnSync|execFile|execFileSync)|\b(?:execSync|spawnSync|execFileSync|execFile|spawn))\s*\(\s*["']([^"']+)["']"#,
        )
        .expect("valid regex")
    })
}

fn go_exec_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // exec.Command("ls", ...) or exec.CommandContext(ctx, "ls", ...): the
        // command is the first string literal after the open paren.
        Regex::new(r#"exec\s*\.\s*Command(?:Context)?\s*\([^"')]*["']([^"']+)["']"#)
            .expect("valid regex")
    })
}

fn rust_command_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"Command\s*::\s*new\s*\(\s*["']([^"']+)["']"#).expect("valid regex")
    })
}

fn ruby_system_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:\bsystem|\bexec|Open3\s*\.\s*\w+|IO\s*\.\s*popen)\s*\(\s*["']([^"']+)["']"#,
        )
        .expect("valid regex")
    })
}

fn ruby_backtick_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"`([^`\n]+)`"#).expect("valid regex"))
}

fn php_exec_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\b(?:exec|shell_exec|system|passthru|proc_open)\s*\(\s*["']([^"']+)["']"#)
            .expect("valid regex")
    })
}

/// Detect process invocations and link the enclosing function to a command
/// target via `invokes`.
fn scan_subprocess(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    match ext {
        "py" => run_invokes(py_subprocess_re(), "subprocess", path, text, result),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            run_invokes(js_childproc_re(), "child_process", path, text, result)
        }
        "go" => run_invokes(go_exec_re(), "exec.Command", path, text, result),
        "rs" => run_invokes(rust_command_re(), "process::Command", path, text, result),
        "rb" => {
            run_invokes(ruby_system_re(), "system", path, text, result);
            run_invokes(ruby_backtick_re(), "backtick", path, text, result);
        }
        "php" => run_invokes(php_exec_re(), "php_exec", path, text, result),
        _ => {}
    }
}

fn run_invokes(re: &Regex, context: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in re.captures_iter(text) {
        let whole = caps.get(0).expect("group 0");
        let cmd = command_name(&caps[1]);
        if cmd.is_empty() {
            continue;
        }
        let line = line_of(text, whole.start());
        let target = NodeId(make_id(&["cmd", &cmd]));
        ensure_target(result, &target, &cmd, "command");
        link(result, path, line, target, "invokes", context);
    }
}

/// The executable from a command argument: the first whitespace-delimited token
/// (a shell line like `os.system("ls -l")` carries args), reduced to a basename.
fn command_name(raw: &str) -> String {
    let first = raw.split_whitespace().next().unwrap_or("");
    Path::new(first)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// --- FFI bindings: PyO3 / node-gyp / JNI ---

/// `#[pyfunction] ... fn <name>` -- group 0 spans the attributes (for a
/// `#[pyo3(name=..)]` override), group 1 is the Rust fn name.
fn pyfunction_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"#\[\s*pyfunction\b[\s\S]{0,160}?\bfn\s+(\w+)").expect("valid regex")
    })
}

/// `#[pyclass] ... struct|enum <name>`.
fn pyclass_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"#\[\s*pyclass\b[\s\S]{0,160}?\b(?:struct|enum)\s+(\w+)").expect("valid regex")
    })
}

/// Function-style module: `#[pymodule] ... fn <name>`.
fn pymodule_fn_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"#\[\s*pymodule\b[\s\S]{0,160}?\bfn\s+(\w+)").expect("valid regex")
    })
}

/// Declarative module: `#[pymodule] ... mod <name>`.
fn pymodule_mod_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"#\[\s*pymodule\b[\s\S]{0,160}?\bmod\s+(\w+)").expect("valid regex")
    })
}

/// `wrap_pyfunction!(path::to::name` -- the registered function (last segment).
fn wrap_pyfunction_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"wrap_pyfunction!\s*\(\s*([\w:]+)").expect("valid regex"))
}

/// `add_class::<path::to::Type>` -- the registered class (last segment).
fn add_class_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"add_class\s*::\s*<\s*([\w:]+)").expect("valid regex"))
}

/// Declarative `#[pymodule_export] use path::to::name;` re-export.
fn pymodule_export_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"#\[\s*pymodule_export\s*\]\s*(?:pub\s+)?use\s+([\w:]+)").expect("valid regex")
    })
}

/// A `#[pyo3(name = "x")]` / `#[pyfunction(name = "x")]` rename in an attribute
/// span.
fn pyo3_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"name\s*=\s*"([^"]+)""#).expect("valid regex"))
}

fn nodegyp_bindings_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"require\s*\(\s*['"](?:bindings|node-gyp-build)['"]\s*\)\s*\(\s*['"]([^'"]+)['"]"#,
        )
        .expect("valid regex")
    })
}

fn nodegyp_require_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"require\s*\(\s*['"]([^'"]*(?:\.node|/build/Release/[^'"]+))['"]"#)
            .expect("valid regex")
    })
}

fn jni_java_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `... native <ret> name(` -- the method name is the word just before `(`.
    RE.get_or_init(|| Regex::new(r"\bnative\b[^;{=]*?\b(\w+)\s*\(").expect("valid regex"))
}

fn jni_native_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // A JNI implementation is a C function named `Java_<pkg>_<Class>_<method>`.
    RE.get_or_init(|| Regex::new(r"\bJava_(\w+)\s*\(").expect("valid regex"))
}

/// Native FFI bindings whose detection is language-specific.
fn scan_ffi_bindings(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    match ext {
        "rs" => scan_pyo3(text, result),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            scan_node_gyp(path, text, result)
        }
        "java" => scan_jni(jni_java_re(), false, path, text, result),
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" => {
            scan_jni(jni_native_re(), true, path, text, result)
        }
        _ => {}
    }
}

/// PyO3 export markers. Detection is split across the file boundary: a
/// `#[pyfunction]`/`#[pyclass]` definition (which may live in a different file
/// from its `#[pymodule]`) is TAGGED with `_pyo3_export`, and each `#[pymodule]`
/// emits a boundary node `pyo3:<module>` carrying `_pyo3_registers` -- the symbol
/// names it registers (`wrap_pyfunction!`/`add_class` for a function-style module,
/// `#[pymodule_export]`/nested defs for a declarative `mod`). The graph-level
/// `resolve_pyo3_modules` pass then links each boundary to the tagged definitions
/// by name across files, and `resolve_pyo3_imports` joins Python importers, so
/// reverse impact crosses from a Rust impl to the Python caller even when the
/// module and the function are in different files. `#[pyo3(name = "..")]` renames
/// are honored. No edges are emitted here -- that is the graph pass's job.
fn scan_pyo3(text: &str, result: &mut ExtractionResult) {
    if !text.contains("pymodule") && !text.contains("pyfunction") && !text.contains("pyclass") {
        return;
    }

    // 1. Tag every exportable definition in this file.
    for caps in pyfunction_def_re().captures_iter(text) {
        tag_pyo3_export(result, &caps[1], pyo3_name(&caps[0]));
    }
    for caps in pyclass_def_re().captures_iter(text) {
        tag_pyo3_export(result, &caps[1], pyo3_name(&caps[0]));
    }

    // 2. Each #[pymodule] (function-style or declarative) becomes a boundary node
    //    carrying the names it registers.
    for (re, declarative) in [(pymodule_fn_re(), false), (pymodule_mod_re(), true)] {
        for caps in re.captures_iter(text) {
            let module = pyo3_name(&caps[0]).unwrap_or_else(|| caps[1].to_string());
            let sig_end = caps.get(0).expect("group 0").end();
            let Some(brace) = text[sig_end..].find('{').map(|i| sig_end + i) else {
                continue;
            };
            let body = &text[brace..block_end(text, brace)];
            emit_pyo3_module(result, &module, &collect_pyo3_registers(body, declarative));
        }
    }
}

/// Tag the (in-file) definition node named `rust_name` as a pyo3 export. The
/// value is the Python-facing name (the `#[pyo3(name)]` override or the Rust
/// name), kept for the import side; matching to registrations is by node name.
fn tag_pyo3_export(result: &mut ExtractionResult, rust_name: &str, py_override: Option<String>) {
    let py = py_override.unwrap_or_else(|| rust_name.to_string());
    if let Some(n) = result
        .nodes
        .iter_mut()
        .find(|n| !n.source_file.is_empty() && node_fn_name(&n.label) == rust_name)
    {
        n.extra.insert("_pyo3_export".to_string(), json!(py));
    }
}

/// The `name = ".."` override found in an attribute span, if any.
fn pyo3_name(attr_span: &str) -> Option<String> {
    pyo3_name_re().captures(attr_span).map(|c| c[1].to_string())
}

/// The symbol names a module body registers. Function-style modules use
/// `wrap_pyfunction!`/`add_class`; declarative modules additionally re-export via
/// `#[pymodule_export]` and define `#[pyfunction]`/`#[pyclass]` inline.
fn collect_pyo3_registers(body: &str, declarative: bool) -> Vec<String> {
    let mut regs: Vec<String> = Vec::new();
    for caps in wrap_pyfunction_re().captures_iter(body) {
        regs.push(last_path_segment(&caps[1]).to_string());
    }
    for caps in add_class_re().captures_iter(body) {
        regs.push(last_path_segment(&caps[1]).to_string());
    }
    if declarative {
        for caps in pymodule_export_re().captures_iter(body) {
            regs.push(last_path_segment(&caps[1]).to_string());
        }
        for caps in pyfunction_def_re().captures_iter(body) {
            regs.push(caps[1].to_string());
        }
        for caps in pyclass_def_re().captures_iter(body) {
            regs.push(caps[1].to_string());
        }
    }
    regs.sort();
    regs.dedup();
    regs
}

/// The last `::`-separated segment of a path (`a::b::c` -> `c`).
fn last_path_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// Create (or extend) the `pyo3:<module>` boundary node, recording the symbol
/// names it registers in `_pyo3_registers` for the graph-level stitch.
fn emit_pyo3_module(result: &mut ExtractionResult, module: &str, registers: &[String]) {
    if module.is_empty() {
        return;
    }
    let id = NodeId(make_id(&["pyo3", "module", module]));
    if let Some(n) = result.nodes.iter_mut().find(|n| n.id == id) {
        // Merge registers if the module appears more than once.
        let mut existing: Vec<String> = n
            .extra
            .get("_pyo3_registers")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        existing.extend(registers.iter().cloned());
        existing.sort();
        existing.dedup();
        n.extra
            .insert("_pyo3_registers".to_string(), json!(existing));
        return;
    }
    let mut extra = Map::new();
    extra.insert("_origin".to_string(), json!("ast"));
    extra.insert("_node_type".to_string(), json!("pyo3_module"));
    extra.insert("_pyo3_registers".to_string(), json!(registers));
    result.nodes.push(Node {
        id,
        label: format!("pyo3:{module}"),
        file_type: FileType::Code,
        source_file: String::new(),
        source_location: None,
        community: None,
        repo: None,
        extra,
    });
}

/// node-gyp / N-API native addon loads (`require('bindings')('addon')` or a
/// direct `.node` / `build/Release/...` require).
fn scan_node_gyp(path: &str, text: &str, result: &mut ExtractionResult) {
    for re in [nodegyp_bindings_re(), nodegyp_require_re()] {
        for caps in re.captures_iter(text) {
            let addon = native_lib_name(&caps[1]);
            if addon.is_empty() {
                continue;
            }
            let line = line_of(text, caps.get(0).expect("group 0").start());
            let target = NodeId(make_id(&["native", "addon", &addon]));
            ensure_target(result, &target, &addon, "native_addon");
            link(result, path, line, target, "binds_native", "node-gyp");
        }
    }
}

/// JNI: a Java `native` method declaration and the matching C `Java_*` function
/// both link to a shared `jni:<method>` target, connecting the two sides once a
/// graph holds both files. `mangled` parses the method out of the C symbol.
fn scan_jni(re: &Regex, mangled: bool, path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in re.captures_iter(text) {
        let raw = &caps[1];
        let method = if mangled {
            raw.rsplit('_').next().unwrap_or(raw)
        } else {
            raw
        };
        if method.is_empty() {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        let target = NodeId(make_id(&["jni", method]));
        ensure_target(result, &target, &format!("jni:{method}"), "jni_symbol");
        link(result, path, line, target, "binds_native", "jni");
    }
}

// --- HTTP/RPC service boundaries ---
//
// A route is keyed by its (normalized) PATH so a server handler and a client
// call to the same path land on the SAME route node and connect at graph build,
// no resolution pass needed for same-repo. The HTTP method rides as edge context.
// Covered servers: Flask/FastAPI, Express, Go net/http, axum, actix. Clients:
// requests/httpx, axios/fetch, Go http, reqwest.
// Limitations (documented future work): parameterized paths (`/users/{id}` vs a
// concrete client `/users/7`) need a graph-level template match; cross-repo
// matching needs federation to relate route nodes by path; axum handlers defined
// in another file fall back to the route's own file node.

fn py_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `@app.get("/p")` / `@router.route("/p", methods=[...])` ... `def handler`.
    // `(?m)` + `^\s*def` anchors the handler to a real definition line, so a
    // commented-out `# def fake` between the decorator and the handler is skipped.
    RE.get_or_init(|| {
        Regex::new(
            r#"(?m)@\s*\w+\s*\.\s*(route|get|post|put|delete|patch|head|options)\s*\(\s*["']([^"']+)["']([\s\S]{0,160}?)^\s*def\s+(\w+)"#,
        )
        .expect("valid regex")
    })
}

fn py_client_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"\b(?:requests|httpx)\s*\.\s*(get|post|put|delete|patch|head|options)\s*\(\s*["']([^"']+)["']"#,
        )
        .expect("valid regex")
    })
}

fn express_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"\b(?:app|router)\s*\.\s*(get|post|put|delete|patch|head|options|all)\s*\(\s*["']([^"']+)["']"#,
        )
        .expect("valid regex")
    })
}

fn axios_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\baxios\s*\.\s*(get|post|put|delete|patch)\s*\(\s*["']([^"']+)["']"#)
            .expect("valid regex")
    })
}

fn fetch_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Require a path/URL-shaped argument (leading `/` or `.`, or an http(s) URL)
    // so `db.fetch('SELECT ...')` and other non-HTTP `.fetch()` calls don't
    // produce bogus route nodes.
    RE.get_or_init(|| {
        Regex::new(r#"\bfetch\s*\(\s*["']([./][^"']*|https?://[^"']+)["']"#).expect("valid regex")
    })
}

fn go_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\.\s*HandleFunc\s*\(\s*["']([^"']+)["']"#).expect("valid regex")
    })
}

fn go_client_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\bhttp\s*\.\s*(Get|Post|Head|PostForm)\s*\(\s*["']([^"']+)["']"#)
            .expect("valid regex")
    })
}

fn axum_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // axum: `.route("/path", get(handler))` -- path, the method router fn, and the
    // named handler. Only the first method in a chained `get(h).post(h2)` is taken.
    RE.get_or_init(|| {
        Regex::new(
            r#"\.\s*route\s*\(\s*["']([^"']+)["']\s*,\s*(get|post|put|delete|patch|head|options)\s*\(\s*([\w:]+)"#,
        )
        .expect("valid regex")
    })
}

fn actix_attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // actix-web: `#[get("/path")] ... fn handler` (decorator style). The bounded
    // `[\s\S]{0,200}?` skips intervening attributes before the fn.
    RE.get_or_init(|| {
        Regex::new(
            r#"#\[\s*(get|post|put|delete|patch|head|options)\s*\(\s*["']([^"']+)["']\s*\)\s*\][\s\S]{0,200}?\bfn\s+(\w+)"#,
        )
        .expect("valid regex")
    })
}

fn reqwest_get_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\breqwest\s*::\s*get\s*\(\s*["']([^"']+)["']"#).expect("valid regex")
    })
}

fn reqwest_method_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Generic builder `.get("url")` / `.post("url")`: require an absolute http(s)
    // URL so a map/router `.get("/local")` is not mistaken for a client call.
    RE.get_or_init(|| {
        Regex::new(r#"\.\s*(get|post|put|patch|delete)\s*\(\s*["'](https?://[^"']+)["']"#)
            .expect("valid regex")
    })
}

fn scan_http(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    match ext {
        "py" => {
            scan_py_routes(path, text, result);
            scan_verb_client(py_client_re(), path, text, result);
        }
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            scan_verb_route(express_route_re(), path, text, result);
            scan_verb_client(axios_re(), path, text, result);
            scan_fetch(path, text, result);
        }
        "go" => {
            scan_go_routes(path, text, result);
            scan_verb_client(go_client_re(), path, text, result);
        }
        "rs" => {
            scan_rust_routes(path, text, result);
            scan_rust_client(path, text, result);
        }
        _ => {}
    }
}

/// Rust route registrations: axum `.route("/p", get(handler))` (handler is a
/// named fn ref) and actix `#[get("/p")] fn handler` (decorator style).
fn scan_rust_routes(path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in axum_route_re().captures_iter(text) {
        let http_path = &caps[1];
        let method = caps[2].to_ascii_uppercase();
        // The handler may be a qualified path (`handlers::serve`); key on its last
        // segment so it matches the function node's bare name.
        let handler = last_path_segment(&caps[3]);
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_handler_named(result, path, line, http_path, &method, handler);
    }
    for caps in actix_attr_re().captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let http_path = &caps[2];
        // Line of the handler fn name, so emit_handler attributes to that fn.
        let line = line_of(text, caps.get(3).expect("group 3").start());
        emit_handler(result, path, line, http_path, &method);
    }
}

/// Rust HTTP clients: `reqwest::get("url")` and the builder `.get/.post("url")`
/// form (absolute URL only).
fn scan_rust_client(path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in reqwest_get_re().captures_iter(text) {
        let url = &caps[1];
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, url, "calls_service", "GET");
    }
    // The bare builder `.get/.post("url")` form is receiver-blind, so only trust
    // it in a file that actually uses reqwest.
    if text.contains("reqwest") {
        for caps in reqwest_method_re().captures_iter(text) {
            let method = caps[1].to_ascii_uppercase();
            let url = &caps[2];
            let line = line_of(text, caps.get(0).expect("group 0").start());
            emit_route(result, path, line, url, "calls_service", &method);
        }
    }
}

/// Python route decorators: attribute `handles` to the decorated function.
fn scan_py_routes(path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in py_route_re().captures_iter(text) {
        let verb = &caps[1];
        let http_path = &caps[2];
        let method = if verb.eq_ignore_ascii_case("route") {
            flask_methods(&caps[3])
        } else {
            verb.to_ascii_uppercase()
        };
        // Line of the handler fn name, so the edge attributes to that function.
        let line = line_of(text, caps.get(4).expect("group 4").start());
        emit_handler(result, path, line, http_path, &method);
    }
}

/// Method from a Flask `methods=["POST", ...]` kwarg; defaults to GET.
fn flask_methods(tail: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r#"methods\s*=\s*\[\s*["'](\w+)["']"#).expect("valid regex"));
    re.captures(tail)
        .map(|c| c[1].to_ascii_uppercase())
        .unwrap_or_else(|| "GET".to_string())
}

/// `(app|router).VERB("/path"` style route registrations (Express).
fn scan_verb_route(re: &Regex, path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in re.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let http_path = &caps[2];
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_handler(result, path, line, http_path, &method);
    }
}

/// `obj.HandleFunc("/path", ...)` (Go net/http + mux). Go 1.22 ServeMux allows a
/// `"METHOD /path"` pattern (e.g. `"GET /healthz"`), so a leading HTTP method is
/// split out into the edge context, leaving a clean path.
fn scan_go_routes(path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in go_route_re().captures_iter(text) {
        let (method, http_path) = split_go_route_pattern(&caps[1]);
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_handler(result, path, line, http_path, method);
    }
}

/// Split a Go 1.22 ServeMux pattern into `(method, path)`. `"GET /healthz"` ->
/// `("GET", "/healthz")`; a pattern with no leading method -> `("ANY", pattern)`.
fn split_go_route_pattern(pattern: &str) -> (&str, &str) {
    let trimmed = pattern.trim_start();
    if let Some((head, rest)) = trimmed.split_once(' ') {
        if is_http_method(head) {
            return (head, rest.trim_start());
        }
    }
    ("ANY", pattern)
}

/// An uppercase HTTP method token.
fn is_http_method(s: &str) -> bool {
    matches!(
        s,
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS" | "CONNECT" | "TRACE"
    )
}

/// `client.VERB("url"` style client calls (requests/httpx/axios/http.Get).
fn scan_verb_client(re: &Regex, path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in re.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let url = &caps[2];
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, url, "calls_service", &method);
    }
}

/// `fetch("url")` defaults to GET (the method option is not parsed).
fn scan_fetch(path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in fetch_re().captures_iter(text) {
        let url = &caps[1];
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, url, "calls_service", "GET");
    }
}

/// Client side: link the enclosing (calling) function to the path-keyed route
/// node via `calls_service`, with the HTTP method as context.
fn emit_route(
    result: &mut ExtractionResult,
    path: &str,
    line: u32,
    http_path: &str,
    relation: &str,
    method: &str,
) {
    let np = norm_path(http_path);
    if np.is_empty() {
        return;
    }
    let target = NodeId(make_id(&["route", &np]));
    ensure_target(result, &target, &np, "route");
    link(result, path, line, target, relation, method);
}

/// Server side: the route node points to its handler via `handled_by` (route ->
/// handler), so reverse-impact from a handler reaches the route and, through the
/// shared route node, the clients that call it. The handler is the function
/// enclosing `line` (decorator/attribute-macro style).
fn emit_handler(
    result: &mut ExtractionResult,
    path: &str,
    line: u32,
    http_path: &str,
    method: &str,
) {
    let handler = enclosing_function(result, line).unwrap_or_else(|| file_node_id(path));
    emit_handler_to(result, path, line, http_path, method, handler);
}

/// Like `emit_handler`, but the handler is a named fn reference (axum
/// `.route("/p", get(handler))`) resolved by name within the file, not the
/// enclosing function.
fn emit_handler_named(
    result: &mut ExtractionResult,
    path: &str,
    line: u32,
    http_path: &str,
    method: &str,
    handler_name: &str,
) {
    let np = norm_path(http_path);
    if np.is_empty() {
        return;
    }
    let route = NodeId(make_id(&["route", &np]));
    ensure_target(result, &route, &np, "route");
    // Record the handler name + method on the route node so a graph pass can
    // resolve the handler when it is defined in another file.
    if let Some(n) = result.nodes.iter_mut().find(|n| n.id == route) {
        n.extra
            .insert("_route_handler".to_string(), json!(handler_name));
        n.extra.insert("_route_method".to_string(), json!(method));
    }
    // Same-file: link immediately. Otherwise the graph-level resolve_route_handlers
    // pass links it cross-file (no file-node fallback, which would be misleading).
    if let Some(handler) = find_function_by_name(result, handler_name) {
        emit_handler_to(result, path, line, http_path, method, handler);
    }
}

/// Push the `handled_by` edge from the path-keyed route node to a resolved
/// handler node.
fn emit_handler_to(
    result: &mut ExtractionResult,
    path: &str,
    line: u32,
    http_path: &str,
    method: &str,
    handler: NodeId,
) {
    let np = norm_path(http_path);
    if np.is_empty() {
        return;
    }
    let route = NodeId(make_id(&["route", &np]));
    ensure_target(result, &route, &np, "route");
    result.edges.push(Edge {
        source: route,
        target: handler,
        relation: "handled_by".to_string(),
        confidence: Confidence::Inferred,
        source_file: path.to_string(),
        source_location: Some(format!("L{line}")),
        confidence_score: Some(Confidence::Inferred.default_score()),
        weight: 1.0,
        context: Some(method.to_string()),
        cross_repo: false,
        extra: Map::new(),
    });
}

// --- gRPC service boundaries (tonic / Python stubs) ---
//
// A gRPC service is keyed by its lowercased name, so a tonic server impl
// (`impl Greeter for X`), a tonic client (`GreeterClient::connect`), and a
// cross-language client (Python `GreeterStub(...)`) all land on one
// `grpc:greeter` node. Server rpc-method impls attach via `handled_by`; clients
// via `calls_service`. Detection is gated on a `tonic`/`grpc` mention in the file
// so the common `<Name>Client` shape is not mistaken for gRPC.

fn tonic_impl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `#[(tonic::)async_trait] impl <Service> for <Type> {` -- the attribute is the
    // gRPC-server tell; capture the service trait and the position of the brace.
    RE.get_or_init(|| {
        Regex::new(r"#\[\s*(?:tonic\s*::\s*)?async_trait\s*\]\s*impl\s+(\w+)\s+for\s+[^\{;]+\{")
            .expect("valid regex")
    })
}

fn async_fn_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\basync\s+fn\s+(\w+)").expect("valid regex"))
}

fn rust_grpc_client_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `GreeterClient::connect(` / `::new(` -- the service is the name before
    // `Client`. Requires a word char glued to `Client` so a bare `Client::new`
    // (e.g. reqwest) does not match.
    RE.get_or_init(|| {
        Regex::new(r"\b(\w+)Client\s*::\s*(?:connect|new)\s*\(").expect("valid regex")
    })
}

fn py_grpc_stub_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `GreeterStub(` -- the gRPC-generated stub class; service is the name before
    // `Stub`.
    RE.get_or_init(|| Regex::new(r"\b(\w+)Stub\s*\(").expect("valid regex"))
}

fn scan_grpc(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    match ext {
        "rs" => scan_rust_grpc(path, text, result),
        "py" => scan_python_grpc(path, text, result),
        _ => {}
    }
}

/// Tonic server impls and clients. Gated on a `tonic` mention so a non-gRPC
/// `<Name>Client` is not picked up.
fn scan_rust_grpc(path: &str, text: &str, result: &mut ExtractionResult) {
    if !text.contains("tonic") {
        return;
    }
    // Server: each rpc-method impl inside `impl <Service> for <Type> { ... }`.
    for caps in tonic_impl_re().captures_iter(text) {
        let svc = &caps[1];
        let whole = caps.get(0).expect("group 0");
        let brace = whole.end() - 1; // the matched `{`
        let body_end = block_end(text, brace);
        let body = &text[brace..body_end];
        // Resolve each method WITHIN the impl's own line span, so two impls in one
        // file that share a method name link to their respective methods.
        let lo = line_of(text, brace);
        let hi = line_of(text, body_end.saturating_sub(1));
        for m in async_fn_re().captures_iter(body) {
            let method = &m[1];
            let line = line_of(text, brace + m.get(1).expect("group 1").start());
            let handler = find_function_in_lines(result, method, lo, hi)
                .or_else(|| find_function_by_name(result, method))
                .unwrap_or_else(|| file_node_id(path));
            emit_grpc_handler(result, path, line, svc, handler);
        }
    }
    // Client: `<Service>Client::connect(...)`, excluding well-known non-gRPC
    // `<Name>Client` types.
    for caps in rust_grpc_client_re().captures_iter(text) {
        let svc = &caps[1];
        if is_non_grpc_client(svc) {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_grpc_client(result, path, line, svc);
    }
}

/// Well-known `<Name>Client` types that are not gRPC clients (so a tonic file
/// that also uses one is not misread).
fn is_non_grpc_client(name: &str) -> bool {
    const DENY: &[&str] = &[
        "http",
        "https",
        "web",
        "reqwest",
        "hyper",
        "ureq",
        "isahc",
        "surf",
        "db",
        "database",
        "sql",
        "mysql",
        "postgres",
        "postgresql",
        "sqlite",
        "redis",
        "mongo",
        "mongodb",
        "cache",
        "api",
        "rest",
        "oauth",
        "s3",
        "aws",
        "gcs",
        "gcp",
        "azure",
        "kafka",
        "amqp",
        "rabbitmq",
        "nats",
        "smtp",
        "imap",
        "ftp",
        "ssh",
        "tcp",
        "udp",
        "ws",
        "websocket",
        "graphql",
        "elastic",
        "elasticsearch",
    ];
    DENY.contains(&name.to_ascii_lowercase().as_str())
}

/// Python gRPC clients: `<Service>Stub(channel)`. Gated on a `grpc` mention.
fn scan_python_grpc(path: &str, text: &str, result: &mut ExtractionResult) {
    if !text.contains("grpc") {
        return;
    }
    for caps in py_grpc_stub_re().captures_iter(text) {
        let svc = &caps[1];
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_grpc_client(result, path, line, svc);
    }
}

/// The `(id, label)` of the service node for a gRPC service name (lowercased).
fn grpc_service_node(svc: &str) -> (NodeId, String) {
    let lower = svc.to_ascii_lowercase();
    let label = format!("grpc:{lower}");
    (NodeId(make_id(&["grpc", &lower])), label)
}

/// Server side: the service node `handled_by` an rpc-method impl.
fn emit_grpc_handler(
    result: &mut ExtractionResult,
    path: &str,
    line: u32,
    svc: &str,
    handler: NodeId,
) {
    let (id, label) = grpc_service_node(svc);
    ensure_target(result, &id, &label, "grpc_service");
    result.edges.push(Edge {
        source: id,
        target: handler,
        relation: "handled_by".to_string(),
        confidence: Confidence::Inferred,
        source_file: path.to_string(),
        source_location: Some(format!("L{line}")),
        confidence_score: Some(Confidence::Inferred.default_score()),
        weight: 1.0,
        context: Some("gRPC".to_string()),
        cross_repo: false,
        extra: Map::new(),
    });
}

/// Client side: the enclosing (calling) function `calls_service` the service node.
fn emit_grpc_client(result: &mut ExtractionResult, path: &str, line: u32, svc: &str) {
    let (id, label) = grpc_service_node(svc);
    ensure_target(result, &id, &label, "grpc_service");
    let source = enclosing_function(result, line).unwrap_or_else(|| file_node_id(path));
    result.edges.push(Edge {
        source,
        target: id,
        relation: "calls_service".to_string(),
        confidence: Confidence::Inferred,
        source_file: path.to_string(),
        source_location: Some(format!("L{line}")),
        confidence_score: Some(Confidence::Inferred.default_score()),
        weight: 1.0,
        context: Some("gRPC".to_string()),
        cross_repo: false,
        extra: Map::new(),
    });
}

/// Byte offset just past the `}` matching the `{` at `open`. Skips braces inside
/// string and char literals and line comments so they do not throw off the count
/// (raw strings and block comments are not handled -- best effort). `saturating_sub`
/// guards the count against an unbalanced `}` rather than underflowing.
fn block_end(text: &str, open: usize) -> usize {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            b'"' => {
                i = skip_string(bytes, i);
                continue;
            }
            b'\'' => {
                i = skip_char(bytes, i);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

/// Byte offset just past the closing `"` of the double-quoted string starting at
/// `start` (a `"`), honoring `\"` escapes. Runs to EOF if unterminated.
fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// Byte offset just past a char literal (`'x'` / `'\n'`) starting at `start` (a
/// `'`). A lifetime (`'a`) has no closing quote, so the `'` is treated as an
/// ordinary byte and `start + 1` is returned.
fn skip_char(bytes: &[u8], start: usize) -> usize {
    if bytes.get(start + 1) == Some(&b'\\') {
        // Escaped char: find the closing quote within a small window.
        let mut i = start + 2;
        while i < bytes.len() && i < start + 8 {
            if bytes[i] == b'\'' {
                return i + 1;
            }
            i += 1;
        }
    } else if bytes.get(start + 2) == Some(&b'\'') {
        // Simple `'c'` char literal.
        return start + 3;
    }
    start + 1
}

/// The id of a function/method/constructor node whose name equals `name`.
fn find_function_by_name(result: &ExtractionResult, name: &str) -> Option<NodeId> {
    result
        .nodes
        .iter()
        .find(|n| is_function_node(n) && node_fn_name(&n.label) == name)
        .map(|n| n.id.clone())
}

/// Like `find_function_by_name`, but restricted to a node whose definition line
/// falls within `[lo, hi]` -- used to resolve a method within its own impl block.
fn find_function_in_lines(
    result: &ExtractionResult,
    name: &str,
    lo: u32,
    hi: u32,
) -> Option<NodeId> {
    result
        .nodes
        .iter()
        .find(|n| {
            is_function_node(n)
                && node_fn_name(&n.label) == name
                && n.span()
                    .is_some_and(|s| s.start_line >= lo && s.start_line <= hi)
        })
        .map(|n| n.id.clone())
}

fn is_function_node(n: &Node) -> bool {
    matches!(
        n.kind(),
        Some(NodeKind::Function | NodeKind::Method | NodeKind::Constructor)
    )
}

/// The bare identifier of a function/method node label: drop a leading `.`
/// (methods are labeled `.foo()`), take the part before `(`, then the last
/// whitespace-delimited word, then drop any `Type::` qualifier and `<..>` generics.
/// `pub async fn foo<T>()` / `MyType::say()` / `.say()` -> `foo` / `say` / `say`.
fn node_fn_name(label: &str) -> &str {
    let l = label.trim_start_matches('.');
    let before_paren = l.split('(').next().unwrap_or(l);
    let last_word = before_paren
        .split_whitespace()
        .last()
        .unwrap_or(before_paren);
    let after_colon = last_word.rsplit("::").next().unwrap_or(last_word);
    after_colon.split('<').next().unwrap_or(after_colon)
}

/// Reduce a URL or route pattern to a normalized path: drop scheme+authority and
/// any query/fragment, ensure a leading `/`, and trim a trailing `/` (keep root).
fn norm_path(raw: &str) -> String {
    let mut p = raw.trim();
    if let Some(idx) = p.find("://") {
        let after = &p[idx + 3..];
        p = match after.find('/') {
            Some(slash) => &after[slash..],
            None => "/",
        };
    }
    let p = p.split(['?', '#']).next().unwrap_or(p).trim();
    if p.is_empty() {
        return String::new();
    }
    let lead = if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    };
    let trimmed = lead.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

// --- shared helpers ---

/// Library basename without directory or extension: `./libmath.so` -> `libmath`.
fn native_lib_name(raw: &str) -> String {
    Path::new(raw)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 1-based line number for a byte offset in `text`.
fn line_of(text: &str, byte: usize) -> u32 {
    text.as_bytes()[..byte.min(text.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count() as u32
        + 1
}

/// Add a target stub node (external, no source file) once. `_node_type` tags it
/// for querying; `_origin` keeps it on par with AST nodes through the build.
fn ensure_target(result: &mut ExtractionResult, id: &NodeId, label: &str, node_type: &str) {
    if result.nodes.iter().any(|n| &n.id == id) {
        return;
    }
    let mut extra = Map::new();
    extra.insert("_origin".to_string(), json!("ast"));
    extra.insert("_node_type".to_string(), json!(node_type));
    result.nodes.push(Node {
        id: id.clone(),
        label: label.to_string(),
        file_type: FileType::Code,
        source_file: String::new(),
        source_location: None,
        community: None,
        repo: None,
        extra,
    });
}

/// Add an INFERRED edge from the function enclosing `line` (else the file node)
/// to `target`.
fn link(
    result: &mut ExtractionResult,
    path: &str,
    line: u32,
    target: NodeId,
    relation: &str,
    context: &str,
) {
    let source = enclosing_function(result, line).unwrap_or_else(|| file_node_id(path));
    result.edges.push(Edge {
        source,
        target,
        relation: relation.to_string(),
        confidence: Confidence::Inferred,
        source_file: path.to_string(),
        source_location: Some(format!("L{line}")),
        confidence_score: Some(Confidence::Inferred.default_score()),
        weight: 1.0,
        context: Some(context.to_string()),
        cross_repo: false,
        extra: Map::new(),
    });
}

/// The innermost function/method node whose span contains `line`.
fn enclosing_function(result: &ExtractionResult, line: u32) -> Option<NodeId> {
    let mut best: Option<(&NodeId, u32)> = None;
    for n in &result.nodes {
        if !matches!(
            n.kind(),
            Some(NodeKind::Function | NodeKind::Method | NodeKind::Constructor)
        ) {
            continue;
        }
        let Some(span) = n.span() else { continue };
        if span.start_line <= line && line <= span.end_line {
            let height = span.end_line - span.start_line;
            if best.is_none_or(|(_, h)| height < h) {
                best = Some((&n.id, height));
            }
        }
    }
    best.map(|(id, _)| id.clone())
}

// --- SQL string-literal detector ---

/// No-op when SQL parsing is unavailable (single-language builds without lang-sql).
#[cfg(not(feature = "lang-sql"))]
fn scan_sql(_ext: &str, _path: &str, _text: &str, _result: &mut ExtractionResult) {}

/// Detect SQL string literals in application code, parse/classify them, and link
/// the enclosing function to the referenced table stubs. INFERRED, best-effort:
/// only string literals whose first keyword is a SQL verb are considered.
#[cfg(feature = "lang-sql")]
fn scan_sql(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    if !matches!(
        ext,
        "py" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "go" | "rs" | "java" | "cs"
    ) {
        return;
    }
    for (lit, start) in string_literals(text) {
        let trimmed = lit.trim_start();
        let verb = trimmed
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let relation = match verb.as_str() {
            "SELECT" => "queries",
            // A CTE (`WITH ...`) can wrap a read or a write; classify by whether a
            // write keyword follows the CTE definitions.
            "WITH" => {
                let up = lit.to_ascii_uppercase();
                if up.contains(" INSERT ")
                    || up.contains(" UPDATE ")
                    || up.contains(" DELETE ")
                    || up.contains(" MERGE ")
                {
                    "writes_to"
                } else {
                    "queries"
                }
            }
            "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "UPSERT" => "writes_to",
            "CALL" | "EXEC" | "EXECUTE" => "calls_proc",
            _ => continue,
        };
        let line = line_of(text, start);
        for table in referenced_tables(&lit) {
            let table_lower = table.to_lowercase();
            let id = NodeId(make_id(&["sql", &table_lower]));
            ensure_target(result, &id, &table, "table");
            link(result, path, line, id, relation, "sql_query");
            // Attach the normalized, truncated query text to the edge link() just
            // pushed, so the auditor can inspect the SQL without re-reading source.
            if let Some(edge) = result.edges.last_mut() {
                let snippet: String = lit.split_whitespace().collect::<Vec<_>>().join(" ");
                edge.extra.insert(
                    "sql".to_string(),
                    json!(snippet.chars().take(400).collect::<String>()),
                );
            }
        }
    }
}

/// Table names referenced by a single SQL string. SELECT statements are walked
/// via the sqlparser AST (handles FROM + JOIN); for INSERT/UPDATE/DELETE/CALL and
/// any parse failure, fall back to a regex over FROM/JOIN/INTO/UPDATE/TABLE so
/// write-statement table names are captured without dialect-specific AST shapes.
#[cfg(feature = "lang-sql")]
fn referenced_tables(sql: &str) -> Vec<String> {
    use sqlparser::ast::{SetExpr, Statement, TableFactor};
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    let mut out: Vec<String> = Vec::new();
    let clean = |raw: &str| {
        raw.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
            .to_string()
    };
    let last_ident = |qualified: &str| clean(qualified.rsplit('.').next().unwrap_or(qualified));

    if let Ok(stmts) = Parser::parse_sql(&GenericDialect {}, sql) {
        for stmt in stmts {
            if let Statement::Query(q) = stmt {
                if let SetExpr::Select(select) = *q.body {
                    for twj in select.from {
                        if let TableFactor::Table { name, .. } = twj.relation {
                            if let Some(p) = name.0.last() {
                                out.push(last_ident(&p.to_string()));
                            }
                        }
                        for j in twj.joins {
                            if let TableFactor::Table { name, .. } = j.relation {
                                if let Some(p) = name.0.last() {
                                    out.push(last_ident(&p.to_string()));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Regex fallback / write-statement coverage: any FROM/JOIN/INTO/UPDATE/TABLE
    // target name. Deduped against what the AST already found.
    static TBL_RE: OnceLock<Regex> = OnceLock::new();
    let re = TBL_RE.get_or_init(|| {
        Regex::new(r#"(?is)\b(?:from|join|into|update|table)\s+[`"\[]?([\w.]+)"#)
            .expect("table regex")
    });
    for caps in re.captures_iter(sql) {
        let name = last_ident(&caps[1]);
        if !name.is_empty() && !out.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
    }
    out
}

/// Yield (contents, byte_offset) for each quoted string literal in `text`
/// (single, double, backtick). Comments are already masked by `augment`.
#[cfg_attr(not(feature = "lang-sql"), allow(dead_code))]
fn string_literals(text: &str) -> Vec<(String, usize)> {
    static LITS: OnceLock<Regex> = OnceLock::new();
    let re = LITS.get_or_init(|| {
        Regex::new(r#"(?s)"((?:[^"\\]|\\.)*)"|'((?:[^'\\]|\\.)*)'|`([^`]*)`"#)
            .expect("string literal regex")
    });
    re.captures_iter(text)
        .filter_map(|c| {
            let m = c.get(1).or_else(|| c.get(2)).or_else(|| c.get(3))?;
            Some((m.as_str().to_string(), m.start()))
        })
        .collect()
}
