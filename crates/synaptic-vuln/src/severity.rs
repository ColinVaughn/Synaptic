use serde::{Deserialize, Serialize};
use synaptic_api::ApplicabilityState;

use crate::advisory::{Advisory, SeverityKind};

/// Qualitative severity band, using the CVSS v3.1 rating scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeverityBand {
    /// The advisory supplies no score this build can evaluate. Distinct from
    /// `None`, which is a scored result of zero.
    Unknown,
    None,
    Low,
    Medium,
    High,
    Critical,
}

/// Where a severity assessment's number came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeverityScoreSource {
    /// Computed from a CVSS v3.x base vector.
    CvssV3Vector,
    /// A CVSS v4.0 vector was present but this build does not compute v4 base
    /// scores. The vector is retained rather than discarded or guessed at.
    CvssV4VectorUnscored,
    /// The advisory carried no severity entry at all.
    Absent,
}

/// A severity conclusion together with the evidence behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeverityAssessment {
    pub band: SeverityBand,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub base_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub vector: Option<String>,
    pub source: SeverityScoreSource,
}

/// Remediation urgency. Published together with every input that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

/// Everything that feeds the priority ordinal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriorityInputs {
    pub severity: SeverityBand,
    pub applicability: ApplicabilityState,
    /// False when the package is reached only through development, build, or
    /// test dependencies.
    pub runtime_reachable: bool,
}

/// Read the best severity the advisory offers.
///
/// A scorable CVSS v3 vector is preferred over a v4 vector, because this build
/// can turn the former into a number. The v4 vector is still retained when it
/// is all that exists: recording "we have a vector we cannot score" is more
/// useful than recording nothing.
pub fn assess_severity(advisory: &Advisory) -> SeverityAssessment {
    for entry in &advisory.severity {
        if entry.kind == SeverityKind::CvssV3 {
            if let Some(score) = cvss_v3_base_score(&entry.score) {
                return SeverityAssessment {
                    band: band_for_score(score),
                    base_score: Some(score),
                    vector: Some(entry.score.clone()),
                    source: SeverityScoreSource::CvssV3Vector,
                };
            }
        }
    }
    for entry in &advisory.severity {
        if entry.kind == SeverityKind::CvssV4 {
            return SeverityAssessment {
                band: SeverityBand::Unknown,
                base_score: None,
                vector: Some(entry.score.clone()),
                source: SeverityScoreSource::CvssV4VectorUnscored,
            };
        }
    }
    SeverityAssessment {
        band: SeverityBand::Unknown,
        base_score: None,
        vector: advisory.severity.first().map(|entry| entry.score.clone()),
        source: SeverityScoreSource::Absent,
    }
}

/// Compute a CVSS v3.x base score from its vector string.
///
/// This is the published v3.1 base-score formula. v3.0 and v3.1 differ only in
/// the rounding helper's definition, and the v3.1 definition is used here
/// because it is the one that rounds reproducibly in binary floating point.
pub fn cvss_v3_base_score(vector: &str) -> Option<f64> {
    let mut metrics = std::collections::BTreeMap::new();
    for component in vector.trim().split('/') {
        if let Some((key, value)) = component.split_once(':') {
            metrics.insert(key.trim(), value.trim());
        }
    }

    let scope_changed = match *metrics.get("S")? {
        "U" => false,
        "C" => true,
        _ => return None,
    };

    let attack_vector = match *metrics.get("AV")? {
        "N" => 0.85,
        "A" => 0.62,
        "L" => 0.55,
        "P" => 0.2,
        _ => return None,
    };
    let attack_complexity = match *metrics.get("AC")? {
        "L" => 0.77,
        "H" => 0.44,
        _ => return None,
    };
    // Privileges Required is the one metric whose weight depends on scope.
    let privileges_required = match (*metrics.get("PR")?, scope_changed) {
        ("N", _) => 0.85,
        ("L", false) => 0.62,
        ("L", true) => 0.68,
        ("H", false) => 0.27,
        ("H", true) => 0.5,
        _ => return None,
    };
    let user_interaction = match *metrics.get("UI")? {
        "N" => 0.85,
        "R" => 0.62,
        _ => return None,
    };

    let impact_metric = |key: &str| -> Option<f64> {
        match *metrics.get(key)? {
            "H" => Some(0.56),
            "L" => Some(0.22),
            "N" => Some(0.0),
            _ => None,
        }
    };
    let confidentiality = impact_metric("C")?;
    let integrity = impact_metric("I")?;
    let availability = impact_metric("A")?;

    let impact_base = 1.0 - ((1.0 - confidentiality) * (1.0 - integrity) * (1.0 - availability));
    let impact = if scope_changed {
        7.52 * (impact_base - 0.029) - 3.25 * (impact_base - 0.02).powi(15)
    } else {
        6.42 * impact_base
    };
    if impact <= 0.0 {
        return Some(0.0);
    }

    let exploitability =
        8.22 * attack_vector * attack_complexity * privileges_required * user_interaction;
    let combined = if scope_changed {
        1.08 * (impact + exploitability)
    } else {
        impact + exploitability
    };

    Some(roundup(combined.min(10.0)))
}

