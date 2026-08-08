use serde::{Deserialize, Serialize};
use synaptic_api::PackageCoordinate;

use crate::applicability::ApplicabilityVerdict;
use crate::lockgraph::PackageKey;
use crate::plan::RemediationPlan;
use crate::reach::{CallSite, EntryPoint, RemediationScope};
use crate::severity::{Priority, SeverityAssessment};

/// One vulnerability, as it applies to one resolved package in one repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub version: u32,
    pub id: String,
    pub advisory_id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<String>,
    pub package: PackageCoordinate,
    pub resolved_version: String,
    /// Chain from a workspace member down to the affected package. Empty when
    /// the package is present in the lockfile but reachable from no root.
    #[serde(default)]
    pub dependency_path: Vec<PackageKey>,
    pub is_direct_dependency: bool,
    pub verdict: ApplicabilityVerdict,
    pub severity: SeverityAssessment,
    pub priority: Priority,
    pub remediation: RemediationPlan,
    #[serde(default)]
    pub references: Vec<String>,
    /// Concrete first-party locations that reach the vulnerable package.
    ///
    /// Empty means none were shown, which happens both when nothing calls the
    /// package and when no graph was available. Read `scope.graph_backed`
    /// before concluding anything from emptiness.
    #[serde(default)]
    pub call_sites: Vec<CallSite>,
    /// Inbound surfaces shown to reach a call site.
    #[serde(default)]
    pub entry_points: Vec<EntryPoint>,
    /// How much first-party code the remediation puts in scope for review.
    #[serde(default)]
    pub scope: RemediationScope,
}

impl Finding {
    pub const VERSION: u32 = 1;
}

/// Content-addressed identity for a finding.
///
/// Keying on the repository, advisory, package and resolved version makes
/// rescanning idempotent: the same vulnerability in the same place keeps one
/// record that accumulates history, rather than creating a new one per scan.
pub fn finding_id(
    repository_identity: &str,
    advisory_id: &str,
    package: &PackageCoordinate,
    resolved_version: &str,
) -> String {
    let identity = format!("{repository_identity}\0{advisory_id}\0{package}\0{resolved_version}");
    let digest = blake3::hash(identity.as_bytes()).to_hex();
    format!("vuln_finding_{}", &digest[..24])
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaptic_api::Ecosystem;

    fn package() -> PackageCoordinate {
        PackageCoordinate::new(Ecosystem::Cargo, "example")
    }

    #[test]
    fn the_same_vulnerability_in_the_same_place_keeps_one_identity() {
        let first = finding_id("repo", "RUSTSEC-1", &package(), "1.0.0");
        let second = finding_id("repo", "RUSTSEC-1", &package(), "1.0.0");

        assert_eq!(first, second);
    }

    #[test]
    fn a_different_resolved_version_is_a_different_finding() {
        let first = finding_id("repo", "RUSTSEC-1", &package(), "1.0.0");
        let second = finding_id("repo", "RUSTSEC-1", &package(), "1.0.1");

        assert_ne!(first, second);
    }

    #[test]
    fn a_different_repository_is_a_different_finding() {
        let first = finding_id("repo-a", "RUSTSEC-1", &package(), "1.0.0");
        let second = finding_id("repo-b", "RUSTSEC-1", &package(), "1.0.0");

        assert_ne!(first, second);
    }

    #[test]
    fn identities_are_prefixed_so_they_are_recognisable_in_logs() {
        let id = finding_id("repo", "RUSTSEC-1", &package(), "1.0.0");

        assert!(id.starts_with("vuln_finding_"));
        assert_eq!(id.len(), "vuln_finding_".len() + 24);
    }
}
