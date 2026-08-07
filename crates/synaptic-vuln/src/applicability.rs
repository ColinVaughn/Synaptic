use serde::{Deserialize, Serialize};
use synaptic_api::ApplicabilityState;

use crate::matching::VersionMatch;

/// One observation that bears on whether a vulnerability applies here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// The resolved version falls inside an affected range.
    VersionInAffectedRange,
    /// The resolved version is outside every affected range.
    VersionOutsideAffectedRange,
    /// The version could not be ordered against the advisory's ranges.
    VersionUndetermined,
    /// The advisory was withdrawn by its publisher.
    AdvisoryWithdrawn,
    /// The advisory names vulnerable functions and one is reachable from
    /// first-party code.
    SymbolReachable,
    /// The advisory names vulnerable functions and none appear reachable.
    SymbolUnreachable,
    /// The advisory names no functions, so reachability cannot be decided from
    /// it. This is an absence of data, not evidence of safety.
    AdvisoryNamesNoFunctions,
    /// First-party code imports or calls the package.
    FirstPartyUsageObserved,
    /// No first-party usage of the package was observed.
    NoFirstPartyUsage,
    /// The package is reached only through development, build, or test edges.
    DevelopmentOnlyDependency,
    /// Nothing recorded what this package is needed for, so runtime
    /// reachability was assumed rather than read. This is an absence of data,
    /// not a finding that the package ships.
    DependencyScopeUnrecorded,
    /// The package is reached only through a dependency no enabled feature
    /// compiles. A feature this build leaves off is still one another build
    /// turns on, so this de-ranks and never dismisses.
    FeatureGated,
    /// Reflection or dynamic dispatch was detected in the reaching region, so
    /// static reachability results cannot be trusted to be complete.
    DynamicHazard,
}

/// What a piece of evidence is allowed to do to the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceDirection {
    /// Decides the verdict outright.
    Gate,
    /// Can raise the state toward `Applicable`.
    Raises,
    /// Recorded and used to de-rank the finding, but never lowers the state.
    /// Nothing in this crate lowers a state on absence of evidence.
    LowersPriority,
    /// Contextual only.
    Informational,
}

/// One recorded observation with its provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicabilityEvidence {
    pub kind: EvidenceKind,
    pub direction: EvidenceDirection,
    pub detail: String,
}

impl ApplicabilityEvidence {
    fn new(kind: EvidenceKind, direction: EvidenceDirection, detail: impl Into<String>) -> Self {
        Self {
            kind,
            direction,
            detail: detail.into(),
        }
    }
}

/// The inputs the combiner reasons over.
///
/// This is a plain data struct with no graph or filesystem dependency so the
/// decision rules can be tested exhaustively in isolation.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplicabilityInput {
    pub version_match: VersionMatch,
    pub advisory_withdrawn: bool,
    /// Function paths the advisory names as vulnerable.
    pub advisory_functions: Vec<String>,
    /// Advisory-named functions that appear reachable from first-party code.
    pub reachable_functions: Vec<String>,
    /// Whether first-party code imports or calls the package at all.
    pub first_party_usage_observed: bool,
    /// Whether the package is declared directly rather than pulled in
    /// transitively.
    pub is_direct_dependency: bool,
    /// Whether any path to the package runs through runtime edges.
    pub runtime_reachable: bool,
    /// Whether anything actually recorded what this package is needed for.
    /// When false, `runtime_reachable` is an assumption, and the verdict says
    /// so instead of presenting it as a reading.
    pub scope_recorded: bool,
    /// Whether every path to the package runs through an optional dependency
    /// no enabled feature compiles.
    pub feature_gated: bool,
    /// Whether dynamic dispatch or reflection was detected nearby.
    pub dynamic_hazard_present: bool,
}

impl Default for ApplicabilityInput {
    fn default() -> Self {
        Self {
            version_match: VersionMatch::Unaffected,
            advisory_withdrawn: false,
            advisory_functions: Vec::new(),
            reachable_functions: Vec::new(),
            first_party_usage_observed: false,
            is_direct_dependency: false,
            runtime_reachable: true,
            scope_recorded: false,
            feature_gated: false,
            dynamic_hazard_present: false,
        }
    }
}

