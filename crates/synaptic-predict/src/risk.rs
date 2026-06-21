//! A heuristic change-risk score from cheap, proven just-in-time defect-
//! prediction features (Kamei et al.): diffusion (how far the change spreads),
//! size (churn), and history (how often the touched code changes). The graph
//! supplies diffusion; git supplies size and history. The score is advisory and
//! UNCALIBRATED -- a later feedback loop is meant to calibrate the weights; until
//! then it ranks changes and, crucially, names *why* (actionability beats a bare
//! number).

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// Raw inputs to the risk score. The graph-derived fields are always available;
/// the git-derived fields (`lines_changed`, `recent_commits_touching`) are 0 when
/// git data was not gathered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RiskFactors {
    pub changed_files: usize,
    pub changed_nodes: usize,
    /// Dependents reached by reverse-impact (the blast radius size).
    pub blast_radius: usize,
    pub public_api_breaks: usize,
    /// Lines added + removed across the changed files (git churn); 0 if unknown.
    pub lines_changed: usize,
    /// Commits touching the changed files in recent history; 0 if unknown.
    pub recent_commits_touching: usize,
}

/// A scored change-risk assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskScore {
    /// 0 (low) .. 100 (high).
    pub score: u8,
    /// "low" | "medium" | "high".
    pub level: String,
    /// The features that drove the score, highest contribution first (the "why").
    pub factors: Vec<String>,
}

/// One weighted feature: a label, its raw value, the saturating cap, and weight.
struct Feature {
    label: &'static str,
    value: usize,
    cap: usize,
    weight: f32,
}

/// Score a change from its risk factors. Each feature saturates at a cap, is
/// weighted, and the weighted sum (in [0,1]) becomes a 0..100 score with low /
/// medium / high bands. `factors` lists the features that contributed most.
pub fn assess_risk(f: &RiskFactors) -> RiskScore {
    // Caps are where a feature stops adding risk; weights sum to 1.0. Diffusion
    // (blast radius) and public-API/size changes carry the most weight, matching
    // the JIT-defect-prediction literature. These are heuristic, not calibrated.
    let features = [
        Feature {
            label: "blast radius",
            value: f.blast_radius,
            cap: 100,
            weight: 0.30,
        },
        Feature {
            label: "public API changes",
            value: f.public_api_breaks,
            cap: 3,
            weight: 0.20,
        },
        Feature {
            label: "lines changed",
            value: f.lines_changed,
            cap: 400,
            weight: 0.20,
        },
        Feature {
            label: "changed files",
            value: f.changed_files,
            cap: 10,
            weight: 0.10,
        },
        Feature {
            label: "changed symbols",
            value: f.changed_nodes,
            cap: 30,
            weight: 0.10,
        },
        Feature {
            label: "recent churn (commits)",
            value: f.recent_commits_touching,
            cap: 20,
            weight: 0.10,
        },
    ];

    let mut total = 0.0f32;
    let mut contribs: Vec<(f32, String)> = Vec::new();
    for ft in &features {
        let saturation = (ft.value as f32 / ft.cap as f32).min(1.0);
        let contribution = saturation * ft.weight;
        total += contribution;
        // Name a feature as a driver when it adds a non-trivial slice of risk, so
        // a high-signal-but-low-count factor (e.g. one public-API break) still
        // shows up in the "why".
        if ft.value > 0 && contribution >= 0.04 {
            contribs.push((contribution, format!("{} ({})", ft.label, ft.value)));
        }
    }
    let total = total.clamp(0.0, 1.0);
    let score = (total * 100.0).round() as u8;
    let level = if score < 33 {
        "low"
    } else if score < 66 {
        "medium"
    } else {
        "high"
    };

    // Heaviest contributor first; tie-break by label for determinism.
    contribs.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    RiskScore {
        score,
        level: level.to_string(),
        factors: contribs.into_iter().map(|(_, s)| s).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivial_change_is_low_risk() {
        let r = assess_risk(&RiskFactors {
            changed_files: 1,
            changed_nodes: 1,
            blast_radius: 0,
            public_api_breaks: 0,
            lines_changed: 5,
            recent_commits_touching: 0,
        });
        assert_eq!(r.level, "low");
        assert!(r.score < 33, "score {} should be low", r.score);
    }

    #[test]
    fn large_widespread_change_is_high_risk_and_explains_why() {
        let r = assess_risk(&RiskFactors {
            changed_files: 12,
            changed_nodes: 40,
            blast_radius: 150,
            public_api_breaks: 6,
            lines_changed: 600,
            recent_commits_touching: 25,
        });
        assert_eq!(r.level, "high");
        assert!(r.score > 66, "score {} should be high", r.score);
        assert!(!r.factors.is_empty(), "high risk names its drivers");
        // Blast radius is the heaviest weight, so it should lead.
        assert!(
            r.factors[0].to_lowercase().contains("blast")
                || r.factors[0].to_lowercase().contains("dependent"),
            "dominant factor first: {:?}",
            r.factors
        );
    }

    #[test]
    fn a_single_public_api_break_is_named_as_a_driver() {
        // High-signal but low-count: one public-API change must still appear in
        // the "why" (it would not under a 0.5-saturation gate).
        let r = assess_risk(&RiskFactors {
            changed_files: 1,
            changed_nodes: 1,
            blast_radius: 0,
            public_api_breaks: 1,
            lines_changed: 0,
            recent_commits_touching: 0,
        });
        assert!(
            r.factors.iter().any(|s| s.contains("public API")),
            "public-API break should be a named driver: {:?}",
            r.factors
        );
    }

    #[test]
    fn more_blast_radius_never_lowers_the_score() {
        let base = RiskFactors {
            changed_files: 2,
            changed_nodes: 3,
            blast_radius: 5,
            public_api_breaks: 0,
            lines_changed: 20,
            recent_commits_touching: 1,
        };
        let mut more = base.clone();
        more.blast_radius = 80;
        assert!(assess_risk(&more).score >= assess_risk(&base).score);
    }
}
