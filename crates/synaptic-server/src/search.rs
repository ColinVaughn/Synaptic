//! Content (text) search across the federated source roots.
//!
//! `structural_search` matches the GRAPH (node kinds, loc, fan-in/out, symbol
//! names); by design it cannot see file CONTENT. This module is the complement:
//! real regex/literal search over the bytes of every source file, routed
//! through the same per-repo roots and containment jail as `get_source`. A hit
//! on a string literal, config value, TODO, or log line comes back with the
//! member repo and the graph-relative path, so the server can attribute it to
//! the node whose span encloses it -- turning a text match into a graph pivot.

use std::path::PathBuf;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};

/// One content match, before graph attribution.
#[derive(Debug, Clone)]
pub struct RawHit {
    /// Federated member tag the file belongs to (`None` for a single repo).
    pub repo: Option<String>,
    /// Path as the graph stores it: `tag/rel` for a federated file, else `rel`,
    /// always `/`-separated. The key the enclosing node is looked up by.
    pub graph_path: String,
    pub line: u64,
    pub col: u64,
    pub matched: String,
    pub line_text: String,
}

/// A source root to search, paired with the federation tag whose files live
/// under it (`None` for the single non-federated `--source-root`).
pub struct Root {
    pub tag: Option<String>,
    pub path: PathBuf,
}

/// What to search for and the bounds on the result.
pub struct Query<'a> {
    pub pattern: &'a str,
    pub literal: bool,
    /// `Some(true)`/`Some(false)` forces case sensitivity; `None` selects "smart
    /// case": case-insensitive unless `pattern` contains an uppercase letter, so
    /// `todo` stays broad while `TODO`/`FIXME` are precise (matching ripgrep -S).
    /// Smart case sharply cuts false positives like a lowercase "todos" matching
    /// `TODO` or a base64 blob matching `HACK`.
    pub case_sensitive: Option<bool>,
    pub path_glob: Option<&'a str>,
    pub max_results: usize,
    /// Longest line/match echoed back; longer ones are truncated so a minified
    /// or bundled line cannot blow up the response.
    pub max_line_len: usize,
}

/// The matches plus enough bookkeeping to render an honest summary.
pub struct Outcome {
    pub hits: Vec<RawHit>,
    pub files_scanned: usize,
    /// More matches existed than `max_results` returned.
    pub truncated: bool,
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("...");
    out
}

/// Run the search over every root, stopping once one more than `max_results`
/// matches have been collected (so truncation can be reported precisely). Hits
/// are returned in a stable order: repo, then path, then line/column.
pub fn run(roots: &[Root], q: &Query) -> Result<Outcome, String> {
    let pat = if q.literal {
        regex::escape(q.pattern)
    } else {
        q.pattern.to_string()
    };
    // Smart case when unspecified: insensitive only if the pattern has no
    // uppercase letter (so `todo` is broad, `TODO`/`FIXME` precise).
    let case_insensitive = match q.case_sensitive {
        Some(cs) => !cs,
        None => !q.pattern.chars().any(|c| c.is_ascii_uppercase()),
    };
    // The grep matcher drives the fast line scan; a plain regex (same pattern)
    // locates the match column on the few lines grep flags.
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(case_insensitive)
        .build(&pat)
        .map_err(|e| format!("invalid search pattern: {e}"))?;
    let col_re: Regex = RegexBuilder::new(&pat)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| format!("invalid search pattern: {e}"))?;

    let mut hits: Vec<RawHit> = Vec::new();
    let mut files_scanned = 0usize;
    // Collect up to one past the cap so `truncated` is exact, not a guess.
    let collect_cap = q.max_results.saturating_add(1);

    'roots: for root in roots {
        let mut wb = WalkBuilder::new(&root.path);
        wb.standard_filters(true)
            .require_git(false)
            .add_custom_ignore_filename(".synapticignore");
        // Never search Synaptic's own generated output, identified WITHOUT relying
        // on the dir's name so a custom `--out` (or a predecessor tool's dir) is
        // pruned too:
        //   1. the canonical `synaptic-out/` (by name), or
        //   2. the generated-file pair an extraction writes (`graph.json` +
        //      `.manifest.json`) -- requiring both keeps a genuine source/fixture
        //      file merely named `graph.json` searchable, or
        //   3. the AST cache it writes at `cache/ast/` -- the hash-keyed serialized
        //      ASTs hold extracted source text (e.g. an `addEventListener` literal),
        //      and this dir can exist with NO graph.json beside it (a cache-only or
        //      predecessor `codegraph-out/` layout) so rules 1-2 would miss it.
        // Pruning the dir also drops its exports (.dot/.svg/.graphml/...) and
        // graph.json.bak* backups, which would otherwise drown real source hits.
        wb.filter_entry(|dent| {
            if dent.depth() == 0 {
                return true; // never prune the search root itself
            }
            if dent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = dent.file_name().to_string_lossy();
                if name.eq_ignore_ascii_case("synaptic-out") {
                    return false;
                }
                let d = dent.path();
                if d.join("graph.json").is_file() && d.join(".manifest.json").is_file() {
                    return false;
                }
                if d.join("cache").join("ast").is_dir() {
                    return false;
                }
            }
            true
        });
        if let Some(glob) = q.path_glob {
            let mut ob = OverrideBuilder::new(&root.path);
            ob.add(glob)
                .map_err(|e| format!("invalid path_glob: {e}"))?;
            let ov = ob.build().map_err(|e| format!("invalid path_glob: {e}"))?;
            wb.overrides(ov);
        }

        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .build();

        for dent in wb.build() {
            let Ok(dent) = dent else { continue };
            if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = dent.path();
            let Ok(rel) = path.strip_prefix(&root.path) else {
                continue;
            };
            files_scanned += 1;
            let rel = rel.to_string_lossy().replace('\\', "/");
            let graph_path = match &root.tag {
                Some(t) => format!("{t}/{rel}"),
                None => rel,
            };

            let _ = searcher.search_path(
                &matcher,
                path,
                UTF8(|lnum, line| {
                    let (col, matched) = match col_re.find(line) {
                        Some(m) => (
                            line[..m.start()].chars().count() as u64 + 1,
                            truncate_chars(m.as_str(), q.max_line_len),
                        ),
                        // grep flagged the line but the column regex did not
                        // re-find it (line-terminator edge); still report it.
                        None => (1, truncate_chars(line.trim_end(), q.max_line_len)),
                    };
                    hits.push(RawHit {
                        repo: root.tag.clone(),
                        graph_path: graph_path.clone(),
                        line: lnum,
                        col,
                        matched,
                        line_text: truncate_chars(
                            line.trim_end_matches(['\n', '\r']),
                            q.max_line_len,
                        ),
                    });
                    // Stop this file (and below, the whole walk) at the cap.
                    Ok(hits.len() < collect_cap)
                }),
            );

            if hits.len() >= collect_cap {
                break 'roots;
            }
        }
    }

    hits.sort_by(|a, b| {
        a.repo
            .cmp(&b.repo)
            .then_with(|| a.graph_path.cmp(&b.graph_path))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.col.cmp(&b.col))
    });
    let truncated = hits.len() > q.max_results;
    hits.truncate(q.max_results);

    Ok(Outcome {
        hits,
        files_scanned,
        truncated,
    })
}
