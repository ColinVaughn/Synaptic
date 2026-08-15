//! The shared corpus of pinned external repositories.
//!
//! One manifest feeds two benchmark suites: `scale` times extraction throughput
//! (see [`crate::scale`]) and `quality` measures extraction correctness (see
//! [`crate::quality`]). A repository is added in exactly one place; two lists
//! would drift, and a repository present in one but absent from the other would
//! be measured for speed and never for accuracy, or the reverse.
//!
//! Every entry declares the `languages` it is asserted to exercise. That field
//! is load-bearing: a test fails when a shipped extractor appears in no entry,
//! so a new language cannot be added without also being benchmarked on real
//! code. The corpus this replaced covered 9 of 39 languages, which is how a
//! case-sensitive extension match that routed `.F90` to no extractor survived to
//! be found by hand.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Suite tag for the throughput benchmark.
pub const SUITE_SCALE: &str = "scale";
/// Suite tag for the extraction-quality benchmark.
pub const SUITE_QUALITY: &str = "quality";

/// An entry with no explicit `suites` is a quality-corpus member. Throughput is
/// the narrower, deliberately curated set (it needs size-tier balance and a
/// stable host to stay comparable across runs), so opting in is explicit there
/// and implicit here.
fn default_suites() -> Vec<String> {
    vec![SUITE_QUALITY.to_string()]
}

/// One pinned external repository.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CorpusRepo {
    pub url: String,
    pub sha: String,
    pub family: String,
    /// Size tier: "small" | "medium" | "large" (advisory grouping label).
    pub tier: String,
    /// Which benchmark suites measure this repository.
    #[serde(default = "default_suites")]
    pub suites: Vec<String>,
    /// Language extractors this entry is asserted to exercise. Names come from
    /// [`LANGUAGES`].
    #[serde(default)]
    pub languages: Vec<String>,
    /// Representative primary-language file used for incremental-path timing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incremental_file: Option<String>,
}

impl CorpusRepo {
    /// Whether this repository is measured by `suite`.
    pub fn in_suite(&self, suite: &str) -> bool {
        self.suites.iter().any(|s| s == suite)
    }

    /// The directory name a clone of this repository gets.
    pub fn name(&self) -> &str {
        repo_name(&self.url)
    }
}

/// The corpus manifest.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct CorpusManifest {
    #[serde(default, rename = "repo")]
    pub repos: Vec<CorpusRepo>,
}

impl CorpusManifest {
    pub fn parse(src: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(src)
    }

    /// Entries measured by `suite`, in manifest order.
    pub fn in_suite<'a>(&'a self, suite: &'a str) -> impl Iterator<Item = &'a CorpusRepo> + 'a {
        self.repos.iter().filter(move |r| r.in_suite(suite))
    }

    /// Every language name any entry claims to exercise.
    pub fn declared_languages(&self) -> BTreeSet<&str> {
        self.repos
            .iter()
            .flat_map(|r| r.languages.iter().map(String::as_str))
            .collect()
    }
}

/// The last path segment of a repository URL, without `.git`.
pub fn repo_name(url: &str) -> &str {
    url.trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or(url)
}

/// A language Synaptic extracts: the manifest name, the extensions that route to
/// it, and whether the language folds identifier case.
///
/// `case_folds` is not cosmetic. Fortran, Pascal, PowerShell, Apex, SQL and
/// classic ASP are case-insensitive by specification, so an extractor that
/// normalizes an identifier's case is correct; comparing its label to the source
/// line byte-for-byte would report a spurious anchor error for every symbol in
/// those languages.
pub struct Language {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub case_folds: bool,
}

