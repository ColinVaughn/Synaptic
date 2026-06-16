//! Recover concrete edit sites for a rename: the definition plus every resolved
//! reference. Call references get a column-accurate span from the AST cache
//! (`RawCall.span`); other references (inherits/implements/uses/...) use
//! the edge's line and are flagged for the agent to locate the exact token.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use codegraph_core::{Confidence, NodeId, Span};
use codegraph_extract::cached_extract_source;
use codegraph_graph::{norm_source_file, KnowledgeGraph};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::resolve::{normalize, Candidate};

/// Cap on a referencing file we will read+parse for column recovery. Past this we
/// skip the file so a pathological/generated file can't stall a plan.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Relations whose reference is a call (recoverable column-accurately from a RawCall).
const CALL_RELATIONS: &[&str] = &["calls"];

/// Relations whose source carries a textual occurrence of the name. A containment
/// edge ("contains"/"declares") is deliberately excluded: it is not a name use.
pub(crate) const REF_RELATIONS: &[&str] = &[
    "calls",
    "references",
    "inherits",
    "extends",
    "implements",
    "uses",
    "mixes_in",
    "embeds",
    "depends_on",
    "imports",
    "imports_from",
    "re_exports",
];

/// One place the agent must edit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EditSite {
    pub file: String,
    /// Column-accurate range when recovered from a RawCall; else `None`.
    pub span: Option<Span>,
    /// Best-known line when `span` is `None`.
    pub line: Option<u32>,
    pub old: String,
    pub new: String,
    pub confidence: Confidence,
    pub reason: String,
    pub needs_review: bool,
    /// Federation member this site lives in, when the graph is multi-repo.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repo: Option<String>,
}

/// One step less trustworthy: an EXTRACTED fact whose target is uncertain (an
/// ambiguous name) is at best INFERRED.
fn downgrade(c: Confidence) -> Confidence {
    match c {
        Confidence::Extracted => Confidence::Inferred,
        other => other,
    }
}

/// Pull the first integer out of a `source_location` like "L42" or "42:7".
fn line_of(loc: &Option<String>) -> Option<u32> {
    loc.as_ref().and_then(|s| {
        s.trim_start_matches('L')
            .split(|c: char| !c.is_ascii_digit())
            .find(|d| !d.is_empty())
            .and_then(|d| d.parse().ok())
    })
}

/// Find the first whole-word occurrence of the rename target on a 1-based line of
/// a file, returning its precise single-line span and whether it is a member
/// access (`recv.name`). `lines` caches each file's content (None = unreadable or
/// over the size cap) so a file is read at most once. This recovers a name-token
/// column for sites that the graph only knows by line (same-file calls) or by a
/// coarse whole-definition span.
fn span_on_line(
    root: &Path,
    file: &str,
    line_no: u32,
    re: &Regex,
    lines: &mut HashMap<String, Option<Vec<String>>>,
) -> Option<(Span, bool)> {
    let entry = lines.entry(file.to_string()).or_insert_with(|| {
        let abs = root.join(file);
        match std::fs::metadata(&abs) {
            Ok(m) if m.len() <= MAX_FILE_BYTES => std::fs::read_to_string(&abs)
                .ok()
                .map(|t| t.lines().map(|l| l.to_string()).collect()),
            _ => None,
        }
    });
    let line = entry.as_ref()?.get((line_no as usize).checked_sub(1)?)?;
    let m = re.find(line)?;
    // A member call (`recv.name()`) renames the same token, but its receiver type
    // is uncertain; the caller keeps it flagged. ASCII byte offsets are char-safe.
    let member = line[..m.start()].trim_end().ends_with('.');
    let span = Span {
        start_line: line_no,
        start_col: (m.start() as u32) + 1,
        end_line: line_no,
        end_col: (m.end() as u32) + 1,
    };
    Some((span, member))
}

