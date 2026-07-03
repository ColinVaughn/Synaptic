//! Cross-language linkage post-passes (gated on the `cross-language` feature).
//!
//! After a file is extracted, these scan its source for coupling the per-language
//! AST walk does not model -- FFI bindings, subprocess invocations, HTTP/RPC calls
//! -- and add INFERRED edges from the enclosing function to a target stub node, so
//! impact analysis can traverse cross-language boundaries. Detection is regex-
//! driven over source (pragmatic and imprecise by design: these are best-effort,
//! low-confidence links, never EXTRACTED facts).

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map};
use synaptic_core::{make_id, Confidence, DynamicSite, Edge, FileType, Node, NodeId, NodeKind};

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
    // Vue/Svelte/Astro components hold their service calls in <script> blocks
    // (or Astro frontmatter). Blank everything else -- offsets preserved so line
    // attribution still matches the delegated AST nodes -- and scan as TS
    // (2026-07 audit: SFCs were skipped by every scanner).
    let (ext, prepared): (&str, std::borrow::Cow<str>) = match ext {
        "vue" | "svelte" | "astro" => ("ts", sfc_script_view(ext, text).into()),
        _ => (ext, text.into()),
    };
    // Blank comments + docstrings (preserving string literals and byte offsets) so
    // a commented-out or documented call is not detected as a real edge.
    let masked = mask_code(ext, &prepared);
    let text = masked.as_str();
    scan_ctypes(ext, path, text, result);
    scan_subprocess(ext, path, text, result);
    scan_ffi_bindings(ext, path, text, result);
    scan_http(ext, path, text, result);
    scan_grpc(ext, path, text, result);
    scan_websocket(ext, path, text, result);
    scan_queues(ext, path, text, result);
    scan_ipc(ext, path, text, result);
    scan_event_bus(ext, path, text, result);
    scan_dotnet_events(ext, path, text, result);
    scan_sql(ext, path, text, result);
    crate::dynamic::scan(path, text, result);
}

/// Same-length view of an SFC keeping only `<script ...>` block bodies (Vue,
/// Svelte) or `---` frontmatter (Astro); everything else becomes spaces,
/// newlines preserved so byte offsets and line numbers stay valid.
fn sfc_script_view(ext: &str, text: &str) -> String {
    let mut out: Vec<u8> = text
        .bytes()
        .map(|b| if b == b'\n' { b'\n' } else { b' ' })
        .collect();
    let src = text.as_bytes();
    let mut keep = |start: usize, end: usize| {
        let end = end.min(src.len());
        if start < end {
            out[start..end].copy_from_slice(&src[start..end]);
        }
    };
    if ext == "astro" {
        // Frontmatter: the body between the leading `---` fence pair.
        if let Some(open) = text.find("---") {
            let body_start = open + 3;
            if let Some(close) = text[body_start..].find("\n---") {
                keep(body_start, body_start + close);
            }
        }
    }
    static OPEN: OnceLock<Regex> = OnceLock::new();
    let open_re = OPEN.get_or_init(|| Regex::new(r#"(?i)<script[^>]*>"#).expect("valid regex"));
    for m in open_re.find_iter(text) {
        let body_start = m.end();
        let body_end = text[body_start..]
            .find("</script")
            .map(|i| body_start + i)
            .unwrap_or(text.len());
        keep(body_start, body_end);
    }
    String::from_utf8(out).unwrap_or_default()
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
        // Shell scripts: `#` comments must be blanked so a commented-out
        // `# curl -X POST https://...` is not a client edge (wave-2 W3).
        // Backticks are command substitution (live code), NOT strings.
        "sh" | "bash" | "zsh" => MaskCfg {
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
    // Loaded-library variables (`lib = CDLL(...)`) feed the call-site pass below.
    let mut lib_vars: Vec<String> = Vec::new();
    static ASSIGN: OnceLock<Regex> = OnceLock::new();
    let assign_re = ASSIGN.get_or_init(|| {
        Regex::new(r#"(\w+)\s*=\s*(?:ctypes\s*\.\s*)?(?:CDLL|WinDLL|OleDLL|PyDLL)\s*\("#)
            .expect("valid regex")
    });
    for caps in assign_re.captures_iter(text) {
        lib_vars.push(caps[1].to_string());
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
    // cffi: `ffi.dlopen("libfoo")` binds like CDLL (2026-07 audit: the module
    // header claimed cffi but no detector existed).
    static DLOPEN: OnceLock<Regex> = OnceLock::new();
    let dlopen_re = DLOPEN
        .get_or_init(|| Regex::new(r#"\.\s*dlopen\s*\(\s*["']([^"']+)["']"#).expect("valid regex"));
    for caps in dlopen_re.captures_iter(text) {
        let lib = native_lib_name(&caps[1]);
        if lib.is_empty() {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        let target = NodeId(make_id(&["native", "lib", &lib]));
        ensure_target(result, &target, &lib, "native_library");
        link(result, path, line, target, "binds_native", "cffi");
    }
    // Call sites on a loaded lib (`lib.add(1, 2)`) link a per-symbol sink, so
    // the Rust/C exporter of `add` and this caller meet at `c_symbol:add`.
    if !lib_vars.is_empty() {
        static CALL: OnceLock<Regex> = OnceLock::new();
        let call_re = CALL.get_or_init(|| {
            Regex::new(r#"\b(\w+)\s*\.\s*([a-zA-Z_]\w*)\s*\("#).expect("valid regex")
        });
        for caps in call_re.captures_iter(text) {
            if !lib_vars.contains(&caps[1].to_string()) {
                continue;
            }
            let sym = caps[2].to_string();
            let line = line_of(text, caps.get(0).expect("group 0").start());
            let target = NodeId(make_id(&["c_symbol", &sym]));
            ensure_target(result, &target, &format!("c_symbol:{sym}"), "c_symbol");
            link(result, path, line, target, "binds_native", "ctypes_call");
        }
    }
}

/// Rust `#[no_mangle] extern "C"` exports: the function is the provider side of
/// a `c_symbol:<name>` sink, meeting ctypes/JNA/etc. callers there.
fn scan_rust_extern_exports(path: &str, text: &str, result: &mut ExtractionResult) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"#\[\s*no_mangle\s*\][\s\S]{0,120}?\bfn\s+(\w+)"#).expect("valid regex")
    });
    for caps in re.captures_iter(text) {
        let sym = caps[1].to_string();
        let line = line_of(text, caps.get(1).expect("group 1").start());
        let target = NodeId(make_id(&["c_symbol", &sym]));
        ensure_target(result, &target, &format!("c_symbol:{sym}"), "c_symbol");
        link(result, path, line, target, "binds_native", "extern_c");
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
        // Gate on a process-Command token: clap's `Command::new("myapp")`
        // CLI-builder idiom would otherwise mint a phantom invokes edge, and a
        // bare `std::process` gate is re-enabled by `std::process::exit` in the
        // same clap file (2026-07 audit + wave-2 W7). Covers `use
        // std::process::Command` and the brace form `use std::process::{Command,..}`.
        "rs" => {
            let gated = text.contains("process::Command")
                || (text.contains("::process::{") && text.contains("Command"));
            if gated {
                run_invokes(rust_command_re(), "process::Command", path, text, result)
            }
        }
        "rb" => {
            run_invokes(ruby_system_re(), "system", path, text, result);
            run_invokes(ruby_backtick_re(), "backtick", path, text, result);
        }
        "php" => run_invokes(php_exec_re(), "php_exec", path, text, result),
        "cs" => {
            run_invokes(cs_process_re(), "Process.Start", path, text, result);
        }
        "java" | "kt" => {
            run_invokes(java_process_re(), "ProcessBuilder", path, text, result);
        }
        "c" | "cpp" | "cc" | "cxx" => {
            run_invokes(c_system_re(), "system", path, text, result);
        }
        "sh" | "bash" | "zsh" => {
            run_invokes(shell_runner_re(), "shell", path, text, result);
            run_invokes(shell_dot_slash_re(), "shell", path, text, result);
        }
        _ => {}
    }
}

fn shell_runner_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // An interpreter invoking an in-repo script: `python tools/x.py`, `node y.js`.
    RE.get_or_init(|| {
        Regex::new(
            r#"(?m)^\s*(?:python3?|node|ruby|sh|bash)\s+([^\s;|&]+\.(?:py|js|mjs|ts|rb|sh))"#,
        )
        .expect("valid regex")
    })
}

fn shell_dot_slash_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // A direct `./path/to/script` execution at line start.
    RE.get_or_init(|| Regex::new(r#"(?m)^\s*\./([^\s;|&]+)"#).expect("valid regex"))
}

fn c_system_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `system("tool")` / `popen("tool", "r")` in C/C++.
    RE.get_or_init(|| Regex::new(r#"\b(?:system|popen)\s*\(\s*"([^"]+)""#).expect("valid regex"))
}

fn java_process_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `new ProcessBuilder("tool", ...)` or `Runtime.getRuntime().exec("tool ...")`.
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:new\s+ProcessBuilder|Runtime\s*\.\s*getRuntime\s*\(\s*\)\s*\.\s*exec)\s*\(\s*"([^"]+)""#,
        )
        .expect("valid regex")
    })
}

fn cs_process_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `Process.Start("tool")` or `new ProcessStartInfo("tool", ...)`.
    RE.get_or_init(|| {
        Regex::new(r#"(?:Process\s*\.\s*Start|new\s+ProcessStartInfo)\s*\(\s*"([^"]+)""#)
            .expect("valid regex")
    })
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
        "rs" => {
            scan_pyo3(text, result);
            scan_rust_extern_exports(path, text, result);
        }
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            scan_node_gyp(path, text, result)
        }
        "java" => scan_jni(jni_java_re(), false, path, text, result),
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" => {
            scan_jni(jni_native_re(), true, path, text, result)
        }
        "cs" => scan_dotnet_pinvoke(path, text, result),
        _ => {}
    }
}

