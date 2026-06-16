use serde::{Deserialize, Serialize};

/// Detection-time file classification. Note: distinct from `core::FileType` —
/// this set has `Video` and no `Rationale`/`Concept` (those are node-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    Code,
    Document,
    Paper,
    Image,
    Video,
}

/// All detection-time file types, in a stable order.
pub const ALL_FILE_TYPES: [FileType; 5] = [
    FileType::Code,
    FileType::Document,
    FileType::Paper,
    FileType::Image,
    FileType::Video,
];

// Extensions CodeGraph classifies as Code, trimmed to **only** extensions the
// extract crate can actually parse: an extension classified as Code with no
// extractor inflates corpus stats and is then silently dropped at extraction
// (the detect/extract "drift" bug). The invariant
// `CODE_EXTENSIONS ⊆ extract::extract_source` is enforced by
// `codegraph-extract`'s `every_detected_code_extension_has_an_extractor`
// test, so adding an extension here without an extractor fails CI.
//
// Deferred (no CodeGraph extractor yet; re-add each here when its extractor
// lands): ejs, ets, toc, luau, r, dm, dme, dmi, dmm, dmf, lfm, lpk, inc.
// (`.inc` is ambiguous - PHP/ASP/Pascal includes - so it stays deferred rather
// than be force-routed to one parser. BYOND `.dm*` need binary parsers + a
// grammar that doesn't exist; deferred indefinitely.)
// Also recognized: csproj/fsproj/vbproj/sln/slnx (.NET, `dotnet.rs`),
// cls/trigger (Apex regex, `apex.rs`), pas/pp/dpr/dpk/lpr (Pascal/Delphi regex,
// `pascal.rs`), razor/cshtml (Razor -> C#, `razor.rs`). Markdown (md/mdx/qmd)
// stays a *document*: it gets structural extraction via a dedicated pass, not
// this Code list. (`.sln`/`.csproj`/`.fsproj` are also recognized for workspace
// project discovery via raw extension checks, independent of this list.)
pub const CODE_EXTENSIONS: &[&str] = &[
    "py", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "go", "rs", "java", "groovy",
    "gradle", "cpp", "cc", "cxx", "c", "h", "hpp", "hh", "rb", "swift", "kt", "kts", "cs", "scala",
    "sc", "php", "lua", "zig", "ps1", "psm1", "ex", "exs", "m", "mm", "jl", "vue", "svelte",
    "astro", "dart", "v", "sv", "svh", "vh", "sql", "f", "f90", "f95", "f03", "f08", "for", "sh",
    "bash", "json", "yaml", "yml", "tf", "tfvars", "hcl", "asp", "asa", "csproj", "fsproj",
    "vbproj", "sln", "slnx", "cls", "trigger", "pas", "pp", "dpr", "dpk", "lpr", "razor", "cshtml",
];
// NB: `yaml`/`yml` are classified as Code (CodeGraph has a YAML structural
// extractor for CI/Compose/k8s; it returns empty for non-config YAML), so they
// are not also listed as documents.
pub(crate) const DOC_EXTENSIONS: &[&str] = &["md", "mdx", "qmd", "txt", "rst", "html"];
pub(crate) const PAPER_EXTENSIONS: &[&str] = &["pdf"];
pub(crate) const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg"];
pub(crate) const OFFICE_EXTENSIONS: &[&str] = &["docx", "xlsx"];
pub(crate) const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "webm", "mkv", "avi", "m4v", "mp3", "wav", "m4a", "ogg",
];
