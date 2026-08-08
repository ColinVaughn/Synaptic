//! Hand a vulnerability finding to the repair loop.
//!
//! `synaptic-api` already knows how to generate a patch, inspect it against a
//! policy, run verification gates and retry on failure. It is driven by an
//! [`ApiChangeEvent`] plus a [`RelevanceAssessment`], and nothing built one of
//! those from a vulnerability, so findings stopped at a sentence of advice.
//! This module is that adapter.
//!
//! A required upgrade is modelled as [`BreakingChangeKind::MinimumSupportedVersionRaised`],
//! which is what it literally is: the lowest version this repository may
//! resolve has been raised. No other kind is invented for it, because the
//! repair loop reasons about the kind and a fabricated one would misdirect it.

use synaptic_api::{
    ApiBreakingChange, ApiChangeEvent, ApiUsageBinding, ApplicabilityReason, ApplicabilityState,
    BindingBasis, BreakingChangeKind, RelevanceAssessment, SdkSymbolAnchor, SourceArtifact,
    VersionRange,
};

use crate::finding::Finding;
use crate::plan::RemediationKind;

/// The pair `synaptic_api::build_repair_brief` needs to build a brief.
#[derive(Debug, Clone, PartialEq)]
pub struct RepairInputs {
    pub event: ApiChangeEvent,
    pub assessment: RelevanceAssessment,
}

