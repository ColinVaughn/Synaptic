use std::path::Path;

use synaptic_api::{
    build_coordination_plan, credential_scope_for_stage, execute_worker_attempt, BoundedJobQueue,
    CancellationToken, CredentialScope, HostedApiJob, JobStage, QueueError, RepositoryImpact,
    RetryPolicy, WorkerAttemptOutcome, WorkerEvent, WorkerEventSink, WorkerJobRunner,
};

fn job(tenant: &str, repository: &str) -> HostedApiJob {
    HostedApiJob::new(
        tenant,
        repository,
        "0123456789abcdef",
        "api_event_123",
        "policy_456",
    )
}

#[test]
fn queue_is_bounded_idempotent_and_tenant_partitioned() {
    let queue = BoundedJobQueue::new(2).unwrap();
    let first = job("tenant-a", "org/payments");
    let second = job("tenant-b", "org/billing");

    assert!(queue.enqueue(first.clone()).unwrap());
    assert!(
        !queue.enqueue(first).unwrap(),
        "duplicate job must be a no-op"
    );
    assert!(queue.enqueue(second.clone()).unwrap());
    assert_eq!(
        queue.enqueue(job("tenant-a", "org/third")),
        Err(QueueError::Full)
    );

    assert_eq!(
        queue.claim("tenant-a").unwrap().repository_identity,
        "org/payments"
    );
    assert!(queue.claim("tenant-a").is_none());
    assert_eq!(queue.claim("tenant-b"), Some(second));
}

#[test]
fn retry_and_cancellation_are_scheduler_friendly_and_observable() {
    struct Fails;
    impl WorkerJobRunner for Fails {
        fn run(
            &self,
            _job: &HostedApiJob,
            _cancellation: &CancellationToken,
        ) -> Result<(), String> {
            Err("temporary token=secret-value failure".into())
        }
    }

    #[derive(Default)]
    struct Events(std::sync::Mutex<Vec<WorkerEvent>>);
    impl WorkerEventSink for Events {
        fn record(&self, event: WorkerEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay_seconds: 5,
        max_delay_seconds: 30,
    };
    let events = Events::default();
    let cancellation = CancellationToken::new();
    let result = execute_worker_attempt(
        &job("tenant-a", "org/payments"),
        1,
        &policy,
        &cancellation,
        &Fails,
        &events,
    );
    assert_eq!(
        result,
        WorkerAttemptOutcome::RetryScheduled { after_seconds: 5 }
    );
    let recorded = events.0.lock().unwrap();
    assert_eq!(recorded.len(), 2);
    assert!(!recorded[1].message.contains("secret-value"));
    drop(recorded);

    cancellation.cancel();
    assert_eq!(
        execute_worker_attempt(
            &job("tenant-a", "org/payments"),
            2,
            &policy,
            &cancellation,
            &Fails,
            &events,
        ),
        WorkerAttemptOutcome::Cancelled
    );
}

#[test]
fn credentials_and_workspaces_are_scoped_to_one_stage_and_repository() {
    let job = job("tenant-a", "org/payments");
    assert_eq!(
        credential_scope_for_stage(&job, JobStage::Fetch),
        CredentialScope::None
    );
    assert_eq!(
        credential_scope_for_stage(&job, JobStage::Repair),
        CredentialScope::None
    );
    assert_eq!(
        credential_scope_for_stage(&job, JobStage::Test),
        CredentialScope::None
    );
    assert_eq!(
        credential_scope_for_stage(&job, JobStage::Publish),
        CredentialScope::RepositoryWrite {
            tenant_id: "tenant-a".into(),
            repository_identity: "org/payments".into(),
        }
    );

    assert!(job
        .validate_workspace(
            Path::new("C:/workers/tenant-a/org-payments"),
            Path::new("C:/workers/tenant-a/org-payments/run-1"),
        )
        .is_ok());
    assert!(job
        .validate_workspace(
            Path::new("C:/workers/tenant-a/org-payments"),
            Path::new("C:/workers/tenant-b/org-billing"),
        )
        .is_err());
}

#[test]
fn federated_impacts_become_separate_repository_jobs_and_tenant_groups() {
    let plan = build_coordination_plan(
        "api_event_123",
        vec![
            RepositoryImpact::new("tenant-a", "org/web", vec!["node:web".into()]),
            RepositoryImpact::new("tenant-a", "org/api", vec!["node:api".into()]),
            RepositoryImpact::new("tenant-b", "other/worker", vec!["node:worker".into()]),
        ],
    )
    .unwrap();

    assert_eq!(plan.repositories.len(), 3);
    assert!(plan
        .repositories
        .iter()
        .all(|repo| !repo.seed_node_ids.is_empty()));
    assert_ne!(
        plan.repositories[0].coordination_group, plan.repositories[2].coordination_group,
        "coordination groups must not cross tenants"
    );
    assert_eq!(
        build_coordination_plan(
            "api_event_123",
            vec![
                RepositoryImpact::new("tenant-a", "org/web", vec!["one".into()]),
                RepositoryImpact::new("tenant-a", "org/web", vec!["two".into()]),
            ],
        )
        .unwrap_err()
        .to_string(),
        "duplicate repository impact for tenant-a/org/web"
    );
}
