use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use synaptic_api::{
    publish_verified_draft, CommandOutput, DraftPublishRequest, GateOutcome, GateResult,
    PublishAction, PublishCommandRunner, PublishError, RepairBrief, VerificationReport,
};

struct MockRunner {
    list_output: String,
    calls: Mutex<Vec<(String, Vec<String>, String)>>,
}

struct LocalGitRunner {
    gh_calls: Mutex<Vec<(Vec<String>, String)>>,
}

impl PublishCommandRunner for LocalGitRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        stdin: &str,
    ) -> Result<CommandOutput, PublishError> {
        if program == "gh" {
            self.gh_calls
                .lock()
                .unwrap()
                .push((args.to_vec(), stdin.into()));
            let stdout = if args.starts_with(&["pr".into(), "list".into()]) {
                "[]\n"
            } else {
                "https://github.example/pr/local-only\n"
            };
            return Ok(CommandOutput {
                success: true,
                stdout: stdout.into(),
                stderr: String::new(),
            });
        }

        let output = Command::new(program).args(args).current_dir(cwd).output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git must be available for publisher integration tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().into()
}

impl PublishCommandRunner for MockRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        _cwd: &Path,
        stdin: &str,
    ) -> Result<CommandOutput, PublishError> {
        self.calls
            .lock()
            .unwrap()
            .push((program.into(), args.to_vec(), stdin.into()));
        let stdout = if program == "git" && args.iter().any(|arg| arg == "status") {
            " M src/client.ts\n".into()
        } else if program == "gh" && args.starts_with(&["pr".into(), "list".into()]) {
            self.list_output.clone()
        } else if program == "gh" && args.iter().any(|arg| arg == "create") {
            "https://github.example/pr/7\n".into()
        } else {
            String::new()
        };
        Ok(CommandOutput {
            success: true,
            stdout,
            stderr: String::new(),
        })
    }
}

fn brief() -> RepairBrief {
    serde_json::from_value(serde_json::json!({
        "version":1,"id":"run_1","repository_identity":"repo","base_sha":"abc123",
        "event":{"version":1,"id":"event_123","vendor":"acme","release":"v2","occurred_at":1,"source":{"uri":"https://acme.example/openapi","revision":"v2","content_digest":"digest","fetched_at":1,"adapter_version":1,"evidence_kind":"openapi"},"changes":[]},
        "applicability":{"version":1,"event_id":"event_123","vendor":"acme","state":"applicable","reasons":["applicable"],"matched_change_ids":["change_1"],"bindings":[],"seed_node_ids":["api:customers"],"observed_versions":["1.5.0"]},
        "usage_bindings":[{"vendor":"acme","operation_node_id":"api:customers","caller_node_id":"fn:create_customer","source_file":"src/client.ts","source_location":"src/client.ts:12","sdk_package":"npm:acme","sdk_member":"customers.create","sdk_version":"1.5.0","basis":"sdk_symbol","confidence":0.98}],
        "impact":{"version":1,"seed_node_ids":["api:customers"],"blast_radius":[{"id":"fn:create_customer","label":"createCustomer","file":"src/client.ts","depth":1,"via_relation":"uses_api","community":7,"repository":"repo"}],"blast_radius_total":2,"at_risk_tests":[]},
        "official_evidence":[],"source_slices":[],"memory":[],"dynamic_hazards":["computed SDK member at src/dynamic.ts:4"],"allowed_files":["src/client.ts"],"required_tests":["tests/client.test.ts"],"verification":[{"gate":"api_usage_invariants","required":true,"description":"old binding removed and replacement present"}]
    })).unwrap()
}

fn verification() -> VerificationReport {
    VerificationReport::from_gates(vec![GateResult {
        gate: "all".into(),
        outcome: GateOutcome::Passed,
        detail: "passed token=publish-secret".into(),
        duration_ms: 1,
    }])
}

