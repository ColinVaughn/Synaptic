use serde::{Deserialize, Serialize};

use crate::advisory::{Affected, RangeEvent};
use crate::matching::parse_version_for_ordering;

/// What kind of remediation is available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemediationKind {
    /// Upgrade to a version the advisory records as fixed.
    Upgrade,
    /// The advisory records no fixed version, so no upgrade resolves this.
    /// Mitigation or removal is required instead.
    NoFixAvailable,
    /// A fixed version exists but could not be ordered against the installed
    /// version, so no specific target can be recommended.
    FixVersionUndetermined,
}

/// How much breakage an upgrade is likely to carry.
///
/// Cargo's compatibility rules are used: for `0.x` releases a minor bump is
/// breaking, so it is reported as major rather than minor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityRisk {
    Patch,
    Minor,
    Major,
    Unknown,
}

/// Whether the recommended version is known to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionAvailability {
    /// Confirmed present in a registry.
    Confirmed,
    /// Derived from advisory metadata without contacting a registry. The
    /// version is what the advisory says fixes the issue; whether it is
    /// published and resolvable has not been checked.
    Unverified,
}

/// A concrete proposal for resolving one finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemediationPlan {
    pub kind: RemediationKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub recommended_version: Option<String>,
    pub availability: VersionAvailability,
    pub compatibility_risk: CompatibilityRisk,
    pub required_changes: Vec<String>,
    pub validation_commands: Vec<String>,
    pub notes: Vec<String>,
}

/// Build a remediation plan for one affected entry and installed version.
///
/// The recommended target is the lowest `fixed` version the advisory records
/// that is above the installed version. Choosing the lowest keeps the upgrade
/// as small as possible, which is what makes the change reviewable.
pub fn plan_remediation(
    installed_version: &str,
    affected: &Affected,
    validation_commands: &[String],
) -> RemediationPlan {
    let installed = parse_version_for_ordering(installed_version);
    let mut fixed_versions = Vec::new();
    let mut saw_unorderable_fix = false;

    for range in &affected.ranges {
        for event in &range.events {
            let RangeEvent::Fixed(candidate) = event else {
                continue;
            };
            match parse_version_for_ordering(candidate) {
                Some(parsed) => fixed_versions.push((parsed, candidate.clone())),
                None => saw_unorderable_fix = true,
            }
        }
    }

    if fixed_versions.is_empty() {
        return if saw_unorderable_fix {
            RemediationPlan {
                kind: RemediationKind::FixVersionUndetermined,
                recommended_version: None,
                availability: VersionAvailability::Unverified,
                compatibility_risk: CompatibilityRisk::Unknown,
                required_changes: Vec::new(),
                validation_commands: validation_commands.to_vec(),
                notes: vec![
                    "the advisory records a fixed version that could not be ordered against \
                     the installed version; read the advisory directly"
                        .into(),
                ],
            }
        } else {
            RemediationPlan {
                kind: RemediationKind::NoFixAvailable,
                recommended_version: None,
                availability: VersionAvailability::Unverified,
                compatibility_risk: CompatibilityRisk::Unknown,
                required_changes: Vec::new(),
                validation_commands: validation_commands.to_vec(),
                notes: vec![
                    "the advisory records no fixed version; remediation requires removing \
                     the dependency, replacing it, or mitigating the exposure"
                        .into(),
                ],
            }
        };
    }

    fixed_versions.sort_by(|left, right| left.0.cmp(&right.0));
    let target = installed
        .as_ref()
        .and_then(|installed| {
            fixed_versions
                .iter()
                .find(|(parsed, _)| parsed > installed)
                .cloned()
        })
        .or_else(|| fixed_versions.first().cloned());

    let Some((_, target_version)) = target else {
        return RemediationPlan {
            kind: RemediationKind::FixVersionUndetermined,
            recommended_version: None,
            availability: VersionAvailability::Unverified,
            compatibility_risk: CompatibilityRisk::Unknown,
            required_changes: Vec::new(),
            validation_commands: validation_commands.to_vec(),
            notes: vec!["no fixed version could be ordered against the installed version".into()],
        };
    };

    let risk = compatibility_risk(installed_version, &target_version);
    let mut notes = vec![format!(
        "the advisory records {target_version} as fixed; this recommendation comes from \
         advisory metadata and has not been checked against a registry"
    )];
    if risk == CompatibilityRisk::Major {
        notes.push(
            "this is a breaking upgrade under semantic-versioning rules; expect source \
             changes and review the upstream changelog before merging"
                .into(),
        );
    }

    RemediationPlan {
        kind: RemediationKind::Upgrade,
        recommended_version: Some(target_version.clone()),
        availability: VersionAvailability::Unverified,
        compatibility_risk: risk,
        required_changes: vec![format!(
            "raise {} to {target_version} in the declaring manifest and refresh the lockfile",
            affected.package
        )],
        validation_commands: validation_commands.to_vec(),
        notes,
    }
}

