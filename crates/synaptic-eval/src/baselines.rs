//! Pinned per-repository quality bounds, and the ratchet that moves them.
//!
//! Measuring quality without gating it only produces a number nobody reads until
//! it is already wrong. Each repository carries a floor on anchor exactness and
//! ceilings on the defect rates; a run that breaches one exits non-zero naming
//! the repository, the metric, and the delta.
//!
//! The ratchet is deliberately one-directional. `--update-baselines` will tighten
//! a bound to a better measurement without ceremony, but it **refuses to loosen**
//! one unless `--allow-regression` is passed as well. Without that asymmetry the
//! update flag would be a way for a bad run to rewrite its own passing grade,
//! and the gate would document regressions rather than prevent them.
//!
//! Determinism and incremental equivalence carry no baseline: they have a
//! principled correct answer, so they always hard-fail.

use serde::{Deserialize, Serialize};

use crate::quality::RepoQuality;

/// Bounds are stored to this many decimal places, and rounded outward, so a
/// measurement that is stable in exact arithmetic cannot fail on float noise.
const PLACES: f64 = 10_000.0;

fn floor4(x: f64) -> f64 {
    (x * PLACES).floor() / PLACES
}
fn ceil4(x: f64) -> f64 {
    (x * PLACES).ceil() / PLACES
}

/// The pinned bounds for one repository.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Baseline {
    pub name: String,
    /// Floor: measured anchor exactness must be at least this.
    pub anchor_exactness_min: f64,
    /// Ceiling: share of files whose grammar errored.
    pub parse_error_rate_max: f64,
    /// Ceiling: share of files that produced no declaration at all.
    pub zero_decl_file_rate_max: f64,
    /// Ceiling on the worst per-language oracle miss rate. Absent when the
    /// oracle was unavailable at pinning time, and never enforced against a run
    /// where the oracle did not execute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctags_missed_rate_max: Option<f64>,
}

/// The baselines file.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Baselines {
    #[serde(default, rename = "repo")]
    pub repos: Vec<Baseline>,
}

impl Baselines {
    pub fn parse(src: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(src)
    }

    pub fn render(&self) -> String {
        let header = "# Pinned quality bounds for `synaptic eval quality`.\n\
                      #\n\
                      # Tightened automatically by `--update-baselines`; loosening one also\n\
                      # requires `--allow-regression`, so a regression has to be an explicit\n\
                      # decision rather than a side effect of re-running the benchmark.\n\n";
        format!(
            "{header}{}",
            toml::to_string_pretty(self).unwrap_or_default()
        )
    }

    fn get(&self, name: &str) -> Option<&Baseline> {
        self.repos.iter().find(|b| b.name == name)
    }
}

/// One bound a run failed to hold.
#[derive(Debug, Clone, PartialEq)]
pub struct Breach {
    pub repo: String,
    pub metric: &'static str,
    pub measured: f64,
    pub bound: f64,
}

impl std::fmt::Display for Breach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} measured {:.4}, bound {:.4} (delta {:+.4})",
            self.repo,
            self.metric,
            self.measured,
            self.bound,
            self.measured - self.bound
        )
    }
}

/// Measured values for one repository, extracted from a result.
fn measured(r: &RepoQuality) -> (f64, f64, f64, Option<f64>) {
    (
        r.anchor_exactness(),
        r.parse_error_rate(),
        r.zero_decl_file_rate(),
        r.oracle.available.then(|| r.oracle.worst_missed_rate()),
    )
}

/// Every bound the results breach, plus the repositories that carry no baseline
/// yet. An unpinned repository is not a failure -- it has nothing to regress
/// against -- but it is reported so it does not stay unpinned by accident.
pub fn check(baselines: &Baselines, results: &[RepoQuality]) -> (Vec<Breach>, Vec<String>) {
    let mut breaches = Vec::new();
    let mut unpinned = Vec::new();
    for r in results {
        let Some(b) = baselines.get(&r.name) else {
            unpinned.push(r.name.clone());
            continue;
        };
        let (anchor, parse_err, zero_decl, missed) = measured(r);
        if anchor < b.anchor_exactness_min {
            breaches.push(Breach {
                repo: r.name.clone(),
                metric: "anchor_exactness",
                measured: anchor,
                bound: b.anchor_exactness_min,
            });
        }
        if parse_err > b.parse_error_rate_max {
            breaches.push(Breach {
                repo: r.name.clone(),
                metric: "parse_error_rate",
                measured: parse_err,
                bound: b.parse_error_rate_max,
            });
        }
        if zero_decl > b.zero_decl_file_rate_max {
            breaches.push(Breach {
                repo: r.name.clone(),
                metric: "zero_decl_file_rate",
                measured: zero_decl,
                bound: b.zero_decl_file_rate_max,
            });
        }
        // Only enforced when both the pin and this run have an oracle number;
        // a machine without ctags must not fail a gate it could not evaluate.
        if let (Some(bound), Some(m)) = (b.ctags_missed_rate_max, missed)
            && m > bound
        {
            breaches.push(Breach {
                repo: r.name.clone(),
                metric: "ctags_missed_rate",
                measured: m,
                bound,
            });
        }
    }
    (breaches, unpinned)
}