/// The CVSS v3.1 `Roundup` helper: round up to one decimal place, defined over
/// integer arithmetic so that values such as 4.02 do not round to 4.1 through
/// floating-point representation error.
fn roundup(input: f64) -> f64 {
    let scaled = (input * 100_000.0).round() as i64;
    if scaled % 10_000 == 0 {
        scaled as f64 / 100_000.0
    } else {
        ((scaled as f64 / 10_000.0).floor() + 1.0) / 10.0
    }
}

/// Map a numeric base score onto the CVSS qualitative scale.
pub fn band_for_score(score: f64) -> SeverityBand {
    if score <= 0.0 {
        SeverityBand::None
    } else if score < 4.0 {
        SeverityBand::Low
    } else if score < 7.0 {
        SeverityBand::Medium
    } else if score < 9.0 {
        SeverityBand::High
    } else {
        SeverityBand::Critical
    }
}

/// Combine severity, applicability, and runtime reachability into an ordinal.
///
/// Fix availability is deliberately absent: whether an upstream patch exists
/// changes the remediation path, not how urgently the risk must be handled.
/// It is recorded on the finding instead of being folded into this number.
pub fn prioritize(inputs: &PriorityInputs) -> Priority {
    if inputs.applicability == ApplicabilityState::NotApplicable {
        return Priority::P3;
    }

    // Unknown severity is treated as Medium: an advisory nobody scored is not
    // thereby harmless, and defaulting it to Low would hide it.
    let base = match inputs.severity {
        SeverityBand::Critical | SeverityBand::High => 0,
        SeverityBand::Medium | SeverityBand::Unknown => 1,
        SeverityBand::Low | SeverityBand::None => 2,
    };
    let review_penalty = usize::from(inputs.applicability == ApplicabilityState::ReviewRequired);
    let scope_penalty = usize::from(!inputs.runtime_reachable);

    match base + review_penalty + scope_penalty {
        0 => Priority::P0,
        1 => Priority::P1,
        2 => Priority::P2,
        _ => Priority::P3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advisory_with_severity(kind: &str, score: &str) -> Advisory {
        let source = format!(
            r#"{{ "id": "OSV-X", "severity": [{{ "type": "{kind}", "score": "{score}" }}] }}"#
        );
        Advisory::parse(&source).unwrap()
    }

    #[test]
    fn scores_the_canonical_worst_case_vector_as_ten_when_scope_changes() {
        let score =
            cvss_v3_base_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H").expect("valid");

        assert!((score - 10.0).abs() < 1e-9, "expected 10.0, got {score}");
    }

    #[test]
    fn scores_an_unchanged_scope_full_impact_vector_as_nine_point_eight() {
        let score =
            cvss_v3_base_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H").expect("valid");

        assert!((score - 9.8).abs() < 1e-9, "expected 9.8, got {score}");
    }

    #[test]
    fn scores_a_hard_to_reach_low_impact_vector_as_three_point_eight() {
        let score =
            cvss_v3_base_score("CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:L/I:L/A:L").expect("valid");

        assert!((score - 3.8).abs() < 1e-9, "expected 3.8, got {score}");
    }

    #[test]
    fn a_vector_with_no_impact_scores_zero() {
        let score =
            cvss_v3_base_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N").expect("valid");

        assert!((score - 0.0).abs() < 1e-9, "expected 0.0, got {score}");
    }

    #[test]
    fn version_three_zero_vectors_use_the_same_base_formula() {
        let score =
            cvss_v3_base_score("CVSS:3.0/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H").expect("valid");

        assert!((score - 9.8).abs() < 1e-9, "expected 9.8, got {score}");
    }

    #[test]
    fn a_vector_missing_a_required_metric_does_not_score() {
        assert_eq!(cvss_v3_base_score("CVSS:3.1/AV:N/AC:L/PR:N"), None);
    }

    #[test]
    fn a_vector_with_an_unknown_metric_value_does_not_score() {
        assert_eq!(
            cvss_v3_base_score("CVSS:3.1/AV:Z/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"),
            None
        );
    }

    #[test]
    fn bands_follow_the_published_cvss_rating_scale() {
        assert_eq!(band_for_score(0.0), SeverityBand::None);
        assert_eq!(band_for_score(0.1), SeverityBand::Low);
        assert_eq!(band_for_score(3.9), SeverityBand::Low);
        assert_eq!(band_for_score(4.0), SeverityBand::Medium);
        assert_eq!(band_for_score(6.9), SeverityBand::Medium);
        assert_eq!(band_for_score(7.0), SeverityBand::High);
        assert_eq!(band_for_score(8.9), SeverityBand::High);
        assert_eq!(band_for_score(9.0), SeverityBand::Critical);
        assert_eq!(band_for_score(10.0), SeverityBand::Critical);
    }

    #[test]
    fn assesses_a_cvss_v3_advisory_from_its_vector() {
        let advisory =
            advisory_with_severity("CVSS_V3", "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H");

        let assessment = assess_severity(&advisory);

        assert_eq!(assessment.band, SeverityBand::Critical);
        assert_eq!(assessment.source, SeverityScoreSource::CvssV3Vector);
        assert!((assessment.base_score.unwrap() - 9.8).abs() < 1e-9);
    }

    #[test]
    fn retains_a_v4_vector_without_inventing_a_score_for_it() {
        let advisory = advisory_with_severity("CVSS_V4", "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N");

        let assessment = assess_severity(&advisory);

        assert_eq!(assessment.band, SeverityBand::Unknown);
        assert_eq!(assessment.base_score, None);
        assert_eq!(assessment.source, SeverityScoreSource::CvssV4VectorUnscored);
        assert_eq!(
            assessment.vector.as_deref(),
            Some("CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N")
        );
    }

    #[test]
    fn prefers_a_scorable_v3_vector_over_an_unscorable_v4_one() {
        let advisory = Advisory::parse(
            r#"{
                "id": "OSV-Y",
                "severity": [
                    { "type": "CVSS_V4", "score": "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N" },
                    { "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }
                ]
            }"#,
        )
        .unwrap();

        let assessment = assess_severity(&advisory);

        assert_eq!(assessment.source, SeverityScoreSource::CvssV3Vector);
        assert_eq!(assessment.band, SeverityBand::Critical);
    }

    #[test]
    fn an_advisory_with_no_severity_is_unknown_not_zero() {
        let advisory = Advisory::parse(r#"{ "id": "OSV-Z" }"#).unwrap();

        let assessment = assess_severity(&advisory);

        assert_eq!(assessment.band, SeverityBand::Unknown);
        assert_eq!(assessment.source, SeverityScoreSource::Absent);
    }

    fn inputs(
        severity: SeverityBand,
        applicability: ApplicabilityState,
        runtime_reachable: bool,
    ) -> PriorityInputs {
        PriorityInputs {
            severity,
            applicability,
            runtime_reachable,
        }
    }

    #[test]
    fn an_applicable_critical_finding_is_p0() {
        let priority = prioritize(&inputs(
            SeverityBand::Critical,
            ApplicabilityState::Applicable,
            true,
        ));

        assert_eq!(priority, Priority::P0);
    }

    #[test]
    fn review_required_demotes_a_critical_finding_one_level() {
        let priority = prioritize(&inputs(
            SeverityBand::Critical,
            ApplicabilityState::ReviewRequired,
            true,
        ));

        assert_eq!(priority, Priority::P1);
    }

    #[test]
    fn a_not_applicable_finding_is_always_lowest_priority() {
        let priority = prioritize(&inputs(
            SeverityBand::Critical,
            ApplicabilityState::NotApplicable,
            true,
        ));

        assert_eq!(priority, Priority::P3);
    }

    #[test]
    fn a_development_only_dependency_is_demoted_one_level() {
        let runtime = prioritize(&inputs(
            SeverityBand::Critical,
            ApplicabilityState::Applicable,
            true,
        ));
        let development = prioritize(&inputs(
            SeverityBand::Critical,
            ApplicabilityState::Applicable,
            false,
        ));

        assert_eq!(runtime, Priority::P0);
        assert_eq!(development, Priority::P1);
    }

    #[test]
    fn an_unknown_severity_is_treated_as_medium_rather_than_ignored() {
        let unknown = prioritize(&inputs(
            SeverityBand::Unknown,
            ApplicabilityState::Applicable,
            true,
        ));
        let medium = prioritize(&inputs(
            SeverityBand::Medium,
            ApplicabilityState::Applicable,
            true,
        ));

        assert_eq!(unknown, medium);
    }

    #[test]
    fn demotion_never_falls_below_the_lowest_priority() {
        let priority = prioritize(&inputs(
            SeverityBand::None,
            ApplicabilityState::ReviewRequired,
            false,
        ));

        assert_eq!(priority, Priority::P3);
    }
}
