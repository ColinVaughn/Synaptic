use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::{RepairBrief, VerificationReport};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait PublishCommandRunner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        stdin: &str,
    ) -> Result<CommandOutput, PublishError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPublishCommandRunner;

impl PublishCommandRunner for SystemPublishCommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        stdin: &str,
    ) -> Result<CommandOutput, PublishError> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if !stdin.is_empty() {
            child
                .stdin
                .take()
                .ok_or_else(|| PublishError::Command("command stdin unavailable".into()))?
                .write_all(stdin.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: bounded_output(&output.stdout),
            stderr: bounded_output(&output.stderr),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DraftPublishRequest {
    pub worktree: PathBuf,
    pub branch: String,
    pub brief: RepairBrief,
    pub verification: VerificationReport,
    pub labels: Vec<String>,
    pub reviewers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeRequestProvider {
    #[default]
    Github,
    Gitlab,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeRequestKind {
    #[default]
    PullRequest,
    MergeRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishContext {
    pub provider: ChangeRequestProvider,
    pub provider_base_url: String,
    pub repository_identity: String,
    pub target_branch: String,
}

impl Default for PublishContext {
    fn default() -> Self {
        Self {
            provider: ChangeRequestProvider::Github,
            provider_base_url: "https://github.com".into(),
            repository_identity: String::new(),
            target_branch: "main".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishAction {
    Created,
    Updated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishResult {
    pub version: u32,
    #[serde(default)]
    pub provider: ChangeRequestProvider,
    #[serde(default)]
    pub kind: ChangeRequestKind,
    pub action: PublishAction,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub number: Option<u64>,
    pub url: String,
    pub branch: String,
    pub marker: String,
}

pub fn deterministic_branch(vendor: &str, event_id: &str) -> Result<String, PublishError> {
    let vendor = safe_component(vendor)?;
    let event = safe_component(event_id)?;
    Ok(format!(
        "synaptic/api/{vendor}/{}",
        event.chars().take(16).collect::<String>()
    ))
}

pub fn publish_verified_draft(
    request: &DraftPublishRequest,
    runner: &dyn PublishCommandRunner,
) -> Result<PublishResult, PublishError> {
    publish_verified_change_request(request, &PublishContext::default(), runner)
}

pub fn publish_verified_change_request(
    request: &DraftPublishRequest,
    context: &PublishContext,
    runner: &dyn PublishCommandRunner,
) -> Result<PublishResult, PublishError> {
    if !request.verification.verified {
        return Err(PublishError::NotVerified);
    }
    validate_publish_context(context)?;
    let expected_branch =
        deterministic_branch(&request.brief.event.vendor, &request.brief.event.id)?;
    if request.branch != expected_branch {
        return Err(PublishError::Branch {
            expected: expected_branch,
            actual: request.branch.clone(),
        });
    }
    let target_reference = format!("refs/heads/{}", context.target_branch);
    let remote = run_checked(
        runner,
        "git",
        &[
            "ls-remote".into(),
            "--exit-code".into(),
            "--refs".into(),
            "origin".into(),
            target_reference.clone(),
        ],
        &request.worktree,
        "",
    )?;
    let lines = remote
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return Err(PublishError::UnsafeContext(
            "target branch lookup returned an ambiguous result".into(),
        ));
    }
    let mut fields = lines[0].split_whitespace();
    let remote_base = fields.next().unwrap_or_default();
    let remote_reference = fields.next().unwrap_or_default();
    if fields.next().is_some()
        || remote_reference != target_reference
        || !matches!(remote_base.len(), 40 | 64)
        || !remote_base
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(PublishError::UnsafeContext(
            "target branch lookup returned an invalid identity".into(),
        ));
    }
    if !remote_base.eq_ignore_ascii_case(&request.brief.base_sha) {
        return Err(PublishError::StaleBase {
            expected: request.brief.base_sha.clone(),
            actual: remote_base.to_ascii_lowercase(),
        });
    }
    let marker = format!(
        "<!-- synaptic-api-event:{} base:{} -->",
        request.brief.event.id, request.brief.base_sha
    );
    let title = format!(
        "Migrate {} API usage for {}",
        request.brief.event.vendor,
        request
            .brief
            .event
            .release
            .as_deref()
            .unwrap_or(&request.brief.event.id)
    );
    let body = crate::redaction::redact_sensitive_text(&render_pr_body(request, &marker));

    let status = run_checked(
        runner,
        "git",
        &["status".into(), "--porcelain".into()],
        &request.worktree,
        "",
    )?;
    if !status.stdout.trim().is_empty() {
        let mut add = vec!["add".into(), "--".into()];
        add.extend(request.brief.allowed_files.iter().cloned());
        run_checked(runner, "git", &add, &request.worktree, "")?;
        run_checked(
            runner,
            "git",
            &[
                "commit".into(),
                "--no-gpg-sign".into(),
                "-m".into(),
                title.clone(),
                "-m".into(),
                format!("Synaptic-API-Event: {}", request.brief.event.id),
            ],
            &request.worktree,
            "",
        )?;
    }
    run_checked(
        runner,
        "git",
        &[
            "push".into(),
            "--set-upstream".into(),
            "origin".into(),
            format!("HEAD:refs/heads/{}", request.branch),
        ],
        &request.worktree,
        "",
    )?;

    match context.provider {
        ChangeRequestProvider::Github => {
            publish_github_change_request(request, context, runner, marker, title, body)
        }
        ChangeRequestProvider::Gitlab => {
            publish_gitlab_change_request(request, context, runner, marker, title, body)
        }
    }
}

fn publish_github_change_request(
    request: &DraftPublishRequest,
    context: &PublishContext,
    runner: &dyn PublishCommandRunner,
    marker: String,
    title: String,
    body: String,
) -> Result<PublishResult, PublishError> {
    let mut list_args = vec![
        "pr".into(),
        "list".into(),
        "--state".into(),
        "open".into(),
        "--search".into(),
        marker.clone(),
        "--json".into(),
        "number,url,headRefName,body".into(),
    ];
    add_repository_argument(&mut list_args, context);

    let list = run_checked(runner, "gh", &list_args, &request.worktree, "")?;
    let candidates: Vec<ExistingPr> = serde_json::from_str(&list.stdout)?;
    let mut matching = candidates
        .into_iter()
        .filter(|pr| pr.head_ref_name == request.branch && pr.body.contains(&marker))
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        return Err(PublishError::DuplicatePr(matching.len()));
    }
    let labels = request.labels.join(",");
    let reviewers = request.reviewers.join(",");
    if let Some(existing) = matching.pop() {
        let mut args = vec![
            "pr".into(),
            "edit".into(),
            existing.number.to_string(),
            "--title".into(),
            title,
            "--body-file".into(),
            "-".into(),
        ];
        if !labels.is_empty() {
            args.extend(["--add-label".into(), labels]);
        }
        if !reviewers.is_empty() {
            args.extend(["--add-reviewer".into(), reviewers]);
        }
        add_repository_argument(&mut args, context);
        run_checked(runner, "gh", &args, &request.worktree, &body)?;
        Ok(PublishResult {
            version: 1,
            provider: ChangeRequestProvider::Github,
            kind: ChangeRequestKind::PullRequest,
            action: PublishAction::Updated,
            number: Some(existing.number),
            url: existing.url,
            branch: request.branch.clone(),
            marker,
        })
    } else {
        let mut args = vec![
            "pr".into(),
            "create".into(),
            "--draft".into(),
            "--head".into(),
            request.branch.clone(),
            "--base".into(),
            context.target_branch.clone(),
            "--title".into(),
            title,
            "--body-file".into(),
            "-".into(),
        ];
        if !labels.is_empty() {
            args.extend(["--label".into(), labels]);
        }
        if !reviewers.is_empty() {
            args.extend(["--reviewer".into(), reviewers]);
        }
        add_repository_argument(&mut args, context);
        let created = run_checked(runner, "gh", &args, &request.worktree, &body)?;
        let url = created.stdout.trim().to_string();
        Ok(PublishResult {
            version: 1,
            provider: ChangeRequestProvider::Github,
            kind: ChangeRequestKind::PullRequest,
            action: PublishAction::Created,
            number: change_request_number(&url),
            url,
            branch: request.branch.clone(),
            marker,
        })
    }
}

fn publish_gitlab_change_request(
    request: &DraftPublishRequest,
    context: &PublishContext,
    runner: &dyn PublishCommandRunner,
    marker: String,
    title: String,
    body: String,
) -> Result<PublishResult, PublishError> {
    let mut list_args = vec![
        "mr".into(),
        "list".into(),
        "--state".into(),
        "opened".into(),
        "--source-branch".into(),
        request.branch.clone(),
        "--output".into(),
        "json".into(),
    ];
    add_repository_argument(&mut list_args, context);
    let list = run_checked(runner, "glab", &list_args, &request.worktree, "")?;
    let candidates: Vec<ExistingMergeRequest> = serde_json::from_str(&list.stdout)?;
    let mut matching = candidates
        .into_iter()
        .filter(|change| {
            change.source_branch == request.branch && change.description.contains(&marker)
        })
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        return Err(PublishError::DuplicateChangeRequest(matching.len()));
    }
    let labels = request.labels.join(",");
    let reviewers = request.reviewers.join(",");
    if let Some(existing) = matching.pop() {
        let mut args = vec![
            "mr".into(),
            "update".into(),
            existing.iid.to_string(),
            "--title".into(),
            title,
            "--description-file".into(),
            "-".into(),
        ];
        if !labels.is_empty() {
            args.extend(["--label".into(), labels]);
        }
        if !reviewers.is_empty() {
            args.extend(["--reviewer".into(), reviewers]);
        }
        add_repository_argument(&mut args, context);
        run_checked(runner, "glab", &args, &request.worktree, &body)?;
        Ok(PublishResult {
            version: 1,
            provider: ChangeRequestProvider::Gitlab,
            kind: ChangeRequestKind::MergeRequest,
            action: PublishAction::Updated,
            number: Some(existing.iid),
            url: existing.web_url,
            branch: request.branch.clone(),
            marker,
        })
    } else {
        let mut args = vec![
            "mr".into(),
            "create".into(),
            "--draft".into(),
            "--source-branch".into(),
            request.branch.clone(),
            "--target-branch".into(),
            context.target_branch.clone(),
            "--title".into(),
            title,
            "--description-file".into(),
            "-".into(),
            "--yes".into(),
        ];
        if !labels.is_empty() {
            args.extend(["--label".into(), labels]);
        }
        if !reviewers.is_empty() {
            args.extend(["--reviewer".into(), reviewers]);
        }
        add_repository_argument(&mut args, context);
        let created = run_checked(runner, "glab", &args, &request.worktree, &body)?;
        let url = output_url(&created.stdout);
        Ok(PublishResult {
            version: 1,
            provider: ChangeRequestProvider::Gitlab,
            kind: ChangeRequestKind::MergeRequest,
            action: PublishAction::Created,
            number: change_request_number(&url),
            url,
            branch: request.branch.clone(),
            marker,
        })
    }
}

fn add_repository_argument(args: &mut Vec<String>, context: &PublishContext) {
    if !context.repository_identity.is_empty() {
        args.extend(["--repo".into(), context.repository_identity.clone()]);
    }
}

fn change_request_number(url: &str) -> Option<u64> {
    url.split(['?', '#'])
        .next()?
        .trim_end_matches('/')
        .rsplit('/')
        .next()?
        .parse()
        .ok()
}

fn validate_publish_context(context: &PublishContext) -> Result<(), PublishError> {
    if !context.provider_base_url.starts_with("https://")
        || context.provider_base_url.len() > 2_048
        || context.provider_base_url.chars().any(char::is_control)
    {
        return Err(PublishError::UnsafeContext("provider base URL".into()));
    }
    if !context.repository_identity.is_empty()
        && (context.repository_identity.len() > 500
            || context.repository_identity.starts_with('/')
            || context.repository_identity.ends_with('/')
            || context.repository_identity.contains("..")
            || context.repository_identity.split('/').any(|part| {
                part.is_empty()
                    || !part.chars().all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
                    })
            }))
    {
        return Err(PublishError::UnsafeContext("repository identity".into()));
    }
    if context.target_branch.is_empty()
        || context.target_branch.len() > 256
        || context.target_branch.starts_with('-')
        || context.target_branch.starts_with('/')
        || context.target_branch.ends_with('/')
        || context.target_branch.contains("..")
        || context.target_branch.contains("//")
        || !context.target_branch.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '/')
        })
    {
        return Err(PublishError::UnsafeContext("target branch".into()));
    }
    Ok(())
}