/// Every language the extractor dispatch routes to.
///
/// `.h` is listed under `c` although the dispatch content-sniffs it between C,
/// C++ and Objective-C; the bucket is an attribution for reporting, not a claim
/// about which extractor ran.
pub const LANGUAGES: &[Language] = &[
    Language {
        name: "rust",
        extensions: &["rs"],
        case_folds: false,
    },
    Language {
        name: "python",
        extensions: &["py"],
        case_folds: false,
    },
    Language {
        name: "javascript",
        extensions: &["js", "jsx", "mjs", "cjs"],
        case_folds: false,
    },
    Language {
        name: "typescript",
        extensions: &["ts", "mts", "cts", "tsx"],
        case_folds: false,
    },
    Language {
        name: "go",
        extensions: &["go"],
        case_folds: false,
    },
    Language {
        name: "java",
        extensions: &["java"],
        case_folds: false,
    },
    Language {
        name: "csharp",
        extensions: &["cs"],
        case_folds: false,
    },
    Language {
        name: "kotlin",
        extensions: &["kt", "kts"],
        case_folds: false,
    },
    Language {
        name: "swift",
        extensions: &["swift"],
        case_folds: false,
    },
    Language {
        name: "c",
        extensions: &["c", "h"],
        case_folds: false,
    },
    Language {
        name: "cpp",
        extensions: &["cpp", "cc", "cxx", "hpp", "hh"],
        case_folds: false,
    },
    Language {
        name: "objc",
        extensions: &["m", "mm"],
        case_folds: false,
    },
    Language {
        name: "json",
        extensions: &["json", "uproject", "uplugin", "mcmeta"],
        case_folds: false,
    },
    Language {
        name: "yaml",
        extensions: &["yaml", "yml"],
        case_folds: false,
    },
    Language {
        name: "hcl",
        extensions: &["tf", "tfvars", "hcl"],
        case_folds: false,
    },
    Language {
        name: "sql",
        extensions: &["sql"],
        case_folds: true,
    },
    Language {
        name: "bash",
        extensions: &["sh", "bash"],
        case_folds: false,
    },
    Language {
        name: "lua",
        extensions: &["lua"],
        case_folds: false,
    },
    Language {
        name: "ruby",
        extensions: &["rb"],
        case_folds: false,
    },
    Language {
        name: "powershell",
        extensions: &["ps1", "psm1"],
        case_folds: true,
    },
    Language {
        name: "php",
        extensions: &["php"],
        case_folds: false,
    },
    Language {
        name: "scala",
        extensions: &["scala", "sc"],
        case_folds: false,
    },
    Language {
        name: "dart",
        extensions: &["dart"],
        case_folds: false,
    },
    Language {
        name: "elixir",
        extensions: &["ex", "exs"],
        case_folds: false,
    },
    Language {
        name: "julia",
        extensions: &["jl"],
        case_folds: false,
    },
    Language {
        name: "zig",
        extensions: &["zig"],
        case_folds: false,
    },
    Language {
        name: "asp",
        extensions: &["asp", "asa"],
        case_folds: true,
    },
    Language {
        name: "groovy",
        extensions: &["groovy", "gradle"],
        case_folds: false,
    },
    Language {
        name: "fortran",
        extensions: &["f90", "f95", "f03", "f08", "f", "for"],
        case_folds: true,
    },
    Language {
        name: "ql",
        extensions: &["ql", "qll"],
        case_folds: false,
    },
    Language {
        name: "verilog",
        extensions: &["v", "sv", "vh", "svh"],
        case_folds: false,
    },
    Language {
        name: "vue",
        extensions: &["vue"],
        case_folds: false,
    },
    Language {
        name: "svelte",
        extensions: &["svelte"],
        case_folds: false,
    },
    Language {
        name: "astro",
        extensions: &["astro"],
        case_folds: false,
    },
    Language {
        name: "dotnet",
        extensions: &["csproj", "fsproj", "vbproj", "sln", "slnx", "xaml"],
        case_folds: false,
    },
    Language {
        name: "markdown",
        extensions: &["md", "mdx", "qmd"],
        case_folds: false,
    },
    Language {
        name: "apex",
        extensions: &["cls", "trigger"],
        case_folds: true,
    },
    Language {
        name: "pascal",
        extensions: &["pas", "pp", "dpr", "dpk", "lpr"],
        case_folds: true,
    },
    Language {
        name: "razor",
        extensions: &["razor", "cshtml"],
        case_folds: false,
    },
];

/// The language a source path belongs to, by extension (case-folded, matching
/// the extractor dispatch).
pub fn language_of(path: &str) -> Option<&'static Language> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|l| l.extensions.contains(&ext.as_str()))
}