/// Compare two versions and classify the upgrade's breakage risk.
///
/// Cargo's compatibility rules are applied: below `1.0.0` the minor component
/// carries the breaking-change signal, so `0.9.x -> 0.10.0` is major.
pub fn compatibility_risk(from: &str, to: &str) -> CompatibilityRisk {
    let (Some(from), Some(to)) = (
        parse_version_for_ordering(from),
        parse_version_for_ordering(to),
    ) else {
        return CompatibilityRisk::Unknown;
    };

    if from.major != to.major {
        CompatibilityRisk::Major
    } else if from.minor != to.minor {
        if from.major == 0 {
            CompatibilityRisk::Major
        } else {
            CompatibilityRisk::Minor
        }
    } else {
        CompatibilityRisk::Patch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisory::{RangeKind, VersionRange};
    use synaptic_api::{Ecosystem, PackageCoordinate};

    fn affected(ranges: Vec<VersionRange>) -> Affected {
        Affected {
            package: PackageCoordinate::new(Ecosystem::Cargo, "example"),
            purl: None,
            ranges,
            versions: Vec::new(),
            affected_functions: Vec::new(),
        }
    }

    fn semver_range(events: Vec<RangeEvent>) -> VersionRange {
        VersionRange {
            kind: RangeKind::SemVer,
            events,
        }
    }

    fn commands() -> Vec<String> {
        vec!["cargo test --workspace".to_string()]
    }

    #[test]
    fn recommends_the_fixed_version_from_the_advisory() {
        let entry = affected(vec![semver_range(vec![
            RangeEvent::Introduced("0.9.0".into()),
            RangeEvent::Fixed("0.9.20".into()),
        ])]);

        let plan = plan_remediation("0.9.18", &entry, &commands());

        assert_eq!(plan.kind, RemediationKind::Upgrade);
        assert_eq!(plan.recommended_version.as_deref(), Some("0.9.20"));
    }

    #[test]
    fn picks_the_lowest_fix_that_is_above_the_installed_version() {
        let entry = affected(vec![
            semver_range(vec![
                RangeEvent::Introduced("1.0.0".into()),
                RangeEvent::Fixed("1.5.0".into()),
            ]),
            semver_range(vec![
                RangeEvent::Introduced("2.0.0".into()),
                RangeEvent::Fixed("2.3.0".into()),
            ]),
        ]);

        let plan = plan_remediation("1.2.0", &entry, &commands());

        assert_eq!(plan.recommended_version.as_deref(), Some("1.5.0"));
    }

    #[test]
    fn ignores_fixes_that_are_below_the_installed_version() {
        let entry = affected(vec![
            semver_range(vec![
                RangeEvent::Introduced("1.0.0".into()),
                RangeEvent::Fixed("1.5.0".into()),
            ]),
            semver_range(vec![
                RangeEvent::Introduced("2.0.0".into()),
                RangeEvent::Fixed("2.3.0".into()),
            ]),
        ]);

        let plan = plan_remediation("2.1.0", &entry, &commands());

        assert_eq!(plan.recommended_version.as_deref(), Some("2.3.0"));
    }

    #[test]
    fn an_advisory_with_no_fixed_event_offers_no_upgrade() {
        let entry = affected(vec![semver_range(vec![RangeEvent::Introduced(
            "1.0.0".into(),
        )])]);

        let plan = plan_remediation("1.2.0", &entry, &commands());

        assert_eq!(plan.kind, RemediationKind::NoFixAvailable);
        assert_eq!(plan.recommended_version, None);
        assert!(
            plan.notes.iter().any(|note| note.contains("no fixed")),
            "the plan must say why there is nothing to upgrade to"
        );
    }

    #[test]
    fn an_unorderable_fix_version_is_reported_rather_than_guessed() {
        let entry = affected(vec![semver_range(vec![
            RangeEvent::Introduced("1.0.0".into()),
            RangeEvent::Fixed("not-a-version".into()),
        ])]);

        let plan = plan_remediation("1.2.0", &entry, &commands());

        assert_eq!(plan.kind, RemediationKind::FixVersionUndetermined);
        assert_eq!(plan.recommended_version, None);
    }

    #[test]
    fn a_recommended_version_is_unverified_without_a_registry() {
        let entry = affected(vec![semver_range(vec![
            RangeEvent::Introduced("0.9.0".into()),
            RangeEvent::Fixed("0.9.20".into()),
        ])]);

        let plan = plan_remediation("0.9.18", &entry, &commands());

        assert_eq!(plan.availability, VersionAvailability::Unverified);
    }

    #[test]
    fn the_plan_carries_the_validation_commands_it_was_given() {
        let entry = affected(vec![semver_range(vec![
            RangeEvent::Introduced("0.9.0".into()),
            RangeEvent::Fixed("0.9.20".into()),
        ])]);

        let plan = plan_remediation("0.9.18", &entry, &commands());

        assert_eq!(plan.validation_commands, commands());
    }

    #[test]
    fn a_patch_bump_is_low_risk() {
        assert_eq!(
            compatibility_risk("1.2.3", "1.2.9"),
            CompatibilityRisk::Patch
        );
    }

    #[test]
    fn a_minor_bump_on_a_stable_release_is_minor_risk() {
        assert_eq!(
            compatibility_risk("1.2.3", "1.5.0"),
            CompatibilityRisk::Minor
        );
    }

    #[test]
    fn a_major_bump_is_major_risk() {
        assert_eq!(
            compatibility_risk("1.2.3", "2.0.0"),
            CompatibilityRisk::Major
        );
    }

    #[test]
    fn a_minor_bump_below_one_point_zero_is_breaking_under_cargo_rules() {
        assert_eq!(
            compatibility_risk("0.9.18", "0.10.0"),
            CompatibilityRisk::Major,
            "cargo treats 0.x minor bumps as incompatible"
        );
    }

    #[test]
    fn a_patch_bump_below_one_point_zero_is_still_a_patch() {
        assert_eq!(
            compatibility_risk("0.9.18", "0.9.20"),
            CompatibilityRisk::Patch
        );
    }

    #[test]
    fn an_unorderable_version_pair_has_unknown_risk() {
        assert_eq!(
            compatibility_risk("mystery", "1.0.0"),
            CompatibilityRisk::Unknown
        );
    }

    #[test]
    fn the_plan_names_the_manifest_change_the_upgrade_requires() {
        let entry = affected(vec![semver_range(vec![
            RangeEvent::Introduced("0.9.0".into()),
            RangeEvent::Fixed("0.9.20".into()),
        ])]);

        let plan = plan_remediation("0.9.18", &entry, &commands());

        assert!(
            plan.required_changes
                .iter()
                .any(|change| change.contains("0.9.20")),
            "required changes must name the target version, got {:?}",
            plan.required_changes
        );
    }

    #[test]
    fn a_major_upgrade_warns_about_source_changes() {
        let entry = affected(vec![semver_range(vec![
            RangeEvent::Introduced("1.0.0".into()),
            RangeEvent::Fixed("2.0.0".into()),
        ])]);

        let plan = plan_remediation("1.2.0", &entry, &commands());

        assert_eq!(plan.compatibility_risk, CompatibilityRisk::Major);
        assert!(
            plan.notes.iter().any(|note| note.contains("breaking")),
            "a major upgrade must warn, got {:?}",
            plan.notes
        );
    }
}
