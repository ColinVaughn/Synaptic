use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use synaptic_api::PackageCoordinate;

/// Conventional location of the policy file inside a repository.
pub const DEFAULT_POLICY_PATH: &str = ".synaptic/vuln-policy.toml";

/// A package this organisation refuses regardless of advisories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenyRule {
    pub package: PackageCoordinate,
    pub reason: String,
    /// A package to use instead, offered to agents that ask for the denied one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub replacement: Option<String>,
}

/// A minimum version this organisation requires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinRule {
    pub package: PackageCoordinate,
    pub minimum: String,
    pub reason: String,
}

/// A time-boxed accepted risk.
///
/// The expiry is mandatory and is what stops an accepted risk from quietly
/// becoming permanent: once the date passes the finding returns to the active
/// set on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExceptionRule {
    pub finding: String,
    pub reason: String,
    /// ISO calendar date, `YYYY-MM-DD`.
    pub expires: String,
    pub approved_by: String,
}

impl ExceptionRule {
    /// Whether the exception still suppresses its finding on `today`.
    ///
    /// The expiry date is inclusive: an exception expiring today is still
    /// active today.
    pub fn is_active(&self, today: &str) -> bool {
        // Both sides are zero-padded `YYYY-MM-DD`, so byte order is date order.
        self.expires.as_str() >= today
    }
}

/// Repository vulnerability policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VulnPolicy {
    pub schema: u32,
    #[serde(default)]
    pub deny: Vec<DenyRule>,
    #[serde(default)]
    pub pin: Vec<PinRule>,
    #[serde(default)]
    pub exception: Vec<ExceptionRule>,
}

impl VulnPolicy {
    pub const SCHEMA: u32 = 1;

    /// Parse and validate a policy document.
    pub fn parse(source: &str) -> Result<Self, PolicyError> {
        let policy: Self = toml::from_str(source)?;
        if policy.schema != Self::SCHEMA {
            return Err(PolicyError::Schema {
                found: policy.schema,
                expected: Self::SCHEMA,
            });
        }
        for rule in &policy.exception {
            if !is_iso_date(&rule.expires) {
                return Err(PolicyError::InvalidExpiry {
                    finding: rule.finding.clone(),
                    expires: rule.expires.clone(),
                });
            }
        }
        Ok(policy)
    }

    /// Load the conventional policy file, if the repository has one.
    ///
    /// A repository without a policy is a normal, valid state, so an absent
    /// file is `Ok(None)` rather than an error.
    pub fn load(root: &Path) -> Result<Option<Self>, PolicyError> {
        let path = root.join(DEFAULT_POLICY_PATH);
        if !path.exists() {
            return Ok(None);
        }
        let source = std::fs::read_to_string(&path).map_err(|source| PolicyError::Io {
            path: path.clone(),
            source,
        })?;
        Self::parse(&source).map(Some)
    }

    /// Stable digest of the policy, recorded on every finding so a decision can
    /// be tied to the exact policy that produced it.
    pub fn digest(&self) -> String {
        let encoded = serde_json::to_vec(self).unwrap_or_default();
        blake3::hash(&encoded).to_hex().to_string()
    }

    pub fn deny_rule(&self, package: &PackageCoordinate) -> Option<&DenyRule> {
        self.deny.iter().find(|rule| &rule.package == package)
    }

    pub fn pin_rule(&self, package: &PackageCoordinate) -> Option<&PinRule> {
        self.pin.iter().find(|rule| &rule.package == package)
    }

