use semver::Version;

use crate::advisory::{Affected, RangeEvent, RangeKind, VersionRange};

/// The result of testing one resolved version against one `affected` entry.
///
/// `Undetermined` exists because "we could not decide" and "it is safe" are
/// different answers, and collapsing them is how scanners under-report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionMatch {
    Affected,
    Unaffected,
    Undetermined(String),
}

impl VersionMatch {
    /// True when the version is affected or could not be decided. Callers that
    /// must not miss a vulnerability use this rather than `== Affected`.
    pub fn needs_review(&self) -> bool {
        !matches!(self, VersionMatch::Unaffected)
    }
}

/// Test a resolved version against one advisory `affected` entry.
///
/// An entry is affected when its enumerated version list contains the version
/// exactly, or when any of its ranges contains it. A range that cannot be
/// evaluated yields `Undetermined` rather than `Unaffected`, and `Affected`
/// from any one range outranks an `Undetermined` from another.
pub fn match_version(version: &str, affected: &Affected) -> VersionMatch {
    if affected.versions.iter().any(|listed| listed == version) {
        return VersionMatch::Affected;
    }
    if affected.ranges.is_empty() {
        return if affected.versions.is_empty() {
            VersionMatch::Undetermined(
                "advisory entry declares neither version ranges nor an explicit version list"
                    .into(),
            )
        } else {
            // The advisory enumerated exactly which versions are affected and
            // this is not one of them.
            VersionMatch::Unaffected
        };
    }

    let mut undetermined = None;
    for range in &affected.ranges {
        match match_range(version, range) {
            VersionMatch::Affected => return VersionMatch::Affected,
            VersionMatch::Undetermined(reason) => {
                undetermined.get_or_insert(reason);
            }
            VersionMatch::Unaffected => {}
        }
    }
    match undetermined {
        Some(reason) => VersionMatch::Undetermined(reason),
        None => VersionMatch::Unaffected,
    }
}

/// Test a resolved version against a single range.
///
/// Events are applied in document order, following the OSV range semantics:
/// `introduced` opens the affected interval, `fixed` closes it exclusively,
/// and `last_affected` closes it inclusively.
pub fn match_range(version: &str, range: &VersionRange) -> VersionMatch {
    if range.kind == RangeKind::Git {
        return VersionMatch::Undetermined(
            "git ranges identify commits rather than released versions".into(),
        );
    }
    if range.events.is_empty() {
        return VersionMatch::Undetermined("range declares no boundary events".into());
    }
    let Some(target) = parse_version_for_ordering(version) else {
        return VersionMatch::Undetermined(format!("cannot order version {version:?}"));
    };

    let mut affected = false;
    for event in &range.events {
        match event {
            // OSV spells "from the very first release" as the literal zero.
            RangeEvent::Introduced(bound) if bound == "0" => affected = true,
            RangeEvent::Introduced(bound) => match parse_version_for_ordering(bound) {
                Some(bound) => {
                    if target >= bound {
                        affected = true;
                    }
                }
                None => return undecidable_bound("introduced", bound),
            },
            RangeEvent::Fixed(bound) => match parse_version_for_ordering(bound) {
                Some(bound) => {
                    if target >= bound {
                        affected = false;
                    }
                }
                None => return undecidable_bound("fixed", bound),
            },
            RangeEvent::LastAffected(bound) => match parse_version_for_ordering(bound) {
                Some(bound) => {
                    if target > bound {
                        affected = false;
                    }
                }
                None => return undecidable_bound("last_affected", bound),
            },
            RangeEvent::Limit(bound) => {
                if bound == "*" {
                    continue;
                }
                match parse_version_for_ordering(bound) {
                    Some(bound) => {
                        if target >= bound {
                            affected = false;
                        }
                    }
                    None => return undecidable_bound("limit", bound),
                }
            }
        }
    }

    if affected {
        VersionMatch::Affected
    } else {
        VersionMatch::Unaffected
    }
}

fn undecidable_bound(event: &str, bound: &str) -> VersionMatch {
    VersionMatch::Undetermined(format!("cannot order {event} boundary {bound:?}"))
}

