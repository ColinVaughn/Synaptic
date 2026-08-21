//! Extraction quality across the pinned real-world corpus.
//!
//! The scale benchmark ([`crate::scale`]) times extraction; a graph that anchored
//! every declaration to the wrong line would post identical timings. This
//! measures whether the graph is *right*, on real repositories, using properties
//! that need no hand labels:
//!
//! - **Anchor exactness** -- the recorded `(file, line)` for a declaration really
//!   does contain that declaration. A 2026-08-13 audit found 1,475 of 31,732
//!   anchors wrong across 14 repositories; that audit was ad hoc, so nothing
//!   stopped the same class of bug returning. This makes it repeatable.
//! - **Parse and recovery health** -- how often the grammar errored, how often a
//!   non-empty source file yielded no declaration at all, and how much the
//!   bounded recovery pass rescued.
//! - **Self-consistency** -- two extractions of one revision must agree, and a
//!   full rebuild must agree with an incremental one. Both have a principled
//!   correct answer, so they are assertions rather than scores.
//!
//! The independent-oracle comparison lives in [`crate::oracle`]; it is the only
//! stage that needs a second tool installed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use synaptic_core::{GraphData, Node};
use synaptic_incremental::{ChangeSet, RebuildOptions, rebuild, topology};

use crate::oracle::{self, OracleOutcome};
use crate::repo_corpus::{CorpusRepo, Language, language_of};

/// How many files the incremental-equivalence check touches.
const INCREMENTAL_SAMPLE_FILES: usize = 5;

/// The identifier an anchor check looks for in the source line.
///
/// Labels are not always bare identifiers. Fortran and Julia produce `.go()` and
/// `.Base()`, methods arrive as `Class.method`, and some extractors append `()`
/// to callables. The name is the last identifier-shaped segment: split on the
/// separators extractors use for qualification, drop an argument list, and take
/// what remains.
pub fn bare_name(label: &str) -> Option<&str> {
    let s = label.trim();
    if s.starts_with("anonymous@") {
        return None;
    }
    // Qualifier and callable decoration the extractors add, not source text.
    let s = s.strip_prefix('.').unwrap_or(s);
    let s = s.strip_suffix("()").unwrap_or(s);
    // Split only on argument/generic openers. `[` is deliberately not a
    // separator: a Ruby operator method is labeled `[]` and a changelog heading
    // is labeled `[0.1] 2007-03-03`, and both are the text to look for.
    let head = s.split(['(', '<']).next().unwrap_or(s).trim();

    // A qualified name (`Gson.toJson`, `Win32Window::MessageHandler`) is looked
    // up by its final identifier segment. Only when the head is a single token:
    // a docstring label is a whole sentence, and splitting one on `.` or `/`
    // picks a meaningless fragment -- `"...From: https://github.com"` yielded
    // the needle `com`, which scored 523 correctly-anchored Python docstrings as
    // wrong.
    if !head.contains(char::is_whitespace)
        && let Some(tail) = head.rsplit(['.', ':', '#', '/', '\\']).next()
        && is_identifier(tail)
    {
        return Some(tail);
    }
    // Otherwise the label is literal text rather than an identifier: a changelog
    // heading, an operator method, a TODO whose label is its whole comment. Take
    // the first token carrying a letter or digit, so a comment marker (`//`)
    // never becomes the needle -- it would match every comment in the file and
    // turn the check into a tautology. Using only one token means a label whose
    // newlines were collapsed into spaces (a Python module docstring) still
    // resolves against the single line it starts on.
    let mut tokens = head.split_whitespace();
    let first = tokens.clone().next()?;
    Some(
        tokens
            .find(|t| t.chars().any(|c| c.is_alphanumeric()))
            .unwrap_or(first),
    )
}

/// Whether a declaration name and a file stem are the same identifier.
///
/// A filename is not always a legal identifier, so an extractor must sanitize
/// it: `Z-Index.razor` declares the Blazor component `Z_Index`. Comparing the
/// two literally reported that correct name as a wrong anchor, so both sides are
/// normalized the way such an extractor must normalize them.
fn same_identifier(name: &str, stem: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    let (a, b) = (norm(name), norm(stem));
    a == b || format!("_{a}") == b || a == format!("_{b}")
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Whether `line` names `label`.
///
/// `case_folds` is passed from the language table: Fortran, Pascal, PowerShell,
/// Apex, SQL and classic ASP are case-insensitive by specification, so an
/// extractor that normalizes case is correct and a byte comparison would report
/// a spurious error for every symbol in those languages.
pub fn anchor_matches(label: &str, line: &str, case_folds: bool) -> bool {
    let Some(name) = bare_name(label) else {
        return false;
    };
    if line.contains(name) {
        return true;
    }
    case_folds
        && line
            .to_ascii_lowercase()
            .contains(&name.to_ascii_lowercase())
}

/// How an anchor resolved against the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The name is on the anchored line.
    OnLine,
    /// The anchored line is the declaration's true start -- its annotation or
    /// attribute block, its docstring opener, or the first line of a signature
    /// wrapped across lines -- and the name follows within it.
    LeadingMatter,
    /// The declaration is named by its file rather than by any text inside it
    /// (a dbt model, a Blazor component), and is anchored at line 1.
    FileNamed,
    /// The label carries no text to look for (`()`), so there is nothing to
    /// check. Excluded from the ratio rather than scored as a failure.
    Unnameable,
    /// The anchored line does not reach the declaration at all.
    Wrong,
}

