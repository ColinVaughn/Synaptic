//! The corpus manifest must keep pace with the extractors.
//!
//! Quality is only measured on languages the corpus actually contains. The
//! 10-repository corpus this replaced reached 9 of Synaptic's 39 languages, so
//! 30 shipped extractors had no real repository behind them, and the bugs that
//! hid there (a case-sensitive extension match that routed `.F90` to no
//! extractor, a regex whose `^\s*` ate the blank line above a Pascal
//! declaration) were found by hand rather than by a benchmark.
//!
//! These tests make that structural: a language cannot ship without a repository
//! that exercises it, and a repository cannot sit in the manifest unpinned.

use std::collections::BTreeSet;

use synaptic_eval::repo_corpus::{CorpusManifest, LANGUAGES, SUITE_QUALITY, SUITE_SCALE};

fn manifest() -> CorpusManifest {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("repo-corpus.toml");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    CorpusManifest::parse(&src).expect("manifest parses")
}

/// The load-bearing guard: adding an extractor without adding a repository that
/// exercises it fails here rather than shipping unmeasured.
#[test]
fn every_extractor_is_benchmarked() {
    let m = manifest();
    let declared = m.declared_languages();
    let missing: Vec<&str> = LANGUAGES
        .iter()
        .map(|l| l.name)
        .filter(|name| !declared.contains(name))
        .collect();
    assert!(
        missing.is_empty(),
        "languages with no repository in repo-corpus.toml: {missing:?}"
    );
}

/// The reverse direction: a manifest entry may not invent a language name, or
/// the coverage guard above could be satisfied by a typo.
#[test]
fn declared_languages_all_exist() {
    let known: BTreeSet<&str> = LANGUAGES.iter().map(|l| l.name).collect();
    let m = manifest();
    let unknown: Vec<&str> = m
        .declared_languages()
        .into_iter()
        .filter(|n| !known.contains(n))
        .collect();
    assert!(unknown.is_empty(), "unknown language names: {unknown:?}");
}

/// Unreal package formats route to the resource indexer rather than to a
/// language extractor: they are binary, deliberately opaque, and emit a resource
/// node with no declarations. There is no anchor or parse health to score, so
/// they carry no language bucket by design rather than by omission.
const RESOURCE_ONLY_EXTENSIONS: &[&str] = &["uasset", "umap"];

/// Every extension the file-type detector calls code must route to an extractor
/// AND be attributable to a language bucket, or its nodes would be silently
/// excluded from every quality metric.
#[test]
fn every_code_extension_has_a_language_bucket() {
    let bucketed: BTreeSet<&str> = LANGUAGES
        .iter()
        .flat_map(|l| l.extensions)
        .copied()
        .collect();
    let orphans: Vec<&str> = synaptic_detect::file_type::CODE_EXTENSIONS
        .iter()
        .copied()
        .filter(|ext| !RESOURCE_ONLY_EXTENSIONS.contains(ext))
        .filter(|ext| synaptic_extract::extract_source(&format!("probe.{ext}"), b"\n").is_some())
        .filter(|ext| !bucketed.contains(ext))
        .collect();
    assert!(
        orphans.is_empty(),
        "extensions that extract but belong to no language bucket: {orphans:?}"
    );
}

/// A benchmark against a moving target measures nothing. Every entry must carry
/// a real 40-hex SHA, not a branch name and not the all-zero placeholder a new
/// entry is drafted with.
#[test]
fn every_repo_is_pinned_to_a_real_sha() {
    let m = manifest();
    let unpinned: Vec<&str> = m
        .repos
        .iter()
        .filter(|r| {
            r.sha.len() != 40
                || !r.sha.chars().all(|c| c.is_ascii_hexdigit())
                || r.sha.chars().all(|c| c == '0')
        })
        .map(|r| r.url.as_str())
        .collect();
    assert!(
        unpinned.is_empty(),
        "entries not pinned to a resolved SHA (run `synaptic eval quality --pin`): {unpinned:?}"
    );
}

#[test]
fn repository_urls_are_unique() {
    let m = manifest();
    let mut seen = BTreeSet::new();
    for r in &m.repos {
        assert!(seen.insert(r.url.as_str()), "duplicate entry: {}", r.url);
    }
}

/// The timing subset stays deliberately small; it is published and compared
/// across releases, so it must not drift upward as the quality corpus grows.
#[test]
fn the_scale_subset_stays_small_and_tiered() {
    let m = manifest();
    let scale: Vec<&str> = m.in_suite(SUITE_SCALE).map(|r| r.name()).collect();
    assert!(
        scale.len() <= 12,
        "scale suite grew to {} repos; published timings are compared across releases",
        scale.len()
    );
    for tier in ["small", "medium", "large"] {
        assert!(
            m.in_suite(SUITE_SCALE).any(|r| r.tier == tier),
            "scale suite has no {tier} repo"
        );
    }
}

/// Everything timed is also scored. A repository fast enough to publish but
/// never checked for correctness is the exact gap this suite exists to close.
#[test]
fn every_timed_repo_is_also_scored() {
    let m = manifest();
    let unscored: Vec<&str> = m
        .in_suite(SUITE_SCALE)
        .filter(|r| !r.in_suite(SUITE_QUALITY))
        .map(|r| r.name())
        .collect();
    assert!(unscored.is_empty(), "timed but never scored: {unscored:?}");
}

/// Language coverage is the point of the corpus, so assert its scale directly:
/// a future edit that guts the manifest should fail loudly here.
#[test]
fn corpus_covers_every_language_across_enough_repositories() {
    let m = manifest();
    assert!(
        m.repos.len() >= 45,
        "corpus shrank to {} repositories",
        m.repos.len()
    );
    assert_eq!(
        m.declared_languages().len(),
        LANGUAGES.len(),
        "every shipped language should appear in the corpus"
    );
}
