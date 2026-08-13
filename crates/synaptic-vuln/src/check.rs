use serde::{Deserialize, Serialize};
use synaptic_api::PackageCoordinate;

use crate::matching::{VersionMatch, match_version, parse_version_for_ordering};
use crate::plan::VersionAvailability;
use crate::policy::VulnPolicy;
use crate::source::AdvisorySource;

/// The answer an agent gets before writing a dependency into a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyVerdict {
    /// Nothing known against this package at this version.
    Allowed,
    /// Usable, but only within the reported constraint.
    Constrained,
    /// Must not be used at the requested version, or at all.
    Blocked,
}

/// A dependency-safety answer with the evidence behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DependencySafety {
    pub verdict: SafetyVerdict,
    pub package: PackageCoordinate,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub requested_version: Option<String>,
    /// Advisory ids that bear on this answer.
    pub advisories: Vec<String>,
    /// A version requirement that satisfies every known advisory and policy pin.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub approved_constraint: Option<String>,
    /// Whether the constrained version is known to be published. Offline this
    /// is always `Unverified`: the constraint is what the advisory says fixes
    /// the issue, not a promise that such a release exists.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub constraint_availability: Option<VersionAvailability>,
    /// Safe replacements, when policy names any.
    pub alternatives: Vec<String>,
    pub reasons: Vec<String>,
}