/// A line that is part of a declaration but does not yet name it.
///
/// Two shapes, both of which put the name a line or more below the declaration's
/// true start:
///
/// - **Leading matter** -- an annotation (`@Override`), an attribute
///   (`[Serializable]`, `#[derive(...)]`, `__attribute((section(...)))`), a
///   comment, or a docstring opener (`"""`).
/// - **An unfinished line** -- a signature wrapped across lines, as SystemVerilog
///   writes them: `function automatic secded_22_16_t` on one line and
///   `prim_secded_22_16_dec (logic [21:0] data_i);` on the next.
///
/// Blank lines are deliberately excluded, and so is any line that closes a
/// statement. An anchor landing on a blank line is the signature of the `^\s*`
/// regex bug that cost Pascal 58% of its anchors (`\s` matches a newline, so the
/// match starts one line early); admitting blanks would hide the very defect
/// this metric exists to catch.
fn leads_into_declaration(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let is_leading_matter = t.starts_with('@')
        || t.starts_with('[')
        || t.starts_with('#')
        || t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with("---")
        || t.starts_with("<!--")
        || t.starts_with("\"\"\"")
        || t.starts_with("'''")
        || t.starts_with("__attribute");
    // A line that ends a statement or closes a block cannot be the head of a
    // declaration that continues below it.
    let finished = t.ends_with(';') || t.ends_with('}') || t.ends_with("*/");
    is_leading_matter || !finished
}

/// Net bracket depth a line opens, so a multi-line annotation such as
/// `@SuppressWarnings({"unchecked",` / `"rawtypes"})` is followed to its close.
fn bracket_delta(line: &str) -> i32 {
    line.chars().fold(0, |d, c| match c {
        '(' | '[' | '{' => d + 1,
        ')' | ']' | '}' => d - 1,
        _ => d,
    })
}

/// How far past its anchor a declaration's leading annotation block may run.
const MAX_LEADING_MATTER_LINES: usize = 256;

/// Resolve one anchor against the file's lines (`at` is 1-based).
///
/// A declaration carrying annotations legitimately *starts* at its first
/// annotation, because that is where its syntax node starts. Scoring only the
/// anchored line would report 62% of annotated Java as misplaced, which is not a
/// defect but a different, defensible convention. So the walk follows an
/// unbroken run of leading matter into the declaration and reports how the
/// anchor resolved, rather than collapsing both cases into one number.
pub fn resolve_anchor(
    lines: &[String],
    at: u32,
    label: &str,
    case_folds: bool,
    file_stem: Option<&str>,
) -> Verdict {
    if bare_name(label).is_none() {
        return Verdict::Unnameable;
    }
    // Some declarations are named by their file, never by any text inside it: a
    // dbt model, a Blazor component, a Vue single-file component. For those the
    // declaration *is* the file, so line 1 is the correct anchor and no line
    // will ever spell the name. Accepted only at line 1, so an extractor that
    // drops such a node at an arbitrary line is still reported.
    if at == 1
        && let Some(stem) = file_stem
        && bare_name(label).is_some_and(|n| same_identifier(n, stem))
    {
        return Verdict::FileNamed;
    }
    let Some(idx) = (at as usize).checked_sub(1) else {
        return Verdict::Wrong;
    };
    let Some(first) = lines.get(idx) else {
        return Verdict::Wrong;
    };
    if anchor_matches(label, first, case_folds) {
        return Verdict::OnLine;
    }
    // Follow an unbroken run of declaration-leading lines (and any bracket
    // continuation they open) into the declaration. Stop at the first line that
    // is neither, so a blank line or a finished statement ends the walk rather
    // than letting it wander into the next declaration.
    let mut depth = 0i32;
    for (offset, line) in lines
        .iter()
        .skip(idx)
        .take(MAX_LEADING_MATTER_LINES)
        .enumerate()
    {
        let continuing = depth > 0;
        // The name is looked for on continuation lines too. An HCL `locals {`
        // block opens a bracket and declares its names inside it, so skipping
        // bracketed lines reported every Terraform local as misplaced.
        if offset > 0 && anchor_matches(label, line, case_folds) {
            return Verdict::LeadingMatter;
        }
        if !leads_into_declaration(line) && !continuing {
            break;
        }
        depth = (depth + bracket_delta(line)).max(0);
    }
    Verdict::Wrong
}

/// The id the extractor gives a file's own node, so it can be told apart from
/// the symbols declared inside it.
///
/// Deliberately not keyed on `kind`: a Markdown heading, a YAML workflow job and
/// an HCL block are all real extracted structure that carries no `NodeKind`.
/// Requiring a kind would silently exclude every such language from the anchor
/// metric, and report its files as empty besides.
pub(crate) fn file_node_id(path: &str) -> synaptic_core::NodeId {
    synaptic_core::NodeId(synaptic_core::make_id(&[path]))
}

pub(crate) fn is_file_node(node: &Node) -> bool {
    node.id == file_node_id(&node.source_file)
}

/// The 1-based line a node claims to be declared on.
///
/// Not every extractor emits a `Span`. Markdown headings, YAML jobs and a
/// sizeable minority of ordinary symbols carry only `source_location: "L79"`.
/// Checking spans alone silently excluded all of them, which is how a metric
/// meant to catch off-by-one anchors would have missed every off-by-one anchor
/// in four languages.
fn anchor_line(node: &Node) -> Option<u32> {
    if let Some(span) = node.span() {
        return Some(span.start_line);
    }
    node.source_location
        .as_deref()
        .and_then(|s| s.strip_prefix('L'))
        .and_then(|n| n.parse().ok())
}

/// Whether a node has a declaration site in this checkout worth anchor-checking.
///
/// File nodes, external stubs and phantom import targets do not, so checking
/// them would measure nothing.
fn is_checkable_declaration(node: &Node) -> bool {
    !node.source_file.is_empty() && anchor_line(node).is_some() && !is_file_node(node)
}

