//! An independent second opinion on what a repository declares.
//!
//! Every other quality measurement is self-referential: it checks Synaptic
//! against Synaptic's own output. This one asks a completely different parser --
//! universal-ctags, which is hand-written and regex-driven rather than
//! tree-sitter based -- what it found in the same checkout, and reports where the
//! two disagree.
//!
//! The result is deliberately a **symmetric difference**, not a recall score. A
//! recall percentage would cast ctags as ground truth; it is not. It misses
//! things Synaptic finds (methods, framework constructs, cross-file structure)
//! and finds things Synaptic deliberately does not model. What is actionable is
//! the asymmetry: "ctags found 12 declarations in Pascal that we did not" names a
//! bug to go and look at, while "recall = 94%" implies an authority no second
//! tool has earned.
//!
//! A language for which ctags emitted nothing is reported as unsupported rather
//! than as a total miss, so Apex or QL do not post a fake 0%.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde::Serialize;

use synaptic_core::GraphData;

use crate::quality::bare_name;
use crate::repo_corpus::language_of;

/// ctags kinds that name something Synaptic does not model as a graph node.
/// Comparing against them would manufacture disagreement rather than find it.
const IGNORED_KINDS: &[&str] = &[
    "local",
    "parameter",
    "argument",
    "label",
    "anonymous",
    "unknown",
    "header",
    "include",
];

/// One tag as the oracle reported it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tag {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub line: u32,
}

/// Per-language comparison against the oracle.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct OracleLanguage {
    pub language: String,
    /// Declarations both tools found.
    pub agreement: usize,
    /// Found by ctags, absent from the graph. The actionable set.
    pub ctags_only: usize,
    /// Found by Synaptic, absent from ctags. Largely legitimate.
    pub synaptic_only: usize,
    /// A few `file:line name` samples from the ctags-only set.
    pub samples: Vec<String>,
}

impl OracleLanguage {
    /// Share of what ctags found that Synaptic did not.
    pub fn missed_rate(&self) -> f64 {
        let d = self.agreement + self.ctags_only;
        if d == 0 {
            0.0
        } else {
            self.ctags_only as f64 / d as f64
        }
    }
}

/// The oracle stage's result for one repository.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct OracleOutcome {
    pub available: bool,
    /// Why the stage did not run, when it did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub per_language: Vec<OracleLanguage>,
    /// Tag lines the oracle emitted that could not be parsed. Reported rather
    /// than swallowed: a parser change that silently halved the oracle's output
    /// would otherwise look like Synaptic improving.
    pub malformed_tag_lines: usize,
}

impl OracleOutcome {
    /// The stage did not run, for the stated reason.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        OracleOutcome {
            available: false,
            reason: Some(reason.into()),
            per_language: Vec::new(),
            malformed_tag_lines: 0,
        }
    }

    /// Worst per-language missed rate, for gating.
    pub fn worst_missed_rate(&self) -> f64 {
        self.per_language
            .iter()
            .map(|l| l.missed_rate())
            .fold(0.0, f64::max)
    }
}

/// Whether universal-ctags is installed and new enough to emit JSON.
///
/// Exuberant ctags answers `--version` too but has no `--output-format=json`, so
/// the check is for the implementation, not merely for a binary on PATH.
pub fn probe() -> Result<String, String> {
    let out = Command::new("ctags")
        .arg("--version")
        .output()
        .map_err(|e| format!("ctags not found on PATH: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if !text.contains("Universal Ctags") {
        return Err(format!(
            "ctags on PATH is not Universal Ctags (JSON output unsupported): {}",
            text.lines().next().unwrap_or("").trim()
        ));
    }
    Ok(text.lines().next().unwrap_or("").trim().to_string())
}

/// Parse ctags JSON-lines output into tags, returning the malformed-line count.
///
/// Pure, so the normalization is testable without the binary installed.
pub fn parse_tags(stdout: &str) -> (Vec<Tag>, usize) {
    let mut tags = Vec::new();
    let mut malformed = 0usize;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            malformed += 1;
            continue;
        };
        // ctags emits `_type: "ptag"` metadata rows alongside real tags.
        if v.get("_type").and_then(|t| t.as_str()) != Some("tag") {
            continue;
        }
        let (Some(name), Some(path)) = (
            v.get("name").and_then(|x| x.as_str()),
            v.get("path").and_then(|x| x.as_str()),
        ) else {
            malformed += 1;
            continue;
        };
        let kind = v
            .get("kind")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string();
        if IGNORED_KINDS.contains(&kind.as_str()) {
            continue;
        }
        tags.push(Tag {
            path: normalize_path(path),
            name: name.to_string(),
            kind,
            line: v.get("line").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        });
    }
    (tags, malformed)
}

