use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// One pinned historical or synthetic migration replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalCaseObservation {
    pub case_id: String,
    pub breaking_change_expected: bool,
    pub breaking_change_detected: bool,
    pub applicable_expected: bool,
    pub applicable_predicted: bool,
    #[serde(default)]
    pub expected_observations: Vec<String>,
    #[serde(default)]
    pub observed_observations: Vec<String>,
    #[serde(default)]
    pub expected_identities: Vec<String>,
    #[serde(default)]
    pub observed_identities: Vec<String>,
    #[serde(default)]
    pub expected_modeled_surfaces: Vec<String>,
    #[serde(default)]
    pub modeled_surfaces: Vec<String>,
    #[serde(default)]
    pub expected_monitored_surfaces: Vec<String>,
    #[serde(default)]
    pub monitored_surfaces: Vec<String>,
    #[serde(default)]
    pub expected_usage_sites: Vec<String>,
    #[serde(default)]
    pub observed_usage_sites: Vec<String>,
    #[serde(default)]
    pub expected_files: Vec<String>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub expected_tests: Vec<String>,
    #[serde(default)]
    pub selected_tests: Vec<String>,
    pub first_attempt_passed: bool,
    pub three_attempt_passed: bool,
    pub graph_invariants_passed: bool,
    pub duplicate_prs: usize,
    #[serde(default)]
    pub detection_millis: u64,
    #[serde(default = "true_by_default")]
    pub repair_verified: bool,
    pub context_bytes: u64,
    pub runtime_millis: u64,
    pub model_cost_microusd: u64,
}

const fn true_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoricalEvaluationReport {
    pub version: u32,
    pub case_count: usize,
    pub classification_precision: f64,
    pub classification_recall: f64,
    pub applicability_precision: f64,
    pub applicability_recall: f64,
    pub usage_localization_precision: f64,
    pub usage_localization_recall: f64,
    pub required_file_recall: f64,
    pub unrelated_file_rate: f64,
    pub relevant_test_recall: f64,
    pub first_attempt_patch_pass_rate: f64,
    pub three_attempt_patch_pass_rate: f64,
    pub graph_invariant_pass_rate: f64,
    pub duplicate_pr_rate: f64,
    pub observation_recall: f64,
    pub identity_precision: f64,
    pub modeled_coverage: f64,
    pub monitored_coverage: f64,
    pub binding_precision: f64,
    pub binding_recall: f64,
    pub event_precision: f64,
    pub test_recall: f64,
    pub median_detection_millis: u64,
    pub repair_verification_rate: f64,
    pub median_context_bytes: u64,
    pub median_runtime_millis: u64,
    pub median_model_cost_microusd: u64,
    pub launch_gate_passed: bool,
}