/// Whether the bounded recovery pass produced this node rather than the grammar.
fn is_recovered(node: &Node) -> bool {
    node.extra
        .get("recovered")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Per-language quality counters for one repository.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct LanguageQuality {
    pub language: String,
    /// Declarations whose anchor was checked (parsed structure only).
    pub anchors_checked: usize,
    pub anchors_exact: usize,
    /// Of the exact ones, how many were anchored at the head of the
    /// declaration's annotation/attribute block rather than on the signature
    /// line itself. Published separately because it is a convention difference,
    /// not a defect, and folding it in silently would overstate precision.
    pub anchors_via_leading_matter: usize,
    /// Of the exact ones, how many are named by their file rather than by any
    /// text inside it, and so are anchored at line 1 by construction.
    pub anchors_file_named: usize,
    /// Nodes whose label carries no text to look for, so no anchor claim could
    /// be checked either way. Published so the denominator above is auditable.
    pub unnameable: usize,
    /// Declarations contributed by the recovery pass, scored separately so
    /// parsed and recovered structure are never averaged together.
    pub recovered_checked: usize,
    pub recovered_exact: usize,
    pub files: usize,
    pub parse_error_files: usize,
    pub zero_decl_files: usize,
    /// Up to a handful of `file:line label` samples for the wrong anchors, so a
    /// regression report names something actionable instead of a percentage.
    pub anchor_failures: Vec<String>,
}

impl LanguageQuality {
    pub fn anchor_exactness(&self) -> f64 {
        ratio(self.anchors_exact, self.anchors_checked)
    }
    pub fn recovered_exactness(&self) -> f64 {
        ratio(self.recovered_exact, self.recovered_checked)
    }
    pub fn parse_error_rate(&self) -> f64 {
        ratio(self.parse_error_files, self.files)
    }
    pub fn zero_decl_file_rate(&self) -> f64 {
        ratio(self.zero_decl_files, self.files)
    }
}

/// `n / d`, with an empty denominator reported as 1.0 for exactness ratios and
/// 0.0 for defect ratios. Both are the "nothing wrong here" reading, and a
/// vacuous score is never allowed to look like a measured one because
/// `*_checked` is published alongside it.
fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 { 0.0 } else { n as f64 / d as f64 }
}

/// Whether the two self-consistency assertions held.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Consistency {
    pub deterministic: bool,
    pub incremental_equivalent: bool,
    /// Populated only on failure, naming the first divergence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One measured repository.
#[derive(Debug, Clone, Serialize)]
pub struct RepoQuality {
    pub name: String,
    pub url: String,
    pub sha: String,
    pub family: String,
    pub declared_languages: Vec<String>,
    pub files: usize,
    pub nodes: usize,
    pub edges: usize,
    pub anchors_checked: usize,
    pub anchors_exact: usize,
    pub recovered_nodes: usize,
    pub parse_error_files: usize,
    pub zero_decl_files: usize,
    pub per_language: Vec<LanguageQuality>,
    pub consistency: Consistency,
    pub oracle: OracleOutcome,
}

impl RepoQuality {
    pub fn anchor_exactness(&self) -> f64 {
        ratio(self.anchors_exact, self.anchors_checked)
    }
    pub fn parse_error_rate(&self) -> f64 {
        ratio(self.parse_error_files, self.files)
    }
    pub fn zero_decl_file_rate(&self) -> f64 {
        ratio(self.zero_decl_files, self.files)
    }
}

/// A repository that could not be measured. Retained in the report rather than
/// dropped, so a partial run cannot read as a complete one.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QualitySkip {
    pub url: String,
    pub reason: String,
}

/// The full quality report.
#[derive(Debug, Clone, Serialize)]
pub struct QualityReport {
    pub env: crate::scale::ScaleEnv,
    pub results: Vec<RepoQuality>,
    pub skipped: Vec<QualitySkip>,
    /// Absent when the oracle binary was not found; the other measurements still ran.
    pub oracle_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle_unavailable_reason: Option<String>,
}

impl QualityReport {
    /// Pooled anchor exactness across every measured repository.
    pub fn pooled_anchor_exactness(&self) -> f64 {
        let exact: usize = self.results.iter().map(|r| r.anchors_exact).sum();
        let checked: usize = self.results.iter().map(|r| r.anchors_checked).sum();
        ratio(exact, checked)
    }

    /// Per-language counters pooled across repositories, language-sorted.
    pub fn pooled_by_language(&self) -> Vec<LanguageQuality> {
        let mut by: BTreeMap<&str, LanguageQuality> = BTreeMap::new();
        for repo in &self.results {
            for lang in &repo.per_language {
                let e = by.entry(lang.language.as_str()).or_default();
                e.language = lang.language.clone();
                e.anchors_checked += lang.anchors_checked;
                e.anchors_exact += lang.anchors_exact;
                e.anchors_via_leading_matter += lang.anchors_via_leading_matter;
                e.anchors_file_named += lang.anchors_file_named;
                e.unnameable += lang.unnameable;
                e.recovered_checked += lang.recovered_checked;
                e.recovered_exact += lang.recovered_exact;
                e.files += lang.files;
                e.parse_error_files += lang.parse_error_files;
                e.zero_decl_files += lang.zero_decl_files;
                for f in &lang.anchor_failures {
                    if e.anchor_failures.len() < MAX_FAILURE_SAMPLES {
                        e.anchor_failures.push(f.clone());
                    }
                }
            }
        }
        by.into_values().collect()
    }
}

/// Cap on retained failure samples, per language per repository.
const MAX_FAILURE_SAMPLES: usize = 5;