/// Repo-relative, forward-slashed, without a `./` prefix.
fn normalize_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    p.strip_prefix("./").unwrap_or(&p).to_string()
}

/// Compare an oracle tag set against a graph, per language.
///
/// Pure: takes the tags already parsed, so the diff arithmetic is tested without
/// invoking anything.
pub fn diff(tags: &[Tag], gd: &GraphData) -> Vec<OracleLanguage> {
    // Key on (file, name). Line numbers deliberately do not participate: the two
    // tools disagree about where a multi-line declaration starts often enough
    // that including the line would report position disputes as missing symbols,
    // and anchor position is already measured directly against the source.
    let mut ctags_by_lang: BTreeMap<&str, BTreeSet<(String, String)>> = BTreeMap::new();
    let mut sample_of: BTreeMap<(String, String), String> = BTreeMap::new();
    for t in tags {
        let Some(lang) = language_of(&t.path) else {
            continue;
        };
        let key = (t.path.clone(), t.name.clone());
        sample_of.insert(key.clone(), format!("{}:{} {}", t.path, t.line, t.name));
        ctags_by_lang.entry(lang.name).or_default().insert(key);
    }

    let mut syn_by_lang: BTreeMap<&str, BTreeSet<(String, String)>> = BTreeMap::new();
    for n in &gd.nodes {
        if n.source_file.is_empty() || crate::quality::is_file_node(n) {
            continue;
        }
        let Some(lang) = language_of(&n.source_file) else {
            continue;
        };
        let Some(name) = bare_name(&n.label) else {
            continue;
        };
        syn_by_lang
            .entry(lang.name)
            .or_default()
            .insert((normalize_path(&n.source_file), name.to_string()));
    }

    // Only languages the oracle actually understood. A language it emitted no
    // tags for is unsupported by ctags, not missed by Synaptic.
    let mut out = Vec::new();
    for (lang, ctags_set) in &ctags_by_lang {
        let empty = BTreeSet::new();
        let syn_set = syn_by_lang.get(lang).unwrap_or(&empty);
        let ctags_only: Vec<&(String, String)> = ctags_set.difference(syn_set).collect();
        out.push(OracleLanguage {
            language: (*lang).to_string(),
            agreement: ctags_set.intersection(syn_set).count(),
            ctags_only: ctags_only.len(),
            synaptic_only: syn_set.difference(ctags_set).count(),
            samples: ctags_only
                .iter()
                .take(5)
                .filter_map(|k| sample_of.get(*k).cloned())
                .collect(),
        });
    }
    out
}