/// .NET P/Invoke: `[DllImport("lib")]` / `[LibraryImport("lib")]` binds the
/// declaring scope to the native library (2026-07 audit: previously invisible,
/// so the native side looked like a 0-dependent leaf).
fn scan_dotnet_pinvoke(path: &str, text: &str, result: &mut ExtractionResult) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"\[\s*(?:DllImport|LibraryImport)\s*\(\s*"([^"]+)""#).expect("valid regex")
    });
    for caps in re.captures_iter(text) {
        let lib = native_lib_name(&caps[1]);
        if lib.is_empty() {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        let target = NodeId(make_id(&["native", "lib", &lib]));
        ensure_target(result, &target, &lib, "native_library");
        link(result, path, line, target, "binds_native", "pinvoke");
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

/// Recover the Java method name from a mangled C export like
/// `Java_pkg_Cls_do_1work` or `Java_pkg_Cls_send__Ljava_lang_String_2`.
/// A plain `_` separates package/class/method components; `_1`/`_2`/`_3` are
/// escapes for `_`/`;`/`[` INSIDE a component, and `__` starts an overload
/// signature suffix. Decode into components, then take the last component
/// before any empty one (the `__` suffix produces an empty component).
fn jni_demangle_method(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut components: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '_' {
            match chars.get(i + 1) {
                Some('1') => {
                    cur.push('_');
                    i += 2;
                }
                Some('2') => {
                    cur.push(';');
                    i += 2;
                }
                Some('3') => {
                    cur.push('[');
                    i += 2;
                }
                _ => {
                    components.push(std::mem::take(&mut cur));
                    i += 1;
                }
            }
        } else {
            cur.push(chars[i]);
            i += 1;
        }
    }
    components.push(cur);
    // `__` (overload signature) yields an empty component; the method is the one
    // just before it. Skip index 0 ("Java").
    if let Some(pos) = components.iter().skip(1).position(String::is_empty) {
        components.get(pos).cloned().unwrap_or_default()
    } else {
        components.pop().unwrap_or_default()
    }
}

/// JNI: a Java `native` method declaration and the matching C `Java_*` function
/// both link to a shared `jni:<method>` target, connecting the two sides once a
/// graph holds both files. `mangled` parses the method out of the C symbol.
fn scan_jni(re: &Regex, mangled: bool, path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in re.captures_iter(text) {
        let raw = &caps[1];
        let demangled;
        let method = if mangled {
            demangled = jni_demangle_method(raw);
            demangled.as_str()
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
    // Group 1 is the receiver (router/blueprint variable) for prefix lookup.
    RE.get_or_init(|| {
        Regex::new(
            r#"(?m)@\s*(\w+)\s*\.\s*(route|get|post|put|delete|patch|head|options)\s*\(\s*["']([^"']+)["']([\s\S]{0,160}?)^\s*def\s+(\w+)"#,
        )
        .expect("valid regex")
    })
}

/// Join a mount/constructor prefix onto a route path (`/api` + `/users` ->
/// `/api/users`); an empty prefix is a no-op.
fn compose_prefix(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        return path.to_string();
    }
    let p = prefix.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{p}{path}")
    } else {
        format!("{p}/{path}")
    }
}

/// Same-file router/blueprint prefixes, by variable name (2026-07 audit: routes
/// were keyed un-prefixed, so `APIRouter(prefix=...)` / `app.use('/api', r)` /
/// `.nest("/api", r)` routes never joined their clients and unrelated services'
/// bare paths falsely merged). Cross-file mounts are out of scope (documented).
fn collect_route_prefixes(ext: &str, text: &str) -> HashMap<String, String> {
    let mut prefixes: HashMap<String, String> = HashMap::new();
    match ext {
        "py" => {
            static CTOR: OnceLock<Regex> = OnceLock::new();
            static KW: OnceLock<Regex> = OnceLock::new();
            static INCLUDE: OnceLock<Regex> = OnceLock::new();
            let ctor = CTOR.get_or_init(|| {
                Regex::new(r#"(\w+)\s*=\s*(?:APIRouter|Blueprint)\s*\(([^)]*)\)"#)
                    .expect("valid regex")
            });
            let kw = KW.get_or_init(|| {
                Regex::new(r#"(?:url_prefix|prefix)\s*=\s*["']([^"']+)["']"#).expect("valid regex")
            });
            for caps in ctor.captures_iter(text) {
                if let Some(k) = kw.captures(&caps[2]) {
                    prefixes.insert(caps[1].to_string(), k[1].to_string());
                }
            }
            // `app.include_router(r, prefix="/v1")` prepends to the router's own
            // prefix (FastAPI); `register_blueprint(bp, url_prefix=...)` replaces
            // the blueprint's (Flask).
            let include = INCLUDE.get_or_init(|| {
                Regex::new(r#"\.\s*(include_router|register_blueprint)\s*\(\s*(\w+)([^)]*)\)"#)
                    .expect("valid regex")
            });
            for caps in include.captures_iter(text) {
                let var = caps[2].to_string();
                let Some(k) = kw.captures(&caps[3]) else {
                    continue;
                };
                let mount = k[1].to_string();
                let existing = prefixes.get(&var).cloned().unwrap_or_default();
                let value = if &caps[1] == "include_router" {
                    compose_prefix(&mount, &existing)
                } else {
                    mount
                };
                prefixes.insert(var, value);
            }
        }
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            static USE: OnceLock<Regex> = OnceLock::new();
            let use_re = USE.get_or_init(|| {
                Regex::new(r#"\b\w+\s*\.\s*use\s*\(\s*["']([^"']+)["']\s*,\s*([A-Za-z_]\w*)"#)
                    .expect("valid regex")
            });
            for caps in use_re.captures_iter(text) {
                prefixes.insert(caps[2].to_string(), caps[1].to_string());
            }
        }
        "rs" => {
            static NEST: OnceLock<Regex> = OnceLock::new();
            let nest = NEST.get_or_init(|| {
                Regex::new(r#"\.\s*nest\s*\(\s*["']([^"']+)["']\s*,\s*&?\s*([A-Za-z_]\w*)"#)
                    .expect("valid regex")
            });
            for caps in nest.captures_iter(text) {
                prefixes.insert(caps[2].to_string(), caps[1].to_string());
            }
        }
        _ => {}
    }
    prefixes
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

fn py_fclient_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // f-string URL: `requests.get(f"http://{host}/api/x")`. Holes become
    // `{param}` via `template_to_path` (2026-07 audit: previously invisible).
    RE.get_or_init(|| {
        Regex::new(
            r#"\b(?:requests|httpx)\s*\.\s*(get|post|put|delete|patch|head|options)\s*\(\s*f["']([^"']+)["']"#,
        )
        .expect("valid regex")
    })
}

/// Python f-string client calls: rewrite `{...}` holes and emit like a plain
/// client call.
fn scan_py_fclient(path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in py_fclient_re().captures_iter(text) {
        let whole = caps.get(0).expect("group 0");
        let method = client_verb_method(&caps[1]);
        let Some(url) = template_to_path(&caps[2], "{", '}') else {
            continue;
        };
        let line = line_of(text, whole.start());
        emit_route(result, path, line, &url, "calls_service", &method);
    }
}

fn express_route_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Any receiver (`const api = express.Router(); api.get(...)`) -- the
        // trailing comma requires a handler argument (so `app.get('port')` and
        // `map.get('key')` are out), and the leading `/` on the path keeps
        // non-URL string keys (`i18n.get('name', fb)`) out (2026-07 audit).
        // Group 1 (receiver) feeds the mount-prefix lookup.
        Regex::new(
            r#"\b(\w+)\s*\.\s*(get|post|put|delete|patch|head|options|all)\s*\(\s*["'](/[^"']*|\*)["']\s*,\s*([^\s,)])"#,
        )
        .expect("valid regex")
    })
}

/// NestJS controller routes: `@Controller('prefix')` on the class composes with
/// `@Get(':id')` / `@Post()` method decorators. Gated on an `@nestjs` import.
fn scan_nestjs_routes(path: &str, text: &str, result: &mut ExtractionResult) {
    if !text.contains("@nestjs") {
        return;
    }
    static CTRL: OnceLock<Regex> = OnceLock::new();
    static VERB: OnceLock<Regex> = OnceLock::new();
    let ctrl_re = CTRL.get_or_init(|| {
        Regex::new(r#"@\s*Controller\s*\(\s*(?:["']([^"']*)["'])?\s*\)[\s\S]{0,160}?\bclass\s+\w+"#)
            .expect("valid regex")
    });
    let mut prefixes: Vec<(usize, String)> = Vec::new();
    for caps in ctrl_re.captures_iter(text) {
        let p = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let p = if p.starts_with('/') {
            p.to_string()
        } else {
            format!("/{p}")
        };
        prefixes.push((caps.get(0).expect("group 0").start(), p));
    }
    if prefixes.is_empty() {
        return;
    }
    let verb_re = VERB.get_or_init(|| {
        Regex::new(
            r#"@\s*(Get|Post|Put|Delete|Patch)\s*\(\s*(?:["']([^"']*)["'])?\s*\)[^(@]{0,200}?(\w+)\s*\("#,
        )
        .expect("valid regex")
    });
    for caps in verb_re.captures_iter(text) {
        let start = caps.get(0).expect("group 0").start();
        let method = caps[1].to_ascii_uppercase();
        let leaf = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let prefix = prefixes
            .iter()
            .filter(|(s, _)| *s <= start)
            .max_by_key(|(s, _)| *s)
            .map(|(_, p)| p.as_str())
            .unwrap_or("");
        let http_path = if leaf.is_empty() {
            prefix.to_string()
        } else {
            compose_prefix(prefix, leaf)
        };
        let line = line_of(text, caps.get(3).expect("group 3").start());
        emit_handler(result, path, line, &http_path, &method);
    }
}

fn axios_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Group 2: plain string URL. Group 3: template-literal URL (`${...}` holes
    // become `{param}` segments via `template_to_path`).
    RE.get_or_init(|| {
        Regex::new(
            r#"\baxios\s*\.\s*(get|post|put|delete|patch)\s*\(\s*(?:["']([^"']+)["']|`([^`]+)`)"#,
        )
        .expect("valid regex")
    })
}

fn fetch_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Require a path/URL-shaped argument (leading `/` or `.`, an http(s) URL, or
    // a template hole) so `db.fetch('SELECT ...')` and other non-HTTP `.fetch()`
    // calls don't produce bogus route nodes. Group 1: plain string; group 2:
    // template literal.
    RE.get_or_init(|| {
        Regex::new(
            r#"\bfetch\s*\(\s*(?:["']([./][^"']*|https?://[^"']+)["']|`((?:[./$]|https?://)[^`]*)`)"#,
        )
        .expect("valid regex")
    })
}

/// Rewrite a template-literal (or f-string) URL into a matchable path: a
/// LEADING hole (`${BASE}/x`, `{base}/x`) is dropped (it is a base-URL variable),
/// every later hole becomes a `{param}` segment piece, and a template with no
/// literal path left (`` fetch(`${url}`) ``) yields None. The scheme/authority,
/// hole-valued or not, is stripped later by `norm_path`.
fn template_to_path(raw: &str, hole_open: &str, hole_close: char) -> Option<String> {
    let mut s = raw.trim().to_string();
    if let Some(rest) = s.strip_prefix(hole_open) {
        let close = rest.find(hole_close)?;
        s = rest[close + 1..].to_string();
    }
    let mut out = String::new();
    let mut remainder = s.as_str();
    while let Some(open) = remainder.find(hole_open) {
        out.push_str(&remainder[..open]);
        let after = &remainder[open + hole_open.len()..];
        match after.find(hole_close) {
            Some(close) => {
                out.push_str("{param}");
                remainder = &after[close + 1..];
            }
            None => {
                out.push_str("{param}");
                remainder = "";
            }
        }
    }
    out.push_str(remainder);
    // Require at least one literal, non-hole path segment after the authority.
    let path_part = match out.find("://") {
        Some(i) => out[i + 3..].find('/').map(|s| &out[i + 3 + s..]),
        None => Some(out.as_str()),
    }?;
    let has_literal = path_part
        .split('/')
        .any(|seg| !seg.is_empty() && !seg.contains("{param}"));
    if has_literal {
        Some(out)
    } else {
        None
    }
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

fn axum_route_open_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // axum: `.route("/path", <method routers>)`. The second argument is walked
    // separately so every pair of a chained `get(h).post(h2)` links its handler
    // (the old single-pair capture dropped the chained ones -- 2026-07 audit).
    RE.get_or_init(|| Regex::new(r#"\.\s*route\s*\(\s*["']([^"']+)["']\s*,"#).expect("valid regex"))
}

fn axum_method_pair_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // One `verb(handler)` pair inside a route's second argument.
    RE.get_or_init(|| {
        Regex::new(r#"\b(get|post|put|delete|patch|head|options)\s*\(\s*([\w:]+)"#)
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

/// Single-assignment string constants of a file (`const U = '/x'`,
/// `API_URL = "http://..."`). A name bound twice maps to None (ambiguous).
fn collect_string_consts(ext: &str, text: &str) -> HashMap<String, Option<String>> {
    static JS: OnceLock<Regex> = OnceLock::new();
    static PY: OnceLock<Regex> = OnceLock::new();
    let (decl_re, rebind_re): (&Regex, Option<&Regex>) = match ext {
        "py" => (
            PY.get_or_init(|| {
                Regex::new(r#"(?m)^\s*([A-Za-z_]\w*)\s*=\s*["']([^"'\n]+)["']\s*$"#)
                    .expect("valid regex")
            }),
            None,
        ),
        _ => {
            static REBIND: OnceLock<Regex> = OnceLock::new();
            (
                JS.get_or_init(|| {
                    Regex::new(
                        r#"(?m)^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_]\w*)\s*=\s*["'`]([^"'`\n]+)["'`]"#,
                    )
                    .expect("valid regex")
                }),
                Some(REBIND.get_or_init(|| {
                    Regex::new(r#"(?m)^\s*([A-Za-z_]\w*)\s*=\s*["'`][^"'`\n]+["'`]"#)
                        .expect("valid regex")
                })),
            )
        }
    };
    let mut consts: HashMap<String, Option<String>> = HashMap::new();
    for caps in decl_re.captures_iter(text) {
        let name = caps[1].to_string();
        // A concatenated value (`'/api/' + v`) is runtime-built; storing the
        // literal prefix would key a truncated route (wave-2 low).
        let end = caps.get(0).expect("group 0").end();
        let concatenated = text[end.min(text.len())..].trim_start().starts_with('+');
        let value = if concatenated {
            None
        } else {
            Some(caps[2].to_string())
        };
        consts
            .entry(name)
            .and_modify(|v| *v = None)
            .or_insert(value);
    }
    // A bare `name = "..."` reassignment (no declaration keyword) makes the
    // name ambiguous in JS; Python's decl regex already catches every binding.
    if let Some(re) = rebind_re {
        for caps in re.captures_iter(text) {
            if let Some(v) = consts.get_mut(&caps[1]) {
                *v = None;
            }
        }
    }
    consts
}

/// `fetch(IDENT)` / `axios.VERB(IDENT` / `requests.VERB(IDENT` resolved through
/// the same-file constant map (one hop -- 2026-07 audit: the standard
/// extract-the-URL hygiene pattern was invisible).
fn scan_const_clients(
    ext: &str,
    path: &str,
    text: &str,
    consts: &HashMap<String, Option<String>>,
    result: &mut ExtractionResult,
) {
    if consts.is_empty() {
        return;
    }
    static JS_FETCH: OnceLock<Regex> = OnceLock::new();
    static JS_AXIOS: OnceLock<Regex> = OnceLock::new();
    static PY_VERB: OnceLock<Regex> = OnceLock::new();
    let mut sites: Vec<(String, String, usize)> = Vec::new(); // (method, name, offset)
    match ext {
        "py" => {
            let re = PY_VERB.get_or_init(|| {
                Regex::new(
                    r#"\b(?:requests|httpx)\s*\.\s*(get|post|put|delete|patch|head|options)\s*\(\s*([A-Za-z_]\w*)\s*[,)]"#,
                )
                .expect("valid regex")
            });
            for caps in re.captures_iter(text) {
                sites.push((
                    caps[1].to_ascii_uppercase(),
                    caps[2].to_string(),
                    caps.get(0).expect("group 0").start(),
                ));
            }
        }
        _ => {
            let fetch = JS_FETCH.get_or_init(|| {
                Regex::new(r#"\bfetch\s*\(\s*([A-Za-z_]\w*)\s*[,)]"#).expect("valid regex")
            });
            for caps in fetch.captures_iter(text) {
                sites.push((
                    "GET".to_string(),
                    caps[1].to_string(),
                    caps.get(0).expect("group 0").start(),
                ));
            }
            let axios = JS_AXIOS.get_or_init(|| {
                Regex::new(
                    r#"\baxios\s*\.\s*(get|post|put|delete|patch)\s*\(\s*([A-Za-z_]\w*)\s*[,)]"#,
                )
                .expect("valid regex")
            });
            for caps in axios.captures_iter(text) {
                sites.push((
                    caps[1].to_ascii_uppercase(),
                    caps[2].to_string(),
                    caps.get(0).expect("group 0").start(),
                ));
            }
        }
    }
    for (method, name, offset) in sites {
        let Some(Some(url)) = consts.get(&name) else {
            continue;
        };
        // Only URL-shaped constants: a path or an absolute http(s) URL.
        if !(url.starts_with('/') || url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        let line = line_of(text, offset);
        emit_route(result, path, line, url, "calls_service", &method);
    }
}

/// Same-file HTTP client instances with a base URL: `axios.create({baseURL})`,
/// `httpx.Client(base_url=...)`, `requests.Session()` (empty base). The
/// instance's verb calls compose base + path (2026-07 audit: instance-based
/// usage is the recommended style of every client library and was invisible).
fn collect_client_bases(ext: &str, text: &str) -> HashMap<String, String> {
    let mut bases: HashMap<String, String> = HashMap::new();
    match ext {
        "py" => {
            static CLIENT: OnceLock<Regex> = OnceLock::new();
            static BASE: OnceLock<Regex> = OnceLock::new();
            static SESSION: OnceLock<Regex> = OnceLock::new();
            let client = CLIENT.get_or_init(|| {
                Regex::new(r#"(\w+)\s*=\s*httpx\s*\.\s*(?:Client|AsyncClient)\s*\(([^)]*)\)"#)
                    .expect("valid regex")
            });
            let base = BASE.get_or_init(|| {
                Regex::new(r#"base_url\s*=\s*["']([^"']+)["']"#).expect("valid regex")
            });
            for caps in client.captures_iter(text) {
                let b = base.captures(&caps[2]).map(|c| c[1].to_string());
                bases.insert(caps[1].to_string(), b.unwrap_or_default());
            }
            let session = SESSION.get_or_init(|| {
                Regex::new(r#"(\w+)\s*=\s*requests\s*\.\s*Session\s*\("#).expect("valid regex")
            });
            for caps in session.captures_iter(text) {
                bases.insert(caps[1].to_string(), String::new());
            }
        }
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            static CREATE: OnceLock<Regex> = OnceLock::new();
            let create = CREATE.get_or_init(|| {
                Regex::new(
                    r#"(\w+)\s*=\s*axios\s*\.\s*create\s*\(\s*\{[^}]*baseURL\s*:\s*["'`]([^"'`]+)["'`]"#,
                )
                .expect("valid regex")
            });
            for caps in create.captures_iter(text) {
                bases.insert(caps[1].to_string(), caps[2].to_string());
            }
        }
        _ => {}
    }
    bases
}

/// Verb calls on a known client instance: `api.get('/users')` with `api` from
/// `collect_client_bases`. The base composes ahead of the call path.
fn scan_instance_clients(
    path: &str,
    text: &str,
    bases: &HashMap<String, String>,
    result: &mut ExtractionResult,
) {
    if bases.is_empty() {
        return;
    }
    static VERB: OnceLock<Regex> = OnceLock::new();
    let re = VERB.get_or_init(|| {
        Regex::new(
            r#"\b([A-Za-z_]\w*)\s*\.\s*(get|post|put|delete|patch|head|options)\s*\(\s*["']([^"']+)["']"#,
        )
        .expect("valid regex")
    });
    for caps in re.captures_iter(text) {
        let Some(base) = bases.get(&caps[1]) else {
            continue;
        };
        let method = caps[2].to_ascii_uppercase();
        let url = if caps[3].contains("://") {
            caps[3].to_string()
        } else {
            compose_prefix(base, &caps[3])
        };
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, &url, "calls_service", &method);
    }
}

fn scan_http(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    let consts = collect_string_consts(ext, text);
    let prefixes = collect_route_prefixes(ext, text);
    let bases = collect_client_bases(ext, text);
    match ext {
        "py" => {
            scan_py_routes(path, text, &prefixes, result);
            scan_django_urls(path, text, result);
            scan_aiohttp_server(path, text, result);
            scan_verb_client(py_client_re(), path, text, result);
            scan_py_fclient(path, text, result);
            scan_py_url_clients(path, text, result);
            scan_const_clients(ext, path, text, &consts, result);
            scan_instance_clients(path, text, &bases, result);
        }
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            scan_express_routes(path, text, &prefixes, result);
            scan_nestjs_routes(path, text, result);
            scan_verb_client(axios_re(), path, text, result);
            scan_fetch(path, text, result);
            scan_const_clients(ext, path, text, &consts, result);
            scan_instance_clients(path, text, &bases, result);
        }
        "go" => {
            scan_go_routes(path, text, result);
            scan_verb_client(go_client_re(), path, text, result);
        }
        "rs" => {
            scan_rust_routes(path, text, &prefixes, result);
            scan_rust_client(path, text, result);
        }
        "cs" => {
            scan_csharp_routes(path, text, result);
            scan_csharp_client(path, text, result);
        }
        "java" | "kt" => {
            scan_java_routes(path, text, result);
            scan_java_client(path, text, result);
        }
        "php" => {
            scan_php_routes(path, text, result);
            scan_php_client(path, text, result);
        }
        "rb" => {
            scan_ruby_routes(path, text, result);
            scan_ruby_client(path, text, result);
        }
        "sh" | "bash" | "zsh" => scan_shell_http(path, text, result),
        _ => {}
    }
}

/// Shell HTTP clients: `curl [-X METHOD] URL` and `wget URL` (2026-07 audit:
/// deploy/init scripts hitting service routes were invisible).
fn scan_shell_http(path: &str, text: &str, result: &mut ExtractionResult) {
    static CURL: OnceLock<Regex> = OnceLock::new();
    static METHOD: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    static WGET: OnceLock<Regex> = OnceLock::new();
    let curl_re = CURL.get_or_init(|| Regex::new(r#"(?m)\bcurl\b([^\n]*)"#).expect("valid regex"));
    let method_re =
        METHOD.get_or_init(|| Regex::new(r#"(?:-X|--request)\s+['"]?(\w+)"#).expect("valid regex"));
    let url_re = URL.get_or_init(|| Regex::new(r#"https?://[^\s'"]+"#).expect("valid regex"));
    for caps in curl_re.captures_iter(text) {
        let tail = &caps[1];
        let Some(url) = url_re.find(tail) else {
            continue;
        };
        let method = method_re
            .captures(tail)
            .map(|m| m[1].to_ascii_uppercase())
            .unwrap_or_else(|| "GET".to_string());
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, url.as_str(), "calls_service", &method);
    }
    let wget_re = WGET.get_or_init(|| Regex::new(r#"(?m)\bwget\b[^\n]*"#).expect("valid regex"));
    for m in wget_re.find_iter(text) {
        let Some(url) = url_re.find(m.as_str()) else {
            continue;
        };
        let line = line_of(text, m.start());
        emit_route(result, path, line, url.as_str(), "calls_service", "GET");
    }
}

/// Laravel/Symfony-style `Route::verb('/p', ...)` registrations.
fn scan_php_routes(path: &str, text: &str, result: &mut ExtractionResult) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"\bRoute\s*::\s*(get|post|put|delete|patch|any)\s*\(\s*["']([^"']+)["']"#)
            .expect("valid regex")
    });
    for caps in re.captures_iter(text) {
        let method = match &caps[1] {
            v if v.eq_ignore_ascii_case("any") => "ANY".to_string(),
            v => v.to_ascii_uppercase(),
        };
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_handler(result, path, line, &caps[2], &method);
    }
}

/// PHP HTTP clients: Guzzle-style `->verb('http://...')` (absolute URL only,
/// mirroring the reqwest builder guard) and the Laravel `Http::verb(...)` facade.
fn scan_php_client(path: &str, text: &str, result: &mut ExtractionResult) {
    static ARROW: OnceLock<Regex> = OnceLock::new();
    static FACADE: OnceLock<Regex> = OnceLock::new();
    let arrow = ARROW.get_or_init(|| {
        Regex::new(r#"->\s*(get|post|put|delete|patch)\s*\(\s*["'](https?://[^"']+)["']"#)
            .expect("valid regex")
    });
    for caps in arrow.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, &caps[2], "calls_service", &method);
    }
    let facade = FACADE.get_or_init(|| {
        Regex::new(r#"\bHttp\s*::\s*(get|post|put|delete|patch)\s*\(\s*["']([^"']+)["']"#)
            .expect("valid regex")
    });
    for caps in facade.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, &caps[2], "calls_service", &method);
    }
}

/// Sinatra / Rails-routes verb declarations: `get '/path' do` / `get '/path',
/// to: 'controller#action'` at line start. Gated on a routing context --
/// request specs spell the SAME verbs as client-side test calls (wave-2 W2).
fn scan_ruby_routes(path: &str, text: &str, result: &mut ExtractionResult) {
    let lower = path.to_ascii_lowercase();
    let is_test_file = lower.ends_with("_spec.rb")
        || lower.ends_with("_test.rb")
        || lower.contains("/spec/")
        || lower.contains("/test/");
    let is_routing_context = lower.ends_with("routes.rb")
        || text.contains("sinatra")
        || text.contains("Sinatra")
        || text.contains(".routes.draw");
    if is_test_file || !is_routing_context {
        return;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?m)^\s*(get|post|put|delete|patch)\s+["']([^"']+)["']"#)
            .expect("valid regex")
    });
    for caps in re.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_handler(result, path, line, &caps[2], &method);
    }
}

/// Ruby HTTP clients: `Net::HTTP.get(URI(...))`, `Faraday.verb(...)`,
/// `HTTParty.verb(...)`.
fn scan_ruby_client(path: &str, text: &str, result: &mut ExtractionResult) {
    static NET: OnceLock<Regex> = OnceLock::new();
    static LIB: OnceLock<Regex> = OnceLock::new();
    let net = NET.get_or_init(|| {
        Regex::new(r#"Net\s*::\s*HTTP\s*\.\s*(get|post)\w*\s*\(\s*URI\s*\(\s*["']([^"']+)["']"#)
            .expect("valid regex")
    });
    for caps in net.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, &caps[2], "calls_service", &method);
    }
    let lib = LIB.get_or_init(|| {
        Regex::new(
            r#"\b(?:Faraday|HTTParty)\s*\.\s*(get|post|put|delete|patch)\s*\(\s*["']([^"']+)["']"#,
        )
        .expect("valid regex")
    });
    for caps in lib.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, &caps[2], "calls_service", &method);
    }
}

/// Spring (`@GetMapping`, `@RequestMapping(value=..., method=...)` with a
/// class-level `@RequestMapping` prefix) and JAX-RS (`@GET` + `@Path`, with a
/// class-level `@Path` prefix) routes. Kotlin Spring uses the same annotations.
fn scan_java_routes(path: &str, text: &str, result: &mut ExtractionResult) {
    static CLASS_PREFIX: OnceLock<Regex> = OnceLock::new();
    static VERB_MAPPING: OnceLock<Regex> = OnceLock::new();
    static REQ_MAPPING: OnceLock<Regex> = OnceLock::new();
    static JAXRS: OnceLock<Regex> = OnceLock::new();

    // `@RequestMapping("/api")` or `@Path("/things")` directly above a class.
    let class_prefix_re = CLASS_PREFIX.get_or_init(|| {
        Regex::new(
            r#"@\s*(?:RequestMapping|Path)\s*\(\s*"([^"]+)"\s*\)[\s\S]{0,200}?\b(?:class|interface)\s+\w+"#,
        )
        .expect("valid regex")
    });
    let mut class_prefixes: Vec<(usize, String)> = Vec::new();
    for caps in class_prefix_re.captures_iter(text) {
        class_prefixes.push((caps.get(0).expect("group 0").start(), caps[1].to_string()));
    }
    let prefix_for = |start: usize| -> &str {
        class_prefixes
            .iter()
            .filter(|(s, _)| *s <= start)
            .max_by_key(|(s, _)| *s)
            .map(|(_, p)| p.as_str())
            .unwrap_or("")
    };

    // `@GetMapping("/users")` ... method name.
    let verb_re = VERB_MAPPING.get_or_init(|| {
        Regex::new(
            r#"@\s*(Get|Post|Put|Delete|Patch)Mapping\s*\(\s*"([^"]+)"\s*\)[^(@]{0,200}?(\w+)\s*\("#,
        )
        .expect("valid regex")
    });
    for caps in verb_re.captures_iter(text) {
        let start = caps.get(0).expect("group 0").start();
        let method = caps[1].to_ascii_uppercase();
        let http_path = compose_prefix(prefix_for(start), &caps[2]);
        let line = line_of(text, caps.get(3).expect("group 3").start());
        emit_handler(result, path, line, &http_path, &method);
    }

    // `@RequestMapping(value = "/x", method = RequestMethod.POST)` on a method.
    let req_re = REQ_MAPPING.get_or_init(|| {
        Regex::new(
            r#"@\s*RequestMapping\s*\(\s*value\s*=\s*"([^"]+)"[^)]*?RequestMethod\s*\.\s*(\w+)[^)]*\)[^(@]{0,200}?(\w+)\s*\("#,
        )
        .expect("valid regex")
    });
    for caps in req_re.captures_iter(text) {
        let start = caps.get(0).expect("group 0").start();
        let http_path = compose_prefix(prefix_for(start), &caps[1]);
        let method = caps[2].to_ascii_uppercase();
        let line = line_of(text, caps.get(3).expect("group 3").start());
        emit_handler(result, path, line, &http_path, &method);
    }

    // JAX-RS: `@GET` (bare verb annotation) with a method-level `@Path`.
    let jaxrs_re = JAXRS.get_or_init(|| {
        Regex::new(
            r#"@\s*(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)\b\s*(?:@\s*Path\s*\(\s*"([^"]+)"\s*\)\s*)?[^(@]{0,200}?(\w+)\s*\("#,
        )
        .expect("valid regex")
    });
    for caps in jaxrs_re.captures_iter(text) {
        // Retrofit interfaces use `@GET("/x")` (parenthesized) -- that is a CLIENT
        // annotation handled by scan_java_client; the bare-verb JAX-RS form only.
        let start = caps.get(0).expect("group 0").start();
        let method = caps[1].to_ascii_uppercase();
        let leaf = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let prefix = prefix_for(start);
        if leaf.is_empty() && prefix.is_empty() {
            continue;
        }
        let http_path = if leaf.is_empty() {
            prefix.to_string()
        } else {
            compose_prefix(prefix, leaf)
        };
        let line = line_of(text, caps.get(3).expect("group 3").start());
        emit_handler(result, path, line, &http_path, &method);
    }
}

/// Java HTTP clients: RestTemplate verbs, `HttpRequest...uri(URI.create(...))`,
/// OkHttp `Request.Builder().url(...)`, Retrofit `@GET("/x")` interface methods.
fn scan_java_client(path: &str, text: &str, result: &mut ExtractionResult) {
    static REST_TEMPLATE: OnceLock<Regex> = OnceLock::new();
    static URI_CREATE: OnceLock<Regex> = OnceLock::new();
    static OKHTTP: OnceLock<Regex> = OnceLock::new();
    static RETROFIT: OnceLock<Regex> = OnceLock::new();

    let rest_re = REST_TEMPLATE.get_or_init(|| {
        Regex::new(
            r#"\.\s*(get|post|put|delete|patch)For(?:Object|Entity|Location)\s*\(\s*"([^"]+)""#,
        )
        .expect("valid regex")
    });
    for caps in rest_re.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, &caps[2], "calls_service", &method);
    }

    // java.net.http: URI.create in a request-builder file (method often set in a
    // later chained call; default GET).
    if text.contains("HttpRequest") {
        let uri_re = URI_CREATE.get_or_init(|| {
            Regex::new(r#"URI\s*\.\s*create\s*\(\s*"([^"]+)""#).expect("valid regex")
        });
        for caps in uri_re.captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            emit_route(result, path, line, &caps[1], "calls_service", "GET");
        }
    }

    if text.contains("okhttp") || text.contains("Request.Builder") {
        let ok_re =
            OKHTTP.get_or_init(|| Regex::new(r#"\.\s*url\s*\(\s*"([^"]+)""#).expect("valid regex"));
        for caps in ok_re.captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            emit_route(result, path, line, &caps[1], "calls_service", "GET");
        }
    }

    // Retrofit: parenthesized verb annotations on interface methods are client
    // declarations (`@GET("/api/retro")`).
    let retro_re = RETROFIT.get_or_init(|| {
        Regex::new(r#"@\s*(?:[\w.]+\.)?(GET|POST|PUT|DELETE|PATCH)\s*\(\s*"([^"]+)"\s*\)"#)
            .expect("valid regex")
    });
    for caps in retro_re.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, &caps[2], "calls_service", &method);
    }
}

/// ASP.NET Core routes: minimal-API `app.MapVerb("/p", ...)` and attribute
/// routing (`[HttpVerb("p")]` methods under an optional class-level
/// `[Route("api/[controller]")]` prefix, `[controller]` substituted from the
/// class name minus its `Controller` suffix).
fn scan_csharp_routes(path: &str, text: &str, result: &mut ExtractionResult) {
    static MAP: OnceLock<Regex> = OnceLock::new();
    let map_re = MAP.get_or_init(|| {
        Regex::new(r#"\.\s*Map(Get|Post|Put|Delete|Patch)\s*\(\s*"([^"]+)""#).expect("valid regex")
    });
    for caps in map_re.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_handler(result, path, line, &caps[2], &method);
    }

    static CLASS_ROUTE: OnceLock<Regex> = OnceLock::new();
    static HTTP_ATTR: OnceLock<Regex> = OnceLock::new();
    let class_route_re = CLASS_ROUTE.get_or_init(|| {
        Regex::new(r#"\[\s*Route\s*\(\s*"([^"]+)"\s*\)\s*\][\s\S]{0,200}?\bclass\s+(\w+)"#)
            .expect("valid regex")
    });
    let http_attr_re = HTTP_ATTR.get_or_init(|| {
        // `[HttpGet]` or `[HttpGet("bulk")]`, then the action method: the last
        // identifier before a `(` on the signature that follows.
        Regex::new(
            r#"\[\s*Http(Get|Post|Put|Delete|Patch)\s*(?:\(\s*"([^"]+)"\s*\))?\s*\](?:\s*\[[^\]]*\])*[^(\[{;]{0,160}?(\w+)\s*\("#,
        )
        .expect("valid regex")
    });
    let mut class_prefixes: Vec<(usize, String)> = Vec::new();
    for caps in class_route_re.captures_iter(text) {
        let class_name = &caps[2];
        let controller = class_name
            .trim_end_matches("Controller")
            .to_ascii_lowercase();
        let prefix = caps[1].replace("[controller]", &controller);
        class_prefixes.push((caps.get(0).expect("group 0").start(), prefix));
    }
    for caps in http_attr_re.captures_iter(text) {
        let start = caps.get(0).expect("group 0").start();
        let method = caps[1].to_ascii_uppercase();
        let leaf = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        // The nearest class-level [Route] above this action supplies the prefix.
        let prefix = class_prefixes
            .iter()
            .filter(|(s, _)| *s <= start)
            .max_by_key(|(s, _)| *s)
            .map(|(_, p)| p.as_str())
            .unwrap_or("");
        let http_path = if leaf.is_empty() {
            if prefix.is_empty() {
                continue;
            }
            prefix.to_string()
        } else {
            compose_prefix(prefix, leaf)
        };
        let line = line_of(text, caps.get(3).expect("group 3").start());
        emit_handler(result, path, line, &http_path, &method);
    }
}

/// HttpClient verb calls (`GetStringAsync`, `PostAsync`, `GetFromJsonAsync`,
/// ...) and explicit `new HttpRequestMessage(HttpMethod.X, "url")`. Gated on an
/// `HttpClient` mention so an arbitrary `.GetAsync("...")` is not misread.
fn scan_csharp_client(path: &str, text: &str, result: &mut ExtractionResult) {
    if !text.contains("HttpClient") {
        return;
    }
    static VERB: OnceLock<Regex> = OnceLock::new();
    static REQMSG: OnceLock<Regex> = OnceLock::new();
    let verb_re = VERB.get_or_init(|| {
        Regex::new(
            r#"\.\s*(Get|Post|Put|Delete|Patch)(?:String|Stream|ByteArray|FromJson)?Async\s*\(\s*"([^"]+)""#,
        )
        .expect("valid regex")
    });
    for caps in verb_re.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, &caps[2], "calls_service", &method);
    }
    let reqmsg_re = REQMSG.get_or_init(|| {
        Regex::new(r#"new\s+HttpRequestMessage\s*\(\s*HttpMethod\s*\.\s*(\w+)\s*,\s*"([^"]+)""#)
            .expect("valid regex")
    });
    for caps in reqmsg_re.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, &caps[2], "calls_service", &method);
    }
}

/// Rust route registrations: axum `.route("/p", get(handler))` (handler is a
/// named fn ref) and actix `#[get("/p")] fn handler` (decorator style).
/// Byte spans of `let <var> = Router::new()...;` statements, so a route
/// registered on a nested router variable can pick up its `.nest` prefix.
fn axum_router_var_spans(text: &str) -> Vec<(String, usize, usize)> {
    static LET: OnceLock<Regex> = OnceLock::new();
    let let_re = LET.get_or_init(|| {
        Regex::new(r#"\blet\s+(?:mut\s+)?([A-Za-z_]\w*)\s*(?::[^=;]*)?=\s*Router\s*::\s*new"#)
            .expect("valid regex")
    });
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    for caps in let_re.captures_iter(text) {
        let start = caps.get(0).expect("group 0").start();
        // Statement end: the first `;` at zero paren/brace depth (skips closure
        // bodies and nested calls).
        let mut depth = 0i32;
        let mut end = text.len();
        let mut i = start;
        while i < bytes.len() {
            match bytes[i] {
                b'(' | b'{' | b'[' => depth += 1,
                b')' | b'}' | b']' => depth -= 1,
                b';' if depth == 0 => {
                    end = i;
                    break;
                }
                b'"' => {
                    i = skip_string(bytes, i);
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
        spans.push((caps[1].to_string(), start, end));
    }
    spans
}

fn scan_rust_routes(
    path: &str,
    text: &str,
    prefixes: &HashMap<String, String>,
    result: &mut ExtractionResult,
) {
    let var_spans = if prefixes.is_empty() {
        Vec::new()
    } else {
        axum_router_var_spans(text)
    };
    for caps in axum_route_open_re().captures_iter(text) {
        let whole = caps.get(0).expect("group 0");
        // A route inside `let sub = Router::new()...;` composes sub's nest prefix.
        let prefix = var_spans
            .iter()
            .find(|(_, s, e)| *s <= whole.start() && whole.start() < *e)
            .and_then(|(var, _, _)| prefixes.get(var))
            .map(String::as_str)
            .unwrap_or("");
        let http_path = compose_prefix(prefix, &caps[1]);
        let http_path = http_path.as_str();
        // Second argument spans from after the comma to the balanced `)` of
        // `.route(`, so a chain never bleeds into the next `.route(...)` call.
        let open = text[whole.start()..whole.end()]
            .find('(')
            .map(|i| whole.start() + i)
            .unwrap_or(whole.start());
        // Bounded + char-safe: an unbalanced `.route(` (paren_end = EOF) must
        // neither scan the rest of the file for handlers nor split a char.
        let span_end = paren_end(text, open).saturating_sub(1).max(whole.end());
        let span_end = floor_char_boundary(text, span_end.min(whole.end() + 2000));
        let args = &text[whole.end()..span_end.max(whole.end())];
        for pair in axum_method_pair_re().captures_iter(args) {
            let method = pair[1].to_ascii_uppercase();
            // The handler may be a qualified path (`handlers::serve`); key on its
            // last segment so it matches the function node's bare name.
            let handler = last_path_segment(&pair[2]);
            let line = line_of(text, whole.start());
            emit_handler_named(result, path, line, http_path, &method, handler);
        }
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
/// The receiver's router/blueprint prefix (if any) composes into the path.
fn scan_py_routes(
    path: &str,
    text: &str,
    prefixes: &HashMap<String, String>,
    result: &mut ExtractionResult,
) {
    for caps in py_route_re().captures_iter(text) {
        let receiver = &caps[1];
        let verb = &caps[2];
        let prefix = prefixes.get(receiver).map(String::as_str).unwrap_or("");
        let http_path = compose_prefix(prefix, &caps[3]);
        let method = if verb.eq_ignore_ascii_case("route") {
            flask_methods(&caps[4])
        } else {
            verb.to_ascii_uppercase()
        };
        // Line of the handler fn name, so the edge attributes to that function.
        let line = line_of(text, caps.get(5).expect("group 5").start());
        emit_handler(result, path, line, &http_path, &method);
    }
}

/// Django URLconf routes: `path("users/", views.list_users)` / `re_path(...)`
/// inside a `urlpatterns` file. Regex anchors are stripped; the named view is
/// recorded for the cross-file handler-resolution pass. Method is ANY (Django
/// dispatches per-view).
fn scan_django_urls(path: &str, text: &str, result: &mut ExtractionResult) {
    if !text.contains("urlpatterns") {
        return;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"\b(?:re_)?path\s*\(\s*r?["']([^"']+)["']\s*,\s*([\w.]+)"#)
            .expect("valid regex")
    });
    for caps in re.captures_iter(text) {
        let raw = caps[1].trim_start_matches('^').trim_end_matches('$');
        let handler = last_path_segment(&caps[2]);
        // `path("api/", include("api.urls"))` mounts a sub-URLconf; `include`
        // is not a view and must not become a resolvable handler name (W9).
        if handler == "include" {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_handler_named(result, path, line, raw, "ANY", handler);
    }
}

/// aiohttp server routes: `app.router.add_get("/p", handler)` (the decorator
/// form `@routes.get("/p")` is covered by the generic decorator scan).
fn scan_aiohttp_server(path: &str, text: &str, result: &mut ExtractionResult) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"\.\s*add_(get|post|put|delete|patch|head|options)\s*\(\s*["']([^"']+)["']\s*,\s*([\w.]+)"#,
        )
        .expect("valid regex")
    });
    for caps in re.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let handler = last_path_segment(&caps[3]);
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_handler_named(result, path, line, &caps[2], &method, handler);
    }
}

/// Python clients beyond requests/httpx: an aiohttp session's verb calls
/// (absolute URL only, gated on an aiohttp mention -- same receiver-blind
/// guard as the reqwest builder) and `urllib.request.urlopen`.
fn scan_py_url_clients(path: &str, text: &str, result: &mut ExtractionResult) {
    if text.contains("aiohttp") {
        static VERB: OnceLock<Regex> = OnceLock::new();
        let verb_re = VERB.get_or_init(|| {
            Regex::new(r#"\.\s*(get|post|put|delete|patch)\s*\(\s*["'](https?://[^"']+)["']"#)
                .expect("valid regex")
        });
        for caps in verb_re.captures_iter(text) {
            let method = caps[1].to_ascii_uppercase();
            let line = line_of(text, caps.get(0).expect("group 0").start());
            emit_route(result, path, line, &caps[2], "calls_service", &method);
        }
    }
    static URLOPEN: OnceLock<Regex> = OnceLock::new();
    let urlopen_re = URLOPEN
        .get_or_init(|| Regex::new(r#"\burlopen\s*\(\s*["']([^"']+)["']"#).expect("valid regex"));
    for caps in urlopen_re.captures_iter(text) {
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_route(result, path, line, &caps[1], "calls_service", "GET");
    }
}

/// Methods from a Flask `methods=["PUT", "POST", ...]` kwarg, joined with `,`;
/// defaults to GET. All entries are kept (the old first-entry capture dropped
/// the rest -- 2026-07 audit).
fn flask_methods(tail: &str) -> String {
    static LIST: OnceLock<Regex> = OnceLock::new();
    static ITEM: OnceLock<Regex> = OnceLock::new();
    let list =
        LIST.get_or_init(|| Regex::new(r#"methods\s*=\s*\[([^\]]*)\]"#).expect("valid regex"));
    let item = ITEM.get_or_init(|| Regex::new(r#"["'](\w+)["']"#).expect("valid regex"));
    let methods: Vec<String> = list
        .captures(tail)
        .map(|c| {
            item.captures_iter(&c[1])
                .map(|m| m[1].to_ascii_uppercase())
                .collect()
        })
        .unwrap_or_default();
    if methods.is_empty() {
        "GET".to_string()
    } else {
        methods.join(",")
    }
}

/// `(app|router).VERB("/path"` style route registrations (Express). A receiver
/// mounted via `app.use('/prefix', receiver)` gets its mount prefix composed.
fn scan_express_routes(
    path: &str,
    text: &str,
    prefixes: &HashMap<String, String>,
    result: &mut ExtractionResult,
) {
    // Known HTTP-client receivers must not register as servers: with the
    // any-receiver widening, `axios.post('/x', data)` otherwise satisfies the
    // path + comma shape (caught by the 2026-07 re-audit).
    const CLIENT_RECEIVERS: &[&str] = &["axios", "got", "ky", "superagent", "request", "http"];
    // A receiver other than the classic app/router only registers routes in a
    // file that visibly uses a server framework -- otherwise every client
    // wrapper (`api.post('/x', payload)`) becomes a fake server (wave-2 W1).
    let has_server_token = [
        "express", "fastify", "koa", "Hono", "hono", "@nestjs", "restify",
    ]
    .iter()
    .any(|t| text.contains(t));
    let client_bases = collect_client_bases("js", text);
    for caps in express_route_re().captures_iter(text) {
        let receiver = &caps[1];
        if CLIENT_RECEIVERS.contains(&receiver) || client_bases.contains_key(receiver) {
            continue;
        }
        if !matches!(receiver, "app" | "router") && !has_server_token {
            continue;
        }
        // A handler argument is a function/identifier/array; a body argument
        // (`{...}`, a string, a number) is a client call's payload.
        if matches!(&caps[4], "{" | "\"" | "'" | "`")
            || caps[4].chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            continue;
        }
        let method = caps[2].to_ascii_uppercase();
        let prefix = prefixes.get(receiver).map(String::as_str).unwrap_or("");
        let http_path = compose_prefix(prefix, &caps[3]);
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_handler(result, path, line, &http_path, &method);
    }
}

/// `obj.HandleFunc("/path", ...)` (Go net/http + mux). Go 1.22 ServeMux allows a
/// `"METHOD /path"` pattern (e.g. `"GET /healthz"`), so a leading HTTP method is
/// split out into the edge context, leaving a clean path. Framework routers
/// (gin/echo/fiber `r.GET`, chi `r.Get`) and gorilla's `.Methods("X")` suffix
/// are covered too (2026-07 audit).
fn scan_go_routes(path: &str, text: &str, result: &mut ExtractionResult) {
    // gorilla: `.HandleFunc("/p", handler).Methods("POST")` -- match first so the
    // plain HandleFunc pass can skip these (no double emit, correct method).
    static GORILLA: OnceLock<Regex> = OnceLock::new();
    let gorilla = GORILLA.get_or_init(|| {
        Regex::new(
            r#"\.\s*HandleFunc\s*\(\s*["']([^"']+)["']\s*,\s*([\w.]+)\s*\)\s*\.\s*Methods\s*\(\s*["'](\w+)["']"#,
        )
        .expect("valid regex")
    });
    let mut gorilla_starts: Vec<usize> = Vec::new();
    for caps in gorilla.captures_iter(text) {
        let whole = caps.get(0).expect("group 0");
        gorilla_starts.push(whole.start());
        let handler = last_path_segment(&caps[2]);
        let method = caps[3].to_ascii_uppercase();
        let line = line_of(text, whole.start());
        emit_handler_named(result, path, line, &caps[1], &method, handler);
    }
    for caps in go_route_re().captures_iter(text) {
        let whole = caps.get(0).expect("group 0");
        // `.HandleFunc(` sits a few bytes into the gorilla match; skip overlaps.
        if gorilla_starts
            .iter()
            .any(|s| *s <= whole.start() && whole.start() < s + 24)
        {
            continue;
        }
        let (method, http_path) = split_go_route_pattern(&caps[1]);
        let line = line_of(text, whole.start());
        emit_handler(result, path, line, http_path, method);
    }
    // gin/echo/fiber `r.GET("/p", handler)` and chi `r.Get("/p", handler)`: any
    // receiver, a verb method, and a named-handler second argument. Gated on a
    // web-framework import so a consul-style `kv.Get("/config", nil)` is not a
    // route (wave-2 W10); non-handler idents (`nil`) are rejected too.
    let has_framework_token = [
        "gin-gonic",
        "labstack/echo",
        "go-chi",
        "chi.NewRouter",
        "gofiber",
        "gorilla/mux",
    ]
    .iter()
    .any(|t| text.contains(t));
    if !has_framework_token {
        return;
    }
    static FRAMEWORK: OnceLock<Regex> = OnceLock::new();
    let framework = FRAMEWORK.get_or_init(|| {
        Regex::new(
            r#"\b\w+\s*\.\s*(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS|Get|Post|Put|Delete|Patch)\s*\(\s*["']([^"']+)["']\s*,\s*([\w.]+)"#,
        )
        .expect("valid regex")
    });
    for caps in framework.captures_iter(text) {
        let method = caps[1].to_ascii_uppercase();
        let http_path = &caps[2];
        if !http_path.starts_with('/') {
            continue;
        }
        let handler = last_path_segment(&caps[3]);
        if matches!(handler, "nil" | "true" | "false") {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_handler_named(result, path, line, http_path, &method, handler);
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

/// Normalize helper-function names that double as method spellings:
/// `PostForm` -> POST (it IS a POST); everything else is already a method.
fn client_verb_method(verb: &str) -> String {
    let up = verb.to_ascii_uppercase();
    match up.as_str() {
        "POSTFORM" => "POST".to_string(),
        _ => up,
    }
}

/// `"{param}"` when the string literal ending at `end` is followed by a `+`
/// (string concatenation): the built URL continues with a runtime value, so the
/// captured prefix alone would key a wrong (truncated) route (2026-07 audit).
fn concat_param(text: &str, end: usize) -> &'static str {
    let rest = text[end.min(text.len())..].trim_start();
    if rest.starts_with('+') {
        "{param}"
    } else {
        ""
    }
}

/// `client.VERB("url"` style client calls (requests/httpx/axios/http.Get).
/// Group 2 is a plain string URL; an optional group 3 is a template literal.
fn scan_verb_client(re: &Regex, path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in re.captures_iter(text) {
        let whole = caps.get(0).expect("group 0");
        let method = client_verb_method(&caps[1]);
        let url = match (caps.get(2), caps.get(3)) {
            (Some(u), _) => format!("{}{}", u.as_str(), concat_param(text, whole.end())),
            (None, Some(t)) => match template_to_path(t.as_str(), "${", '}') {
                Some(u) => u,
                None => continue,
            },
            _ => continue,
        };
        let line = line_of(text, whole.start());
        emit_route(result, path, line, &url, "calls_service", &method);
    }
}

/// `fetch("url")` -- the `method:` option in the second argument is respected
/// (default GET), and a concatenated tail becomes a `{param}` segment.
fn scan_fetch(path: &str, text: &str, result: &mut ExtractionResult) {
    static METHOD: OnceLock<Regex> = OnceLock::new();
    let method_re =
        METHOD.get_or_init(|| Regex::new(r#"method\s*:\s*["'](\w+)["']"#).expect("valid regex"));
    for caps in fetch_re().captures_iter(text) {
        let whole = caps.get(0).expect("group 0");
        let url = match (caps.get(1), caps.get(2)) {
            (Some(u), _) => format!("{}{}", u.as_str(), concat_param(text, whole.end())),
            (None, Some(t)) => match template_to_path(t.as_str(), "${", '}') {
                Some(u) => u,
                None => continue,
            },
            _ => continue,
        };
        // Look for `method: 'X'` inside the same call's options argument only:
        // the window is bounded by the fetch call's balanced `)` so a later
        // call's options never bleed backwards (wave-2 W4), with a hard cap for
        // pathological bodies.
        let open = text[whole.start()..whole.end()]
            .find('(')
            .map(|i| whole.start() + i)
            .unwrap_or(whole.start());
        let call_end = paren_end(text, open).saturating_sub(1);
        let window_end = floor_char_boundary(text, call_end.min(whole.end() + 400));
        let window = &text[whole.end()..window_end.max(whole.end())];
        let method = method_re
            .captures(window)
            .map(|m| m[1].to_ascii_uppercase())
            .unwrap_or_else(|| "GET".to_string());
        let line = line_of(text, whole.start());
        emit_route(result, path, line, &url, "calls_service", &method);
    }
}

/// Canonical form of a normalized route path: every parameter segment becomes
/// `{p}` (`{p*}` for a catch-all), literal segments are lowercased. Equivalent
/// templates across frameworks (`:id` / `{id}` / `<int:id>`) share one canon;
/// a literal `/users/id` does not join the `/users/{id}` template.
fn route_canon(np: &str) -> String {
    let mut out = String::new();
    for seg in np.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        let catchall = seg.starts_with('*') || seg.contains("{*") || seg.starts_with(":*");
        let param = catchall || seg.starts_with(':') || seg.contains('{') || seg.contains('<');
        if catchall {
            out.push_str("{p*}");
        } else if param {
            out.push_str("{p}");
        } else {
            out.push_str(&seg.to_ascii_lowercase());
        }
    }
    if out.is_empty() {
        "/".to_string()
    } else {
        out
    }
}

/// FNV-1a 32-bit hex of `s`. Appended to route ids because `make_id`'s
/// punctuation folding alone collides distinct paths (`/a-b` vs `/a/b`,
/// `/users/id` vs `/users/{id}` -- 2026-07 audit).
fn fnv32(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// Collision-safe node id for a route path: readable `make_id` prefix over the
/// canonical path, plus the canon's FNV hash to keep folded-away distinctions.
fn route_node_id(np: &str) -> NodeId {
    let canon = route_canon(np);
    NodeId(format!("{}_{}", make_id(&["route", &canon]), fnv32(&canon)))
}

/// Collision-safe id for a non-route boundary node (queue/ws/ipc/event):
/// `make_id` folding alone collides `orders.new` with `orders_new` (same fix
/// as routes; wave-2 low).
fn boundary_node_id(prefix: &str, key: &str) -> NodeId {
    NodeId(format!("{}_{}", make_id(&[prefix, key]), fnv32(key)))
}

/// `ensure_target` for a route node, recording `_route_canon` so federation-side
/// dedup can merge equivalent templates by canon instead of raw label.
fn ensure_route_target(result: &mut ExtractionResult, id: &NodeId, np: &str) {
    ensure_target(result, id, np, "route");
    if let Some(n) = result.nodes.iter_mut().find(|n| &n.id == id) {
        n.extra
            .entry("_route_canon".to_string())
            .or_insert_with(|| json!(route_canon(np)));
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
    // A bare `/` is too generic to key on (every service has one) -- same guard
    // the WebSocket endpoints use (2026-07 audit).
    if np.is_empty() || np == "/" {
        return;
    }
    let target = route_node_id(&np);
    ensure_route_target(result, &target, &np);
    // Matching stays path-keyed (by design), but the authority is preserved as
    // edge context so a consumer can tell `svc-a/users` from `svc-b/users`.
    let ctx = match url_authority(http_path) {
        Some(host) => format!("{method} {host}"),
        None => method.to_string(),
    };
    link(result, path, line, target, relation, &ctx);
}

/// The authority (host[:port]) of an absolute URL, userinfo stripped; None for
/// relative paths or authority-less/hole-valued authorities.
fn url_authority(raw: &str) -> Option<String> {
    let idx = raw.find("://")?;
    let after = &raw[idx + 3..];
    let end = after.find('/').unwrap_or(after.len());
    let auth = &after[..end];
    let host = auth.rsplit('@').next().unwrap_or(auth);
    if host.is_empty() || host.contains("{param}") || host.contains("${") {
        None
    } else {
        Some(host.to_string())
    }
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
    // Bare `/` is too generic to key (mirrors the client-side guard; wave-2 W11
    // -- canon dedup would merge every service's root cross-repo).
    if np.is_empty() || np == "/" {
        return;
    }
    let route = route_node_id(&np);
    ensure_route_target(result, &route, &np);
    // Record the handler name + method on the route node so a graph pass can
    // resolve the handler when it is defined in another file. Chained
    // registrations (`get(a).post(b)`) APPEND to `_route_handlers` -- the old
    // single slot kept only the last pair (wave-2 W6); the single-slot keys
    // stay for the first pair for compatibility.
    if let Some(n) = result.nodes.iter_mut().find(|n| n.id == route) {
        n.extra
            .entry("_route_handler".to_string())
            .or_insert_with(|| json!(handler_name));
        n.extra
            .entry("_route_method".to_string())
            .or_insert_with(|| json!(method));
        let list = n
            .extra
            .entry("_route_handlers".to_string())
            .or_insert_with(|| json!([]));
        if let Some(arr) = list.as_array_mut() {
            let entry = json!({ "name": handler_name, "method": method });
            if !arr.contains(&entry) {
                arr.push(entry);
            }
        }
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
    if np.is_empty() || np == "/" {
        return;
    }
    let route = route_node_id(&np);
    ensure_route_target(result, &route, &np);
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
    // Generated protobuf/gRPC code defines the Stub/Client classes themselves;
    // treating those definitions as call sites minted edges from the codegen
    // (2026-07 audit). The handwritten callers are what we want.
    if is_grpc_codegen_file(path) {
        return;
    }
    match ext {
        "rs" => scan_rust_grpc(path, text, result),
        "py" => scan_python_grpc(path, text, result),
        "go" => scan_go_grpc(path, text, result),
        "java" | "kt" => scan_java_grpc(path, text, result),
        "cs" => scan_csharp_grpc(path, text, result),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            scan_js_grpc(path, text, result)
        }
        _ => {}
    }
}

/// True for generated protobuf/gRPC sources (`*_pb2.py`, `*_pb2_grpc.py`,
/// `*.pb.go`, `*_grpc.pb.go`, `*_pb.js`, `*_grpc_pb.js`).
fn is_grpc_codegen_file(path: &str) -> bool {
    let stem = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    stem.ends_with("_pb2")
        || stem.ends_with("_pb2_grpc")
        || stem.ends_with(".pb")
        || stem.ends_with("_grpc.pb")
        || stem.ends_with("_pb")
        || stem.ends_with("_grpc_pb")
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
    // Server side (2026-07 audit: only clients were detected, so a Python gRPC
    // service dead-ended one-sided): a `<Svc>Servicer` subclass or an
    // `add_<Svc>Servicer_to_server(...)` registration.
    static SERVICER: OnceLock<Regex> = OnceLock::new();
    let servicer_re = SERVICER.get_or_init(|| {
        Regex::new(
            r#"(?:class\s+\w+\s*\(\s*[\w.]*?(\w+)Servicer\s*\)|add_(\w+)Servicer_to_server)"#,
        )
        .expect("valid regex")
    });
    let mut seen: Vec<String> = Vec::new();
    for caps in servicer_re.captures_iter(text) {
        let svc = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        if svc.is_empty() || seen.contains(&svc) {
            continue;
        }
        seen.push(svc.clone());
        let line = line_of(text, caps.get(0).expect("group 0").start());
        let handler = enclosing_function(result, line).unwrap_or_else(|| file_node_id(path));
        emit_grpc_handler(result, path, line, &svc, handler);
    }
}

/// Go gRPC: `pb.RegisterGreeterServer(s, impl)` (server) and
/// `pb.NewGreeterClient(conn)` (client), gated on a grpc mention.
fn scan_go_grpc(path: &str, text: &str, result: &mut ExtractionResult) {
    if !text.contains("grpc") {
        return;
    }
    static REG: OnceLock<Regex> = OnceLock::new();
    static NEW: OnceLock<Regex> = OnceLock::new();
    let reg_re =
        REG.get_or_init(|| Regex::new(r#"\bRegister(\w+)Server\s*\("#).expect("valid regex"));
    for caps in reg_re.captures_iter(text) {
        let line = line_of(text, caps.get(0).expect("group 0").start());
        let handler = enclosing_function(result, line).unwrap_or_else(|| file_node_id(path));
        emit_grpc_handler(result, path, line, &caps[1], handler);
    }
    let new_re = NEW.get_or_init(|| Regex::new(r#"\bNew(\w+)Client\s*\("#).expect("valid regex"));
    for caps in new_re.captures_iter(text) {
        let svc = &caps[1];
        if is_non_grpc_client(svc) {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_grpc_client(result, path, line, svc);
    }
}

/// Java gRPC: `extends <Svc>Grpc.<Svc>ImplBase` (server) and
/// `<Svc>Grpc.newBlockingStub(...)` (client).
fn scan_java_grpc(path: &str, text: &str, result: &mut ExtractionResult) {
    if !text.contains("Grpc") {
        return;
    }
    static IMPL: OnceLock<Regex> = OnceLock::new();
    static STUB: OnceLock<Regex> = OnceLock::new();
    let impl_re = IMPL.get_or_init(|| {
        Regex::new(r#"extends\s+(\w+)Grpc\s*\.\s*\w*ImplBase"#).expect("valid regex")
    });
    for caps in impl_re.captures_iter(text) {
        let line = line_of(text, caps.get(0).expect("group 0").start());
        let handler = enclosing_function(result, line).unwrap_or_else(|| file_node_id(path));
        emit_grpc_handler(result, path, line, &caps[1], handler);
    }
    let stub_re = STUB.get_or_init(|| {
        Regex::new(r#"(\w+)Grpc\s*\.\s*new(?:Blocking|Async|Future)?Stub\s*\("#)
            .expect("valid regex")
    });
    for caps in stub_re.captures_iter(text) {
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_grpc_client(result, path, line, &caps[1]);
    }
}

/// C# Grpc.Net: `: Greeter.GreeterBase` (server) and
/// `new Greeter.GreeterClient(channel)` (client); the qualifier and the
/// Base/Client prefix must agree, which the codegen guarantees.
fn scan_csharp_grpc(path: &str, text: &str, result: &mut ExtractionResult) {
    if !text.contains("Grpc") {
        return;
    }
    static BASE: OnceLock<Regex> = OnceLock::new();
    static CLIENT: OnceLock<Regex> = OnceLock::new();
    let base_re =
        BASE.get_or_init(|| Regex::new(r#":\s*(\w+)\s*\.\s*(\w+)Base\b"#).expect("valid regex"));
    for caps in base_re.captures_iter(text) {
        if caps[1] != caps[2] {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        let handler = enclosing_function(result, line).unwrap_or_else(|| file_node_id(path));
        emit_grpc_handler(result, path, line, &caps[1], handler);
    }
    let client_re = CLIENT
        .get_or_init(|| Regex::new(r#"new\s+(\w+)\s*\.\s*(\w+)Client\s*\("#).expect("valid regex"));
    for caps in client_re.captures_iter(text) {
        if caps[1] != caps[2] {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        emit_grpc_client(result, path, line, &caps[1]);
    }
}

/// JS/TS @grpc/grpc-js: `new <Svc>Client(addr, creds)` clients, gated on a
/// grpc-js mention (server-side `addService` shapes are not regex-tractable).
fn scan_js_grpc(path: &str, text: &str, result: &mut ExtractionResult) {
    if !text.contains("@grpc") && !text.contains("grpc-js") {
        return;
    }
    static NEW: OnceLock<Regex> = OnceLock::new();
    let new_re = NEW.get_or_init(|| Regex::new(r#"new\s+(\w+)Client\s*\("#).expect("valid regex"));
    for caps in new_re.captures_iter(text) {
        let svc = &caps[1];
        if is_non_grpc_client(svc) {
            continue;
        }
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

// --- message-queue / pub-sub boundaries ---
//
// Queue-mediated coupling is the canonical "0 static dependents" hazard: a
// producer and a consumer never reference each other, only a topic name. Each
// detected site attaches to a `queue #<topic>` boundary node (node type
// `queue_topic`), producers via `calls_service` and consumers via `handled_by`
// (context "queue"), so cross-repo flagging and reverse impact apply unchanged.
// Every pattern is gated on a library token -- a generic `.publish(`/`.subscribe(`
// alone never mints a boundary (2026-07 audit).

/// Attach a queue-topic boundary site. Producer = Client role, consumer = Server.
fn queue_message(result: &mut ExtractionResult, path: &str, line: u32, topic: &str, role: WsRole) {
    let key = topic.trim().to_ascii_lowercase();
    if key.is_empty() {
        return;
    }
    let node = boundary_node_id("queue", &key);
    ensure_target(
        result,
        &node,
        &format!("queue #{}", topic.trim()),
        "queue_topic",
    );
    boundary_link(result, path, line, node, role, "queue");
}

fn scan_queues(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    match ext {
        "py" => scan_queues_python(path, text, result),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            scan_queues_ecmascript(path, text, result)
        }
        "java" | "kt" => scan_queues_java(path, text, result),
        _ => {}
    }
}

fn scan_queues_python(path: &str, text: &str, result: &mut ExtractionResult) {
    // A file can mention several queue libraries (kafka + redis); the generic
    // publish/subscribe shapes overlap, so dedup per (site, topic, role).
    let mut seen: std::collections::HashSet<(usize, String, bool)> =
        std::collections::HashSet::new();
    let mut emit_all = |re: &Regex, role: WsRole, result: &mut ExtractionResult| {
        for caps in re.captures_iter(text) {
            let start = caps.get(0).expect("group 0").start();
            let is_client = matches!(role, WsRole::Client);
            if !seen.insert((start, caps[1].to_string(), is_client)) {
                continue;
            }
            let line = line_of(text, start);
            queue_message(result, path, line, &caps[1], role);
        }
    };
    if text.contains("kafka") || text.contains("Kafka") {
        static SEND: OnceLock<Regex> = OnceLock::new();
        static SUB: OnceLock<Regex> = OnceLock::new();
        // Producer-shaped receiver required: a `sock.send("ping")` in a file
        // that merely imports kafka is not a publish (wave-2 W13).
        emit_all(
            SEND.get_or_init(|| {
                Regex::new(r#"\b\w*[Pp]roducer\w*\s*\.\s*send\s*\(\s*["']([^"']+)["']"#)
                    .expect("valid regex")
            }),
            WsRole::Client,
            result,
        );
        emit_all(
            SUB.get_or_init(|| {
                Regex::new(
                    r#"(?:\.\s*subscribe\s*\(\s*\[?\s*|KafkaConsumer\s*\(\s*)["']([^"']+)["']"#,
                )
                .expect("valid regex")
            }),
            WsRole::Server,
            result,
        );
    }
    if text.contains("pika") || text.contains("amqp") {
        static PUB: OnceLock<Regex> = OnceLock::new();
        static CON: OnceLock<Regex> = OnceLock::new();
        emit_all(
            PUB.get_or_init(|| {
                Regex::new(r#"basic_publish\s*\([^)]*routing_key\s*=\s*["']([^"']+)["']"#)
                    .expect("valid regex")
            }),
            WsRole::Client,
            result,
        );
        emit_all(
            CON.get_or_init(|| {
                Regex::new(r#"basic_consume\s*\([^)]*queue\s*=\s*["']([^"']+)["']"#)
                    .expect("valid regex")
            }),
            WsRole::Server,
            result,
        );
    }
    if text.contains("nats") || text.contains("redis") {
        static PUB: OnceLock<Regex> = OnceLock::new();
        static SUB: OnceLock<Regex> = OnceLock::new();
        emit_all(
            PUB.get_or_init(|| {
                Regex::new(r#"\.\s*publish\s*\(\s*["']([^"']+)["']"#).expect("valid regex")
            }),
            WsRole::Client,
            result,
        );
        emit_all(
            SUB.get_or_init(|| {
                Regex::new(r#"\.\s*subscribe\s*\(\s*["']([^"']+)["']"#).expect("valid regex")
            }),
            WsRole::Server,
            result,
        );
    }
    if text.contains("celery") || text.contains("Celery") {
        // Worker: `@app.task def name` keys `task:<name>`; producers use
        // `send_task("pkg.name")` (last dotted segment) or `name.delay(...)`.
        static TASK: OnceLock<Regex> = OnceLock::new();
        static SEND_TASK: OnceLock<Regex> = OnceLock::new();
        static DELAY: OnceLock<Regex> = OnceLock::new();
        let task_re = TASK.get_or_init(|| {
            Regex::new(r#"@\s*[\w.]*task\b[\s\S]{0,80}?def\s+(\w+)"#).expect("valid regex")
        });
        for caps in task_re.captures_iter(text) {
            let line = line_of(text, caps.get(1).expect("group 1").start());
            queue_message(
                result,
                path,
                line,
                &format!("task:{}", &caps[1]),
                WsRole::Server,
            );
        }
        let send_re = SEND_TASK.get_or_init(|| {
            Regex::new(r#"send_task\s*\(\s*["']([\w.]+)["']"#).expect("valid regex")
        });
        for caps in send_re.captures_iter(text) {
            let name = caps[1].rsplit('.').next().unwrap_or(&caps[1]).to_string();
            let line = line_of(text, caps.get(0).expect("group 0").start());
            queue_message(result, path, line, &format!("task:{name}"), WsRole::Client);
        }
        let delay_re = DELAY.get_or_init(|| {
            Regex::new(r#"\b(\w+)\s*\.\s*(?:delay|apply_async)\s*\("#).expect("valid regex")
        });
        for caps in delay_re.captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            queue_message(
                result,
                path,
                line,
                &format!("task:{}", &caps[1]),
                WsRole::Client,
            );
        }
    }
}

fn scan_queues_ecmascript(path: &str, text: &str, result: &mut ExtractionResult) {
    if text.contains("kafkajs") || text.contains("Kafka") {
        static SEND: OnceLock<Regex> = OnceLock::new();
        static SUB: OnceLock<Regex> = OnceLock::new();
        let send_re = SEND.get_or_init(|| {
            Regex::new(r#"\.\s*send\s*\(\s*\{[^}]*topic\s*:\s*["'`]([^"'`]+)["'`]"#)
                .expect("valid regex")
        });
        for caps in send_re.captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            queue_message(result, path, line, &caps[1], WsRole::Client);
        }
        let sub_re = SUB.get_or_init(|| {
            Regex::new(r#"\.\s*subscribe\s*\(\s*\{[^}]*topic\s*:\s*["'`]([^"'`]+)["'`]"#)
                .expect("valid regex")
        });
        for caps in sub_re.captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            queue_message(result, path, line, &caps[1], WsRole::Server);
        }
    }
    if text.contains("amqplib") || text.contains("amqp") {
        static SEND: OnceLock<Regex> = OnceLock::new();
        static CON: OnceLock<Regex> = OnceLock::new();
        let send_re = SEND.get_or_init(|| {
            Regex::new(r#"sendToQueue\s*\(\s*["'`]([^"'`]+)["'`]"#).expect("valid regex")
        });
        for caps in send_re.captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            queue_message(result, path, line, &caps[1], WsRole::Client);
        }
        let con_re = CON.get_or_init(|| {
            Regex::new(r#"\.\s*consume\s*\(\s*["'`]([^"'`]+)["'`]"#).expect("valid regex")
        });
        for caps in con_re.captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            queue_message(result, path, line, &caps[1], WsRole::Server);
        }
    }
    if text.contains("nats") || text.contains("redis") {
        static PUB: OnceLock<Regex> = OnceLock::new();
        static SUB: OnceLock<Regex> = OnceLock::new();
        let pub_re = PUB.get_or_init(|| {
            Regex::new(r#"\.\s*publish\s*\(\s*["'`]([^"'`]+)["'`]"#).expect("valid regex")
        });
        for caps in pub_re.captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            queue_message(result, path, line, &caps[1], WsRole::Client);
        }
        let sub_re = SUB.get_or_init(|| {
            Regex::new(r#"\.\s*subscribe\s*\(\s*["'`]([^"'`]+)["'`]"#).expect("valid regex")
        });
        for caps in sub_re.captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            queue_message(result, path, line, &caps[1], WsRole::Server);
        }
    }
}

fn scan_queues_java(path: &str, text: &str, result: &mut ExtractionResult) {
    if !text.contains("kafka") && !text.contains("Kafka") {
        return;
    }
    static LISTENER: OnceLock<Regex> = OnceLock::new();
    static TEMPLATE: OnceLock<Regex> = OnceLock::new();
    // `@KafkaListener(topics = "orders")` ... method -- attribute to the method.
    let listener_re = LISTENER.get_or_init(|| {
        Regex::new(
            r#"@\s*KafkaListener\s*\([^)]*topics\s*=\s*["']([^"']+)["'][^)]*\)[^(@]{0,200}?(\w+)\s*\("#,
        )
        .expect("valid regex")
    });
    for caps in listener_re.captures_iter(text) {
        let line = line_of(text, caps.get(2).expect("group 2").start());
        queue_message(result, path, line, &caps[1], WsRole::Server);
    }
    let template_re = TEMPLATE.get_or_init(|| {
        Regex::new(r#"[Tt]emplate\s*\.\s*send\s*\(\s*["']([^"']+)["']"#).expect("valid regex")
    });
    for caps in template_re.captures_iter(text) {
        let line = line_of(text, caps.get(0).expect("group 0").start());
        queue_message(result, path, line, &caps[1], WsRole::Client);
    }
}

// --- WebSocket message + endpoint boundaries ---
//
// WebSocket coupling the per-language AST walk does not model: a client opens a
// socket and exchanges JSON command messages (or socket.io events) with a server.
// Two boundary-node kinds, both keyed so a client and a server meet at one node
// after federation (no resolution pass for same-repo):
//   - endpoint  `wsendpoint:<path>` -- the socket URL path itself (named paths
//     only; a bare `/` is too generic to key on)
//   - message   `wsmsg:<command>`   -- one application message type / event name
// Client sites attach via `calls_service`; server handlers via `handled_by` -- the
// same relations HTTP/gRPC use, so the federation cross-repo flagging applies
// unchanged. Detection is regex/heuristic and INFERRED. Covered: JS/TS raw `ws`
// (`.send({cmd})` + `case`) and socket.io (`emit`/`on`); C# WebSocketSharp /
// System.Net.WebSockets (`AddWebSocketService` + `case`); Python `websockets` +
// python-socketio; Rust tungstenite (endpoint only). The command-keyed node is
// intentionally endpoint-independent because the URL and the message sites are
// routinely in different files (e.g. a connector module vs. domain modules).

/// `ws://` / `wss://` URL (any quote/template form); the path is taken with
/// `norm_path`. Stops at a quote, backtick, or whitespace.
fn ws_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)wss?://[^\s"'`]+"#).expect("valid regex"))
}

/// A client message send: `.send({ cmd: 'x' })` / `.request({ type: "x" })` etc.
/// Requires a send-ish method AND a command-keyed object literal, so a plain
/// `res.send(body)` or `arr.emit` is not mistaken for a WebSocket message.
fn ws_send_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"\.\s*(?:send|request|sendCommand|sendMessage|postMessage|invoke)\s*\(\s*\{[\s\S]{0,200}?\b(?:cmd|command|action|messageType|msgType|type|event)\s*:\s*["'`]([A-Za-z0-9_.:+-]+)["'`]"#,
        )
        .expect("valid regex")
    })
}

/// A server dispatch arm: `case "x":` / `case 'x':` (string-literal only). Scoped
/// to files that look like WebSocket code (see `is_ws_file`).
fn ws_case_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\bcase\s+["']([A-Za-z0-9_.:+-]+)["']\s*:"#).expect("valid regex")
    })
}

/// C# WebSocketSharp service registration: `AddWebSocketService<T>("/path")`.
fn cs_ws_service_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"AddWebSocketService\s*<\s*\w+\s*>\s*\(\s*"([^"]+)""#).expect("valid regex")
    })
}

/// socket.io / EventEmitter `.emit("evt"` (client send).
fn sio_emit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\.\s*emit\s*\(\s*["'`]([A-Za-z0-9_.:+-]+)["'`]"#).expect("valid regex")
    })
}

/// socket.io / EventEmitter `.on("evt"` / `@sio.on("evt")` (server handler).
fn sio_on_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\.\s*on\s*\(\s*["'`]([A-Za-z0-9_.:+-]+)["'`]"#).expect("valid regex")
    })
}

/// True if the source looks like it uses a WebSocket / socket.io API. Used to
/// scope the broader heuristics (`case`, `emit`/`on`) that would otherwise fire on
/// ordinary switch statements and EventEmitters.
fn is_ws_file(text: &str) -> bool {
    // API-shaped tokens only: a lowercase string literal like
    // `transports: ['websocket']` must not turn a Redux reducer's switch arms
    // into ws handlers (wave-2 W12). `WebSocket` (camel) matches the browser /
    // C# / Sharp APIs; the dotted lowercase forms match module usage
    // (`websocket.DefaultDialer`, `websockets.connect`).
    const TOKENS: &[&str] = &[
        "WebSocket",
        "websocket.",
        "websockets.",
        "socket.io",
        "socketio",
        "AddWebSocketService",
        "tungstenite",
        "ws://",
        "wss://",
        "ServerEndpoint",
    ];
    TOKENS.iter().any(|t| text.contains(t))
}

/// socket.io / ws lifecycle events that are not application messages.
fn is_reserved_ws_event(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "connect"
            | "disconnect"
            | "disconnecting"
            | "connect_error"
            | "error"
            | "message"
            | "close"
            | "open"
            | "ping"
            | "pong"
            | "newlistener"
            | "removelistener"
            | "data"
            | "end"
            | "exit"
            | "ready"
            | "drain"
            | "sigint"
            | "sigterm"
    )
}

/// Whether a WebSocket boundary site is a client (caller) or a server (handler).
#[derive(Clone, Copy)]
enum WsRole {
    Client,
    Server,
}

fn scan_websocket(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    match ext {
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            scan_ws_ecmascript(path, text, result)
        }
        "cs" => scan_ws_csharp(path, text, result),
        "py" => scan_ws_python(path, text, result),
        "rs" => scan_ws_rust(path, text, result),
        "go" => scan_ws_go(path, text, result),
        "java" | "kt" => scan_ws_java(path, text, result),
        _ => {}
    }
}

/// Go gorilla/nhooyr WebSockets: `Dial("ws://...")` client endpoints; the
/// server side (an Upgrader) carries no URL, so it is not endpoint-keyed.
fn scan_ws_go(path: &str, text: &str, result: &mut ExtractionResult) {
    if !is_ws_file(text) {
        return;
    }
    for m in ws_url_re().find_iter(text) {
        let line = line_of(text, m.start());
        let role = ws_role_at(text, m.start(), false);
        ws_endpoint(result, path, line, m.as_str(), role);
    }
}

/// Java/Jakarta `@ServerEndpoint("/path")` WebSocket servers.
fn scan_ws_java(path: &str, text: &str, result: &mut ExtractionResult) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"@\s*ServerEndpoint\s*\(\s*"([^"]+)"\s*\)"#).expect("valid regex")
    });
    for caps in re.captures_iter(text) {
        let line = line_of(text, caps.get(0).expect("group 0").start());
        ws_endpoint(result, path, line, &caps[1], WsRole::Server);
    }
    if is_ws_file(text) {
        for m in ws_url_re().find_iter(text) {
            let line = line_of(text, m.start());
            let role = ws_role_at(text, m.start(), false);
            ws_endpoint(result, path, line, m.as_str(), role);
        }
    }
}

/// JS/TS: in WebSocket/socket.io files only -- client `.send({cmd})`, endpoints
/// from `ws://` URLs, socket.io `emit`/`on`, and raw `case` dispatch. The send
/// scan is gated too: an HTTP `res.send({ type: ... })` response body must not
/// mint a message boundary.
fn scan_ws_ecmascript(path: &str, text: &str, result: &mut ExtractionResult) {
    if !is_ws_file(text) {
        return;
    }
    for caps in ws_send_re().captures_iter(text) {
        let cmd = &caps[1];
        let line = line_of(text, caps.get(0).expect("group 0").start());
        ws_message(result, path, line, cmd, WsRole::Client);
    }
    for m in ws_url_re().find_iter(text) {
        let line = line_of(text, m.start());
        ws_endpoint(result, path, line, m.as_str(), WsRole::Client);
    }
    for caps in sio_emit_re().captures_iter(text) {
        let evt = &caps[1];
        if is_reserved_ws_event(evt) {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        ws_message(result, path, line, evt, WsRole::Client);
    }
    for caps in sio_on_re().captures_iter(text) {
        let evt = &caps[1];
        if is_reserved_ws_event(evt) {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        ws_message(result, path, line, evt, WsRole::Server);
    }
    for caps in ws_case_re().captures_iter(text) {
        let cmd = &caps[1];
        let line = line_of(text, caps.get(0).expect("group 0").start());
        ws_message(result, path, line, cmd, WsRole::Server);
    }
}

/// C#: server `AddWebSocketService<T>("/path")` + `case "x":` handlers; client
/// `ws://` URLs (ClientWebSocket).
fn scan_ws_csharp(path: &str, text: &str, result: &mut ExtractionResult) {
    if !is_ws_file(text) {
        return;
    }
    for caps in cs_ws_service_re().captures_iter(text) {
        let ep = &caps[1];
        let line = line_of(text, caps.get(0).expect("group 0").start());
        ws_endpoint(result, path, line, ep, WsRole::Server);
    }
    let is_client = text.contains("ClientWebSocket");
    for m in ws_url_re().find_iter(text) {
        let line = line_of(text, m.start());
        let role = if is_client {
            WsRole::Client
        } else {
            WsRole::Server
        };
        ws_endpoint(result, path, line, m.as_str(), role);
    }
    for caps in ws_case_re().captures_iter(text) {
        let cmd = &caps[1];
        let line = line_of(text, caps.get(0).expect("group 0").start());
        ws_message(result, path, line, cmd, WsRole::Server);
    }
}

/// Python: `websockets.connect`/socket.io client `emit`; `websockets.serve` and
/// `@sio.on("evt")` handlers; `ws://` URLs for the endpoint.
/// Role of one `ws://` URL site: the enclosing call decides -- a URL inside
/// `connect(...)`/`Dial(...)` is a client, inside `serve(...)` a server; only
/// when neither is nearby does the file-level hint apply (2026-07 audit: the
/// old per-FILE flag inverted one half of any proxy/relay file).
fn ws_role_at(text: &str, url_start: usize, file_is_server: bool) -> WsRole {
    let window_start = floor_char_boundary(text, url_start.saturating_sub(48));
    let before = &text[window_start..url_start];
    // Call-shaped tokens only: a variable NAMED `server_url` must not classify
    // a pure client's connect URL as a server (wave-2 W5).
    if before.contains("connect(") || before.contains("Dial(") || before.contains("dial(") {
        return WsRole::Client;
    }
    if before.contains("serve(") || before.contains("accept(") || before.contains("accept_async(") {
        return WsRole::Server;
    }
    if file_is_server {
        WsRole::Server
    } else {
        WsRole::Client
    }
}

fn scan_ws_python(path: &str, text: &str, result: &mut ExtractionResult) {
    if !is_ws_file(text) {
        return;
    }
    let file_is_server = text.contains("websockets.serve") || text.contains(".serve(");
    for m in ws_url_re().find_iter(text) {
        let line = line_of(text, m.start());
        let role = ws_role_at(text, m.start(), file_is_server);
        ws_endpoint(result, path, line, m.as_str(), role);
    }
    for caps in sio_emit_re().captures_iter(text) {
        let evt = &caps[1];
        if is_reserved_ws_event(evt) {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        ws_message(result, path, line, evt, WsRole::Client);
    }
    for caps in sio_on_re().captures_iter(text) {
        let evt = &caps[1];
        if is_reserved_ws_event(evt) {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        ws_message(result, path, line, evt, WsRole::Server);
    }
}

/// Rust: tungstenite/tokio-tungstenite endpoints from `ws://` URLs. Per-command
/// dispatch over a decoded frame is not regex-tractable, so Rust is endpoint-only.
fn scan_ws_rust(path: &str, text: &str, result: &mut ExtractionResult) {
    if !is_ws_file(text) {
        return;
    }
    let is_server = text.contains("accept_async") || text.contains("accept_hdr_async");
    for m in ws_url_re().find_iter(text) {
        let line = line_of(text, m.start());
        let role = if is_server {
            WsRole::Server
        } else {
            WsRole::Client
        };
        ws_endpoint(result, path, line, m.as_str(), role);
    }
}

/// Attach a WebSocket message-type boundary node `wsmsg:<command>` (keyed on the
/// lowercased command, endpoint-independent). Client sites `calls_service` it;
/// server handlers are reached from it via `handled_by`.
fn ws_message(result: &mut ExtractionResult, path: &str, line: u32, command: &str, role: WsRole) {
    let key = command.to_ascii_lowercase();
    if key.is_empty() {
        return;
    }
    let node = boundary_node_id("wsmsg", &key);
    ensure_target(result, &node, &format!("ws #{command}"), "ws_message");
    boundary_link(result, path, line, node, role, "ws");
}

/// Attach a WebSocket endpoint boundary node `wsendpoint:<path>` (named paths
/// only -- a bare `/` is too generic to key on).
fn ws_endpoint(result: &mut ExtractionResult, path: &str, line: u32, raw_path: &str, role: WsRole) {
    let np = norm_path(raw_path);
    if np.is_empty() || np == "/" {
        return;
    }
    let node = boundary_node_id("wsendpoint", &np);
    ensure_target(result, &node, &format!("ws {np}"), "ws_endpoint");
    boundary_link(result, path, line, node, role, "ws");
}

/// Attach a message-boundary site to its channel node. Client: `enclosing fn ->
/// node` (`calls_service`). Server: `node -> enclosing fn` (`handled_by`),
/// mirroring the HTTP route handler direction. `context` tags the edge (`ws`,
/// `ipc`). Shared by the WebSocket and Electron-IPC detectors.
fn boundary_link(
    result: &mut ExtractionResult,
    path: &str,
    line: u32,
    node: NodeId,
    role: WsRole,
    context: &str,
) {
    match role {
        WsRole::Client => link(result, path, line, node, "calls_service", context),
        WsRole::Server => {
            let handler = enclosing_function(result, line).unwrap_or_else(|| file_node_id(path));
            result.edges.push(Edge {
                source: node,
                target: handler,
                relation: "handled_by".to_string(),
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
    }
}

/// Electron IPC detector. Senders (`ipcRenderer.invoke/send('ch')`,
/// `webContents.send('ch')`) `calls_service` a channel-keyed `ipc #<ch>` node;
/// handlers (`ipcMain.handle/on('ch', fn)`, renderer `ipcRenderer.on('ch', fn)`)
/// are reached from it via `handled_by`. So a main-process handler invoked only
/// across the IPC boundary -- which has no static caller -- gains one through the
/// channel node, and a renderer->main call connects in the graph. JS/TS only,
/// gated on an electron IPC API token to avoid firing on ordinary `.on`/`.handle`.
fn scan_ipc(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    if !matches!(
        ext,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts"
    ) {
        return;
    }
    if !(text.contains("ipcMain") || text.contains("ipcRenderer") || text.contains("webContents")) {
        return;
    }
    let mut scan = |re: &Regex, role: WsRole| {
        for caps in re.captures_iter(text) {
            let channel = &caps[1];
            let line = line_of(text, caps.get(0).expect("group 0").start());
            ipc_message(result, path, line, channel, role);
        }
    };
    // ipcMain.handle/on + ipcRenderer.on are handlers (server side); the renderer's
    // invoke/send and a main webContents.send are the senders (client side).
    scan(ipc_handle_re(), WsRole::Server);
    scan(ipc_renderer_on_re(), WsRole::Server);
    scan(ipc_invoke_re(), WsRole::Client);
    scan(webcontents_send_re(), WsRole::Client);
}

/// Attach an Electron IPC channel boundary node `ipc:<channel>` keyed on the
/// channel name (case-insensitive via `make_id`). Senders `calls_service` it;
/// handlers are reached from it via `handled_by`.
fn ipc_message(result: &mut ExtractionResult, path: &str, line: u32, channel: &str, role: WsRole) {
    if channel.is_empty() {
        return;
    }
    let node = boundary_node_id("ipc", channel);
    ensure_target(result, &node, &format!("ipc #{channel}"), "ipc_channel");
    boundary_link(result, path, line, node, role, "ipc");
}

/// `ipcMain.handle('ch'` / `.handleOnce` / `.on` / `.once` (main-process handler).
fn ipc_handle_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"ipcMain\s*\.\s*(?:handle|handleOnce|on|once)\s*\(\s*["'`]([A-Za-z0-9_.:+/-]+)["'`]"#,
        )
        .expect("valid regex")
    })
}

/// `ipcRenderer.invoke('ch'` / `.send` / `.sendSync` / `.postMessage` (sender).
fn ipc_invoke_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"ipcRenderer\s*\.\s*(?:invoke|send|sendSync|postMessage)\s*\(\s*["'`]([A-Za-z0-9_.:+/-]+)["'`]"#,
        )
        .expect("valid regex")
    })
}

/// `ipcRenderer.on('ch'` / `.once` (renderer listening for a main->renderer push).
fn ipc_renderer_on_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"ipcRenderer\s*\.\s*(?:on|once|addListener)\s*\(\s*["'`]([A-Za-z0-9_.:+/-]+)["'`]"#,
        )
        .expect("valid regex")
    })
}

/// `<x>.webContents.send('ch'` (main pushing to a renderer).
fn webcontents_send_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"webContents\s*\.\s*send\s*\(\s*["'`]([A-Za-z0-9_.:+/-]+)["'`]"#)
            .expect("valid regex")
    })
}

/// JS/TS event-bus detector. `<x>.emit('e')` publishes (the enclosing fn
/// `calls_service` an `event #e` channel node); `<x>.on/.once/.addListener('e')`
/// subscribes (the channel reaches the handler via `handled_by`), so a handler
/// invoked only across the bus is no longer a 0-caller node. The emit/on scan is
/// gated on an `EventEmitter` token in the file so ordinary `.on(`/`.emit(`
/// (jQuery, RxJS, sockets) do not fire. DOM custom events are detected separately
/// and need no such gate.
fn scan_event_bus(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    if !matches!(
        ext,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts"
    ) {
        return;
    }
    if text.contains("EventEmitter") {
        for caps in emit_re().captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            event_message(result, path, line, &caps[1], WsRole::Client);
        }
        for caps in on_re().captures_iter(text) {
            let line = line_of(text, caps.get(0).expect("group 0").start());
            event_message(result, path, line, &caps[1], WsRole::Server);
        }
    }
    scan_custom_event(path, text, result);
}

/// Attach an event-bus channel boundary node `event:<name>` (case-insensitive via
/// `make_id`). Publishers `calls_service` it; subscribers are reached via
/// `handled_by`.
fn event_message(result: &mut ExtractionResult, path: &str, line: u32, name: &str, role: WsRole) {
    if name.is_empty() {
        return;
    }
    let node = boundary_node_id("event", &name.to_ascii_lowercase());
    ensure_target(result, &node, &format!("event #{name}"), "event_channel");
    boundary_link(result, path, line, node, role, "event");
}

/// `<emitter>.emit('e'` (publisher).
fn emit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\.\s*emit\s*\(\s*["'`]([A-Za-z0-9_.:+/-]+)["'`]"#).expect("valid regex")
    })
}

/// `<emitter>.on/.once/.addListener/.prependListener('e'` (subscriber).
fn on_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"\.\s*(?:on|once|addListener|prependListener)\s*\(\s*["'`]([A-Za-z0-9_.:+/-]+)["'`]"#,
        )
        .expect("valid regex")
    })
}

/// DOM custom events. `dispatchEvent(new CustomEvent('e'))` publishes; a matching
/// `addEventListener('e')` subscribes. Standard DOM event names are skipped so we
/// do not mint a channel per `click`/`load`.
fn scan_custom_event(path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in custom_event_re().captures_iter(text) {
        let line = line_of(text, caps.get(0).expect("group 0").start());
        event_message(result, path, line, &caps[1], WsRole::Client);
    }
    for caps in add_listener_re().captures_iter(text) {
        let name = &caps[1];
        if is_standard_dom_event(name) {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        event_message(result, path, line, name, WsRole::Server);
    }
}

/// `new CustomEvent('e'` (publisher payload).
fn custom_event_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"new\s+CustomEvent\s*\(\s*["'`]([A-Za-z0-9_.:+/-]+)["'`]"#)
            .expect("valid regex")
    })
}

/// `<target>.addEventListener('e'` (subscriber).
fn add_listener_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\.\s*addEventListener\s*\(\s*["'`]([A-Za-z0-9_.:+/-]+)["'`]"#)
            .expect("valid regex")
    })
}

/// Standard DOM/window/media event names that are not app-level buses.
fn is_standard_dom_event(name: &str) -> bool {
    const STD: &[&str] = &[
        "click",
        "dblclick",
        "mousedown",
        "mouseup",
        "mousemove",
        "mouseenter",
        "mouseleave",
        "mouseover",
        "mouseout",
        "contextmenu",
        "keydown",
        "keyup",
        "keypress",
        "input",
        "change",
        "submit",
        "focus",
        "blur",
        "focusin",
        "focusout",
        "load",
        "unload",
        "beforeunload",
        "resize",
        "scroll",
        "wheel",
        "drag",
        "dragstart",
        "dragend",
        "dragover",
        "dragenter",
        "dragleave",
        "drop",
        "touchstart",
        "touchend",
        "touchmove",
        "touchcancel",
        "pointerdown",
        "pointerup",
        "pointermove",
        "pointerenter",
        "pointerleave",
        "animationend",
        "transitionend",
        "play",
        "pause",
        "ended",
        "error",
        "abort",
        "loadeddata",
        "canplay",
        "message",
        "open",
        "close",
        "online",
        "offline",
        "visibilitychange",
        "hashchange",
        "popstate",
        "storage",
        "domcontentloaded",
        "readystatechange",
    ];
    STD.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// C# in-process events. `Foo?.Invoke(` raises (publisher); `x.Foo += handler`
/// subscribes -- but `+=` only counts when `Foo` is a known event (declared with
/// `event` or raised via `?.Invoke` in this file), so an arithmetic `total += x`
/// does not mint a spurious channel. The idiomatic null-conditional `?.Invoke`
/// avoids `Dispatcher/Action/Task.Invoke` false positives.
fn scan_dotnet_events(ext: &str, path: &str, text: &str, result: &mut ExtractionResult) {
    if ext != "cs" {
        return;
    }
    let mut event_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for caps in cs_event_decl_re().captures_iter(text) {
        event_names.insert(caps[1].to_string());
    }
    for caps in cs_invoke_re().captures_iter(text) {
        let name = caps[1].to_string();
        let line = line_of(text, caps.get(0).expect("group 0").start());
        event_message(result, path, line, &name, WsRole::Client);
        event_names.insert(name);
    }
    for caps in cs_subscribe_re().captures_iter(text) {
        let name = &caps[1];
        if !event_names.contains(name) {
            continue;
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        event_message(result, path, line, name, WsRole::Server);
    }
    // Cross-file subscribe (2026-07 audit): `obj.Changed += OnChanged` where the
    // `event` declaration lives in another file. Requires a dotted PascalCase
    // member and a handler-shaped RHS (method group / new ...EventHandler /
    // lambda) so arithmetic `item.Count += n` stays out.
    for caps in cs_member_subscribe_re().captures_iter(text) {
        let name = &caps[1];
        if event_names.contains(name) {
            continue; // already handled by the same-file path above
        }
        let line = line_of(text, caps.get(0).expect("group 0").start());
        event_message(result, path, line, name, WsRole::Server);
    }
}

/// `x.Name += <handler>` with `Name` PascalCase and a handler-shaped right side:
/// an uppercase method-group identifier, `new ...EventHandler(...)`, or a lambda.
fn cs_member_subscribe_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Handler-shaped right side only: `OnX`, `new ...EventHandler`, a lambda,
    // or a bare PascalCase method group ENDING the statement. A dotted RHS
    // (`Line.Price`) or a call (`Convert.ToDecimal(...)`) is arithmetic on a
    // property, not a subscription (wave-2 W8).
    RE.get_or_init(|| {
        Regex::new(
            r#"\b\w+\s*\.\s*([A-Z]\w*)\s*\+=\s*(?:new\s+\w*EventHandler|On[A-Z]\w*|[A-Z]\w*\s*;|\([^)]*\)\s*=>|\w+\s*=>)"#,
        )
        .expect("valid regex")
    })
}

/// `event <Type> Name;` / `= ` / `{` -- captures the event identifier.
fn cs_event_decl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\bevent\s+[^;={]*?\b([A-Za-z_]\w*)\s*(?:;|=>|=|\{)"#).expect("valid regex")
    })
}

/// `Name?.Invoke(` (idiomatic event raise).
fn cs_invoke_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\b([A-Za-z_]\w*)\s*\?\.\s*Invoke\s*\("#).expect("valid regex"))
}

/// `Name += ` (event subscription; gated on `Name` being a known event).
fn cs_subscribe_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\b([A-Za-z_]\w*)\s*\+=\s*"#).expect("valid regex"))
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
    // Relative dot segments (`./api/x`, `../api/x`) key the same route as the
    // rooted path -- a `/./api/x` node could never join `/api/x` (2026-07 audit).
    while let Some(rest) = p.strip_prefix("./").or_else(|| p.strip_prefix("../")) {
        p = rest;
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

/// Largest index `<= i` that is a char boundary of `text`. Regex match offsets
/// are always boundaries, but arithmetic windows (`end + 200`, `start - 48`)
/// can land inside a multi-byte char and panic the slice (wave-2 W14).
fn floor_char_boundary(text: &str, i: usize) -> usize {
    let mut i = i.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Byte offset just past the `)` matching the `(` at `open`. Same string/char/
/// line-comment skipping as `block_end` (best effort; raw strings and block
/// comments are not handled). Returns `text.len()` when unbalanced.
fn paren_end(text: &str, open: usize) -> usize {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
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
    text.len()
}

/// Library basename without directory or extension: `./libmath.so` -> `libmath`.
fn native_lib_name(raw: &str) -> String {
    Path::new(raw)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 1-based line number for a byte offset in `text`.
pub(crate) fn line_of(text: &str, byte: usize) -> u32 {
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

/// Record a dynamic-dispatch site on the node enclosing `line`, else on the file
/// node (created if absent). Used by the reflection/dispatch detectors in the
/// sibling `dynamic` module.
pub(crate) fn attach_dynamic_site(
    result: &mut ExtractionResult,
    path: &str,
    line: u32,
    site: DynamicSite,
) {
    let host = enclosing_function(result, line).unwrap_or_else(|| ensure_file_node(result, path));
    if let Some(node) = result.nodes.iter_mut().find(|n| n.id == host) {
        node.push_dynamic_site(site);
    }
}

/// Ensure a real (located) file node exists for `path`, returning its id. Distinct
/// from `ensure_target`, which makes external (source-less) stubs.
pub(crate) fn ensure_file_node(result: &mut ExtractionResult, path: &str) -> NodeId {
    let id = file_node_id(path);
    if !result.nodes.iter().any(|n| n.id == id) {
        let mut extra = Map::new();
        extra.insert("_origin".to_string(), json!("ast"));
        result.nodes.push(Node {
            id: id.clone(),
            label: path.to_string(),
            file_type: FileType::Code,
            source_file: path.to_string(),
            source_location: None,
            community: None,
            repo: None,
            extra,
        });
    }
    id
}

/// The innermost function/method node whose span contains `line`.
pub(crate) fn enclosing_function(result: &ExtractionResult, line: u32) -> Option<NodeId> {
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
        let up = trimmed.to_ascii_uppercase();
        let relation = match verb.as_str() {
            "SELECT" => "queries",
            // A CTE (`WITH ...`) can wrap a read or a write; classify by whether a
            // write keyword follows the CTE definitions.
            "WITH" => {
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
        // Clause gate: a leading SQL verb is not enough. Require the companion
        // clause that a real statement of that shape must contain, so prose and
        // UI strings like `'Update password'` or `'DELETE account'` (no SET / no
        // FROM) are not mistaken for queries.
        let shaped = match verb.as_str() {
            "SELECT" => sql_clause_present(&up, "FROM"),
            "WITH" => sql_clause_present(&up, "SELECT") && sql_clause_present(&up, "FROM"),
            "INSERT" => sql_clause_present(&up, "INTO"),
            "UPDATE" => sql_clause_present(&up, "SET"),
            "DELETE" => sql_clause_present(&up, "FROM"),
            "MERGE" => sql_clause_present(&up, "INTO") || sql_clause_present(&up, "USING"),
            "UPSERT" => sql_clause_present(&up, "INTO") || sql_clause_present(&up, "SET"),
            "CALL" | "EXEC" | "EXECUTE" => trimmed.contains('('),
            _ => false,
        };
        if !shaped {
            continue;
        }
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

/// Whole-word presence of an (already uppercased) SQL keyword in `up`. Used by the
/// clause gate so a string is only treated as SQL when it carries the structural
/// keyword a real statement of its shape requires.
#[cfg(feature = "lang-sql")]
fn sql_clause_present(up: &str, kw: &str) -> bool {
    up.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|tok| tok == kw)
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