/// Score one built graph against its checkout.
///
/// Pure apart from reading the checkout's source files, so it is exercised
/// directly by tests over synthetic graphs.
pub fn score_graph(dir: &Path, gd: &GraphData) -> Vec<LanguageQuality> {
    // Cache each file's lines once. A repository can carry tens of thousands of
    // declarations across a few thousand files; re-reading per node would make
    // the check quadratic in the common case.
    let mut lines_of: BTreeMap<&str, Option<Vec<String>>> = BTreeMap::new();
    let mut by_lang: BTreeMap<&str, LanguageQuality> = BTreeMap::new();

    // File-level counters first: which files exist, which errored, and which
    // produced nothing but their own file node. That last case is the silent
    // hole the recovery pass exists to fill -- indistinguishable, in the graph,
    // from a file that genuinely declares nothing.
    let mut files: BTreeMap<&str, (bool, bool)> = BTreeMap::new(); // (parse_error, has_decl)
    for node in &gd.nodes {
        if node.source_file.is_empty() {
            continue;
        }
        let entry = files
            .entry(node.source_file.as_str())
            .or_insert((false, false));
        if node
            .extra
            .get("parse_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            entry.0 = true;
        }
        if !is_file_node(node) {
            entry.1 = true;
        }
    }
    for (path, (parse_error, has_decl)) in &files {
        let Some(lang) = language_of(path) else {
            continue;
        };
        let e = by_lang.entry(lang.name).or_default();
        e.language = lang.name.to_string();
        e.files += 1;
        if *parse_error {
            e.parse_error_files += 1;
        }
        if !*has_decl {
            e.zero_decl_files += 1;
        }
    }

    for node in &gd.nodes {
        if !is_checkable_declaration(node) {
            continue;
        }
        let Some(lang) = language_of(&node.source_file) else {
            continue;
        };
        let lines = lines_of
            .entry(node.source_file.as_str())
            .or_insert_with(|| read_lines(dir, &node.source_file));
        let Some(lines) = lines.as_ref() else {
            continue; // file unreadable from here (submodule, filtered blob)
        };
        // Lines are 1-based; an anchor past the end of the file is itself a
        // failure, not a reason to skip the node.
        let at = anchor_line(node).expect("checked above");
        let stem = std::path::Path::new(node.source_file.as_str())
            .file_stem()
            .and_then(|s| s.to_str());
        let verdict = resolve_anchor(lines, at, &node.label, lang.case_folds, stem);
        let e = by_lang.entry(lang.name).or_default();
        e.language = lang.name.to_string();
        if verdict == Verdict::Unnameable {
            // A synthesized label such as `()` names nothing to look for.
            // Counting it wrong would charge the extractor for a node that has
            // no source name at all; it is published as its own number instead.
            e.unnameable += 1;
            continue;
        }
        let ok = verdict != Verdict::Wrong;
        if is_recovered(node) {
            e.recovered_checked += 1;
            e.recovered_exact += usize::from(ok);
        } else {
            e.anchors_checked += 1;
            e.anchors_exact += usize::from(ok);
            if verdict == Verdict::LeadingMatter {
                e.anchors_via_leading_matter += 1;
            }
            if verdict == Verdict::FileNamed {
                e.anchors_file_named += 1;
            }
            if !ok && e.anchor_failures.len() < MAX_FAILURE_SAMPLES {
                e.anchor_failures
                    .push(format!("{}:{} {}", node.source_file, at, node.label));
            }
        }
    }

    by_lang.into_values().collect()
}

fn read_lines(dir: &Path, rel: &str) -> Option<Vec<String>> {
    std::fs::read_to_string(dir.join(rel))
        .ok()
        .map(|s| s.lines().map(str::to_string).collect())
}

fn build_full(dir: &Path) -> Result<GraphData, String> {
    let out = rebuild(
        &RebuildOptions {
            root: dir.to_path_buf(),
            directed: true,
            force: true,
        },
        &ChangeSet::Full,
        None,
    )
    .map_err(|e| e.to_string())?;
    Ok(out.kg.to_graph_data())
}

/// The first difference between two topologies, if any.
fn first_divergence(
    label: &str,
    a: &(Vec<String>, Vec<(String, String, String)>),
    b: &(Vec<String>, Vec<(String, String, String)>),
) -> Option<String> {
    let an: BTreeSet<&String> = a.0.iter().collect();
    let bn: BTreeSet<&String> = b.0.iter().collect();
    if an != bn {
        return Some(format!(
            "{label}: node sets differ; first -/+ {:?}/{:?}",
            an.difference(&bn).next(),
            bn.difference(&an).next()
        ));
    }
    let ae: BTreeSet<&(String, String, String)> = a.1.iter().collect();
    let be: BTreeSet<&(String, String, String)> = b.1.iter().collect();
    if ae != be {
        return Some(format!(
            "{label}: edge sets differ; first -/+ {:?}/{:?}",
            ae.difference(&be).next(),
            be.difference(&ae).next()
        ));
    }
    None
}

/// Deterministic sample of source files for the incremental-equivalence check:
/// the lexicographically first extracted files that still exist on disk.
fn sample_files(dir: &Path, gd: &GraphData) -> Vec<PathBuf> {
    let mut paths: Vec<&str> = gd
        .nodes
        .iter()
        .map(|n| n.source_file.as_str())
        .filter(|s| !s.is_empty() && language_of(s).is_some() && dir.join(s).is_file())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths
        .into_iter()
        .take(INCREMENTAL_SAMPLE_FILES)
        .map(PathBuf::from)
        .collect()
}

/// Extract twice and compare, then rebuild incrementally and compare.
///
/// Neither property tolerates a baseline: a second extraction of the same bytes
/// must produce the same graph, and touching files without editing them must not
/// change the topology.
fn check_consistency(dir: &Path, gd: &GraphData) -> Result<Consistency, String> {
    let second = build_full(dir)?;
    let a = topology(gd);
    let b = topology(&second);
    let deterministic_detail = first_divergence("determinism", &a, &b);

    let files = sample_files(dir, gd);
    let mut incremental_detail = None;
    if !files.is_empty() {
        let outcome = rebuild(
            &RebuildOptions {
                root: dir.to_path_buf(),
                directed: true,
                force: true,
            },
            &ChangeSet::Incremental(files.clone()),
            Some(gd),
        )
        .map_err(|e| e.to_string())?;
        let c = topology(&outcome.kg.to_graph_data());
        incremental_detail = first_divergence("incremental", &a, &c).or_else(|| {
            (outcome.reextracted != files.len()).then(|| {
                format!(
                    "incremental: re-extracted {} of {} touched files",
                    outcome.reextracted,
                    files.len()
                )
            })
        });
    }

    Ok(Consistency {
        deterministic: deterministic_detail.is_none(),
        incremental_equivalent: incremental_detail.is_none(),
        detail: deterministic_detail.or(incremental_detail),
    })
}