fn output_url(output: &str) -> String {
    output
        .split_whitespace()
        .find(|part| part.starts_with("https://"))
        .unwrap_or(output.trim())
        .trim_end_matches([',', ')', ']'])
        .to_string()
}

fn render_pr_body(request: &DraftPublishRequest, marker: &str) -> String {
    let mut body = format!(
        "## Synaptic API migration\n\nVendor: `{}`\n\nRelease/event: `{}`\n\nOfficial source: {}\n\nSource digest: `{}`\n\nObserved versions: {}\n\nGraph blast radius: {} node(s)\n\n",
        request.brief.event.vendor,
        request
            .brief
            .event
            .release
            .as_deref()
            .unwrap_or(&request.brief.event.id),
        request.brief.event.source.uri,
        request.brief.event.source.content_digest,
        request.brief.applicability.observed_versions.join(", "),
        request.brief.impact.blast_radius_total,
    );
    body.push_str("### Why this repository is affected\n\n");
    body.push_str(&format!(
        "- Repository: `{}`\n- Matched change IDs: {}\n- Applicable usage bindings: {}\n\n",
        request.brief.repository_identity,
        display_or_none(&request.brief.applicability.matched_change_ids),
        request.brief.usage_bindings.len(),
    ));
    body.push_str("### Matched API usage\n\n");
    if request.brief.usage_bindings.is_empty() {
        body.push_str("- No concrete usage binding was retained.\n");
    }
    for binding in &request.brief.usage_bindings {
        let location = binding
            .source_location
            .as_deref()
            .unwrap_or(&binding.source_file);
        let symbol = binding
            .sdk_member
            .as_deref()
            .map(|member| format!("; SDK `{member}`"))
            .unwrap_or_default();
        body.push_str(&format!(
            "- `{location}` -> operation `{}` via `{:?}`{symbol}; confidence {:.2}\n",
            binding.operation_node_id, binding.basis, binding.confidence,
        ));
    }
    body.push_str("\n### Graph communities and repositories\n\n");
    let communities = request
        .brief
        .impact
        .blast_radius
        .iter()
        .filter_map(|hit| hit.community)
        .collect::<std::collections::BTreeSet<_>>();
    for community in communities {
        body.push_str(&format!("- Community `{community}` is affected\n"));
    }
    for hit in &request.brief.impact.blast_radius {
        let repository = hit
            .repository
            .as_deref()
            .unwrap_or(&request.brief.repository_identity);
        body.push_str(&format!(
            "- `{}` in `{}` (repository `{repository}`, depth {}, via `{}`)\n",
            hit.label, hit.file, hit.depth, hit.via_relation,
        ));
    }
    body.push('\n');
    body.push_str("### Files in scope\n\n");
    for file in &request.brief.allowed_files {
        body.push_str(&format!(
            "- `{file}` — selected by API usage/impact graph\n"
        ));
    }
    body.push_str("\n### Required invariants\n\n");
    for requirement in &request.brief.verification {
        body.push_str(&format!(
            "- `{}`: {}{}\n",
            requirement.gate,
            requirement.description,
            if requirement.required {
                " (required)"
            } else {
                ""
            },
        ));
    }
    body.push_str("\n### Verification\n\n");
    for gate in &request.verification.gates {
        body.push_str(&format!(
            "- `{:?}` {}: {}\n",
            gate.outcome, gate.gate, gate.detail
        ));
    }
    if !request.brief.required_tests.is_empty() {
        body.push_str("\nTests selected by graph:\n");
        for test in &request.brief.required_tests {
            body.push_str(&format!("- `{test}`\n"));
        }
    }
    body.push_str("\n### Uncertainty and human review\n\n");
    if request.brief.dynamic_hazards.is_empty() {
        body.push_str("- No dynamic-dispatch hazards were localized.\n");
    } else {
        for hazard in &request.brief.dynamic_hazards {
            body.push_str(&format!("- `{hazard}`\n"));
        }
    }
    body.push_str(
        "- This is intentionally a draft: human review is required, and normal CI and branch protections remain authoritative.\n\n",
    );
    body.push_str(marker);
    body.push('\n');
    body
}