#[test]
fn verified_publish_commits_pushes_and_creates_one_draft_with_marker() {
    let runner = MockRunner {
        list_output: "[]".into(),
        calls: Mutex::new(Vec::new()),
    };
    let result = publish_verified_draft(
        &DraftPublishRequest {
            worktree: Path::new(".").into(),
            branch: "synaptic/api/acme/event_123".into(),
            brief: brief(),
            verification: verification(),
            labels: vec!["api-migration".into()],
            reviewers: vec!["platform".into()],
        },
        &runner,
    )
    .unwrap();
    assert_eq!(result.action, PublishAction::Created);
    let calls = runner.calls.lock().unwrap();
    assert!(calls
        .iter()
        .any(|(program, args, _)| program == "git" && args.iter().any(|arg| arg == "commit")));
    let create = calls
        .iter()
        .find(|(program, args, _)| program == "gh" && args.iter().any(|arg| arg == "create"))
        .unwrap();
    assert!(create.1.iter().any(|arg| arg == "--draft"));
    assert!(create
        .2
        .contains("<!-- synaptic-api-event:event_123 base:abc123 -->"));
    assert!(create.2.contains("src/client.ts:12"));
    assert!(create.2.contains("customers.create"));
    assert!(create.2.contains("Community `7`"));
    assert!(create
        .2
        .contains("old binding removed and replacement present"));
    assert!(create.2.contains("computed SDK member at src/dynamic.ts:4"));
    assert!(create.2.contains("human review is required"));
    assert!(!create.2.contains("publish-secret"));
}

#[test]
fn replay_updates_matching_pr_and_unverified_runs_cannot_publish() {
    let runner = MockRunner {
        list_output:r#"[{"number":12,"url":"https://github.example/pr/12","headRefName":"synaptic/api/acme/event_123","body":"<!-- synaptic-api-event:event_123 base:abc123 -->"}]"#.into(),
        calls:Mutex::new(Vec::new())
    };
    let request = DraftPublishRequest {
        worktree: Path::new(".").into(),
        branch: "synaptic/api/acme/event_123".into(),
        brief: brief(),
        verification: verification(),
        labels: vec![],
        reviewers: vec![],
    };
    let result = publish_verified_draft(&request, &runner).unwrap();
    assert_eq!(result.action, PublishAction::Updated);
    assert_eq!(result.number, Some(12));
    assert!(runner
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|(program, args, _)| program == "gh" && args.iter().any(|arg| arg == "edit")));

    let mut invalid = request;
    invalid.verification = VerificationReport::from_gates(vec![GateResult {
        gate: "tests".into(),
        outcome: GateOutcome::Inconclusive,
        detail: "none".into(),
        duration_ms: 0,
    }]);
    assert!(publish_verified_draft(&invalid, &runner).is_err());
}

#[test]
fn verified_publish_uses_real_git_against_a_local_remote_only() {
    let directory = tempfile::tempdir().unwrap();
    let remote = directory.path().join("remote.git");
    let worktree = directory.path().join("worktree");
    std::fs::create_dir_all(worktree.join("src")).unwrap();

    git(
        directory.path(),
        &["init", "--bare", remote.to_str().unwrap()],
    );
    git(directory.path(), &["init", worktree.to_str().unwrap()]);
    git(&worktree, &["config", "user.name", "Synaptic Test"]);
    git(
        &worktree,
        &["config", "user.email", "synaptic-test@example.invalid"],
    );
    std::fs::write(
        worktree.join("src/client.ts"),
        "export const version = 1;\n",
    )
    .unwrap();
    git(&worktree, &["add", "src/client.ts"]);
    git(&worktree, &["commit", "--no-gpg-sign", "-m", "base"]);
    git(
        &worktree,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let base_sha = git(&worktree, &["rev-parse", "HEAD"]);
    std::fs::write(
        worktree.join("src/client.ts"),
        "export const version = 2;\n",
    )
    .unwrap();

    let runner = LocalGitRunner {
        gh_calls: Mutex::new(Vec::new()),
    };
    let mut repair_brief = brief();
    repair_brief.base_sha = base_sha;
    let request = DraftPublishRequest {
        worktree: worktree.clone(),
        branch: "synaptic/api/acme/event_123".into(),
        brief: repair_brief,
        verification: verification(),
        labels: vec!["api-migration".into()],
        reviewers: vec![],
    };

    let result = publish_verified_draft(&request, &runner).unwrap();

    assert_eq!(result.action, PublishAction::Created);
    let local_head = git(&worktree, &["rev-parse", "HEAD"]);
    let remote_head = git(
        directory.path(),
        &[
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/synaptic/api/acme/event_123",
        ],
    );
    assert_eq!(remote_head, local_head);
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
    assert!(git(&worktree, &["log", "-1", "--format=%B"]).contains("Synaptic-API-Event: event_123"));

    let gh_calls = runner.gh_calls.lock().unwrap();
    let create = gh_calls
        .iter()
        .find(|(args, _)| args.starts_with(&["pr".into(), "create".into()]))
        .expect("draft PR create command");
    assert!(create.0.iter().any(|arg| arg == "--draft"));
    assert!(create.1.contains("<!-- synaptic-api-event:event_123 base:"));
}