/// Measure one checked-out repository end to end.
pub fn measure_repo(
    dir: &Path,
    repo: &CorpusRepo,
    run_oracle: bool,
) -> Result<RepoQuality, String> {
    let gd = build_full(dir)?;
    let per_language = score_graph(dir, &gd);
    let consistency = check_consistency(dir, &gd)?;
    let oracle = if run_oracle {
        oracle::compare(dir, &gd)
    } else {
        OracleOutcome::unavailable("oracle stage disabled for this run")
    };

    let files = gd
        .nodes
        .iter()
        .map(|n| n.source_file.as_str())
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>()
        .len();

    Ok(RepoQuality {
        name: repo.name().to_string(),
        url: repo.url.clone(),
        sha: repo.sha.clone(),
        family: repo.family.clone(),
        declared_languages: repo.languages.clone(),
        files,
        nodes: gd.nodes.len(),
        edges: gd.links.len(),
        anchors_checked: per_language.iter().map(|l| l.anchors_checked).sum(),
        anchors_exact: per_language.iter().map(|l| l.anchors_exact).sum(),
        recovered_nodes: per_language.iter().map(|l| l.recovered_checked).sum(),
        parse_error_files: per_language.iter().map(|l| l.parse_error_files).sum(),
        zero_decl_files: per_language.iter().map(|l| l.zero_decl_files).sum(),
        per_language,
        consistency,
        oracle,
    })
}

/// The language table entry for a name, for callers rendering reports.
pub fn language(name: &str) -> Option<&'static Language> {
    crate::repo_corpus::LANGUAGES
        .iter()
        .find(|l| l.name == name)
}

/// Which corpus members a run covers.
#[derive(Debug, Clone, Default)]
pub struct QualityFilter {
    /// Only repositories declaring this language.
    pub language: Option<String>,
    /// Only this repository, by manifest name.
    pub repo: Option<String>,
    /// Skip the oracle stage even when ctags is installed.
    pub skip_oracle: bool,
}

impl QualityFilter {
    fn selects(&self, repo: &CorpusRepo) -> bool {
        if let Some(name) = &self.repo
            && repo.name() != name
        {
            return false;
        }
        if let Some(lang) = &self.language
            && !repo.languages.iter().any(|l| l == lang)
        {
            return false;
        }
        true
    }
}