fn display_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".into()
    } else {
        values
            .iter()
            .map(|value| format!("`{value}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn run_checked(
    runner: &dyn PublishCommandRunner,
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin: &str,
) -> Result<CommandOutput, PublishError> {
    let output = runner.run(program, args, cwd, stdin)?;
    if output.success {
        Ok(output)
    } else {
        Err(PublishError::Command(format!(
            "{program} failed: {}",
            output.stderr
        )))
    }
}

fn safe_component(value: &str) -> Result<String, PublishError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(PublishError::UnsafeIdentity(value));
    }
    Ok(value)
}

fn bounded_output(bytes: &[u8]) -> String {
    const CAP: usize = 1024 * 1024;
    String::from_utf8_lossy(&bytes[..bytes.len().min(CAP)]).into_owned()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExistingPr {
    number: u64,
    url: String,
    head_ref_name: String,
    body: String,
}

#[derive(Debug, Deserialize)]
struct ExistingMergeRequest {
    iid: u64,
    #[serde(alias = "webUrl")]
    web_url: String,
    #[serde(alias = "sourceBranch")]
    source_branch: String,
    #[serde(default, alias = "body")]
    description: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("only a fully verified run can publish a pull request")]
    NotVerified,
    #[error("publish branch mismatch: expected {expected:?}, got {actual:?}")]
    Branch { expected: String, actual: String },
    #[error("unsafe publish identity {0:?}")]
    UnsafeIdentity(String),
    #[error("multiple open pull requests match the same event marker: {0}")]
    DuplicatePr(usize),
    #[error("multiple open change requests match the same event marker: {0}")]
    DuplicateChangeRequest(usize),
    #[error("unsafe publish context: {0}")]
    UnsafeContext(String),
    #[error("target branch moved after verification: expected {expected}, got {actual}")]
    StaleBase { expected: String, actual: String },
    #[error("publish command failed: {0}")]
    Command(String),
    #[error("publish I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("publish JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}