/// Convert a finding into repair-loop inputs.
///
/// Returns `None` when the advisory offers no version to move to. The repair
/// loop generates and verifies a change; with no target there is no change to
/// generate, and emitting an event anyway would ask an agent to invent one.
///
/// `occurred_at` is supplied by the caller rather than read from the clock so
/// the same finding always converts to the same event, which is what lets the
/// repair ledger deduplicate runs.
pub fn repair_inputs(finding: &Finding, occurred_at: i64) -> Option<RepairInputs> {
    if finding.remediation.kind != RemediationKind::Upgrade {
        return None;
    }
    let target = finding.remediation.recommended_version.as_ref()?;
    let vendor = format!(
        "{}:{}",
        finding.package.ecosystem.as_str(),
        finding.package.name
    );
    let change_id = format!("{}-min-version", finding.advisory_id);

    // Everything below the fix is affected. Built from the target rather than
    // the advisory's own ranges so the event says exactly what the upgrade
    // must clear; an unparseable target degrades to `any` rather than
    // silently narrowing the range to nothing.
    let affected_versions =
        VersionRange::parse(&format!("<{target}")).unwrap_or_else(|_| VersionRange::any());

    let mut members: Vec<String> = finding
        .call_sites
        .iter()
        .map(|site| site.member.clone())
        .collect();
    members.sort();
    members.dedup();
    let old_sdk_symbols = members
        .into_iter()
        .map(|member| SdkSymbolAnchor {
            package: finding.package.name.clone(),
            member,
            signature: None,
        })
        .collect();

    let mut migration_summary = format!(
        "Raise {} from {} to {target} to clear {}.",
        finding.package.name, finding.resolved_version, finding.advisory_id
    );
    for change in &finding.remediation.required_changes {
        migration_summary.push(' ');
        migration_summary.push_str(change);
    }

    let event = ApiChangeEvent {
        version: ApiChangeEvent::VERSION,
        id: finding.id.clone(),
        vendor: vendor.clone(),
        release: Some(target.clone()),
        occurred_at,
        source: SourceArtifact {
            uri: finding
                .references
                .first()
                .cloned()
                .unwrap_or_else(|| format!("osv:{}", finding.advisory_id)),
            revision: finding.advisory_id.clone(),
            etag: None,
            last_modified: None,
            // The finding id is already a digest of repository, advisory,
            // package and version, so it identifies this content exactly.
            content_digest: finding.id.clone(),
            fetched_at: occurred_at,
            adapter_version: 1,
            evidence_kind: "osv_advisory".into(),
        },
        changes: vec![ApiBreakingChange {
            change_id: change_id.clone(),
            kind: BreakingChangeKind::MinimumSupportedVersionRaised,
            affected_versions,
            old_operation: None,
            new_operation: None,
            old_sdk_symbols,
            new_sdk_symbols: Vec::new(),
            migration_summary,
            evidence: Vec::new(),
            confidence: 1.0,
        }],
    };

    let bindings: Vec<ApiUsageBinding> = finding
        .call_sites
        .iter()
        .map(|site| ApiUsageBinding {
            vendor: vendor.clone(),
            operation_node_id: format!("{vendor}#{}", site.member),
            caller_node_id: site.symbol_id.clone(),
            source_file: site.file.clone(),
            source_location: site.line.map(|line| format!("L{line}")),
            sdk_package: Some(finding.package.name.clone()),
            sdk_member: Some(site.member.clone()),
            sdk_version: Some(finding.resolved_version.clone()),
            api_version: None,
            basis: BindingBasis::SdkSymbol,
            confidence: 1.0,
        })
        .collect();

    let mut seed_node_ids: Vec<String> = finding
        .call_sites
        .iter()
        .map(|site| site.symbol_id.clone())
        .collect();
    seed_node_ids.sort();
    seed_node_ids.dedup();

    // The state is carried across unchanged. Promoting a `ReviewRequired`
    // finding to `Applicable` here would let the repair loop patch something
    // this crate never showed was reachable.
    let reasons = match finding.verdict.state {
        ApplicabilityState::Applicable => vec![ApplicabilityReason::Applicable],
        _ if bindings.is_empty() => vec![ApplicabilityReason::NoObservedUsage],
        // No variant describes "reached, but not proven vulnerable", and
        // inventing one would misreport the evidence.
        _ => Vec::new(),
    };

    let assessment = RelevanceAssessment {
        version: 1,
        event_id: finding.id.clone(),
        vendor,
        state: finding.verdict.state,
        reasons,
        matched_change_ids: vec![change_id],
        bindings,
        seed_node_ids,
        observed_versions: vec![finding.resolved_version.clone()],
    };

    Some(RepairInputs { event, assessment })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applicability::{assess_applicability, ApplicabilityInput};
    use crate::matching::VersionMatch;
    use crate::plan::{CompatibilityRisk, RemediationPlan, VersionAvailability};
    use crate::reach::{CallSite, EntryPoint, EntryPointKind};
    use crate::severity::{Priority, SeverityAssessment, SeverityBand, SeverityScoreSource};
    use synaptic_api::{Ecosystem, PackageCoordinate};

    fn call_site() -> CallSite {
        CallSite {
            symbol: "handle_items()".into(),
            symbol_id: "handle_items".into(),
            file: "src/api.rs".into(),
            line: Some(31),
            member: "parse".into(),
        }
    }

    fn finding(kind: RemediationKind, recommended: Option<&str>, reachable: bool) -> Finding {
        let verdict = assess_applicability(&ApplicabilityInput {
            version_match: VersionMatch::Affected,
            advisory_functions: vec!["leaf::parse".into()],
            reachable_functions: if reachable {
                vec!["leaf::parse".into()]
            } else {
                Vec::new()
            },
            first_party_usage_observed: reachable,
            is_direct_dependency: true,
            runtime_reachable: true,
            scope_recorded: true,
            ..Default::default()
        });
        Finding {
            version: Finding::VERSION,
            id: "vuln_finding_abc".into(),
            advisory_id: "RUSTSEC-2026-0001".into(),
            aliases: vec!["CVE-2026-1111".into()],
            summary: Some("leaf is vulnerable".into()),
            package: PackageCoordinate::new(Ecosystem::Cargo, "leaf"),
            resolved_version: "0.9.18".into(),
            dependency_path: Vec::new(),
            is_direct_dependency: true,
            verdict,
            severity: SeverityAssessment {
                band: SeverityBand::High,
                base_score: Some(7.5),
                source: SeverityScoreSource::CvssV3Vector,
                vector: None,
            },
            priority: Priority::P1,
            remediation: RemediationPlan {
                kind,
                recommended_version: recommended.map(Into::into),
                availability: VersionAvailability::Unverified,
                compatibility_risk: CompatibilityRisk::Patch,
                required_changes: vec!["raise leaf to 0.9.20".into()],
                validation_commands: vec!["cargo test --workspace".into()],
                notes: Vec::new(),
            },
            references: vec!["https://rustsec.org/advisories/RUSTSEC-2026-0001".into()],
            call_sites: vec![call_site()],
            entry_points: vec![EntryPoint {
                kind: EntryPointKind::HttpRoute,
                label: "/items".into(),
                id: "route_items".into(),
                path: vec!["/items".into(), "handle_items()".into()],
            }],
            scope: Default::default(),
        }
    }

    #[test]
    fn an_upgradeable_finding_becomes_an_event_naming_the_package_and_target() {
        let inputs = repair_inputs(
            &finding(RemediationKind::Upgrade, Some("0.9.20"), true),
            1_700_000_000,
        )
        .expect("an upgradeable finding produces repair inputs");

        assert_eq!(inputs.event.vendor, "cargo:leaf");
        assert_eq!(inputs.event.release.as_deref(), Some("0.9.20"));
        assert_eq!(inputs.event.id, "vuln_finding_abc");
    }

    #[test]
    fn the_change_is_a_minimum_version_raise_carrying_the_named_symbols() {
        let inputs = repair_inputs(
            &finding(RemediationKind::Upgrade, Some("0.9.20"), true),
            1_700_000_000,
        )
        .expect("inputs");

        assert_eq!(inputs.event.changes.len(), 1);
        let change = &inputs.event.changes[0];
        assert_eq!(
            change.kind,
            BreakingChangeKind::MinimumSupportedVersionRaised
        );
        assert_eq!(
            change.old_sdk_symbols,
            vec![SdkSymbolAnchor {
                package: "leaf".into(),
                member: "parse".into(),
                signature: None,
            }]
        );
    }

    #[test]
    fn a_finding_with_no_fix_version_produces_no_repair_inputs() {
        assert!(
            repair_inputs(
                &finding(RemediationKind::NoFixAvailable, None, true),
                1_700_000_000
            )
            .is_none(),
            "there is no target to generate a patch toward"
        );
    }

    #[test]
    fn every_call_site_becomes_a_usage_binding() {
        let inputs = repair_inputs(
            &finding(RemediationKind::Upgrade, Some("0.9.20"), true),
            1_700_000_000,
        )
        .expect("inputs");

        assert_eq!(inputs.assessment.bindings.len(), 1);
        let binding = &inputs.assessment.bindings[0];
        assert_eq!(binding.caller_node_id, "handle_items");
        assert_eq!(binding.source_file, "src/api.rs");
        assert_eq!(binding.source_location.as_deref(), Some("L31"));
        assert_eq!(binding.sdk_member.as_deref(), Some("parse"));
        assert_eq!(binding.basis, BindingBasis::SdkSymbol);
    }

    #[test]
    fn the_assessment_carries_the_findings_applicability_state() {
        let applicable = repair_inputs(
            &finding(RemediationKind::Upgrade, Some("0.9.20"), true),
            1_700_000_000,
        )
        .expect("inputs");
        assert_eq!(
            applicable.assessment.state,
            ApplicabilityState::Applicable,
            "a reachable finding must reach the repair loop"
        );

        let unproven = repair_inputs(
            &finding(RemediationKind::Upgrade, Some("0.9.20"), false),
            1_700_000_000,
        )
        .expect("inputs");
        assert_eq!(
            unproven.assessment.state,
            ApplicabilityState::ReviewRequired,
            "an unproven finding must not be silently promoted to Applicable"
        );
    }

    #[test]
    fn the_seeds_are_the_call_site_symbols() {
        let inputs = repair_inputs(
            &finding(RemediationKind::Upgrade, Some("0.9.20"), true),
            1_700_000_000,
        )
        .expect("inputs");

        assert_eq!(inputs.assessment.seed_node_ids, vec!["handle_items"]);
    }

    #[test]
    fn the_event_cites_the_advisory_it_came_from() {
        let inputs = repair_inputs(
            &finding(RemediationKind::Upgrade, Some("0.9.20"), true),
            1_700_000_000,
        )
        .expect("inputs");

        assert_eq!(inputs.event.source.revision, "RUSTSEC-2026-0001");
        assert_eq!(inputs.event.source.evidence_kind, "osv_advisory");
    }

    #[test]
    fn an_undetermined_fix_version_produces_no_repair_inputs() {
        assert!(
            repair_inputs(
                &finding(RemediationKind::FixVersionUndetermined, None, true),
                1_700_000_000
            )
            .is_none(),
            "an unorderable target must not be guessed at"
        );
    }
}