/// Clone and measure every quality-suite repository in the manifest.
///
/// The oracle is probed once up front rather than per repository: a missing
/// binary is a property of the machine, and probing 50 times would report the
/// same absence 50 times.
pub fn run_quality(
    manifest_path: &Path,
    cache_dir: &Path,
    filter: &QualityFilter,
) -> Result<QualityReport, String> {
    use crate::repo_corpus::{CorpusManifest, SUITE_QUALITY};

    let manifest =
        CorpusManifest::parse(&std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;

    let probe = oracle::probe();
    let oracle_available = probe.is_ok() && !filter.skip_oracle;
    let oracle_unavailable_reason = match (&probe, filter.skip_oracle) {
        (_, true) => Some("oracle stage disabled with --skip-oracle".to_string()),
        (Err(e), _) => Some(e.clone()),
        _ => None,
    };

    let mut results = Vec::new();
    let mut skipped = Vec::new();
    for repo in manifest.in_suite(SUITE_QUALITY) {
        if !filter.selects(repo) {
            continue;
        }
        match crate::scale::ensure_checkout(cache_dir, repo)
            .and_then(|dir| measure_repo(&dir, repo, oracle_available))
        {
            Ok(r) => {
                eprintln!(
                    "  {:<28} anchors {:>6}/{:<6} ({:.2}%)",
                    r.name,
                    r.anchors_exact,
                    r.anchors_checked,
                    r.anchor_exactness() * 100.0
                );
                results.push(r);
            }
            Err(e) => {
                eprintln!("SKIP {}: {e}", repo.url);
                skipped.push(QualitySkip {
                    url: repo.url.clone(),
                    reason: e,
                });
            }
        }
    }

    Ok(QualityReport {
        env: crate::scale::ScaleEnv::detect(),
        results,
        skipped,
        oracle_available,
        oracle_unavailable_reason,
    })
}

/// Resolve every manifest URL's current default-branch HEAD and rewrite its pin.
///
/// Curating fifty repositories by hand is the expensive part of this corpus;
/// re-pinning them should not also be. Unreachable URLs are returned rather than
/// silently left behind, and their existing pins are preserved.
pub fn pin_manifest(manifest_path: &Path) -> Result<(usize, Vec<String>), String> {
    use crate::repo_corpus::{CorpusManifest, repin};

    let src = std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?;
    let manifest = CorpusManifest::parse(&src).map_err(|e| e.to_string())?;

    let mut resolved = BTreeMap::new();
    let mut failures = Vec::new();
    for repo in &manifest.repos {
        match crate::scale::git_output(&["ls-remote", &repo.url, "HEAD"], None) {
            Ok(out) => match out.split_whitespace().next() {
                Some(sha) if sha.len() >= 40 => {
                    resolved.insert(repo.url.clone(), sha.to_string());
                }
                _ => failures.push(format!("{}: no HEAD in ls-remote output", repo.url)),
            },
            Err(e) => failures.push(format!("{}: {e}", repo.url)),
        }
    }

    std::fs::write(manifest_path, repin(&src, &resolved)).map_err(|e| e.to_string())?;
    Ok((resolved.len(), failures))
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaptic_core::node_kind::{KindValue, NodeKind};
    use synaptic_core::span::Span;
    use synaptic_core::{NodeId, node::Node};

    fn decl(label: &str, file: &str, line: u32) -> Node {
        let mut n = Node {
            id: NodeId(format!("{file}::{label}")),
            label: label.to_string(),
            source_file: file.to_string().into(),
            ..Default::default()
        };
        n.kind = Some(KindValue::Known(NodeKind::Function));
        n.set_span(Span {
            start_line: line,
            start_col: 0,
            end_line: line,
            end_col: 0,
        });
        n
    }

    /// A node with structure but no `NodeKind`, as Markdown headings, YAML jobs
    /// and HCL blocks arrive: anchored by `source_location`, with no `Span`.
    fn kindless(label: &str, file: &str, line: u32) -> Node {
        Node {
            id: NodeId(format!("{file}::{label}")),
            label: label.to_string(),
            source_file: file.to_string().into(),
            source_location: Some(format!("L{line}")),
            ..Default::default()
        }
    }

    /// The file's own node, built with the same id the extractor uses.
    fn file_node(file: &str) -> Node {
        Node {
            id: file_node_id(file),
            label: file.rsplit('/').next().unwrap_or(file).to_string(),
            source_file: file.to_string().into(),
            ..Default::default()
        }
    }

    #[test]
    fn bare_name_strips_qualifiers_and_argument_lists() {
        assert_eq!(bare_name("handle_request()"), Some("handle_request"));
        assert_eq!(bare_name(".go()"), Some("go"));
        assert_eq!(bare_name(".Base()"), Some("Base"));
        assert_eq!(bare_name("Gson.toJson"), Some("toJson"));
        assert_eq!(
            bare_name("Win32Window::MessageHandler()"),
            Some("MessageHandler")
        );
        assert_eq!(bare_name("Vec<T>"), Some("Vec"));
        assert_eq!(bare_name("()"), None);
        assert_eq!(bare_name("anonymous@42"), None);
        assert_eq!(bare_name(""), None);
    }

    /// Not every label is an identifier. A Ruby operator method, a changelog
    /// heading and a docstring are all real anchors, and treating them as
    /// unnameable scored 448 correct clap anchors as wrong.
    #[test]
    fn bare_name_handles_labels_that_are_not_identifiers() {
        // Ruby `def [](key)` / `def <<(x)`.
        assert_eq!(bare_name(".[]()"), Some("[]"));
        assert!(anchor_matches(".[]()", "      def [](key)", false));
        // Markdown changelog heading `## [0.1] 2007-03-03`.
        assert_eq!(bare_name("[0.1] 2007-03-03"), Some("[0.1]"));
        assert!(anchor_matches(
            "[0.1] 2007-03-03",
            "## [0.1] 2007-03-03",
            false
        ));
        // A docstring label whose newlines were collapsed into spaces still
        // resolves against the single line it starts on.
        assert_eq!(
            bare_name("Copyright 2019 Google LLC    Licensed under the Apache"),
            Some("Copyright")
        );
    }

    /// A sentence is not a qualified name. Splitting one on `.` or `/` picked a
    /// trailing fragment -- `com` out of a URL, `sqlf` out of "`.sqlfluff`" --
    /// and scored 523 correctly-anchored Python docstrings as wrong.
    #[test]
    fn a_multi_word_label_is_never_split_as_a_qualified_name() {
        assert_eq!(
            bare_name("Methods for loading config files. This includes `.sqlfluff`"),
            Some("Methods")
        );
        assert_eq!(
            bare_name("Replaces variables in docs. From: https://github.com"),
            Some("Replaces")
        );
        // A genuine qualified name still resolves by its last segment.
        assert_eq!(bare_name("Gson.toJson"), Some("toJson"));
        assert_eq!(bare_name("a::b::c"), Some("c"));
    }

    fn src(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    /// A Java method annotated `@Override` starts, syntactically, at the
    /// annotation. Scoring only the anchored line reported 62% of gson's
    /// declarations as misplaced when none of them were.
    #[test]
    fn an_annotation_block_leads_into_its_declaration() {
        let lines = src("@Override\npublic String toString() {\n  return \"x\";\n}\n");
        assert_eq!(
            resolve_anchor(&lines, 1, ".toString()", false, None),
            Verdict::LeadingMatter
        );
        assert_eq!(
            resolve_anchor(&lines, 2, ".toString()", false, None),
            Verdict::OnLine
        );
    }

    /// A multi-line annotation is followed to its closing bracket.
    #[test]
    fn a_multiline_annotation_is_followed_to_the_declaration() {
        let lines = src(
            "@SuppressWarnings({\"unchecked\",\n  \"rawtypes\"})\npublic static void main(String[] a) {\n",
        );
        assert_eq!(
            resolve_anchor(&lines, 1, ".main()", false, None),
            Verdict::LeadingMatter
        );
    }

    /// C# attributes and Rust derives lead the same way.
    #[test]
    fn attributes_and_derives_lead_too() {
        let cs = src("[Serializable]\npublic class Order { }\n");
        assert_eq!(
            resolve_anchor(&cs, 1, "Order", false, None),
            Verdict::LeadingMatter
        );
        let rs = src("#[derive(Debug, Clone)]\npub struct Span {\n");
        assert_eq!(
            resolve_anchor(&rs, 1, "Span", false, None),
            Verdict::LeadingMatter
        );
    }

    #[test]
    fn large_parameterized_test_attribute_blocks_reach_the_method() {
        let mut lines = vec!["[Theory]".to_string()];
        lines.extend((0..100).map(|n| format!("[InlineData({n})]")));
        lines.push("public void accepts_all_cases(int value) {}".to_string());
        assert_eq!(
            resolve_anchor(&lines, 1, ".accepts_all_cases()", false, None),
            Verdict::LeadingMatter
        );
    }

    /// The bug this metric exists to catch: `^\s*` matches a newline, so the
    /// match starts on the blank line above the declaration. A blank line must
    /// end the walk, or the leading-matter allowance would hide it.
    #[test]
    fn a_blank_line_above_a_declaration_is_still_wrong() {
        let lines = src("procedure Foo;\n\nprocedure Bar;\n");
        assert_eq!(
            resolve_anchor(&lines, 2, "Bar", true, None),
            Verdict::Wrong,
            "an anchor on the blank line must not walk into the declaration"
        );
    }

    /// The walk must not wander across unrelated code into a later declaration.
    #[test]
    fn the_walk_stops_at_unrelated_code() {
        let lines = src("let x = compute();\nprintln!(\"hi\");\nfn target() {}\n");
        assert_eq!(
            resolve_anchor(&lines, 1, "target()", false, None),
            Verdict::Wrong
        );
    }

    /// SystemVerilog wraps a signature across lines: the return type is on the
    /// declaration's first line and the name on the next.
    #[test]
    fn a_wrapped_signature_reaches_its_name() {
        let lines = src(
            "  function automatic secded_22_16_t\n      prim_secded_22_16_dec (logic [21:0] data_i);\n",
        );
        assert_eq!(
            resolve_anchor(&lines, 1, "prim_secded_22_16_dec()", false, None),
            Verdict::LeadingMatter
        );
    }

    /// Terraform declares its locals inside a `locals {` block, so the name sits
    /// on a bracketed continuation line rather than beside the anchor.
    #[test]
    fn a_block_opener_leads_into_names_declared_inside_it() {
        let lines = src("locals {
  azs = slice(data.aws_availability_zones.available.names, 0, 3)
}
");
        assert_eq!(
            resolve_anchor(&lines, 1, "local.azs", false, None),
            Verdict::LeadingMatter
        );
    }

    /// A Python docstring node is anchored at its opening quotes; the text it is
    /// labeled with starts on the next line.
    #[test]
    fn a_docstring_opener_leads_into_its_text() {
        let lines = src("\"\"\"\nCopyright 2019 Google LLC\n\"\"\"\n");
        assert_eq!(
            resolve_anchor(
                &lines,
                1,
                "Copyright 2019 Google LLC   Licensed",
                false,
                None
            ),
            Verdict::LeadingMatter
        );
    }

    /// A GNU attribute precedes the function it decorates.
    #[test]
    fn a_gnu_attribute_leads_into_its_function() {
        let lines =
            src("__attribute ((section(\".text_test_foo\"), noinline))\nvoid target_foo() {\n");
        assert_eq!(
            resolve_anchor(&lines, 1, "target_foo()", false, None),
            Verdict::LeadingMatter
        );
    }

    /// A label with nothing to look for is excluded, not scored wrong: charging
    /// the extractor for a node that has no source name would be inventing a
    /// defect.
    #[test]
    fn an_unnameable_label_is_excluded_from_the_ratio() {
        assert_eq!(bare_name("()"), None);
        let lines = src("static struct { int x; } v;\n");
        assert_eq!(
            resolve_anchor(&lines, 1, "()", false, None),
            Verdict::Unnameable
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        let gd = GraphData {
            nodes: vec![decl("alpha()", "a.rs", 1), decl("()", "a.rs", 1)],
            links: vec![],
            ..Default::default()
        };
        let rust = score_graph(dir.path(), &gd);
        assert_eq!(rust[0].anchors_checked, 1, "only the nameable node counts");
        assert_eq!(rust[0].anchors_exact, 1);
        assert_eq!(rust[0].unnameable, 1);
    }

    /// A node labeled with the text it was found in (a TODO's label is the whole
    /// comment) is still checkable, rather than scored wrong for its shape.
    #[test]
    fn a_comment_labeled_node_resolves_by_its_first_identifier() {
        // Not `//`: a comment marker matches every comment in the file, which
        // would make the check pass for any anchor pointing at any comment.
        assert_eq!(bare_name("// TODO: fix the reader"), Some("TODO:"));
        let lines = src("// TODO: fix the reader\n");
        assert_eq!(
            resolve_anchor(&lines, 1, "// TODO: fix the reader", false, None),
            Verdict::OnLine
        );
    }

    #[test]
    fn an_anchor_past_the_end_resolves_wrong() {
        let lines = src("fn alpha() {}\n");
        assert_eq!(
            resolve_anchor(&lines, 99, "alpha()", false, None),
            Verdict::Wrong
        );
        assert_eq!(
            resolve_anchor(&lines, 0, "alpha()", false, None),
            Verdict::Wrong
        );
    }

    #[test]
    fn anchor_matches_requires_the_name_on_the_line() {
        assert!(anchor_matches("route()", "fn route(req: Req) {", false));
        assert!(!anchor_matches("route()", "fn other(req: Req) {", false));
    }

    /// Fortran and friends fold identifier case by specification, so a
    /// case-normalized label is correct rather than a wrong anchor.
    #[test]
    fn case_folding_languages_tolerate_normalized_labels() {
        assert!(anchor_matches(".go()", "SUBROUTINE GO()", true));
        assert!(!anchor_matches(".go()", "SUBROUTINE GO()", false));
    }

    #[test]
    fn score_graph_counts_exact_and_wrong_anchors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "fn alpha() {}\nfn beta() {}\n",
        )
        .unwrap();
        let gd = GraphData {
            nodes: vec![
                decl("alpha()", "src/lib.rs", 1),
                decl("beta()", "src/lib.rs", 1), // wrong: beta is on line 2
            ],
            links: vec![],
            ..Default::default()
        };
        let langs = score_graph(dir.path(), &gd);
        let rust = langs.iter().find(|l| l.language == "rust").unwrap();
        assert_eq!(rust.anchors_checked, 2);
        assert_eq!(rust.anchors_exact, 1);
        assert_eq!(rust.anchor_exactness(), 0.5);
        assert_eq!(rust.anchor_failures.len(), 1);
        assert!(
            rust.anchor_failures[0].contains("beta"),
            "{:?}",
            rust.anchor_failures
        );
    }

    /// A span past the end of the file is a failure, not a silent skip: that is
    /// exactly the shape of an off-by-one that eats a blank line.
    #[test]
    fn span_past_end_of_file_counts_as_wrong() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        let gd = GraphData {
            nodes: vec![decl("alpha()", "a.rs", 99)],
            links: vec![],
            ..Default::default()
        };
        let rust = score_graph(dir.path(), &gd);
        assert_eq!(rust[0].anchors_checked, 1);
        assert_eq!(rust[0].anchors_exact, 0);
    }

    #[test]
    fn recovered_nodes_are_scored_in_their_own_bucket() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        let mut recovered = decl("alpha()", "a.rs", 1);
        recovered
            .extra
            .insert("recovered".into(), serde_json::Value::Bool(true));
        let gd = GraphData {
            nodes: vec![recovered],
            links: vec![],
            ..Default::default()
        };
        let rust = score_graph(dir.path(), &gd);
        assert_eq!(rust[0].anchors_checked, 0, "parsed bucket must stay empty");
        assert_eq!(rust[0].recovered_checked, 1);
        assert_eq!(rust[0].recovered_exact, 1);
    }

    #[test]
    fn parse_error_and_zero_declaration_files_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.rs"), "fn ???\n").unwrap();
        std::fs::write(dir.path().join("quiet.rs"), "// nothing here\n").unwrap();
        std::fs::write(dir.path().join("ok.rs"), "fn alpha() {}\n").unwrap();
        let mut broken = file_node("broken.rs");
        broken
            .extra
            .insert("parse_error".into(), serde_json::Value::Bool(true));
        let gd = GraphData {
            nodes: vec![
                broken,
                file_node("quiet.rs"),
                file_node("ok.rs"),
                decl("alpha()", "ok.rs", 1),
            ],
            links: vec![],
            ..Default::default()
        };
        let rust = score_graph(dir.path(), &gd);
        let rust = rust.iter().find(|l| l.language == "rust").unwrap();
        assert_eq!(rust.files, 3);
        assert_eq!(rust.parse_error_files, 1);
        assert_eq!(
            rust.zero_decl_files, 2,
            "only the two files with nothing but their file node"
        );
        assert!((rust.parse_error_rate() - 1.0 / 3.0).abs() < 1e-9);
    }

    /// Markdown headings, YAML jobs and HCL blocks carry no `NodeKind`. Keying
    /// the metrics on `kind` would exclude those languages from anchor checking
    /// entirely and report every one of their files as an empty hole.
    #[test]
    fn kindless_structure_is_measured_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("README.md"),
            "# Overview\n\nsome prose\n## Details\n",
        )
        .unwrap();
        let gd = GraphData {
            nodes: vec![
                file_node("README.md"),
                kindless("Overview", "README.md", 1),
                kindless("Details", "README.md", 4),
            ],
            links: vec![],
            ..Default::default()
        };
        let md = score_graph(dir.path(), &gd);
        let md = md.iter().find(|l| l.language == "markdown").unwrap();
        assert_eq!(md.anchors_checked, 2, "headings are anchor-checked");
        assert_eq!(md.anchors_exact, 2);
        assert_eq!(md.zero_decl_files, 0, "the file is not an empty hole");
    }

    /// Anchors arrive two ways. Reading only `span` was blind to Markdown,
    /// YAML, JSON and a minority of ordinary symbols.
    #[test]
    fn anchor_line_reads_span_or_source_location() {
        assert_eq!(anchor_line(&decl("a()", "a.rs", 12)), Some(12));
        assert_eq!(anchor_line(&kindless("Overview", "R.md", 79)), Some(79));
        let bare = Node {
            source_file: "a.rs".into(),
            ..Default::default()
        };
        assert_eq!(anchor_line(&bare), None, "nothing to check against");
        let malformed = Node {
            source_file: "a.rs".into(),
            source_location: Some("not-a-line".into()),
            ..Default::default()
        };
        assert_eq!(anchor_line(&malformed), None);
    }

    /// A dbt model and a Blazor component are named by their file; no line
    /// inside ever spells the name, so line 1 is the only correct anchor.
    /// Accepting it anywhere else would stop the metric from catching an
    /// extractor that drops such a node at an arbitrary line.
    #[test]
    fn a_file_named_declaration_is_correct_only_at_line_one() {
        let lines =
            src("with orders as (\n    select * from stg_orders\n)\nselect * from orders\n");
        assert_eq!(
            resolve_anchor(&lines, 1, "customers", false, Some("customers")),
            Verdict::FileNamed
        );
        assert_eq!(
            resolve_anchor(&lines, 3, "customers", false, Some("customers")),
            Verdict::Wrong,
            "an arbitrary line is still a defect"
        );
        // A name that is not the file's stem gets no such allowance.
        assert_eq!(
            resolve_anchor(&lines, 1, "something_else", false, Some("customers")),
            Verdict::Wrong
        );
    }

    /// A filename is not always a legal identifier. `Z-Index.razor` declares the
    /// component `Z_Index`, and comparing the two literally reported a correct
    /// name as a wrong anchor.
    #[test]
    fn a_sanitized_file_name_still_counts_as_file_named() {
        let lines = src("<div>markup</div>\n");
        assert_eq!(
            resolve_anchor(&lines, 1, "Z_Index", false, Some("Z-Index")),
            Verdict::FileNamed
        );
        assert_eq!(
            resolve_anchor(&lines, 1, "_2Fa", false, Some("2fa")),
            Verdict::FileNamed,
            "a leading digit forces a prefix"
        );
        // Sanitization is not a licence to match a different name.
        assert_eq!(
            resolve_anchor(&lines, 1, "Unrelated", false, Some("Z-Index")),
            Verdict::Wrong
        );
    }

    /// The file node is identified by the extractor's own id derivation, not by
    /// a label heuristic; a symbol named after its file must still be checked.
    #[test]
    fn a_file_node_is_told_apart_from_a_symbol_sharing_its_name() {
        let file = file_node("src/Parser.rs");
        assert!(is_file_node(&file));
        let symbol = decl("Parser.rs", "src/Parser.rs", 1);
        assert!(!is_file_node(&symbol), "same label, different id");
    }

    #[test]
    fn empty_denominators_do_not_fabricate_a_score() {
        let empty = LanguageQuality::default();
        assert_eq!(empty.anchor_exactness(), 0.0);
        assert_eq!(empty.parse_error_rate(), 0.0);
        assert_eq!(empty.anchors_checked, 0, "the denominator is published too");
    }

    #[test]
    fn divergence_names_the_first_differing_node() {
        let a = (vec!["x".to_string()], vec![]);
        let b = (vec!["y".to_string()], vec![]);
        let d = first_divergence("determinism", &a, &b).expect("differs");
        assert!(d.contains("node sets differ"), "{d}");
        assert!(first_divergence("determinism", &a, &a).is_none());
    }

    #[test]
    fn divergence_names_differing_edges() {
        let e = ("a".to_string(), "b".to_string(), "calls".to_string());
        let a = (vec!["a".to_string(), "b".to_string()], vec![e.clone()]);
        let b = (vec!["a".to_string(), "b".to_string()], vec![]);
        let d = first_divergence("incremental", &a, &b).expect("differs");
        assert!(d.contains("edge sets differ"), "{d}");
    }
}