    /// The unexpired exception for a finding, if one exists.
    pub fn active_exception(&self, finding_id: &str, today: &str) -> Option<&ExceptionRule> {
        self.exception
            .iter()
            .find(|rule| rule.finding == finding_id && rule.is_active(today))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("policy is not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("policy schema {found} is not supported (expected {expected})")]
    Schema { found: u32, expected: u32 },
    #[error("exception for {finding} has an invalid expiry {expires:?}; use YYYY-MM-DD")]
    InvalidExpiry { finding: String, expires: String },
    #[error("cannot read policy at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Whether a string is a plausible `YYYY-MM-DD` calendar date.
fn is_iso_date(value: &str) -> bool {
    let parts = value.split('-').collect::<Vec<_>>();
    if parts.len() != 3 {
        return false;
    }
    let widths = [4, 2, 2];
    for (part, width) in parts.iter().zip(widths) {
        if part.len() != width || !part.chars().all(|ch| ch.is_ascii_digit()) {
            return false;
        }
    }
    let month = parts[1].parse::<u32>().unwrap_or(0);
    let day = parts[2].parse::<u32>().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaptic_api::Ecosystem;

    const SAMPLE: &str = r#"
schema = 1

[[deny]]
package = "npm:request"
reason = "unmaintained"
replacement = "npm:undici"

[[pin]]
package = "cargo:example-crate"
minimum = "0.10.66"
reason = "organisation floor"

[[exception]]
finding = "vuln_finding_abc"
reason = "vulnerable path unreachable"
expires = "2026-11-01"
approved_by = "security-review"
"#;

    #[test]
    fn parses_deny_pin_and_exception_rules() {
        let policy = VulnPolicy::parse(SAMPLE).unwrap();

        assert_eq!(policy.deny.len(), 1);
        assert_eq!(policy.deny[0].package.to_string(), "npm:request");
        assert_eq!(policy.deny[0].replacement.as_deref(), Some("npm:undici"));
        assert_eq!(policy.pin[0].minimum, "0.10.66");
        assert_eq!(policy.exception[0].approved_by, "security-review");
    }

    #[test]
    fn an_unsupported_schema_is_rejected() {
        let error = VulnPolicy::parse("schema = 99").unwrap_err();

        assert!(matches!(error, PolicyError::Schema { found: 99, .. }));
    }

    #[test]
    fn an_exception_without_a_valid_expiry_is_rejected() {
        let source = r#"
schema = 1

[[exception]]
finding = "vuln_finding_abc"
reason = "later"
expires = "soon"
approved_by = "someone"
"#;

        let error = VulnPolicy::parse(source).unwrap_err();

        assert!(matches!(error, PolicyError::InvalidExpiry { .. }));
    }

    #[test]
    fn an_exception_is_active_up_to_and_including_its_expiry_date() {
        let policy = VulnPolicy::parse(SAMPLE).unwrap();
        let rule = &policy.exception[0];

        assert!(rule.is_active("2026-10-31"));
        assert!(rule.is_active("2026-11-01"), "expiry is inclusive");
        assert!(!rule.is_active("2026-11-02"));
    }

    #[test]
    fn an_expired_exception_stops_suppressing_its_finding() {
        let policy = VulnPolicy::parse(SAMPLE).unwrap();

        assert!(policy
            .active_exception("vuln_finding_abc", "2026-08-05")
            .is_some());
        assert!(
            policy
                .active_exception("vuln_finding_abc", "2027-01-01")
                .is_none(),
            "an accepted risk must not become permanent by default"
        );
    }

    #[test]
    fn looks_up_rules_by_normalized_package_coordinate() {
        let policy = VulnPolicy::parse(SAMPLE).unwrap();

        let denied = policy.deny_rule(&PackageCoordinate::new(Ecosystem::Npm, "REQUEST"));
        let pinned = policy.pin_rule(&PackageCoordinate::new(Ecosystem::Cargo, "example-crate"));

        assert!(denied.is_some(), "coordinate lookup must normalize case");
        assert!(pinned.is_some());
    }

    #[test]
    fn the_digest_changes_when_the_policy_changes() {
        let first = VulnPolicy::parse(SAMPLE).unwrap();
        let second = VulnPolicy::parse("schema = 1").unwrap();

        assert_eq!(first.digest(), VulnPolicy::parse(SAMPLE).unwrap().digest());
        assert_ne!(first.digest(), second.digest());
    }

    #[test]
    fn a_repository_without_a_policy_file_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(VulnPolicy::load(dir.path()).unwrap(), None);
    }

    #[test]
    fn loads_the_policy_from_the_conventional_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_POLICY_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, SAMPLE).unwrap();

        let policy = VulnPolicy::load(dir.path()).unwrap().expect("policy loads");

        assert_eq!(policy.deny.len(), 1);
    }

    #[test]
    fn an_empty_policy_is_valid() {
        let policy = VulnPolicy::parse("schema = 1").unwrap();

        assert!(policy.deny.is_empty());
        assert!(policy.pin.is_empty());
        assert!(policy.exception.is_empty());
    }
}