/// Decide whether an agent may use a package, and at what version.
///
/// This is the guardrail an agent calls *before* adding a dependency. It is
/// deliberately answerable without a version: asking "is this package safe at
/// all, and what floor should I use" is the common case when generating code.
pub fn check_dependency(
    package: &PackageCoordinate,
    requested_version: Option<&str>,
    source: &dyn AdvisorySource,
    policy: Option<&VulnPolicy>,
) -> DependencySafety {
    let mut safety = DependencySafety {
        verdict: SafetyVerdict::Allowed,
        package: package.clone(),
        requested_version: requested_version.map(str::to_string),
        advisories: Vec::new(),
        approved_constraint: None,
        constraint_availability: None,
        alternatives: Vec::new(),
        reasons: Vec::new(),
    };

    // A policy denial outranks everything: it is a decision already taken.
    if let Some(rule) = policy.and_then(|policy| policy.deny_rule(package)) {
        safety.verdict = SafetyVerdict::Blocked;
        safety.reasons.push(format!(
            "policy denies {package}: {reason}",
            reason = rule.reason
        ));
        if let Some(replacement) = &rule.replacement {
            safety.alternatives.push(replacement.clone());
            safety.reasons.push(format!("use {replacement} instead"));
        }
        return safety;
    }

    let mut floor: Option<semver::Version> = None;
    let mut floor_text: Option<String> = None;
    let mut raise_floor = |candidate: &str, safety: &mut DependencySafety| {
        match parse_version_for_ordering(candidate) {
            Some(parsed) => {
                if floor.as_ref().is_none_or(|current| &parsed > current) {
                    floor = Some(parsed);
                    floor_text = Some(candidate.to_string());
                }
            }
            None => {
                safety.reasons.push(format!(
                    "a required version {candidate:?} could not be ordered"
                ));
            }
        }
    };

    let mut blocked = false;
    let mut undecidable = false;
    let mut unfixed_advisory = false;

    for advisory in source.advisories_for(package) {
        if advisory.is_withdrawn() {
            continue;
        }
        for affected in &advisory.affected {
            if &affected.package != package {
                continue;
            }

            let mut lowest_fix: Option<String> = None;
            for range in &affected.ranges {
                for event in &range.events {
                    if let crate::advisory::RangeEvent::Fixed(candidate) = event {
                        let better = lowest_fix.as_ref().is_none_or(|current| {
                            match (
                                parse_version_for_ordering(candidate),
                                parse_version_for_ordering(current),
                            ) {
                                (Some(new), Some(old)) => new < old,
                                _ => false,
                            }
                        });
                        if better {
                            lowest_fix = Some(candidate.clone());
                        }
                    }
                }
            }

            let relevant = match requested_version {
                Some(version) => match match_version(version, affected) {
                    VersionMatch::Affected => {
                        blocked = true;
                        true
                    }
                    VersionMatch::Undetermined(reason) => {
                        undecidable = true;
                        safety.reasons.push(format!(
                            "{}: version could not be ordered ({reason})",
                            advisory.id
                        ));
                        true
                    }
                    VersionMatch::Unaffected => false,
                },
                // With no version in hand, any advisory on the package is
                // relevant: the caller needs the floor before choosing one.
                None => true,
            };

            if !relevant {
                continue;
            }
            if !safety.advisories.contains(&advisory.id) {
                safety.advisories.push(advisory.id.clone());
            }
            match &lowest_fix {
                Some(fix) => {
                    raise_floor(fix, &mut safety);
                    safety
                        .reasons
                        .push(format!("{}: affected; fixed in {fix}", advisory.id));
                }
                None => {
                    unfixed_advisory = true;
                    safety.reasons.push(format!(
                        "{}: affected and the advisory records no fixed version",
                        advisory.id
                    ));
                }
            }
        }
    }

    if let Some(rule) = policy.and_then(|policy| policy.pin_rule(package)) {
        let below = requested_version
            .and_then(parse_version_for_ordering)
            .zip(parse_version_for_ordering(&rule.minimum))
            .is_some_and(|(requested, minimum)| requested < minimum);
        if below || requested_version.is_none() {
            raise_floor(&rule.minimum, &mut safety);
            safety.reasons.push(format!(
                "policy pins {package} to >={}: {}",
                rule.minimum, rule.reason
            ));
        }
        if below {
            blocked = true;
        }
    }

    safety.approved_constraint = floor_text.map(|version| format!(">={version}"));
    if safety.approved_constraint.is_some() {
        safety.constraint_availability = Some(VersionAvailability::Unverified);
    }

    safety.verdict = if blocked || (unfixed_advisory && requested_version.is_some()) {
        SafetyVerdict::Blocked
    } else if undecidable || safety.approved_constraint.is_some() {
        SafetyVerdict::Constrained
    } else {
        SafetyVerdict::Allowed
    };

    safety
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisory::Advisory;
    use crate::source::LocalDirSource;
    use synaptic_api::Ecosystem;

    fn cargo(name: &str) -> PackageCoordinate {
        PackageCoordinate::new(Ecosystem::Cargo, name)
    }

    fn corpus(documents: &[&str]) -> LocalDirSource {
        let advisories = documents
            .iter()
            .map(|body| Advisory::parse(body).expect("fixture parses"))
            .collect();
        LocalDirSource::from_advisories("test-corpus", advisories)
    }

    const VULNERABLE_EXAMPLE: &str = r#"{
        "id": "RUSTSEC-2026-0001",
        "summary": "example is vulnerable",
        "affected": [
            {
                "package": { "ecosystem": "crates.io", "name": "example" },
                "ranges": [
                    {
                        "type": "SEMVER",
                        "events": [{ "introduced": "0" }, { "fixed": "1.5.0" }]
                    }
                ]
            }
        ]
    }"#;

    #[test]
    fn a_package_with_no_advisories_is_allowed() {
        let safety = check_dependency(&cargo("clean"), Some("1.0.0"), &corpus(&[]), None);

        assert_eq!(safety.verdict, SafetyVerdict::Allowed);
        assert!(safety.advisories.is_empty());
    }

    #[test]
    fn a_vulnerable_version_is_blocked() {
        let safety = check_dependency(
            &cargo("example"),
            Some("1.2.0"),
            &corpus(&[VULNERABLE_EXAMPLE]),
            None,
        );

        assert_eq!(safety.verdict, SafetyVerdict::Blocked);
        assert_eq!(safety.advisories, vec!["RUSTSEC-2026-0001".to_string()]);
    }

    #[test]
    fn a_blocked_version_is_told_which_constraint_would_be_safe() {
        let safety = check_dependency(
            &cargo("example"),
            Some("1.2.0"),
            &corpus(&[VULNERABLE_EXAMPLE]),
            None,
        );

        assert_eq!(safety.approved_constraint.as_deref(), Some(">=1.5.0"));
        assert_eq!(
            safety.constraint_availability,
            Some(VersionAvailability::Unverified),
            "the tool must not imply it checked the registry"
        );
    }

    #[test]
    fn a_fixed_version_of_a_previously_vulnerable_package_is_allowed() {
        let safety = check_dependency(
            &cargo("example"),
            Some("1.5.0"),
            &corpus(&[VULNERABLE_EXAMPLE]),
            None,
        );

        assert_eq!(safety.verdict, SafetyVerdict::Allowed);
    }

    #[test]
    fn asking_without_a_version_returns_the_floor_to_use() {
        let safety = check_dependency(
            &cargo("example"),
            None,
            &corpus(&[VULNERABLE_EXAMPLE]),
            None,
        );

        assert_eq!(safety.verdict, SafetyVerdict::Constrained);
        assert_eq!(safety.approved_constraint.as_deref(), Some(">=1.5.0"));
    }

    #[test]
    fn a_denied_package_is_blocked_and_offered_a_replacement() {
        let policy = VulnPolicy::parse(
            r#"
schema = 1

[[deny]]
package = "cargo:example"
reason = "unmaintained"
replacement = "cargo:successor"
"#,
        )
        .unwrap();

        let safety = check_dependency(
            &cargo("example"),
            Some("9.9.9"),
            &corpus(&[]),
            Some(&policy),
        );

        assert_eq!(safety.verdict, SafetyVerdict::Blocked);
        assert_eq!(safety.alternatives, vec!["cargo:successor".to_string()]);
        assert!(
            safety
                .reasons
                .iter()
                .any(|note| note.contains("unmaintained"))
        );
    }

    #[test]
    fn a_version_below_a_policy_pin_is_blocked_with_the_floor() {
        let policy = VulnPolicy::parse(
            r#"
schema = 1

[[pin]]
package = "cargo:example"
minimum = "2.0.0"
reason = "organisation floor"
"#,
        )
        .unwrap();

        let safety = check_dependency(
            &cargo("example"),
            Some("1.9.0"),
            &corpus(&[]),
            Some(&policy),
        );

        assert_eq!(safety.verdict, SafetyVerdict::Blocked);
        assert_eq!(safety.approved_constraint.as_deref(), Some(">=2.0.0"));
    }

    #[test]
    fn a_version_at_or_above_the_pin_is_allowed() {
        let policy = VulnPolicy::parse(
            r#"
schema = 1

[[pin]]
package = "cargo:example"
minimum = "2.0.0"
reason = "organisation floor"
"#,
        )
        .unwrap();

        let safety = check_dependency(
            &cargo("example"),
            Some("2.0.0"),
            &corpus(&[]),
            Some(&policy),
        );

        assert_eq!(safety.verdict, SafetyVerdict::Allowed);
    }

    #[test]
    fn a_pin_raises_the_floor_above_the_advisory_fix() {
        let policy = VulnPolicy::parse(
            r#"
schema = 1

[[pin]]
package = "cargo:example"
minimum = "3.0.0"
reason = "organisation floor above the advisory fix"
"#,
        )
        .unwrap();

        let safety = check_dependency(
            &cargo("example"),
            None,
            &corpus(&[VULNERABLE_EXAMPLE]),
            Some(&policy),
        );

        assert_eq!(
            safety.approved_constraint.as_deref(),
            Some(">=3.0.0"),
            "the stricter of policy floor and advisory fix wins"
        );
    }

    #[test]
    fn a_withdrawn_advisory_does_not_block() {
        let withdrawn = r#"{
            "id": "RUSTSEC-2026-0002",
            "withdrawn": "2026-05-01T00:00:00Z",
            "affected": [
                {
                    "package": { "ecosystem": "crates.io", "name": "example" },
                    "ranges": [
                        { "type": "SEMVER", "events": [{ "introduced": "0" }, { "fixed": "9.0.0" }] }
                    ]
                }
            ]
        }"#;

        let safety = check_dependency(
            &cargo("example"),
            Some("1.0.0"),
            &corpus(&[withdrawn]),
            None,
        );

        assert_eq!(safety.verdict, SafetyVerdict::Allowed);
    }

    #[test]
    fn an_undecidable_version_is_constrained_rather_than_allowed() {
        let safety = check_dependency(
            &cargo("example"),
            Some("not-a-version"),
            &corpus(&[VULNERABLE_EXAMPLE]),
            None,
        );

        assert_eq!(
            safety.verdict,
            SafetyVerdict::Constrained,
            "an unorderable version must never come back clean"
        );
    }

    #[test]
    fn an_advisory_with_no_fix_blocks_without_offering_a_constraint() {
        let unfixed = r#"{
            "id": "RUSTSEC-2026-0003",
            "affected": [
                {
                    "package": { "ecosystem": "crates.io", "name": "example" },
                    "ranges": [{ "type": "SEMVER", "events": [{ "introduced": "0" }] }]
                }
            ]
        }"#;

        let safety = check_dependency(&cargo("example"), Some("1.0.0"), &corpus(&[unfixed]), None);

        assert_eq!(safety.verdict, SafetyVerdict::Blocked);
        assert_eq!(safety.approved_constraint, None);
        assert!(
            safety
                .reasons
                .iter()
                .any(|note| note.contains("no fixed version"))
        );
    }

    #[test]
    fn the_reasons_always_name_the_advisory_that_drove_the_verdict() {
        let safety = check_dependency(
            &cargo("example"),
            Some("1.2.0"),
            &corpus(&[VULNERABLE_EXAMPLE]),
            None,
        );

        assert!(
            safety
                .reasons
                .iter()
                .any(|note| note.contains("RUSTSEC-2026-0001"))
        );
    }
}