/// Parse a version leniently enough for real lockfiles and advisories.
///
/// Strict semver rejects the two- and one-component forms that several
/// ecosystems publish ("1.2", "3"). Those are padded, but only when every
/// numeric component really is numeric, so genuinely unparseable input still
/// reports as unparseable instead of being coerced into a wrong ordering.
pub(crate) fn parse_version_for_ordering(raw: &str) -> Option<Version> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(version) = Version::parse(raw) {
        return Some(version);
    }

    let split = raw.find(['-', '+']).unwrap_or(raw.len());
    let (core, suffix) = raw.split_at(split);
    let mut components = core.split('.').collect::<Vec<_>>();
    if components.len() > 3 || components.iter().any(|part| part.is_empty()) {
        return None;
    }
    if !components
        .iter()
        .all(|part| part.chars().all(|ch| ch.is_ascii_digit()))
    {
        return None;
    }
    while components.len() < 3 {
        components.push("0");
    }
    Version::parse(&format!("{}{suffix}", components.join("."))).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaptic_api::{Ecosystem, PackageCoordinate};

    fn affected_with(ranges: Vec<VersionRange>, versions: Vec<&str>) -> Affected {
        Affected {
            package: PackageCoordinate::new(Ecosystem::Cargo, "example"),
            purl: None,
            ranges,
            versions: versions.into_iter().map(str::to_string).collect(),
            affected_functions: Vec::new(),
        }
    }

    fn semver_range(events: Vec<RangeEvent>) -> VersionRange {
        VersionRange {
            kind: RangeKind::SemVer,
            events,
        }
    }

    #[test]
    fn a_version_between_introduced_and_fixed_is_affected() {
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("0.9.0".into()),
                RangeEvent::Fixed("0.9.20".into()),
            ])],
            vec![],
        );

        assert_eq!(match_version("0.9.18", &affected), VersionMatch::Affected);
    }

    #[test]
    fn the_fixed_version_itself_is_not_affected() {
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("0.9.0".into()),
                RangeEvent::Fixed("0.9.20".into()),
            ])],
            vec![],
        );

        assert_eq!(match_version("0.9.20", &affected), VersionMatch::Unaffected);
        assert_eq!(match_version("0.9.21", &affected), VersionMatch::Unaffected);
    }

    #[test]
    fn a_version_below_the_introduced_boundary_is_unaffected() {
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("0.9.0".into()),
                RangeEvent::Fixed("0.9.20".into()),
            ])],
            vec![],
        );

        assert_eq!(match_version("0.8.9", &affected), VersionMatch::Unaffected);
    }

    #[test]
    fn introduced_zero_means_every_version_from_the_beginning() {
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("0".into()),
                RangeEvent::Fixed("1.2.3".into()),
            ])],
            vec![],
        );

        assert_eq!(match_version("0.0.1", &affected), VersionMatch::Affected);
        assert_eq!(match_version("1.2.2", &affected), VersionMatch::Affected);
        assert_eq!(match_version("1.2.3", &affected), VersionMatch::Unaffected);
    }

    #[test]
    fn last_affected_is_inclusive_unlike_fixed() {
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("1.0.0".into()),
                RangeEvent::LastAffected("1.4.0".into()),
            ])],
            vec![],
        );

        assert_eq!(match_version("1.4.0", &affected), VersionMatch::Affected);
        assert_eq!(match_version("1.4.1", &affected), VersionMatch::Unaffected);
    }

    #[test]
    fn an_explicitly_enumerated_version_is_affected() {
        let affected = affected_with(vec![], vec!["2.0.0-custom.build"]);

        assert_eq!(
            match_version("2.0.0-custom.build", &affected),
            VersionMatch::Affected
        );
    }

    #[test]
    fn any_matching_range_makes_the_version_affected() {
        let affected = affected_with(
            vec![
                semver_range(vec![
                    RangeEvent::Introduced("1.0.0".into()),
                    RangeEvent::Fixed("1.1.0".into()),
                ]),
                semver_range(vec![
                    RangeEvent::Introduced("2.0.0".into()),
                    RangeEvent::Fixed("2.1.0".into()),
                ]),
            ],
            vec![],
        );

        assert_eq!(match_version("2.0.5", &affected), VersionMatch::Affected);
        assert_eq!(match_version("1.5.0", &affected), VersionMatch::Unaffected);
    }

    #[test]
    fn a_prerelease_below_the_fix_is_still_affected() {
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("0.9.0".into()),
                RangeEvent::Fixed("0.9.20".into()),
            ])],
            vec![],
        );

        assert_eq!(
            match_version("0.9.20-rc.1", &affected),
            VersionMatch::Affected
        );
    }

    #[test]
    fn an_unparseable_version_is_undetermined_never_unaffected() {
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("1.0.0".into()),
                RangeEvent::Fixed("2.0.0".into()),
            ])],
            vec![],
        );

        let result = match_version("not-a-version", &affected);

        assert!(
            matches!(result, VersionMatch::Undetermined(_)),
            "expected Undetermined, got {result:?}"
        );
        assert!(result.needs_review());
    }

    #[test]
    fn a_range_with_no_events_is_undetermined() {
        let affected = affected_with(vec![semver_range(vec![])], vec![]);

        assert!(matches!(
            match_version("1.0.0", &affected),
            VersionMatch::Undetermined(_)
        ));
    }

    #[test]
    fn an_affected_entry_with_no_ranges_and_no_versions_is_undetermined() {
        let affected = affected_with(vec![], vec![]);

        assert!(matches!(
            match_version("1.0.0", &affected),
            VersionMatch::Undetermined(_)
        ));
    }

    #[test]
    fn a_git_range_is_undetermined_rather_than_silently_unaffected() {
        let affected = affected_with(
            vec![VersionRange {
                kind: RangeKind::Git,
                events: vec![
                    RangeEvent::Introduced("abc123".into()),
                    RangeEvent::Fixed("def456".into()),
                ],
            }],
            vec![],
        );

        assert!(matches!(
            match_version("1.0.0", &affected),
            VersionMatch::Undetermined(_)
        ));
    }

    #[test]
    fn build_metadata_does_not_change_ordering() {
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("1.0.0".into()),
                RangeEvent::Fixed("2.0.0".into()),
            ])],
            vec![],
        );

        assert_eq!(
            match_version("1.5.0+build.7", &affected),
            VersionMatch::Affected
        );
    }

    #[test]
    fn a_cargo_style_two_component_version_is_parsed() {
        // Lockfiles and advisories both carry versions like "1.2" for some
        // ecosystems; treating them as unparseable would lose real findings.
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("1.0".into()),
                RangeEvent::Fixed("1.3".into()),
            ])],
            vec![],
        );

        assert_eq!(match_version("1.2", &affected), VersionMatch::Affected);
    }

    #[test]
    fn a_single_range_can_be_matched_directly() {
        let range = semver_range(vec![
            RangeEvent::Introduced("1.0.0".into()),
            RangeEvent::Fixed("2.0.0".into()),
        ]);

        assert_eq!(match_range("1.5.0", &range), VersionMatch::Affected);
    }
}
