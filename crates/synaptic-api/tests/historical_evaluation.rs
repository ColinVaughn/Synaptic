use synaptic_api::{HistoricalCaseObservation, HistoricalEvaluationReport};

fn case(id: &str) -> HistoricalCaseObservation {
    HistoricalCaseObservation {
        case_id: id.into(),
        breaking_change_expected: true,
        breaking_change_detected: true,
        applicable_expected: true,
        applicable_predicted: true,
        expected_observations: vec!["stripe".into()],
        observed_observations: vec!["stripe".into()],
        expected_identities: vec!["stripe".into()],
        observed_identities: vec!["stripe".into()],
        expected_modeled_surfaces: vec!["stripe.customers.create".into()],
        modeled_surfaces: vec!["stripe.customers.create".into()],
        expected_monitored_surfaces: vec!["stripe.customers.create".into()],
        monitored_surfaces: vec!["stripe.customers.create".into()],
        expected_usage_sites: vec!["src/client.ts:4".into()],
        observed_usage_sites: vec!["src/client.ts:4".into()],
        expected_files: vec!["src/client.ts".into()],
        changed_files: vec!["src/client.ts".into()],
        expected_tests: vec!["tests/client.test.ts".into()],
        selected_tests: vec!["tests/client.test.ts".into()],
        first_attempt_passed: true,
        three_attempt_passed: true,
        graph_invariants_passed: true,
        duplicate_prs: 0,
        detection_millis: 250,
        repair_verified: true,
        context_bytes: 8_000,
        runtime_millis: 1_000,
        model_cost_microusd: 25_000,
    }
}

#[test]
fn historical_report_computes_every_launch_metric_deterministically() {
    let mut observations = vec![
        case("stripe-node"),
        case("stripe-python"),
        case("pager-node"),
    ];
    let mut negative = case("unused-sdk");
    negative.breaking_change_expected = false;
    negative.breaking_change_detected = false;
    negative.applicable_expected = false;
    negative.applicable_predicted = false;
    negative.expected_observations.clear();
    negative.observed_observations.clear();
    negative.expected_identities.clear();
    negative.observed_identities.clear();
    negative.expected_modeled_surfaces.clear();
    negative.modeled_surfaces.clear();
    negative.expected_monitored_surfaces.clear();
    negative.monitored_surfaces.clear();
    negative.expected_usage_sites.clear();
    negative.observed_usage_sites.clear();
    negative.expected_files.clear();
    negative.changed_files.clear();
    negative.expected_tests.clear();
    negative.selected_tests.clear();
    negative.first_attempt_passed = false;
    negative.three_attempt_passed = false;
    negative.graph_invariants_passed = false;
    observations.push(negative);

    let first = HistoricalEvaluationReport::from_observations(&observations);
    let second = HistoricalEvaluationReport::from_observations(&observations);
    assert_eq!(first, second);
    assert_eq!(first.case_count, 4);
    assert_eq!(first.classification_precision, 1.0);
    assert_eq!(first.classification_recall, 1.0);
    assert_eq!(first.applicability_precision, 1.0);
    assert_eq!(first.usage_localization_precision, 1.0);
    assert_eq!(first.required_file_recall, 1.0);
    assert_eq!(first.unrelated_file_rate, 0.0);
    assert_eq!(first.relevant_test_recall, 1.0);
    assert_eq!(first.first_attempt_patch_pass_rate, 1.0);
    assert_eq!(first.three_attempt_patch_pass_rate, 1.0);
    assert_eq!(first.graph_invariant_pass_rate, 1.0);
    assert_eq!(first.duplicate_pr_rate, 0.0);
    assert_eq!(first.observation_recall, 1.0);
    assert_eq!(first.identity_precision, 1.0);
    assert_eq!(first.modeled_coverage, 1.0);
    assert_eq!(first.monitored_coverage, 1.0);
    assert_eq!(first.binding_precision, 1.0);
    assert_eq!(first.binding_recall, 1.0);
    assert_eq!(first.event_precision, 1.0);
    assert_eq!(first.test_recall, 1.0);
    assert_eq!(first.median_detection_millis, 250);
    assert_eq!(first.repair_verification_rate, 1.0);
    assert_eq!(first.median_context_bytes, 8_000);
    assert_eq!(first.median_runtime_millis, 1_000);
    assert_eq!(first.median_model_cost_microusd, 25_000);
    assert!(first.launch_gate_passed);
}

#[test]
fn launch_gate_favors_precision_and_duplicate_safety() {
    let mut observations = vec![case("safe")];
    let mut noisy = case("noise");
    noisy.applicable_expected = false;
    noisy.applicable_predicted = true;
    noisy.expected_usage_sites.clear();
    noisy.observed_usage_sites = vec!["src/unrelated.ts:1".into()];
    noisy.expected_files.clear();
    noisy.changed_files = vec!["src/unrelated.ts".into()];
    noisy.duplicate_prs = 1;
    observations.push(noisy);

    let report = HistoricalEvaluationReport::from_observations(&observations);
    assert!(report.applicability_precision < 0.95);
    assert!(report.unrelated_file_rate > 0.0);
    assert!(report.duplicate_pr_rate > 0.0);
    assert!(!report.launch_gate_passed);
}