/// Produce the next baselines file from a run.
///
/// Returns `Err` listing every bound that would be loosened when
/// `allow_regression` is false, so the refusal names what it is protecting
/// rather than failing opaquely.
pub fn ratchet(
    existing: &Baselines,
    results: &[RepoQuality],
    allow_regression: bool,
) -> Result<Baselines, Vec<String>> {
    let mut out = existing.clone();
    let mut loosened = Vec::new();

    for r in results {
        let (anchor, parse_err, zero_decl, missed) = measured(r);
        let (anchor, parse_err, zero_decl) = (floor4(anchor), ceil4(parse_err), ceil4(zero_decl));
        let missed = missed.map(ceil4);

        match out.repos.iter_mut().find(|b| b.name == r.name) {
            None => out.repos.push(Baseline {
                name: r.name.clone(),
                anchor_exactness_min: anchor,
                parse_error_rate_max: parse_err,
                zero_decl_file_rate_max: zero_decl,
                ctags_missed_rate_max: missed,
            }),
            Some(b) => {
                let mut note = |metric: &str, from: f64, to: f64| {
                    loosened.push(format!(
                        "{}: {metric} would loosen {from:.4} -> {to:.4}",
                        r.name
                    ));
                };
                if anchor < b.anchor_exactness_min {
                    note("anchor_exactness_min", b.anchor_exactness_min, anchor);
                    if allow_regression {
                        b.anchor_exactness_min = anchor;
                    }
                } else {
                    b.anchor_exactness_min = anchor;
                }
                if parse_err > b.parse_error_rate_max {
                    note("parse_error_rate_max", b.parse_error_rate_max, parse_err);
                    if allow_regression {
                        b.parse_error_rate_max = parse_err;
                    }
                } else {
                    b.parse_error_rate_max = parse_err;
                }
                if zero_decl > b.zero_decl_file_rate_max {
                    note(
                        "zero_decl_file_rate_max",
                        b.zero_decl_file_rate_max,
                        zero_decl,
                    );
                    if allow_regression {
                        b.zero_decl_file_rate_max = zero_decl;
                    }
                } else {
                    b.zero_decl_file_rate_max = zero_decl;
                }
                // A run without an oracle leaves any existing pin untouched
                // rather than erasing it.
                if let Some(m) = missed {
                    match b.ctags_missed_rate_max {
                        Some(prev) if m > prev => {
                            note("ctags_missed_rate_max", prev, m);
                            if allow_regression {
                                b.ctags_missed_rate_max = Some(m);
                            }
                        }
                        _ => b.ctags_missed_rate_max = Some(m),
                    }
                }
            }
        }
    }

    out.repos.sort_by(|a, b| a.name.cmp(&b.name));
    if loosened.is_empty() || allow_regression {
        Ok(out)
    } else {
        Err(loosened)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::{OracleLanguage, OracleOutcome};
    use crate::quality::Consistency;

    fn result(name: &str, anchors_exact: usize, anchors_checked: usize) -> RepoQuality {
        RepoQuality {
            name: name.to_string(),
            url: format!("https://example.com/{name}"),
            sha: "deadbeef".into(),
            family: "test".into(),
            declared_languages: vec!["rust".into()],
            files: 10,
            nodes: 20,
            edges: 5,
            anchors_checked,
            anchors_exact,
            recovered_nodes: 0,
            parse_error_files: 0,
            zero_decl_files: 0,
            per_language: vec![],
            consistency: Consistency {
                deterministic: true,
                incremental_equivalent: true,
                detail: None,
            },
            oracle: OracleOutcome::default(),
        }
    }

    #[test]
    fn a_measurement_below_the_floor_is_a_breach() {
        let b = Baselines {
            repos: vec![Baseline {
                name: "memchr".into(),
                anchor_exactness_min: 1.0,
                parse_error_rate_max: 0.0,
                zero_decl_file_rate_max: 0.0,
                ctags_missed_rate_max: None,
            }],
        };
        let (breaches, unpinned) = check(&b, &[result("memchr", 9, 10)]);
        assert!(unpinned.is_empty());
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].metric, "anchor_exactness");
        assert!(breaches[0].to_string().contains("memchr"));
    }

    #[test]
    fn a_repo_with_no_baseline_is_reported_not_failed() {
        let (breaches, unpinned) = check(&Baselines::default(), &[result("newrepo", 1, 1)]);
        assert!(breaches.is_empty());
        assert_eq!(unpinned, vec!["newrepo"]);
    }

    #[test]
    fn ratchet_tightens_an_improved_bound() {
        let existing = Baselines {
            repos: vec![Baseline {
                name: "memchr".into(),
                anchor_exactness_min: 0.9,
                parse_error_rate_max: 0.5,
                zero_decl_file_rate_max: 0.5,
                ctags_missed_rate_max: None,
            }],
        };
        let next = ratchet(&existing, &[result("memchr", 10, 10)], false).expect("tightening");
        assert_eq!(next.repos[0].anchor_exactness_min, 1.0);
        assert_eq!(next.repos[0].parse_error_rate_max, 0.0);
    }

    /// The core guarantee: re-running the benchmark on a regression must not be
    /// able to quietly bless it.
    #[test]
    fn ratchet_refuses_to_loosen_without_the_flag() {
        let existing = Baselines {
            repos: vec![Baseline {
                name: "memchr".into(),
                anchor_exactness_min: 1.0,
                parse_error_rate_max: 0.0,
                zero_decl_file_rate_max: 0.0,
                ctags_missed_rate_max: None,
            }],
        };
        let err = ratchet(&existing, &[result("memchr", 8, 10)], false).unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].contains("anchor_exactness_min"), "{err:?}");
        assert!(err[0].contains("1.0000 -> 0.8000"), "{err:?}");

        let forced = ratchet(&existing, &[result("memchr", 8, 10)], true).expect("forced");
        assert_eq!(forced.repos[0].anchor_exactness_min, 0.8);
    }

    #[test]
    fn ratchet_adds_an_unpinned_repo() {
        let next = ratchet(&Baselines::default(), &[result("fresh", 7, 10)], false).unwrap();
        assert_eq!(next.repos.len(), 1);
        assert_eq!(next.repos[0].name, "fresh");
        assert_eq!(next.repos[0].anchor_exactness_min, 0.7);
    }

    /// A machine without ctags must neither fail the oracle gate nor erase a pin
    /// that a machine with ctags established.
    #[test]
    fn an_absent_oracle_neither_gates_nor_erases() {
        let existing = Baselines {
            repos: vec![Baseline {
                name: "memchr".into(),
                anchor_exactness_min: 1.0,
                parse_error_rate_max: 0.0,
                zero_decl_file_rate_max: 0.0,
                ctags_missed_rate_max: Some(0.1),
            }],
        };
        let r = result("memchr", 10, 10); // oracle unavailable by default
        let (breaches, _) = check(&existing, std::slice::from_ref(&r));
        assert!(breaches.is_empty(), "{breaches:?}");
        let next = ratchet(&existing, &[r], false).unwrap();
        assert_eq!(next.repos[0].ctags_missed_rate_max, Some(0.1));
    }

    #[test]
    fn an_oracle_regression_is_gated_when_both_sides_have_a_number() {
        let existing = Baselines {
            repos: vec![Baseline {
                name: "memchr".into(),
                anchor_exactness_min: 0.0,
                parse_error_rate_max: 1.0,
                zero_decl_file_rate_max: 1.0,
                ctags_missed_rate_max: Some(0.1),
            }],
        };
        let mut r = result("memchr", 10, 10);
        r.oracle = OracleOutcome {
            available: true,
            reason: None,
            per_language: vec![OracleLanguage {
                language: "rust".into(),
                agreement: 5,
                ctags_only: 5,
                ..Default::default()
            }],
            malformed_tag_lines: 0,
        };
        let (breaches, _) = check(&existing, &[r]);
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].metric, "ctags_missed_rate");
    }

    #[test]
    fn bounds_round_outward_so_exact_reruns_hold() {
        let next = ratchet(&Baselines::default(), &[result("r", 2, 3)], false).unwrap();
        // 2/3 floors to 0.6666, strictly below the measured value.
        assert_eq!(next.repos[0].anchor_exactness_min, 0.6666);
        let (breaches, _) = check(&next, &[result("r", 2, 3)]);
        assert!(breaches.is_empty(), "a rerun of the same pin must hold");
    }

    #[test]
    fn baselines_round_trip_through_toml() {
        let b = ratchet(&Baselines::default(), &[result("r", 3, 4)], false).unwrap();
        let parsed = Baselines::parse(&b.render()).expect("round trip");
        assert_eq!(parsed, b);
    }
}