impl HistoricalEvaluationReport {
    pub fn from_observations(observations: &[HistoricalCaseObservation]) -> Self {
        let classification = confusion(
            observations
                .iter()
                .map(|case| (case.breaking_change_expected, case.breaking_change_detected)),
        );
        let applicability = confusion(
            observations
                .iter()
                .map(|case| (case.applicable_expected, case.applicable_predicted)),
        );
        let usage = set_scores(
            observations,
            |case| &case.expected_usage_sites,
            |case| &case.observed_usage_sites,
        );
        let observation = set_scores(
            observations,
            |case| &case.expected_observations,
            |case| &case.observed_observations,
        );
        let identity = set_scores(
            observations,
            |case| &case.expected_identities,
            |case| &case.observed_identities,
        );
        let modeled = set_scores(
            observations,
            |case| &case.expected_modeled_surfaces,
            |case| &case.modeled_surfaces,
        );
        let monitored = set_scores(
            observations,
            |case| &case.expected_monitored_surfaces,
            |case| &case.monitored_surfaces,
        );
        let files = set_scores(
            observations,
            |case| &case.expected_files,
            |case| &case.changed_files,
        );
        let tests = set_scores(
            observations,
            |case| &case.expected_tests,
            |case| &case.selected_tests,
        );
        let applicable = observations
            .iter()
            .filter(|case| case.applicable_expected)
            .collect::<Vec<_>>();
        let first_attempt_patch_pass_rate = fraction(
            applicable
                .iter()
                .filter(|case| case.first_attempt_passed)
                .count(),
            applicable.len(),
        );
        let three_attempt_patch_pass_rate = fraction(
            applicable
                .iter()
                .filter(|case| case.three_attempt_passed)
                .count(),
            applicable.len(),
        );
        let graph_invariant_pass_rate = fraction(
            applicable
                .iter()
                .filter(|case| case.graph_invariants_passed)
                .count(),
            applicable.len(),
        );
        let duplicate_count = observations.iter().map(|case| case.duplicate_prs).sum();
        let duplicate_pr_rate = fraction(duplicate_count, observations.len());
        let repair_verification_rate = fraction(
            applicable
                .iter()
                .filter(|case| case.repair_verified)
                .count(),
            applicable.len(),
        );
        let mut report = Self {
            version: 1,
            case_count: observations.len(),
            classification_precision: classification.precision(),
            classification_recall: classification.recall(),
            applicability_precision: applicability.precision(),
            applicability_recall: applicability.recall(),
            usage_localization_precision: usage.precision(),
            usage_localization_recall: usage.recall(),
            required_file_recall: files.recall(),
            unrelated_file_rate: fraction(files.false_positives, files.observed),
            relevant_test_recall: tests.recall(),
            first_attempt_patch_pass_rate,
            three_attempt_patch_pass_rate,
            graph_invariant_pass_rate,
            duplicate_pr_rate,
            observation_recall: observation.recall(),
            identity_precision: identity.precision(),
            modeled_coverage: modeled.recall(),
            monitored_coverage: monitored.recall(),
            binding_precision: usage.precision(),
            binding_recall: usage.recall(),
            event_precision: classification.precision(),
            test_recall: tests.recall(),
            median_detection_millis: median(observations.iter().map(|case| case.detection_millis)),
            repair_verification_rate,
            median_context_bytes: median(observations.iter().map(|case| case.context_bytes)),
            median_runtime_millis: median(observations.iter().map(|case| case.runtime_millis)),
            median_model_cost_microusd: median(
                observations.iter().map(|case| case.model_cost_microusd),
            ),
            launch_gate_passed: false,
        };
        report.launch_gate_passed = report.case_count >= 3
            && report.classification_precision >= 0.95
            && report.classification_recall >= 0.90
            && report.applicability_precision >= 0.95
            && report.usage_localization_precision >= 0.95
            && report.usage_localization_recall >= 0.90
            && report.required_file_recall >= 0.95
            && report.unrelated_file_rate <= 0.05
            && report.relevant_test_recall >= 0.95
            && report.three_attempt_patch_pass_rate >= 0.80
            && report.graph_invariant_pass_rate == 1.0
            && report.duplicate_pr_rate == 0.0;
        report.launch_gate_passed = report.launch_gate_passed
            && report.observation_recall >= 0.90
            && report.identity_precision >= 0.95
            && report.modeled_coverage >= 0.90
            && report.monitored_coverage >= 0.90
            && report.binding_precision >= 0.95
            && report.binding_recall >= 0.90
            && report.event_precision >= 0.95
            && report.test_recall >= 0.95
            && report.repair_verification_rate >= 0.80;
        report
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Confusion {
    true_positives: usize,
    false_positives: usize,
    false_negatives: usize,
}

impl Confusion {
    fn precision(self) -> f64 {
        fraction(
            self.true_positives,
            self.true_positives + self.false_positives,
        )
    }

    fn recall(self) -> f64 {
        fraction(
            self.true_positives,
            self.true_positives + self.false_negatives,
        )
    }
}

fn confusion(values: impl Iterator<Item = (bool, bool)>) -> Confusion {
    let mut result = Confusion::default();
    for (expected, observed) in values {
        match (expected, observed) {
            (true, true) => result.true_positives += 1,
            (false, true) => result.false_positives += 1,
            (true, false) => result.false_negatives += 1,
            (false, false) => {}
        }
    }
    result
}

#[derive(Debug, Clone, Copy, Default)]
struct SetScores {
    true_positives: usize,
    false_positives: usize,
    expected: usize,
    observed: usize,
}

impl SetScores {
    fn precision(self) -> f64 {
        fraction(self.true_positives, self.observed)
    }

    fn recall(self) -> f64 {
        fraction(self.true_positives, self.expected)
    }
}

fn set_scores(
    observations: &[HistoricalCaseObservation],
    expected: impl Fn(&HistoricalCaseObservation) -> &Vec<String>,
    observed: impl Fn(&HistoricalCaseObservation) -> &Vec<String>,
) -> SetScores {
    let mut score = SetScores::default();
    for case in observations {
        let expected = expected(case)
            .iter()
            .map(|value| format!("{}\0{value}", case.case_id))
            .collect::<BTreeSet<_>>();
        let observed = observed(case)
            .iter()
            .map(|value| format!("{}\0{value}", case.case_id))
            .collect::<BTreeSet<_>>();
        score.true_positives += expected.intersection(&observed).count();
        score.false_positives += observed.difference(&expected).count();
        score.expected += expected.len();
        score.observed += observed.len();
    }
    score
}

fn fraction(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn median(values: impl Iterator<Item = u64>) -> u64 {
    let mut values = values.collect::<Vec<_>>();
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() - 1) / 2]
}