/// The conclusion, with every observation that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplicabilityVerdict {
    pub state: ApplicabilityState,
    pub evidence: Vec<ApplicabilityEvidence>,
    pub runtime_reachable: bool,
}

/// Decide whether a vulnerability applies, and record why.
///
/// The rules are deliberately asymmetric. `NotApplicable` is only ever reached
/// through positive disconfirming evidence: a withdrawn advisory, or a version
/// that provably falls outside every affected range. Unreachable symbols,
/// absent first-party usage, and development-only scope are all recorded and
/// all de-rank the finding, but none of them can push it to `NotApplicable`,
/// because static reachability is incomplete in every language this tool reads
/// and "we found nothing" is not the same as "there is nothing".
pub fn assess_applicability(input: &ApplicabilityInput) -> ApplicabilityVerdict {
    let mut evidence = Vec::new();

    if input.advisory_withdrawn {
        evidence.push(ApplicabilityEvidence::new(
            EvidenceKind::AdvisoryWithdrawn,
            EvidenceDirection::Gate,
            "the publisher withdrew this advisory",
        ));
        return ApplicabilityVerdict {
            state: ApplicabilityState::NotApplicable,
            evidence,
            runtime_reachable: input.runtime_reachable,
        };
    }

    match &input.version_match {
        VersionMatch::Unaffected => {
            evidence.push(ApplicabilityEvidence::new(
                EvidenceKind::VersionOutsideAffectedRange,
                EvidenceDirection::Gate,
                "the resolved version falls outside every affected range",
            ));
            return ApplicabilityVerdict {
                state: ApplicabilityState::NotApplicable,
                evidence,
                runtime_reachable: input.runtime_reachable,
            };
        }
        VersionMatch::Undetermined(reason) => {
            evidence.push(ApplicabilityEvidence::new(
                EvidenceKind::VersionUndetermined,
                EvidenceDirection::Gate,
                format!("the version could not be ordered against the advisory: {reason}"),
            ));
        }
        VersionMatch::Affected => {
            evidence.push(ApplicabilityEvidence::new(
                EvidenceKind::VersionInAffectedRange,
                EvidenceDirection::Gate,
                "the resolved version falls inside an affected range",
            ));
        }
    }

    if !input.runtime_reachable {
        evidence.push(ApplicabilityEvidence::new(
            EvidenceKind::DevelopmentOnlyDependency,
            EvidenceDirection::LowersPriority,
            "reached only through development, build, or test dependencies",
        ));
    } else if !input.scope_recorded {
        evidence.push(ApplicabilityEvidence::new(
            EvidenceKind::DependencyScopeUnrecorded,
            EvidenceDirection::Informational,
            "nothing records what this package is needed for, so runtime use is assumed",
        ));
    }
    if input.feature_gated {
        evidence.push(ApplicabilityEvidence::new(
            EvidenceKind::FeatureGated,
            EvidenceDirection::LowersPriority,
            "reached only through an optional dependency no enabled feature compiles",
        ));
    }
    if input.dynamic_hazard_present {
        evidence.push(ApplicabilityEvidence::new(
            EvidenceKind::DynamicHazard,
            EvidenceDirection::Informational,
            "dynamic dispatch or reflection nearby, so static reachability is incomplete",
        ));
    }
    if input.first_party_usage_observed {
        evidence.push(ApplicabilityEvidence::new(
            EvidenceKind::FirstPartyUsageObserved,
            EvidenceDirection::Raises,
            "first-party code imports or calls this package",
        ));
    } else {
        evidence.push(ApplicabilityEvidence::new(
            EvidenceKind::NoFirstPartyUsage,
            EvidenceDirection::LowersPriority,
            "no first-party usage observed; this bounds nothing on its own",
        ));
    }

    // An undetermined version gate can never be raised to Applicable: without
    // knowing the version is in range, reachability proves only that code is
    // reached, not that it is vulnerable.
    let version_confirmed = matches!(input.version_match, VersionMatch::Affected);

    let state = if input.advisory_functions.is_empty() {
        evidence.push(ApplicabilityEvidence::new(
            EvidenceKind::AdvisoryNamesNoFunctions,
            EvidenceDirection::Informational,
            "the advisory names no vulnerable functions, so reachability is undecidable from it",
        ));
        if version_confirmed
            && input.is_direct_dependency
            && input.runtime_reachable
            && input.first_party_usage_observed
        {
            ApplicabilityState::Applicable
        } else {
            ApplicabilityState::ReviewRequired
        }
    } else if input.reachable_functions.is_empty() {
        evidence.push(ApplicabilityEvidence::new(
            EvidenceKind::SymbolUnreachable,
            EvidenceDirection::LowersPriority,
            "no advisory-named function appears reachable from first-party code",
        ));
        ApplicabilityState::ReviewRequired
    } else {
        evidence.push(ApplicabilityEvidence::new(
            EvidenceKind::SymbolReachable,
            EvidenceDirection::Raises,
            format!(
                "reachable advisory-named functions: {}",
                input.reachable_functions.join(", ")
            ),
        ));
        if version_confirmed {
            ApplicabilityState::Applicable
        } else {
            ApplicabilityState::ReviewRequired
        }
    };

    ApplicabilityVerdict {
        state,
        evidence,
        runtime_reachable: input.runtime_reachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(verdict: &ApplicabilityVerdict) -> Vec<EvidenceKind> {
        verdict.evidence.iter().map(|item| item.kind).collect()
    }

    #[test]
    fn a_withdrawn_advisory_is_not_applicable() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            advisory_withdrawn: true,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert_eq!(verdict.state, ApplicabilityState::NotApplicable);
        assert!(kinds(&verdict).contains(&EvidenceKind::AdvisoryWithdrawn));
    }

    #[test]
    fn a_version_outside_every_range_is_not_applicable() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Unaffected,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert_eq!(verdict.state, ApplicabilityState::NotApplicable);
        assert!(kinds(&verdict).contains(&EvidenceKind::VersionOutsideAffectedRange));
    }

    #[test]
    fn an_undetermined_version_needs_review_rather_than_being_dismissed() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Undetermined("cannot order".into()),
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert_eq!(verdict.state, ApplicabilityState::ReviewRequired);
        assert!(kinds(&verdict).contains(&EvidenceKind::VersionUndetermined));
    }

    #[test]
    fn a_reachable_vulnerable_function_makes_the_finding_applicable() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            advisory_functions: vec!["example::vulnerable".into()],
            reachable_functions: vec!["example::vulnerable".into()],
            first_party_usage_observed: true,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert_eq!(verdict.state, ApplicabilityState::Applicable);
        assert!(kinds(&verdict).contains(&EvidenceKind::SymbolReachable));
    }

    #[test]
    fn named_functions_that_look_unreachable_still_require_review() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            advisory_functions: vec!["example::vulnerable".into()],
            reachable_functions: Vec::new(),
            first_party_usage_observed: true,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert_eq!(
            verdict.state,
            ApplicabilityState::ReviewRequired,
            "unreachability is not proof of safety"
        );
        assert!(kinds(&verdict).contains(&EvidenceKind::SymbolUnreachable));
    }

    #[test]
    fn a_used_direct_runtime_dependency_with_no_function_data_is_applicable() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            advisory_functions: Vec::new(),
            first_party_usage_observed: true,
            is_direct_dependency: true,
            runtime_reachable: true,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert_eq!(verdict.state, ApplicabilityState::Applicable);
        assert!(kinds(&verdict).contains(&EvidenceKind::AdvisoryNamesNoFunctions));
        assert!(kinds(&verdict).contains(&EvidenceKind::FirstPartyUsageObserved));
    }

    #[test]
    fn a_transitive_dependency_with_no_function_data_requires_review() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            advisory_functions: Vec::new(),
            first_party_usage_observed: false,
            is_direct_dependency: false,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert_eq!(verdict.state, ApplicabilityState::ReviewRequired);
    }

    #[test]
    fn absent_first_party_usage_never_produces_not_applicable() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            first_party_usage_observed: false,
            is_direct_dependency: true,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert_eq!(verdict.state, ApplicabilityState::ReviewRequired);
        assert!(kinds(&verdict).contains(&EvidenceKind::NoFirstPartyUsage));
    }

    #[test]
    fn a_development_only_dependency_is_recorded_but_still_needs_review() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            runtime_reachable: false,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert_eq!(
            verdict.state,
            ApplicabilityState::ReviewRequired,
            "build and test dependencies are a real supply-chain surface"
        );
        assert!(!verdict.runtime_reachable);
        assert!(kinds(&verdict).contains(&EvidenceKind::DevelopmentOnlyDependency));
    }

    #[test]
    fn a_dynamic_hazard_is_recorded_alongside_an_unreachable_symbol() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            advisory_functions: vec!["example::vulnerable".into()],
            reachable_functions: Vec::new(),
            dynamic_hazard_present: true,
            first_party_usage_observed: true,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert_eq!(verdict.state, ApplicabilityState::ReviewRequired);
        assert!(kinds(&verdict).contains(&EvidenceKind::DynamicHazard));
    }

    #[test]
    fn every_verdict_records_the_version_gate_it_passed() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert!(kinds(&verdict).contains(&EvidenceKind::VersionInAffectedRange));
        assert!(!verdict.evidence.is_empty());
    }

    #[test]
    fn the_version_gate_is_recorded_as_a_gate() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);
        let gate = verdict
            .evidence
            .iter()
            .find(|item| item.kind == EvidenceKind::VersionInAffectedRange)
            .expect("gate evidence is always present");

        assert_eq!(gate.direction, EvidenceDirection::Gate);
    }

    #[test]
    fn no_input_combination_yields_not_applicable_without_a_gate_reason() {
        // Exhaustive sweep of the boolean inputs. NotApplicable must only ever
        // come from a withdrawn advisory or a version outside every range.
        //
        // This is the crate's central invariant, so the sweep is enumerated as
        // a bitmask rather than nested loops: every new input costs one line
        // here, which makes it cheap to keep exhaustive as signals are added.
        // Skipping that is how a de-ranking signal quietly becomes a dismissing
        // one.
        const INPUTS: u32 = 7;
        for combination in 0..(1 << INPUTS) {
            let bit = |index: u32| combination & (1 << index) != 0;
            for functions in [Vec::new(), vec!["f".to_string()]] {
                for reachable in [Vec::new(), vec!["f".to_string()]] {
                    let input = ApplicabilityInput {
                        version_match: VersionMatch::Affected,
                        advisory_withdrawn: bit(0),
                        advisory_functions: functions.clone(),
                        reachable_functions: reachable.clone(),
                        first_party_usage_observed: bit(1),
                        is_direct_dependency: bit(2),
                        runtime_reachable: bit(3),
                        scope_recorded: bit(4),
                        feature_gated: bit(5),
                        dynamic_hazard_present: bit(6),
                    };

                    let verdict = assess_applicability(&input);

                    if verdict.state == ApplicabilityState::NotApplicable {
                        assert!(
                            input.advisory_withdrawn,
                            "NotApplicable reached without a gate reason: {input:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_unrecorded_scope_is_reported_as_an_assumption() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            runtime_reachable: true,
            scope_recorded: false,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);
        let evidence = verdict
            .evidence
            .iter()
            .find(|item| item.kind == EvidenceKind::DependencyScopeUnrecorded)
            .expect("an assumed scope is recorded as one");

        assert_eq!(evidence.direction, EvidenceDirection::Informational);
        assert!(verdict.runtime_reachable, "the assumption still stands");
    }

    #[test]
    fn a_recorded_runtime_scope_is_not_reported_as_an_assumption() {
        let input = ApplicabilityInput {
            version_match: VersionMatch::Affected,
            runtime_reachable: true,
            scope_recorded: true,
            ..Default::default()
        };

        let verdict = assess_applicability(&input);

        assert!(!kinds(&verdict).contains(&EvidenceKind::DependencyScopeUnrecorded));
    }
}
