//! Evolutionary coupling (co-change) mining: which files have historically
//! changed together with the files being changed now. This catches coupling that
//! static analysis misses (e.g. a schema and its serializer that share no import
//! but always change together). The git-log mining lives in the caller; this
//! module is the pure association-rule computation, so it is testable without git.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// A file that historically co-changes with the change under forecast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoChange {
    pub file: String,
    /// Commits where this file changed together with a changed file.
    pub support: usize,
    /// `support / (commits touching the changed files)`, as a 0..100 percent
    /// (an integer to keep the forecast comparable/Eq).
    pub confidence_pct: u8,
}

/// Options for [`co_change`].
#[derive(Debug, Clone)]
pub struct CoChangeOptions {
    /// Minimum co-change count to suggest a file (filters one-off coincidences).
    pub min_support: usize,
    /// Minimum confidence percent to suggest a file.
    pub min_confidence_pct: u8,
    /// Max suggestions returned.
    pub top: usize,
}

impl Default for CoChangeOptions {
    fn default() -> Self {
        CoChangeOptions {
            min_support: 2,
            min_confidence_pct: 30,
            top: 10,
        }
    }
}

fn norm(p: &str) -> String {
    p.replace('\\', "/")
}

/// Mine co-change suggestions from commit transactions (each a list of the files
/// one commit touched) for the current `changed` files. A file `B` is suggested
/// with support = the number of commits that touched both `B` and some changed
/// file, and confidence = support / (commits touching any changed file). The
/// changed files themselves are excluded. Deterministic: sorted by confidence,
/// then support, then path.
pub fn co_change(
    transactions: &[Vec<String>],
    changed: &[String],
    opts: &CoChangeOptions,
) -> Vec<CoChange> {
    let changed_set: HashSet<String> = changed.iter().map(|f| norm(f)).collect();
    let mut relevant_total = 0usize;
    let mut support: HashMap<String, usize> = HashMap::new();
    for tx in transactions {
        let files: HashSet<String> = tx.iter().map(|f| norm(f)).collect();
        if !files.iter().any(|f| changed_set.contains(f)) {
            continue; // commit untouched by the change
        }
        relevant_total += 1;
        for f in &files {
            if !changed_set.contains(f) {
                *support.entry(f.clone()).or_insert(0) += 1;
            }
        }
    }
    if relevant_total == 0 {
        return Vec::new();
    }

    let mut out: Vec<CoChange> = support
        .into_iter()
        .filter_map(|(file, sup)| {
            if sup < opts.min_support {
                return None;
            }
            let confidence_pct = ((sup as f32 / relevant_total as f32) * 100.0).round() as u8;
            (confidence_pct >= opts.min_confidence_pct).then_some(CoChange {
                file,
                support: sup,
                confidence_pct,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.confidence_pct
            .cmp(&a.confidence_pct)
            .then_with(|| b.support.cmp(&a.support))
            .then_with(|| a.file.cmp(&b.file))
    });
    out.truncate(opts.top);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(files: &[&str]) -> Vec<String> {
        files.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn suggests_strongly_coupled_files() {
        // a.py changed in 3 commits; b.py in all 3, c.py in 1.
        let transactions = vec![
            tx(&["a.py", "b.py"]),
            tx(&["a.py", "b.py"]),
            tx(&["a.py", "b.py", "c.py"]),
        ];
        let out = co_change(
            &transactions,
            &["a.py".to_string()],
            &CoChangeOptions::default(),
        );
        // b.py: support 3, confidence 100. c.py: support 1 -> below min_support 2.
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].file, "b.py");
        assert_eq!(out[0].support, 3);
        assert_eq!(out[0].confidence_pct, 100);
    }

    #[test]
    fn excludes_changed_files_and_unrelated_commits() {
        let transactions = vec![
            tx(&["a.py", "b.py"]),
            tx(&["a.py", "b.py"]),
            tx(&["x.py", "y.py"]), // touches no changed file -> ignored
        ];
        let out = co_change(
            &transactions,
            &["a.py".to_string()],
            &CoChangeOptions::default(),
        );
        let files: Vec<&str> = out.iter().map(|c| c.file.as_str()).collect();
        assert_eq!(files, vec!["b.py"]);
        assert!(
            !files.contains(&"a.py"),
            "the changed file is not its own suggestion"
        );
        assert!(!files.contains(&"x.py") && !files.contains(&"y.py"));
    }

    #[test]
    fn ranks_by_confidence_then_support_and_respects_thresholds() {
        // a.py in 4 commits. b.py co-changes 4/4 (100%), c.py 2/4 (50%), d.py 1/4.
        let transactions = vec![
            tx(&["a.py", "b.py", "c.py"]),
            tx(&["a.py", "b.py", "c.py"]),
            tx(&["a.py", "b.py"]),
            tx(&["a.py", "b.py", "d.py"]),
        ];
        let out = co_change(
            &transactions,
            &["a.py".to_string()],
            &CoChangeOptions::default(),
        );
        let files: Vec<&str> = out.iter().map(|c| c.file.as_str()).collect();
        // b (100%, sup 4) first, c (50%, sup 2) second; d (sup 1) filtered.
        assert_eq!(files, vec!["b.py", "c.py"]);
        assert_eq!(out[0].confidence_pct, 100);
        assert_eq!(out[1].confidence_pct, 50);
    }

    #[test]
    fn normalizes_windows_separators() {
        let transactions = vec![tx(&["src/a.py", "src/b.py"]), tx(&["src/a.py", "src/b.py"])];
        let out = co_change(
            &transactions,
            &["src\\a.py".to_string()],
            &CoChangeOptions::default(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file, "src/b.py");
    }
}