/// Build the edit-site list. `root` is the repo root used to read referencing
/// files' current bytes; `cache_dir` is `<root>/codegraph-out/cache` (or `None`
/// to extract fresh). `name_ambiguous` is true when several definitions share the
/// old name, which lowers every by-name call-site match.
pub fn recover_sites(
    kg: &KnowledgeGraph,
    target: &Candidate,
    old_name: &str,
    new_name: &str,
    name_ambiguous: bool,
    root: &Path,
    cache_dir: Option<&Path>,
) -> Vec<EditSite> {
    let target_id = NodeId(target.id.clone());
    let mut sites: Vec<EditSite> = Vec::new();

    // 1. Definition site (rename the declaration itself).
    sites.push(EditSite {
        file: target.file.clone(),
        span: target.span,
        line: target.span.map(|s| s.start_line),
        old: old_name.to_string(),
        new: new_name.to_string(),
        confidence: Confidence::Extracted,
        reason: "definition".to_string(),
        needs_review: target.span.is_none(),
        repo: kg.node(&target_id).and_then(|n| n.repo.clone()),
    });

    // 2. Incoming structural edges: collect the referencing files and, for calls,
    //    the resolver's per-(file, caller) confidence + line so we can score raw
    //    calls and recover same-file calls the extractor resolved without a RawCall.
    let want = normalize(old_name);
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut call_edges: HashMap<(String, NodeId), (Confidence, Option<u32>)> = HashMap::new();
    for e in kg.incident_edges(&target_id) {
        if e.target != target_id {
            continue; // incoming references only
        }
        if !REF_RELATIONS.contains(&e.relation.as_str()) {
            continue;
        }
        // Edge source_file is not normalized at build time; normalize it so it
        // matches node paths (forward-slash) for grouping/dedup/cache keying.
        let sf = norm_source_file(&e.source_file, None);
        files.insert(sf.clone());
        if CALL_RELATIONS.contains(&e.relation.as_str()) {
            call_edges.insert(
                (sf, e.source.clone()),
                (e.confidence, line_of(&e.source_location)),
            );
        } else {
            // Non-call reference: column unknown, the agent locates the token.
            sites.push(EditSite {
                file: sf,
                span: None,
                line: line_of(&e.source_location),
                old: old_name.to_string(),
                new: new_name.to_string(),
                confidence: e.confidence,
                reason: format!("{} reference", e.relation),
                needs_review: true,
                repo: kg.node(&e.source).and_then(|n| n.repo.clone()),
            });
        }
    }

    // 3. Column-accurate call sites from the AST cache. Re-extract each
    //    referencing file (cache hit when warm; fresh + cached on miss). Track
    //    which call edges a RawCall covered, so same-file calls the extractor
    //    resolved into edges (no RawCall) can be recovered afterward.
    let mut covered: HashSet<(String, NodeId)> = HashSet::new();
    for file in &files {
        let abs = root.join(file);
        match std::fs::metadata(&abs) {
            Ok(m) if m.len() > MAX_FILE_BYTES => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        // The AST cache is keyed on the OS-native relative path used by extract;
        // reconstruct it from the normalized form so a warm cache actually hits.
        let key_path = file.replace('/', std::path::MAIN_SEPARATOR_STR);
        let Some(res) = cached_extract_source(cache_dir, &key_path, &bytes) else {
            continue;
        };
        for rc in &res.raw_calls {
            if normalize(&rc.callee) != want {
                continue;
            }
            let key = (file.clone(), rc.caller.clone());
            let conf = call_edges.get(&key).map(|(c, _)| *c);
            let (confidence, review) = match (conf, name_ambiguous) {
                // Resolved to this target and the name is unique: trust the edge.
                (Some(c), false) => (c, c != Confidence::Extracted || rc.span.is_none()),
                // Resolved, but the name is shared across definitions: downgrade.
                (Some(c), true) => (downgrade(c), true),
                // Name matched a call but no edge tied it to this target. When the
                // name is ambiguous this match likely belongs to another same-named
                // symbol, so skip it rather than point the agent at the wrong token.
                (None, true) => continue,
                (None, false) => (Confidence::Inferred, true),
            };
            if conf.is_some() {
                covered.insert(key);
            }
            // The recorded span covers the whole call expression, so for a member
            // call (`recv.method()`) its start column is the receiver, not the
            // callee token. Don't advertise a wrong column: drop to line-only and
            // let the agent (or the textual scan) locate the exact token.
            let span = if rc.is_member_call { None } else { rc.span };
            let needs_review = review || rc.is_member_call;
            sites.push(EditSite {
                file: file.clone(),
                span,
                line: span
                    .map(|s| s.start_line)
                    .or_else(|| line_of(&rc.source_location)),
                old: old_name.to_string(),
                new: new_name.to_string(),
                confidence,
                reason: "call site".to_string(),
                needs_review,
                repo: kg.node(&rc.caller).and_then(|n| n.repo.clone()),
            });
        }
    }

    // 4. Resolved call edges with no matching RawCall: same-file calls the
    //    extractor resolved at extraction time (no RawCall is emitted for them).
    //    Emit a line-only site so a same-file reference is never silently missed.
    for ((file, caller), (conf, line)) in &call_edges {
        if covered.contains(&(file.clone(), caller.clone())) {
            continue;
        }
        let confidence = if name_ambiguous {
            downgrade(*conf)
        } else {
            *conf
        };
        sites.push(EditSite {
            file: file.clone(),
            span: None,
            line: *line,
            old: old_name.to_string(),
            new: new_name.to_string(),
            confidence,
            reason: "call site".to_string(),
            needs_review: true,
            repo: kg.node(caller).and_then(|n| n.repo.clone()),
        });
    }

    // 5. Backfill a precise name-token column for sites the graph only knows by
    //    line (same-file calls, member calls, non-call references) or by a coarse
    //    whole-definition span. A precise column lets these dedup against the
    //    textual scan (no double-listing) and lets a trustworthy same-file *direct*
    //    call leave `review` -- it was only flagged for want of a locator.
    if let Ok(re) = Regex::new(&format!(r"\b{}\b", regex::escape(old_name))) {
        let mut lines: HashMap<String, Option<Vec<String>>> = HashMap::new();
        for s in &mut sites {
            let coarse = match s.span {
                None => true,
                Some(sp) => sp.start_line != sp.end_line,
            };
            if !coarse {
                continue;
            }
            let Some(line_no) = s.line else { continue };
            let Some((span, member)) = span_on_line(root, &s.file, line_no, &re, &mut lines) else {
                continue;
            };
            s.span = Some(span);
            // Promote only a same-file direct call: a member call's receiver type
            // is uncertain and a non-call reference's token may sit in a comment.
            if s.reason == "call site" && !member && s.confidence == Confidence::Extracted {
                s.needs_review = false;
            }
        }
    }

    sites.sort_by(|a, b| {
        (a.file.as_str(), a.line, a.reason.as_str()).cmp(&(
            b.file.as_str(),
            b.line,
            b.reason.as_str(),
        ))
    });
    sites.dedup_by(|a, b| {
        a.file == b.file && a.span == b.span && a.line == b.line && a.reason == b.reason
    });
    sites
}

/// Textual fallback: whole-word occurrences of `old_name` across the files
/// CodeGraph indexed. This surfaces references the conservative graph does not
/// record as edges (type annotations, `Enum::Variant` paths, generics). Every
/// site is flagged `needs_review` since a textual match can land in a comment or
/// string. Returns at most `max_sites` (sorted, deterministic).
pub fn text_scan(
    kg: &KnowledgeGraph,
    old_name: &str,
    new_name: &str,
    name_ambiguous: bool,
    root: &Path,
    max_sites: usize,
) -> Vec<EditSite> {
    let Ok(re) = Regex::new(&format!(r"\b{}\b", regex::escape(old_name))) else {
        return Vec::new();
    };
    // Distinct indexed files, normalized + sorted for determinism, with the repo
    // tag of a node in each (for cross-repo annotation).
    let mut repo_of: HashMap<String, Option<String>> = HashMap::new();
    let files: BTreeSet<String> = kg
        .nodes()
        .filter(|n| !n.source_file.is_empty())
        .map(|n| {
            let f = norm_source_file(&n.source_file, None);
            repo_of.entry(f.clone()).or_insert_with(|| n.repo.clone());
            f
        })
        .collect();

    let confidence = if name_ambiguous {
        Confidence::Ambiguous
    } else {
        Confidence::Inferred
    };
    let mut sites = Vec::new();
    'files: for file in &files {
        let abs = root.join(file);
        match std::fs::metadata(&abs) {
            Ok(m) if m.len() > MAX_FILE_BYTES => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let Ok(text) = std::fs::read_to_string(&abs) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            for m in re.find_iter(line) {
                if sites.len() >= max_sites {
                    break 'files;
                }
                // 1-based line + column (byte offset; ASCII identifiers => char col).
                let start_line = (i as u32) + 1;
                let start_col = (m.start() as u32) + 1;
                let end_col = (m.end() as u32) + 1;
                sites.push(EditSite {
                    file: file.clone(),
                    span: Some(Span {
                        start_line,
                        start_col,
                        end_line: start_line,
                        end_col,
                    }),
                    line: Some(start_line),
                    old: old_name.to_string(),
                    new: new_name.to_string(),
                    confidence,
                    reason: "textual reference".to_string(),
                    needs_review: true,
                    repo: repo_of.get(file).cloned().flatten(),
                });
            }
        }
    }
    sites
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_of_parses_common_forms() {
        assert_eq!(line_of(&Some("L42".into())), Some(42));
        assert_eq!(line_of(&Some("42:7".into())), Some(42));
        assert_eq!(line_of(&Some("L10-L20".into())), Some(10));
        assert_eq!(line_of(&None), None);
        assert_eq!(line_of(&Some("none".into())), None);
    }

    #[test]
    fn downgrade_only_lowers_extracted() {
        assert_eq!(downgrade(Confidence::Extracted), Confidence::Inferred);
        assert_eq!(downgrade(Confidence::Inferred), Confidence::Inferred);
        assert_eq!(downgrade(Confidence::Ambiguous), Confidence::Ambiguous);
    }
}