/// Run the oracle over a checkout and diff it against the graph.
///
/// `-f -` is not optional: without it ctags writes a `tags` file into the
/// checkout, which would dirty the pinned tree the next repetition measures.
pub fn compare(dir: &Path, gd: &GraphData) -> OracleOutcome {
    if let Err(e) = probe() {
        return OracleOutcome::unavailable(e);
    }
    let out = match Command::new("ctags")
        .current_dir(dir)
        .args([
            "--output-format=json",
            "--fields=+n",
            "--quiet",
            "-R",
            "-f",
            "-",
            ".",
        ])
        .output()
    {
        Ok(o) => o,
        Err(e) => return OracleOutcome::unavailable(format!("running ctags: {e}")),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (tags, malformed) = parse_tags(&stdout);
    if tags.is_empty() {
        return OracleOutcome::unavailable(format!(
            "ctags produced no tags ({} malformed lines); stderr: {}",
            malformed,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    OracleOutcome {
        available: true,
        reason: None,
        per_language: diff(&tags, gd),
        malformed_tag_lines: malformed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaptic_core::node_kind::{KindValue, NodeKind};
    use synaptic_core::{NodeId, node::Node};

    fn decl(label: &str, file: &str) -> Node {
        let mut n = Node {
            id: NodeId(format!("{file}::{label}")),
            label: label.to_string(),
            source_file: file.to_string(),
            ..Default::default()
        };
        n.kind = Some(KindValue::Known(NodeKind::Function));
        n
    }

    #[test]
    fn parses_tag_lines_and_skips_metadata() {
        let src = r#"{"_type":"ptag","name":"JSON_OUTPUT_VERSION"}
{"_type":"tag","name":"alpha","path":"./src/lib.rs","kind":"function","line":3}
{"_type":"tag","name":"beta","path":"src/lib.rs","kind":"function","line":9}
"#;
        let (tags, malformed) = parse_tags(src);
        assert_eq!(malformed, 0);
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].path, "src/lib.rs", "./ prefix must be stripped");
        assert_eq!(tags[0].line, 3);
    }

    #[test]
    fn malformed_lines_are_counted_not_swallowed() {
        let src = "not json at all\n{\"_type\":\"tag\",\"path\":\"a.rs\"}\n";
        let (tags, malformed) = parse_tags(src);
        assert!(tags.is_empty());
        assert_eq!(malformed, 2, "unparseable line and a tag with no name");
    }

    #[test]
    fn ignored_kinds_are_dropped() {
        let src = r#"{"_type":"tag","name":"tmp","path":"a.rs","kind":"local","line":4}
{"_type":"tag","name":"real","path":"a.rs","kind":"function","line":5}
"#;
        let (tags, _) = parse_tags(src);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "real");
    }

    #[test]
    fn diff_splits_agreement_from_each_side() {
        let tags = vec![
            Tag {
                path: "a.rs".into(),
                name: "shared".into(),
                kind: "function".into(),
                line: 1,
            },
            Tag {
                path: "a.rs".into(),
                name: "ctags_found".into(),
                kind: "function".into(),
                line: 7,
            },
        ];
        let gd = GraphData {
            nodes: vec![decl("shared()", "a.rs"), decl("synaptic_found()", "a.rs")],
            links: vec![],
            ..Default::default()
        };
        let d = diff(&tags, &gd);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].language, "rust");
        assert_eq!(d[0].agreement, 1);
        assert_eq!(d[0].ctags_only, 1);
        assert_eq!(d[0].synaptic_only, 1);
        assert_eq!(d[0].missed_rate(), 0.5);
        assert_eq!(d[0].samples, vec!["a.rs:7 ctags_found"]);
    }

    /// A language the oracle knows nothing about must not be reported as a
    /// total miss; Apex and QL would otherwise post a fabricated 100%.
    #[test]
    fn languages_the_oracle_never_tagged_are_omitted() {
        let tags = vec![Tag {
            path: "a.rs".into(),
            name: "shared".into(),
            kind: "function".into(),
            line: 1,
        }];
        let gd = GraphData {
            nodes: vec![decl("shared()", "a.rs"), decl("Thing", "force-app/T.cls")],
            links: vec![],
            ..Default::default()
        };
        let d = diff(&tags, &gd);
        assert_eq!(d.len(), 1, "only rust was tagged: {d:?}");
        assert!(!d.iter().any(|l| l.language == "apex"));
    }

    #[test]
    fn worst_missed_rate_is_the_gating_number() {
        let outcome = OracleOutcome {
            available: true,
            reason: None,
            per_language: vec![
                OracleLanguage {
                    language: "rust".into(),
                    agreement: 9,
                    ctags_only: 1,
                    ..Default::default()
                },
                OracleLanguage {
                    language: "pascal".into(),
                    agreement: 1,
                    ctags_only: 3,
                    ..Default::default()
                },
            ],
            malformed_tag_lines: 0,
        };
        assert!((outcome.worst_missed_rate() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn unavailable_carries_its_reason() {
        let o = OracleOutcome::unavailable("ctags not found on PATH");
        assert!(!o.available);
        assert!(o.reason.unwrap().contains("not found"));
    }
}