/// Rewrite each entry's `sha` in place, preserving comments and formatting.
///
/// A serialize round-trip through `toml` would drop every comment in the
/// manifest, and those comments carry the pinning policy. This walks lines
/// instead: after a `url = "..."` line, the next `sha = "..."` line belongs to
/// that entry. `resolved` maps URL to the new SHA; a URL absent from it keeps
/// its current pin.
pub fn repin(src: &str, resolved: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(src.len());
    let mut pending: Option<&String> = None;
    for line in src.lines() {
        let trimmed = line.trim_start();
        if let Some(url) = toml_string_value(trimmed, "url") {
            pending = resolved.get(url);
            out.push_str(line);
        } else if trimmed.starts_with("sha") && toml_string_value(trimmed, "sha").is_some() {
            match pending.take() {
                Some(sha) => {
                    let indent = &line[..line.len() - trimmed.len()];
                    out.push_str(indent);
                    out.push_str(&format!("sha = \"{sha}\""));
                }
                None => out.push_str(line),
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// The string value of a `key = "value"` TOML line, if that is what this is.
fn toml_string_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[repo]]
url = "https://github.com/x/y"
sha = "deadbeef"
family = "systems-rust"
tier = "small"
suites = ["scale", "quality"]
languages = ["rust"]
incremental_file = "src/lib.rs"

[[repo]]
url = "https://github.com/a/b"
sha = "cafebabe"
family = "scripting-lua"
tier = "medium"
languages = ["lua"]
"#;

    #[test]
    fn parses_suites_and_languages() {
        let m = CorpusManifest::parse(SAMPLE).unwrap();
        assert_eq!(m.repos.len(), 2);
        assert_eq!(m.repos[0].suites, vec!["scale", "quality"]);
        assert_eq!(m.repos[0].incremental_file.as_deref(), Some("src/lib.rs"));
        assert_eq!(m.repos[0].languages, vec!["rust"]);
    }

    /// An entry that names no suite must still be measured for quality, or
    /// adding a repository without boilerplate would silently benchmark nothing.
    #[test]
    fn missing_suites_defaults_to_quality_only() {
        let m = CorpusManifest::parse(SAMPLE).unwrap();
        assert_eq!(m.repos[1].suites, vec![SUITE_QUALITY]);
        assert!(m.repos[1].in_suite(SUITE_QUALITY));
        assert!(!m.repos[1].in_suite(SUITE_SCALE));
    }

    #[test]
    fn suite_filter_selects_members() {
        let m = CorpusManifest::parse(SAMPLE).unwrap();
        let scale: Vec<&str> = m.in_suite(SUITE_SCALE).map(|r| r.name()).collect();
        assert_eq!(scale, vec!["y"]);
        let quality: Vec<&str> = m.in_suite(SUITE_QUALITY).map(|r| r.name()).collect();
        assert_eq!(quality, vec!["y", "b"]);
    }

    #[test]
    fn declared_languages_pool_across_entries() {
        let m = CorpusManifest::parse(SAMPLE).unwrap();
        assert_eq!(
            m.declared_languages().into_iter().collect::<Vec<_>>(),
            vec!["lua", "rust"]
        );
    }

    #[test]
    fn repo_name_is_last_segment() {
        assert_eq!(repo_name("https://github.com/BurntSushi/memchr"), "memchr");
        assert_eq!(
            repo_name("https://github.com/BurntSushi/memchr.git"),
            "memchr"
        );
        assert_eq!(repo_name("https://github.com/a/b/"), "b");
    }

    #[test]
    fn language_lookup_folds_extension_case() {
        assert_eq!(
            language_of("src/thing.F90").map(|l| l.name),
            Some("fortran")
        );
        assert_eq!(
            language_of("src/thing.f90").map(|l| l.name),
            Some("fortran")
        );
        assert_eq!(language_of("a/b.rs").map(|l| l.name), Some("rust"));
        assert_eq!(language_of("README").map(|l| l.name), None);
    }

    #[test]
    fn case_folding_languages_are_flagged() {
        for name in ["fortran", "pascal", "powershell", "apex", "sql", "asp"] {
            let l = LANGUAGES.iter().find(|l| l.name == name).unwrap();
            assert!(l.case_folds, "{name} is case-insensitive by specification");
        }
        assert!(
            !LANGUAGES
                .iter()
                .find(|l| l.name == "rust")
                .unwrap()
                .case_folds
        );
    }

    #[test]
    fn language_names_and_extensions_are_unique() {
        let mut names = BTreeSet::new();
        let mut exts = BTreeSet::new();
        for l in LANGUAGES {
            assert!(names.insert(l.name), "duplicate language {}", l.name);
            for e in l.extensions {
                assert!(exts.insert(*e), "extension {e} claimed twice");
            }
        }
    }

    #[test]
    fn repin_rewrites_shas_and_keeps_comments() {
        let src = "# policy comment\n[[repo]]\nurl = \"https://github.com/x/y\"\nsha = \"old\"\ntier = \"small\"\n";
        let mut resolved = std::collections::BTreeMap::new();
        resolved.insert("https://github.com/x/y".to_string(), "new".to_string());
        let out = repin(src, &resolved);
        assert!(out.contains("# policy comment"), "{out}");
        assert!(out.contains("sha = \"new\""), "{out}");
        assert!(!out.contains("old"), "{out}");
    }

    /// A URL the resolver could not reach keeps its existing pin, so a partial
    /// network failure cannot blank out the corpus.
    #[test]
    fn repin_leaves_unresolved_entries_alone() {
        let src = "[[repo]]\nurl = \"https://github.com/x/y\"\nsha = \"keep\"\n";
        let out = repin(src, &std::collections::BTreeMap::new());
        assert!(out.contains("sha = \"keep\""), "{out}");
    }
}
