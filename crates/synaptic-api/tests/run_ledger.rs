use synaptic_api::{ApiRunStore, GateOutcome, GateResult, RunState, VerificationReport};

#[test]
fn run_keys_and_state_transitions_are_idempotent_and_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let store = ApiRunStore::new(root.path());
    let first = store.begin("repo", "base1", "event1", "policy1").unwrap();
    let replay = store.begin("repo", "base1", "event1", "policy1").unwrap();
    assert_eq!(first, replay);
    assert_ne!(
        first.id,
        store
            .begin("repo", "base2", "event1", "policy1")
            .unwrap()
            .id
    );

    let mut invalid = first.clone();
    assert!(store
        .transition(&mut invalid, RunState::PrOpen, None, None)
        .is_err());

    let mut run = first;
    store
        .transition(&mut run, RunState::Repairing, None, None)
        .unwrap();
    let verification = VerificationReport::from_gates(vec![GateResult {
        gate: "all".into(),
        outcome: GateOutcome::Passed,
        detail: "passed".into(),
        duration_ms: 1,
    }]);
    store
        .transition(&mut run, RunState::Verified, Some(verification), None)
        .unwrap();
    store
        .transition(
            &mut run,
            RunState::PrOpen,
            None,
            Some("https://github.example/pr/1".into()),
        )
        .unwrap();
    assert_eq!(store.load(&run.id).unwrap().state, RunState::PrOpen);
}

#[test]
fn concurrent_begin_reuses_one_valid_record() {
    let root = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(ApiRunStore::new(root.path()));
    let handles = (0..8)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || store.begin("repo", "base", "event", "policy").unwrap().id)
        })
        .collect::<Vec<_>>();
    let ids = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 1);
    assert_eq!(store.list().unwrap().len(), 1);
}
