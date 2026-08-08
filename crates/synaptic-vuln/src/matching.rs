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
    // Several ecosystems record the git tag verbatim, so a version arrives as
    // `v8.1.0` while OSV ranges are bare semver. Unstripped, every comparison
    // came back undetermined and each advisory for the package turned into a
    // finding. Only a `v` immediately followed by a digit is a tag prefix;
    // `vNext` is a name and must stay unparseable.
    let raw = match raw.strip_prefix(['v', 'V']) {
        Some(rest) if rest.starts_with(|ch: char| ch.is_ascii_digit()) => rest,
        _ => raw,
    };
    if let Ok(version) = Version::parse(raw) {
        return Some(version);
    }

    let split = raw.find(['-', '+']).unwrap_or(raw.len());
    let (core, suffix) = raw.split_at(split);

    // PEP 440 and RubyGems attach a pre-release with no separator (`1.0.0a1`,
    // `5.1b7`) or after a dot (`3.0.0.beta1`); semver wants it after a `-`.
    // Splitting at the first letter recovers the ordering, and a pre-release
    // still sorts below its release, which is what both ecosystems mean.
    let (core, prerelease) = match core.find(|ch: char| ch.is_ascii_alphabetic()) {
        Some(index) => (&core[..index], Some(&core[index..])),
        None => (core, None),
    };
    let core = core.trim_end_matches('.');

    let components = core.split('.').collect::<Vec<_>>();
    // More than three numeric components (RubyGems `2.0.9.2`) has no faithful
    // semver mapping. Left undetermined on purpose: a guessed ordering could
    // report a vulnerable version as unaffected, and nothing here may do that.
    if core.is_empty() || components.len() > 3 || components.iter().any(|part| part.is_empty()) {
        return None;
    }
    // Re-emit each component from its parsed value, because semver rejects the
    // zero padding real advisories carry (`2017.11.05`).
    let mut numbers = Vec::with_capacity(3);
    for part in components {
        numbers.push(part.parse::<u64>().ok()?);
    }
    while numbers.len() < 3 {
        numbers.push(0);
    }

    let core = numbers
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".");
    let normalized = match prerelease {
        Some(pre) => format!("{core}-{pre}{suffix}"),
        None => format!("{core}{suffix}"),
    };
    Version::parse(&normalized).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leading_v_is_ignored_when_ordering_versions() {
        // Packagist/composer.lock writes tags verbatim (`v8.1.0`) while OSV
        // ranges are bare semver. Left unstripped every comparison came back
        // undetermined, so EVERY advisory for the package became a finding:
        // symfony-demo reported 150 of them, all spurious.
        assert_eq!(
            parse_version_for_ordering("v8.1.0"),
            parse_version_for_ordering("8.1.0"),
            "a tag prefix must not change the ordering"
        );
    }

    #[test]
    fn a_prefixed_version_still_matches_an_affected_range() {
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("8.0.0".into()),
                RangeEvent::Fixed("8.2.0".into()),
            ])],
            Vec::new(),
        );

        assert_eq!(match_version("v8.1.0", &affected), VersionMatch::Affected);
    }

    #[test]
    fn a_prefixed_version_outside_every_range_is_unaffected() {
        let affected = affected_with(
            vec![semver_range(vec![
                RangeEvent::Introduced("8.0.0".into()),
                RangeEvent::Fixed("8.2.0".into()),
            ])],
            Vec::new(),
        );

        assert_eq!(match_version("v9.0.0", &affected), VersionMatch::Unaffected);
    }

    #[test]
    fn an_attached_prerelease_orders_below_its_release() {
        // PEP 440 writes `1.0.0a1` and RubyGems `3.0.0.beta1`; semver needs the
        // pre-release after a `-`. Unconverted these bounds were undecidable,
        // and every advisory carrying one became a review-required finding.
        for raw in ["1.0.0a1", "3.0.0.beta1", "5.1b7"] {
            let parsed = parse_version_for_ordering(raw)
                .unwrap_or_else(|| panic!("{raw} must be orderable"));
            assert!(!parsed.pre.is_empty(), "{raw} must keep a pre-release");
        }

        let pre = parse_version_for_ordering("1.0.0a1").expect("prerelease");
        let release = parse_version_for_ordering("1.0.0").expect("release");
        assert!(pre < release, "a pre-release sorts below its release");
    }

    #[test]
    fn a_zero_padded_component_is_orderable() {
        // `2017.11.05` is a real advisory bound; semver rejects leading zeros.
        assert_eq!(
            parse_version_for_ordering("2017.11.05"),
            parse_version_for_ordering("2017.11.5")
        );
    }

    #[test]
    fn a_four_component_version_stays_undetermined_rather_than_guessed() {
        // RubyGems allows `2.0.9.2`, which has no faithful semver mapping.
        // Undetermined keeps the finding at review-required; inventing an
        // ordering could produce a false "unaffected", which this crate must
        // never do.
        assert!(parse_version_for_ordering("2.0.9.2").is_none());
    }

    #[test]
    fn a_bare_v_is_not_a_version() {
        assert!(parse_version_for_ordering("v").is_none());
        assert!(parse_version_for_ordering("vNext").is_none());
    }

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
