//! Graph-native API maintenance commands.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use synaptic_api::PatchVerifier;

use crate::cli::ApiAction;

pub(crate) fn run_api(action: ApiAction) -> Result<()> {
    match action {
        ApiAction::Init { root } => run_init(root),
        ApiAction::Inventory {
            root,
            config,
            vendor,
            json,
        } => run_inventory(root, config, vendor.as_deref(), json),
        ApiAction::Coverage {
            root,
            graph,
            config,
            runtime_evidence,
            behavioral_evidence,
            json,
            require_complete,
        } => run_coverage(
            root,
            graph,
            config,
            runtime_evidence,
            behavioral_evidence,
            json,
            require_complete,
        ),
        ApiAction::Discover { root, json } => run_discover(root, json),
        ApiAction::CheckPlan {
            root,
            json,
            require_complete,
        } => run_check_plan(root, json, require_complete),
        ApiAction::Scan {
            root,
            config,
            vendor,
            offline,
            json,
        } => run_scan(root, config, vendor.as_deref(), offline, json),
        ApiAction::Impact {
            event,
            root,
            graph,
            config,
            allowed_paths,
            json,
        } => run_impact(event, root, graph, config, allowed_paths, json),
        ApiAction::Repair {
            event,
            root,
            graph,
            config,
            dry_run,
            agent_command,
            candidate,
            repository_identity,
            network_guard,
            json,
        } => run_repair(
            event,
            root,
            graph,
            config,
            dry_run,
            agent_command,
            candidate,
            repository_identity,
            network_guard,
            json,
        )
        .map(|_| ()),
        ApiAction::Verify { run, root, json } => run_verify(run, root, json),
        ApiAction::Publish {
            run,
            root,
            provider,
            provider_base_url,
            repository,
            target_branch,
            json,
        } => run_publish(
            run,
            root,
            json,
            PublishOptions {
                provider,
                provider_base_url,
                repository,
                target_branch,
            },
        ),
        ApiAction::ExportRun {
            run,
            root,
            output,
            json,
        } => run_export(run, root, output, json),
        ApiAction::ImportRun {
            bundle,
            expected_digest,
            root,
            json,
        } => run_import(bundle, expected_digest, root, json),
        ApiAction::Run {
            root,
            vendor,
            offline,
            dry_run,
            agent_command,
            network_guard,
            defer_publish,
            provider,
            provider_base_url,
            repository,
            target_branch,
            json,
        } => run_composed(
            root,
            vendor.as_deref(),
            offline,
            dry_run,
            agent_command,
            network_guard,
            defer_publish,
            PublishOptions {
                provider,
                provider_base_url,
                repository,
                target_branch,
            },
            json,
        ),
    }
}

fn run_check_plan(root: PathBuf, json: bool, require_complete: bool) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize repository root {}", root.display()))?;
    let plan = synaptic_sandbox::detect_command_plan(&root)
        .with_context(|| format!("detect build/test plan under {}", root.display()))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!(
            "Verification plan: {} project(s), {} unresolved capability/capabilities, {} directorie(s) scanned{}",
            plan.projects.len(),
            plan.gaps.len(),
            plan.directories_scanned,
            if plan.truncated { " (truncated)" } else { "" }
        );
        for project in &plan.projects {
            println!(
                "\n{} ({}):",
                project.ecosystem,
                display_project_root(&project.root)
            );
            for command in &project.checks {
                println!("  check: {command}");
            }
            for command in &project.tests {
                println!("  test:  {command}");
            }
        }
        if !plan.gaps.is_empty() {
            println!("\nUnresolved (repair publication fails closed):");
            for gap in &plan.gaps {
                println!(
                    "  {} ({}) {:?}: {}",
                    gap.ecosystem,
                    display_project_root(&gap.root),
                    gap.capability,
                    gap.reason
                );
            }
        }
    }
    if require_complete && (!plan.gaps.is_empty() || plan.truncated) {
        bail!(
            "verification plan is incomplete: {} unresolved capability/capabilities",
            plan.gaps.len()
        );
    }
    Ok(())
}

struct ImpactContext {
    registry: synaptic_api::VendorRegistry,
    event: synaptic_api::ApiChangeEvent,
    assessment: synaptic_api::RelevanceAssessment,
    graph: synaptic_graph::KnowledgeGraph,
    brief: Option<synaptic_api::RepairBrief>,
    policy_digest: String,
}

#[derive(Serialize)]
struct AgentRequest<'a> {
    version: u32,
    brief: &'a synaptic_api::RepairBrief,
    #[serde(skip_serializing_if = "Option::is_none")]
    prior_patch: Option<&'a synaptic_api::GeneratedPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prior_failure: Option<&'a synaptic_api::RepairFailure>,
    output_contract: &'static str,
}

struct CliPatchGenerator {
    command: String,
    execution: synaptic_sandbox::ExecutionPolicy,
}

#[derive(Clone)]
struct CandidatePatchGenerator {
    patch: synaptic_api::GeneratedPatch,
}

impl synaptic_api::PatchGenerator for CandidatePatchGenerator {
    fn generate(
        &self,
        _brief: &synaptic_api::RepairBrief,
        _worktree: &Path,
    ) -> std::result::Result<synaptic_api::GeneratedPatch, synaptic_api::PatchGenerationError> {
        Ok(self.patch.clone())
    }
}

impl CliPatchGenerator {
    fn invoke(
        &self,
        brief: &synaptic_api::RepairBrief,
        worktree: &Path,
        prior_patch: Option<&synaptic_api::GeneratedPatch>,
        prior_failure: Option<&synaptic_api::RepairFailure>,
    ) -> std::result::Result<synaptic_api::GeneratedPatch, synaptic_api::PatchGenerationError> {
        if !self.command.contains("{request}") {
            return Err(synaptic_api::PatchGenerationError::InvalidOutput(
                "agent command must contain the {request} placeholder".into(),
            ));
        }
        let directory = worktree.join("synaptic-out/api-maintenance");
        std::fs::create_dir_all(&directory)?;
        let request_path = directory.join("agent-request.json");
        let request = AgentRequest {
            version: 1,
            brief,
            prior_patch,
            prior_failure,
            output_contract: "emit only JSON: {\"unified_diff\":\"...\",\"rationale\":\"...\"}",
        };
        std::fs::write(&request_path, serde_json::to_vec_pretty(&request)?)?;
        let command = self
            .command
            .replace("{request}", &shell_quote(&request_path));
        let result = synaptic_sandbox::run_command_with_policy(
            "patch-generator",
            &command,
            worktree,
            Duration::from_secs(300),
            100_000,
            &self.execution,
        );
        let _ = std::fs::remove_file(request_path);
        if result.status != synaptic_sandbox::CommandStatus::Passed {
            return Err(synaptic_api::PatchGenerationError::Failed(format!(
                "{:?}: {}",
                result.status, result.output
            )));
        }
        let patch: synaptic_api::GeneratedPatch = serde_json::from_str(result.output.trim())?;
        if patch.unified_diff.trim().is_empty() || patch.rationale.trim().is_empty() {
            return Err(synaptic_api::PatchGenerationError::InvalidOutput(
                "agent returned an empty diff or rationale".into(),
            ));
        }
        Ok(patch)
    }
}

impl synaptic_api::PatchGenerator for CliPatchGenerator {
    fn generate(
        &self,
        brief: &synaptic_api::RepairBrief,
        worktree: &Path,
    ) -> std::result::Result<synaptic_api::GeneratedPatch, synaptic_api::PatchGenerationError> {
        self.invoke(brief, worktree, None, None)
    }

    fn retry(
        &self,
        brief: &synaptic_api::RepairBrief,
        worktree: &Path,
        prior_patch: &synaptic_api::GeneratedPatch,
        failure: &synaptic_api::RepairFailure,
    ) -> std::result::Result<synaptic_api::GeneratedPatch, synaptic_api::PatchGenerationError> {
        self.invoke(brief, worktree, Some(prior_patch), Some(failure))
    }
}

struct CliPatchVerifier<'a> {
    session: &'a synaptic_sandbox::RepairSession,
    before: &'a synaptic_graph::KnowledgeGraph,
    event: &'a synaptic_api::ApiChangeEvent,
    assessment: &'a synaptic_api::RelevanceAssessment,
    config: &'a synaptic_api::ApiMaintenanceConfig,
    required_tests: &'a [String],
    baseline_project_gate: synaptic_api::GateResult,
    execution: synaptic_sandbox::ExecutionPolicy,
}

impl PatchVerifier for CliPatchVerifier<'_> {
    fn verify(
        &self,
        _worktree: &Path,
        patch: &synaptic_api::GeneratedPatch,
        inspection: &synaptic_api::PatchInspection,
    ) -> synaptic_api::VerificationReport {
        let started = std::time::Instant::now();
        let mut gates = vec![self.baseline_project_gate.clone()];
        if let Err(error) = dependency_consistency(self.session.path(), inspection) {
            gates.push(gate(
                "patch_integrity",
                synaptic_api::GateOutcome::Failed,
                error,
            ));
            return synaptic_api::VerificationReport::from_gates(gates);
        }
        if let Err(error) = self.session.apply_patch(patch.unified_diff.as_bytes()) {
            gates.push(gate(
                "patch_integrity",
                synaptic_api::GateOutcome::Failed,
                error.to_string(),
            ));
            return synaptic_api::VerificationReport::from_gates(gates);
        }
        gates.push(synaptic_api::GateResult {
            gate: "patch_integrity".into(),
            outcome: synaptic_api::GateOutcome::Passed,
            detail: format!(
                "{} file(s), {} changed line(s), applied to pinned base",
                inspection.changed_files.len(),
                inspection.added_lines + inspection.removed_lines
            ),
            duration_ms: started.elapsed().as_millis(),
        });

        let rebuild_started = std::time::Instant::now();
        let options = synaptic_incremental::RebuildOptions {
            root: self.session.path().to_path_buf(),
            directed: self.before.directed,
            force: true,
        };
        let before_data = self.before.to_graph_data();
        let changed = inspection
            .changed_files
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let incremental = synaptic_incremental::rebuild(
            &options,
            &synaptic_incremental::ChangeSet::Incremental(changed),
            Some(&before_data),
        );
        let full =
            synaptic_incremental::rebuild(&options, &synaptic_incremental::ChangeSet::Full, None);
        let after = match (incremental, full) {
            (Ok(incremental), Ok(full)) => {
                let parity = synaptic_incremental::topology(&incremental.kg.to_graph_data())
                    == synaptic_incremental::topology(&full.kg.to_graph_data());
                let invariants = synaptic_api::verify_api_invariants(
                    self.before,
                    &incremental.kg,
                    self.event,
                    self.assessment,
                    parity,
                );
                let passed = invariants.passed || !self.config.require_graph_invariants;
                gates.push(synaptic_api::GateResult {
                    gate: "api_usage_invariants".into(),
                    outcome: if passed {
                        synaptic_api::GateOutcome::Passed
                    } else {
                        synaptic_api::GateOutcome::Failed
                    },
                    detail: invariants
                        .checks
                        .iter()
                        .map(|check| format!("{}={}: {}", check.name, check.passed, check.detail))
                        .collect::<Vec<_>>()
                        .join("; "),
                    duration_ms: rebuild_started.elapsed().as_millis(),
                });
                incremental.kg
            }
            (incremental, full) => {
                gates.push(gate(
                    "api_usage_invariants",
                    synaptic_api::GateOutcome::Failed,
                    format!("incremental={:?}; full={:?}", incremental.err(), full.err()),
                ));
                let _ = self.session.reset_attempt();
                return synaptic_api::VerificationReport::from_gates(gates);
            }
        };

        gates.push(run_project_gate(
            "selected_tests_and_build",
            self.session.path(),
            self.config,
            self.required_tests,
            &inspection.changed_files,
            &self.execution,
        ));
        gates.push(run_policy_gate(
            self.session.path(),
            self.config,
            &self.execution,
        ));
        let forecast = synaptic_predict::forecast_changes(
            &after,
            &inspection.changed_files,
            &synaptic_predict::ForecastOptions::default(),
        );
        let risk = forecast.risk.as_ref().map_or(0, |risk| risk.score);
        let final_passed = risk <= self.config.max_risk_score
            && forecast.new_cycles.is_empty()
            && forecast.removed_apis.is_empty();
        gates.push(gate(
            "final_forecast",
            if final_passed {
                synaptic_api::GateOutcome::Passed
            } else {
                synaptic_api::GateOutcome::Failed
            },
            format!(
                "risk {risk}/{}; new cycles {}; removed APIs {}",
                self.config.max_risk_score,
                forecast.new_cycles.len(),
                forecast.removed_apis.len()
            ),
        ));
        let report = synaptic_api::VerificationReport::from_gates(gates);
        if !report.verified {
            let _ = self.session.reset_attempt();
        }
        report
    }
}

fn prepare_impact(
    root: &Path,
    event_id: &str,
    graph_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    allowed_paths: Vec<String>,
) -> Result<ImpactContext> {
    let registry = load_registry(root, config_path, None)?;
    let event = synaptic_api::ApiEventStore::new(root)
        .load_event(event_id)
        .with_context(|| format!("loading API event {event_id}"))?;
    let inventory = synaptic_api::inventory(root, &registry)?;
    let graph_path = graph_path.unwrap_or_else(|| root.join("synaptic-out/graph.json"));
    let data = crate::commands::common::load_graph_data(&graph_path, None)
        .with_context(|| format!("loading API usage graph {}", graph_path.display()))?;
    let allowed_paths = if allowed_paths.is_empty() {
        registry.config().allowed_paths.clone()
    } else {
        allowed_paths
    };
    let assessment =
        synaptic_api::evaluate_relevance(&event, &registry, &inventory, &data, &allowed_paths);
    let base_sha = data
        .built_at_commit
        .clone()
        .or_else(|| git_head(root).ok())
        .unwrap_or_else(|| "working-tree".into());
    let graph = synaptic_graph::KnowledgeGraph::from_graph_data(data);
    let repository_identity = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let memory = relevant_memory(root, &event.vendor);
    let brief = if assessment.state == synaptic_api::ApplicabilityState::Applicable {
        Some(synaptic_api::build_repair_brief(
            synaptic_api::RepairBriefRequest {
                repository_root: root,
                repository_identity: &repository_identity,
                base_sha: &base_sha,
                event: &event,
                assessment: &assessment,
                graph: &graph,
                memory: &memory,
                budget: &synaptic_api::BriefBudget {
                    max_files: registry.config().max_files,
                    ..synaptic_api::BriefBudget::default()
                },
            },
        )?)
    } else {
        None
    };
    let policy_digest = synaptic_api::maintenance_policy_digest(registry.config())?;
    Ok(ImpactContext {
        registry,
        event,
        assessment,
        graph,
        brief,
        policy_digest,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_repair(
    event_id: String,
    root: PathBuf,
    graph_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    dry_run: bool,
    agent_command: Option<String>,
    candidate: Option<PathBuf>,
    repository_identity: Option<String>,
    network_guard: Vec<String>,
    json: bool,
) -> Result<Option<String>> {
    repair_event(
        &event_id,
        &root,
        graph_path,
        config_path,
        dry_run,
        agent_command.as_deref(),
        candidate.as_deref(),
        repository_identity.as_deref(),
        network_guard,
        json,
        true,
    )
}

fn maintenance_repository_identity(explicit: Option<&str>, root: &Path) -> Result<String> {
    let Some(identity) = explicit else {
        return Ok(root
            .canonicalize()
            .unwrap_or_else(|_| root.to_path_buf())
            .to_string_lossy()
            .into_owned());
    };
    let normalized = identity.trim();
    let segments = normalized.split('/').collect::<Vec<_>>();
    if normalized != identity
        || normalized.len() > 512
        || segments.len() < 2
        || segments.iter().any(|segment| {
            segment.is_empty()
                || *segment == "."
                || *segment == ".."
                || !segment
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
        })
    {
        bail!("repository identity must be a canonical provider namespace/repository path");
    }
    Ok(normalized.into())
}

#[allow(clippy::too_many_arguments)]
fn repair_event(
    event_id: &str,
    root: &Path,
    graph_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    dry_run: bool,
    agent_command: Option<&str>,
    candidate: Option<&Path>,
    repository_identity: Option<&str>,
    network_guard: Vec<String>,
    json: bool,
    emit: bool,
) -> Result<Option<String>> {
    let context = prepare_impact(root, event_id, graph_path, config_path, Vec::new())?;
    let repository_identity = maintenance_repository_identity(repository_identity, root)?;
    let base_sha = context
        .brief
        .as_ref()
        .map(|brief| brief.base_sha.as_str())
        .unwrap_or("no-base");
    let ledger = synaptic_api::ApiRunStore::new(root);
    let mut run = ledger.begin(
        &repository_identity,
        base_sha,
        &context.event.id,
        &context.policy_digest,
    )?;
    let Some(mut brief) = context.brief.clone() else {
        let state = match context.assessment.state {
            synaptic_api::ApplicabilityState::NotApplicable => {
                synaptic_api::RunState::NotApplicable
            }
            synaptic_api::ApplicabilityState::ReviewRequired => {
                synaptic_api::RunState::ReviewRequired
            }
            synaptic_api::ApplicabilityState::Applicable => unreachable!(),
        };
        ledger.transition(&mut run, state, None, None)?;
        if emit {
            emit_json_or_text(
                json,
                &serde_json::json!({"version":1,"run":run,"assessment":context.assessment}),
                &format!("API event {event_id}: {:?}", context.assessment.state),
            )?;
        }
        return Ok(Some(run.id));
    };
    brief.id = run.id.clone();
    let directory = run_directory(root, &run.id)?;
    std::fs::create_dir_all(&directory)?;
    write_pretty(directory.join("event.json"), &context.event)?;
    write_pretty(directory.join("impact.json"), &context.assessment)?;
    write_pretty(directory.join("repair-brief.json"), &brief)?;
    if dry_run {
        if emit {
            emit_json_or_text(
                json,
                &serde_json::json!({"version":1,"run":run,"repair_brief":brief,"dry_run":true}),
                &format!("Dry run {} produced repair brief {}", run.id, brief.id),
            )?;
        }
        return Ok(Some(run.id));
    }
    if matches!(
        run.state,
        synaptic_api::RunState::Verified | synaptic_api::RunState::PrOpen
    ) {
        if emit {
            emit_json_or_text(
                json,
                &serde_json::json!({"version":1,"run":run,"reused":true}),
                &format!("Reusing completed API run {} ({:?})", run.id, run.state),
            )?;
        }
        return Ok(Some(run.id));
    }
    if agent_command.is_some() && candidate.is_some() {
        bail!("--agent-command and --candidate are mutually exclusive");
    }
    if agent_command.is_none() && candidate.is_none() {
        bail!("--agent-command or --candidate is required unless --dry-run is used");
    }
    ledger.transition(&mut run, synaptic_api::RunState::Repairing, None, None)?;
    let session = synaptic_sandbox::RepairSession::create(
        root,
        &brief.base_sha,
        &context.event.vendor,
        &context.event.id,
    )?;
    let execution = synaptic_sandbox::ExecutionPolicy {
        network: synaptic_sandbox::NetworkPolicy::Disabled,
        network_guard: (!network_guard.is_empty()).then_some(network_guard),
        scrub_credentials: true,
    };
    let (generator, max_attempts): (Box<dyn synaptic_api::PatchGenerator>, usize) =
        if let Some(candidate) = candidate {
            let metadata = std::fs::metadata(candidate)
                .with_context(|| format!("inspect patch candidate {}", candidate.display()))?;
            if metadata.len() > 8 * 1024 * 1024 {
                bail!("patch candidate exceeds the 8 MiB limit");
            }
            let patch: synaptic_api::GeneratedPatch = read_json(candidate.to_path_buf())?;
            if patch.unified_diff.trim().is_empty() || patch.rationale.trim().is_empty() {
                bail!("patch candidate must include a non-empty unified_diff and rationale");
            }
            (Box::new(CandidatePatchGenerator { patch }), 1)
        } else {
            (
                Box::new(CliPatchGenerator {
                    command: agent_command.unwrap_or_default().into(),
                    execution: execution.clone(),
                }),
                context.registry.config().max_attempts,
            )
        };
    let baseline_project_gate = run_project_gate(
        "baseline_tests_and_build",
        session.path(),
        context.registry.config(),
        &brief.required_tests,
        &brief.allowed_files,
        &execution,
    );
    write_pretty(
        directory.join("baseline-verification.json"),
        &baseline_project_gate,
    )?;
    if baseline_project_gate.outcome != synaptic_api::GateOutcome::Passed {
        let verification =
            synaptic_api::VerificationReport::from_gates(vec![baseline_project_gate.clone()]);
        let terminal_state =
            if baseline_project_gate.outcome == synaptic_api::GateOutcome::Inconclusive {
                synaptic_api::RunState::Inconclusive
            } else {
                synaptic_api::RunState::VerificationFailed
            };
        ledger.transition(&mut run, terminal_state, Some(verification.clone()), None)?;
        record_run_memory(
            root,
            &context,
            &run,
            synaptic_memory::MemoryKind::FailedAttempt,
            if baseline_project_gate.outcome == synaptic_api::GateOutcome::Failed {
                synaptic_memory::VerificationStatus::Failed
            } else {
                synaptic_memory::VerificationStatus::Unknown
            },
            &baseline_project_gate.detail,
            None,
        );
        if emit {
            emit_json_or_text(
                json,
                &serde_json::json!({
                    "version":1,
                    "run":run,
                    "baseline_verification":verification,
                    "agent_invoked":false
                }),
                &format!(
                    "API repair {} stopped because the baseline build/test gate did not pass",
                    run.id
                ),
            )?;
        }
        return Ok(Some(run.id));
    }
    let verifier = CliPatchVerifier {
        session: &session,
        before: &context.graph,
        event: &context.event,
        assessment: &context.assessment,
        config: context.registry.config(),
        required_tests: &brief.required_tests,
        baseline_project_gate,
        execution,
    };
    let policy = synaptic_api::PatchPolicy {
        allowed_files: brief.allowed_files.clone(),
        max_files: context.registry.config().max_files,
        max_changed_lines: context.registry.config().max_changed_lines,
        allow_workflows: context.registry.config().allow_workflow_changes,
        allow_generated: context.registry.config().allow_generated_changes,
        ..synaptic_api::PatchPolicy::default()
    };
    let outcome = match synaptic_api::run_repair_attempts(
        &brief,
        session.path(),
        &policy,
        generator.as_ref(),
        &verifier,
        max_attempts,
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            ledger.transition(&mut run, synaptic_api::RunState::RepairFailed, None, None)?;
            record_run_memory(
                root,
                &context,
                &run,
                synaptic_memory::MemoryKind::FailedAttempt,
                synaptic_memory::VerificationStatus::Failed,
                &error.to_string(),
                None,
            );
            return Err(error.into());
        }
    };
    write_pretty(directory.join("repair-outcome.json"), &outcome)?;
    if let Some(summary) = synaptic_api::failed_attempt_summary(&outcome) {
        record_run_memory(
            root,
            &context,
            &run,
            synaptic_memory::MemoryKind::FailedAttempt,
            synaptic_memory::VerificationStatus::Failed,
            &summary,
            None,
        );
    }
    if !outcome.verified {
        let state = match outcome.final_verification.as_ref() {
            Some(report)
                if report
                    .gates
                    .iter()
                    .any(|gate| gate.outcome == synaptic_api::GateOutcome::Inconclusive) =>
            {
                synaptic_api::RunState::Inconclusive
            }
            Some(_) => synaptic_api::RunState::VerificationFailed,
            None => synaptic_api::RunState::RepairFailed,
        };
        ledger.transition(&mut run, state, outcome.final_verification.clone(), None)?;
        if emit {
            emit_json_or_text(
                json,
                &serde_json::json!({"version":1,"run":run,"outcome":outcome}),
                &format!("API repair {} failed verification", run.id),
            )?;
        }
        return Ok(Some(run.id));
    }
    let patch = outcome
        .final_patch
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("verified outcome has no patch"))?;
    let verification = outcome
        .final_verification
        .clone()
        .ok_or_else(|| anyhow::anyhow!("verified outcome has no verification report"))?;
    let files = outcome
        .attempts
        .last()
        .and_then(|attempt| attempt.inspection.as_ref())
        .map(|inspection| inspection.changed_files.clone())
        .ok_or_else(|| anyhow::anyhow!("verified outcome has no patch inspection"))?;
    std::fs::write(directory.join("proposed.patch"), &patch.unified_diff)?;
    write_pretty(directory.join("verification.json"), &verification)?;
    let title = format!(
        "Migrate {} API for {}",
        context.event.vendor, context.event.id
    );
    let commit = session.commit_verified(&title, &context.event.id, &files)?;
    let branch = session.retain_verified_branch()?;
    let manifest = RepairManifest {
        version: 1,
        run_id: run.id.clone(),
        event_id: context.event.id.clone(),
        branch,
        commit,
    };
    write_pretty(directory.join("run.json"), &manifest)?;
    ledger.transition(
        &mut run,
        synaptic_api::RunState::Verified,
        Some(verification),
        None,
    )?;
    record_run_memory(
        root,
        &context,
        &run,
        synaptic_memory::MemoryKind::AgentTask,
        synaptic_memory::VerificationStatus::Passed,
        "API repair verified locally",
        None,
    );
    if emit {
        emit_json_or_text(
            json,
            &serde_json::json!({"version":1,"run":run,"manifest":manifest,"outcome":outcome}),
            &format!(
                "Verified API repair {} on branch {}",
                run.id, manifest.branch
            ),
        )?;
    }
    Ok(Some(run.id))
}

fn run_impact(
    event_id: String,
    root: PathBuf,
    graph_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    allowed_paths: Vec<String>,
    json: bool,
) -> Result<()> {
    let context = prepare_impact(&root, &event_id, graph_path, config_path, allowed_paths)?;
    if let Some(brief) = &context.brief {
        let directory = run_directory(&root, &brief.id)?;
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join("impact.json"),
            serde_json::to_vec_pretty(&context.assessment)?,
        )?;
        std::fs::write(
            directory.join("repair-brief.json"),
            serde_json::to_vec_pretty(brief)?,
        )?;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "version": 1,
                "assessment": context.assessment,
                "repair_brief": context.brief,
            }))?
        );
    } else {
        println!(
            "API event {}: {:?}",
            context.event.id, context.assessment.state
        );
        for reason in &context.assessment.reasons {
            println!("  {:?}", reason);
        }
        if let Some(brief) = context.brief {
            println!(
                "Repair brief {}: {} file(s), {} test(s), blast radius {}",
                brief.id,
                brief.allowed_files.len(),
                brief.required_tests.len(),
                brief.impact.blast_radius_total
            );
        }
    }
    Ok(())
}

fn run_init(root: PathBuf) -> Result<()> {
    let directory = root.join(".synaptic");
    let path = directory.join("api-maintenance.toml");
    if path.exists() {
        bail!("API maintenance config already exists: {}", path.display());
    }
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let template = r#"# Synaptic API Maintainer policy. Draft PRs only; never auto-merges.
schema = 1
mode = "draft_pr"
base_branch = "main"
max_files = 12
max_changed_lines = 800
max_attempts = 3
require_resolved_version = true
require_graph_invariants = true
require_tests = true

# Add one or more vendors. Sources and SDK mappings are data, not orchestration.
# [[vendors]]
# id = "stripe"
# packages = ["npm:stripe", "pypi:stripe"]
# hosts = ["api.stripe.com"]
# auto_repair_confidence = 0.92
# [[vendors.sources]]
# kind = "static_contract"
# path = "fixtures/stripe-openapi.json"
# affected_versions = ">=1.0.0, <2.0.0"
"#;
    std::fs::write(&path, template).with_context(|| format!("writing {}", path.display()))?;
    println!("Created {}", path.display());
    Ok(())
}

fn run_inventory(
    root: PathBuf,
    config_path: Option<PathBuf>,
    vendor: Option<&str>,
    json: bool,
) -> Result<()> {
    let registry = load_registry(&root, config_path, vendor)?;
    let report = synaptic_api::inventory(&root, &registry)
        .with_context(|| format!("scanning API dependencies under {}", root.display()))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        render_inventory(&report);
    }
    Ok(())
}

fn run_coverage(
    root: PathBuf,
    graph_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    runtime_evidence: Vec<PathBuf>,
    behavioral_evidence: Vec<PathBuf>,
    json: bool,
    require_complete: bool,
) -> Result<()> {
    let graph_path = graph_path.unwrap_or_else(|| root.join("synaptic-out/graph.json"));
    let graph = crate::commands::common::load_graph_data(&graph_path, None)
        .with_context(|| format!("loading API coverage graph {}", graph_path.display()))?;
    let (dependencies, sbom) = synaptic_api::scan_dependencies_and_sbom_evidence(&root)
        .with_context(|| {
            format!(
                "scanning dependency and SBOM inventory under {}",
                root.display()
            )
        })?;
    let registry = match config_path {
        Some(config_path) => Some(load_registry(&root, Some(config_path), None)?),
        None => synaptic_api::load_optional_registry(&root)?,
    };
    let mut runtime = load_runtime_evidence(&root, &runtime_evidence)?;
    let behavioral = load_behavioral_evidence(&root, &behavioral_evidence)?;
    runtime.extend(
        behavioral
            .iter()
            .map(synaptic_api::BehavioralEvidenceReport::as_runtime_evidence),
    );
    let mut report = synaptic_api::analyze_api_coverage_with_evidence(
        &graph,
        &dependencies,
        registry.as_ref(),
        &runtime,
        &sbom,
    );
    report.behavioral_review_candidates = behavioral
        .iter()
        .flat_map(|report| report.review_candidates.iter().cloned())
        .collect();
    report
        .behavioral_review_candidates
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        render_coverage(&report);
    }
    if require_complete && !report.complete {
        bail!(
            "API coverage has {} unresolved coverage gap(s); inspect `synaptic api coverage --json`",
            report.gaps.len()
        );
    }
    Ok(())
}

fn load_runtime_evidence(
    root: &Path,
    paths: &[PathBuf],
) -> Result<Vec<synaptic_api::RuntimeEvidenceReport>> {
    paths
        .iter()
        .map(|path| {
            let resolved = if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            };
            let bytes = std::fs::read(&resolved)
                .with_context(|| format!("reading runtime evidence {}", resolved.display()))?;
            synaptic_api::import_runtime_evidence(&path.to_string_lossy(), &bytes)
                .with_context(|| format!("importing runtime evidence {}", resolved.display()))
        })
        .collect()
}

fn load_behavioral_evidence(
    root: &Path,
    paths: &[PathBuf],
) -> Result<Vec<synaptic_api::BehavioralEvidenceReport>> {
    paths
        .iter()
        .map(|path| {
            let resolved = if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            };
            let bytes = std::fs::read(&resolved)
                .with_context(|| format!("reading behavioral evidence {}", resolved.display()))?;
            synaptic_api::import_behavioral_evidence(&path.to_string_lossy(), &bytes)
                .with_context(|| format!("importing behavioral evidence {}", resolved.display()))
        })
        .collect()
}

fn run_discover(root: PathBuf, json: bool) -> Result<()> {
    let report = synaptic_api::discover_contracts(&root)
        .with_context(|| format!("discovering API contracts under {}", root.display()))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "API contract discovery: {} candidate(s), {} parsed, {} rejected",
            report.candidates_scanned,
            report.contracts.len(),
            report.rejected.len()
        );
        for contract in &report.contracts {
            println!(
                "  {}: {:?} {} ({} operations)",
                contract.path, contract.format, contract.format_version, contract.operations
            );
        }
        for rejected in &report.rejected {
            println!("  {}: rejected: {}", rejected.path, rejected.error);
        }
    }
    Ok(())
}

/// Refresh the deterministic coverage ledger produced alongside a graph build.
/// This deliberately does not require API-maintenance configuration: unknown
/// surfaces are the primary evidence the ledger exists to preserve.
pub(crate) fn refresh_coverage_artifact(
    root: &Path,
    out_dir: &Path,
    graph: &synaptic_core::GraphData,
) -> Result<synaptic_api::ApiCoverageReport> {
    let (dependencies, sbom) = synaptic_api::scan_dependencies_and_sbom_evidence(root)
        .with_context(|| {
            format!(
                "scanning dependency and SBOM inventory under {}",
                root.display()
            )
        })?;
    refresh_coverage_artifact_with_inventory(root, out_dir, graph, &dependencies, &sbom)
}

pub(crate) fn refresh_coverage_artifact_with_inventory(
    root: &Path,
    out_dir: &Path,
    graph: &synaptic_core::GraphData,
    dependencies: &[synaptic_api::Dependency],
    sbom: &synaptic_api::SbomEvidenceReport,
) -> Result<synaptic_api::ApiCoverageReport> {
    let registry = synaptic_api::load_optional_registry(root)?;
    let conventional_runtime = root.join(".synaptic/runtime-evidence");
    let runtime_paths = if conventional_runtime.is_dir() {
        let mut paths = std::fs::read_dir(&conventional_runtime)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        paths.sort();
        paths
    } else {
        Vec::new()
    };
    let runtime = load_runtime_evidence(root, &runtime_paths)?;
    let conventional_behavioral = root.join(".synaptic/behavioral-evidence");
    let behavioral_paths = if conventional_behavioral.is_dir() {
        let mut paths = std::fs::read_dir(&conventional_behavioral)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        paths.sort();
        paths
    } else {
        Vec::new()
    };
    let behavioral = load_behavioral_evidence(root, &behavioral_paths)?;
    let mut runtime = runtime;
    runtime.extend(
        behavioral
            .iter()
            .map(synaptic_api::BehavioralEvidenceReport::as_runtime_evidence),
    );
    let mut report = synaptic_api::analyze_api_coverage_with_evidence(
        graph,
        dependencies,
        registry.as_ref(),
        &runtime,
        sbom,
    );
    report.behavioral_review_candidates = behavioral
        .iter()
        .flat_map(|report| report.review_candidates.iter().cloned())
        .collect();
    report
        .behavioral_review_candidates
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    write_pretty(out_dir.join("api-maintenance/coverage.json"), &report)?;
    if !behavioral.is_empty() {
        write_pretty(
            out_dir.join("api-maintenance/behavioral-evidence.json"),
            &behavioral,
        )?;
    }
    let discovery = synaptic_api::discover_contracts(root)?;
    write_pretty(
        out_dir.join("api-maintenance/contract-discovery.json"),
        &discovery,
    )?;
    let candidate = synaptic_api::candidate_profile_toml(&discovery)?;
    let candidate_path = out_dir.join("api-maintenance/candidate-profile.toml");
    if let Some(parent) = candidate_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(candidate_path, candidate)?;
    Ok(report)
}

fn run_scan(
    root: PathBuf,
    config_path: Option<PathBuf>,
    vendor: Option<&str>,
    offline: bool,
    json: bool,
) -> Result<()> {
    let registry = load_registry(&root, config_path, vendor)?;
    let report = synaptic_api::scan_repository(
        &root,
        &registry,
        &synaptic_api::SystemArtifactFetcher,
        offline,
    )
    .with_context(|| format!("scanning API sources for {}", root.display()))?;
    record_scan_events(&root, &report)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "API source scan: {} source(s), {} breaking event(s), {} review candidate(s)",
            report.sources.len(),
            report.events.len(),
            report.review_candidates.len()
        );
        for source in &report.sources {
            println!(
                "  {} {}: {:?}",
                source.vendor, source.revision, source.disposition
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RepairManifest {
    version: u32,
    run_id: String,
    event_id: String,
    branch: String,
    commit: String,
}

#[derive(Debug, Clone)]
struct PublishOptions {
    provider: String,
    provider_base_url: Option<String>,
    repository: Option<String>,
    target_branch: Option<String>,
}

impl Default for PublishOptions {
    fn default() -> Self {
        Self {
            provider: "github".into(),
            provider_base_url: None,
            repository: None,
            target_branch: None,
        }
    }
}

fn publish_context(
    options: &PublishOptions,
    configured_base_branch: &str,
) -> Result<synaptic_api::PublishContext> {
    let provider = match options.provider.trim().to_ascii_lowercase().as_str() {
        "github" => synaptic_api::ChangeRequestProvider::Github,
        "gitlab" => synaptic_api::ChangeRequestProvider::Gitlab,
        value => bail!("unsupported change-request provider {value:?}; use github or gitlab"),
    };
    let provider_base_url = options
        .provider_base_url
        .clone()
        .unwrap_or_else(|| match provider {
            synaptic_api::ChangeRequestProvider::Github => "https://github.com".into(),
            synaptic_api::ChangeRequestProvider::Gitlab => "https://gitlab.com".into(),
        });
    Ok(synaptic_api::PublishContext {
        provider,
        provider_base_url,
        repository_identity: options.repository.clone().unwrap_or_default(),
        target_branch: options
            .target_branch
            .clone()
            .unwrap_or_else(|| configured_base_branch.into()),
    })
}

fn run_verify(run_id: String, root: PathBuf, json: bool) -> Result<()> {
    let (verification, digest) = validate_run(&run_id, &root)?;
    emit_json_or_text(
        json,
        &serde_json::json!({"version":1,"run":run_id,"verification":verification,"patch_digest":digest}),
        &format!("Run {run_id} is conclusively verified"),
    )
}

fn validate_run(run_id: &str, root: &Path) -> Result<(synaptic_api::VerificationReport, String)> {
    let directory = run_directory(root, run_id)?;
    let outcome: synaptic_api::RepairOutcome = read_json(directory.join("repair-outcome.json"))?;
    let verification: synaptic_api::VerificationReport =
        read_json(directory.join("verification.json"))?;
    if outcome.run_id != run_id || !outcome.verified || !verification.verified {
        bail!("run {run_id} is not conclusively verified");
    }
    let patch = outcome
        .final_patch
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("verified run has no final patch"))?;
    let digest = blake3::hash(patch.unified_diff.as_bytes())
        .to_hex()
        .to_string();
    if outcome
        .attempts
        .last()
        .map(|attempt| attempt.patch_digest.as_str())
        != Some(&digest)
    {
        bail!("run {run_id} patch digest does not match its verified attempt");
    }
    Ok((verification, digest))
}

fn run_publish(run_id: String, root: PathBuf, json: bool, options: PublishOptions) -> Result<()> {
    let _ = validate_run(&run_id, &root)?;
    let directory = run_directory(&root, &run_id)?;
    let manifest: RepairManifest = read_json(directory.join("run.json"))?;
    let brief: synaptic_api::RepairBrief = read_json(directory.join("repair-brief.json"))?;
    let verification: synaptic_api::VerificationReport =
        read_json(directory.join("verification.json"))?;
    let registry = load_registry(&root, None, Some(&brief.event.vendor))?;
    let publish_context = publish_context(&options, &registry.config().base_branch)?;
    let ledger = synaptic_api::ApiRunStore::new(&root);
    let mut run = ledger.load(&run_id)?;
    if !matches!(
        run.state,
        synaptic_api::RunState::Verified | synaptic_api::RunState::PrOpen
    ) {
        bail!("run {run_id} is {:?}, not publishable", run.state);
    }
    let session = synaptic_sandbox::RepairSession::create(
        &root,
        &manifest.branch,
        &brief.event.vendor,
        &brief.event.id,
    )?;
    let result = synaptic_api::publish_verified_change_request(
        &synaptic_api::DraftPublishRequest {
            worktree: session.path().to_path_buf(),
            branch: manifest.branch.clone(),
            brief: brief.clone(),
            verification: verification.clone(),
            labels: registry.config().publish.labels.clone(),
            reviewers: registry.config().publish.reviewers.clone(),
        },
        &publish_context,
        &synaptic_api::SystemPublishCommandRunner,
    )?;
    let _branch = session.retain_verified_branch()?;
    write_pretty(directory.join("pr.json"), &result)?;
    if run.state == synaptic_api::RunState::Verified {
        ledger.transition(
            &mut run,
            synaptic_api::RunState::PrOpen,
            None,
            Some(result.url.clone()),
        )?;
    }
    let event = synaptic_api::ApiEventStore::new(&root).load_event(&manifest.event_id)?;
    let context = prepare_impact(&root, &event.id, None, None, Vec::new())?;
    record_run_memory(
        &root,
        &context,
        &run,
        synaptic_memory::MemoryKind::PullRequest,
        synaptic_memory::VerificationStatus::Passed,
        "Verified API migration draft PR opened or updated",
        Some(&result.url),
    );
    emit_json_or_text(
        json,
        &serde_json::json!({"version":1,"run":run,"publish":result}),
        &format!("Draft PR for run {run_id}: {}", result.url),
    )
}

fn run_export(run_id: String, root: PathBuf, output: PathBuf, json: bool) -> Result<()> {
    let (verification, _) = validate_run(&run_id, &root)?;
    let directory = run_directory(&root, &run_id)?;
    let run = synaptic_api::ApiRunStore::new(&root).load(&run_id)?;
    let event: synaptic_api::ApiChangeEvent = read_json(directory.join("event.json"))?;
    let brief: synaptic_api::RepairBrief = read_json(directory.join("repair-brief.json"))?;
    let outcome: synaptic_api::RepairOutcome = read_json(directory.join("repair-outcome.json"))?;
    let patch = std::fs::read_to_string(directory.join("proposed.patch"))?;
    let handoff =
        synaptic_api::VerifiedRunHandoff::new(run, event, brief, outcome, verification, patch)?;
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)
        .with_context(|| format!("create verified-run handoff {}", output.display()))?;
    let mut bytes = serde_json::to_vec_pretty(&handoff)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    emit_json_or_text(
        json,
        &serde_json::json!({
            "version": 1,
            "run": run_id,
            "output": output,
            "bundle_digest": handoff.bundle_digest,
            "patch_digest": handoff.patch_digest
        }),
        &format!(
            "Exported verified API run {run_id} to {} ({})",
            output.display(),
            handoff.bundle_digest
        ),
    )
}

fn run_import(bundle: PathBuf, expected_digest: String, root: PathBuf, json: bool) -> Result<()> {
    const MAX_HANDOFF_BYTES: u64 = 64 * 1024 * 1024;
    let metadata = std::fs::metadata(&bundle)
        .with_context(|| format!("inspect verified-run handoff {}", bundle.display()))?;
    if metadata.len() > MAX_HANDOFF_BYTES {
        bail!("verified-run handoff exceeds the 64 MiB limit");
    }
    let handoff: synaptic_api::VerifiedRunHandoff = read_json(bundle.clone())?;
    handoff.verify()?;
    if handoff.bundle_digest != expected_digest {
        bail!("verified-run handoff digest does not match --expected-digest");
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize publication checkout {}", root.display()))?;
    let head = git_stdout(&root, &["rev-parse", "HEAD"])?;
    if head != handoff.run.base_sha {
        bail!(
            "publication checkout HEAD {head} does not match verified base {}",
            handoff.run.base_sha
        );
    }
    if !git_stdout(&root, &["status", "--porcelain"])?.is_empty() {
        bail!("publication checkout must be clean before importing a verified run");
    }
    let registry = load_registry(&root, None, Some(&handoff.event.vendor))?;
    let policy_digest = synaptic_api::maintenance_policy_digest(registry.config())?;
    if policy_digest != handoff.run.policy_digest {
        bail!("publication checkout policy digest differs from the verified run");
    }
    let policy = synaptic_api::PatchPolicy {
        allowed_files: handoff.brief.allowed_files.clone(),
        max_files: registry.config().max_files,
        max_changed_lines: registry.config().max_changed_lines,
        allow_workflows: registry.config().allow_workflow_changes,
        allow_generated: registry.config().allow_generated_changes,
        ..synaptic_api::PatchPolicy::default()
    };
    let inspection = synaptic_api::validate_patch(&root, &handoff.patch, &policy)?;
    let session = synaptic_sandbox::RepairSession::create(
        &root,
        &handoff.run.base_sha,
        &handoff.event.vendor,
        &handoff.event.id,
    )?;
    if session.branch() != handoff.branch {
        bail!("verified-run handoff branch does not match the repair session");
    }
    session.apply_patch(handoff.patch.as_bytes())?;
    let title = format!(
        "Migrate {} API for {}",
        handoff.event.vendor, handoff.event.id
    );
    let commit = session.commit_verified(&title, &handoff.event.id, &inspection.changed_files)?;
    let branch = session.retain_verified_branch()?;
    synaptic_api::ApiEventStore::new(&root).put_event(&handoff.event)?;
    let directory = run_directory(&root, &handoff.run.id)?;
    std::fs::create_dir_all(&directory)?;
    write_pretty(directory.join("event.json"), &handoff.event)?;
    write_pretty(directory.join("repair-brief.json"), &handoff.brief)?;
    write_pretty(directory.join("repair-outcome.json"), &handoff.outcome)?;
    write_pretty(directory.join("verification.json"), &handoff.verification)?;
    std::fs::write(directory.join("proposed.patch"), &handoff.patch)?;
    write_pretty(
        directory.join("run.json"),
        &RepairManifest {
            version: 1,
            run_id: handoff.run.id.clone(),
            event_id: handoff.event.id.clone(),
            branch,
            commit,
        },
    )?;
    synaptic_api::ApiRunStore::new(&root).import_verified(&handoff.run)?;
    emit_json_or_text(
        json,
        &serde_json::json!({
            "version": 1,
            "run": handoff.run.id,
            "branch": handoff.branch,
            "bundle_digest": handoff.bundle_digest,
            "patch_digest": handoff.patch_digest
        }),
        &format!("Imported verified API run {}", handoff.run.id),
    )
}

fn git_stdout(root: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

#[allow(clippy::too_many_arguments)]
fn run_composed(
    root: PathBuf,
    vendor: Option<&str>,
    offline: bool,
    dry_run: bool,
    agent_command: Option<String>,
    network_guard: Vec<String>,
    defer_publish: bool,
    publish_options: PublishOptions,
    json: bool,
) -> Result<()> {
    let registry = load_registry(&root, None, vendor)?;
    let scan = synaptic_api::scan_repository(
        &root,
        &registry,
        &synaptic_api::SystemArtifactFetcher,
        offline,
    )?;
    record_scan_events(&root, &scan)?;
    let events = synaptic_api::ApiEventStore::new(&root)
        .list_events()?
        .into_iter()
        .filter(|event| {
            registry
                .vendor(&event.vendor)
                .is_some_and(|vendor| vendor.enabled)
        })
        .collect::<Vec<_>>();
    let mut runs = Vec::new();
    let mut outcomes = Vec::new();
    if events.is_empty() {
        outcomes.push(serde_json::json!({
            "run": serde_json::Value::Null,
            "state": "no_change",
            "event": format!("scan:{}", vendor.unwrap_or("all")),
            "base_sha": git_stdout(&root, &["rev-parse", "HEAD"])?,
            "policy_digest": synaptic_api::maintenance_policy_digest(registry.config())?
        }));
    }
    for event in &events {
        if let Some(run_id) = repair_event(
            &event.id,
            &root,
            None,
            None,
            dry_run,
            agent_command.as_deref(),
            None,
            publish_options.repository.as_deref(),
            network_guard.clone(),
            false,
            false,
        )? {
            if !dry_run
                && !defer_publish
                && synaptic_api::ApiRunStore::new(&root).load(&run_id)?.state
                    == synaptic_api::RunState::Verified
            {
                run_publish(run_id.clone(), root.clone(), false, publish_options.clone())?;
            }
            let completed = synaptic_api::ApiRunStore::new(&root).load(&run_id)?;
            outcomes.push(serde_json::json!({
                "run": run_id,
                "state": completed.state,
                "event": completed.event_id,
                "base_sha": completed.base_sha,
                "policy_digest": completed.policy_digest
            }));
            runs.push(run_id);
        }
    }
    emit_json_or_text(
        json,
        &serde_json::json!({"version":1,"scan":scan,"events_considered":events.len(),"runs":runs,"outcomes":outcomes,"dry_run":dry_run,"publication_deferred":defer_publish}),
        &format!(
            "API maintenance run: {} event(s), {} repository run(s)",
            events.len(),
            runs.len()
        ),
    )
}

fn gate(
    name: &str,
    outcome: synaptic_api::GateOutcome,
    detail: String,
) -> synaptic_api::GateResult {
    synaptic_api::GateResult {
        gate: name.into(),
        outcome,
        detail,
        duration_ms: 0,
    }
}

fn run_project_gate(
    gate_name: &str,
    root: &Path,
    config: &synaptic_api::ApiMaintenanceConfig,
    required_tests: &[String],
    changed_files: &[String],
    execution: &synaptic_sandbox::ExecutionPolicy,
) -> synaptic_api::GateResult {
    let detected = match synaptic_sandbox::detect_command_plan(root) {
        Ok(plan) => plan,
        Err(error) => {
            return gate(
                gate_name,
                synaptic_api::GateOutcome::Inconclusive,
                format!("build/test detection failed closed: {error}"),
            );
        }
    };
    let explicit_check = config
        .commands
        .check
        .as_ref()
        .is_some_and(|command| !command.trim().is_empty());
    let explicit_test = config
        .commands
        .test
        .as_ref()
        .is_some_and(|command| !command.trim().is_empty());
    let projects = detected
        .projects
        .iter()
        .filter(|project| project.is_relevant_to(changed_files))
        .collect::<Vec<_>>();
    let unresolved = detected
        .gaps
        .iter()
        .filter(|gap| gap.is_relevant_to(changed_files))
        .filter(|gap| {
            !gap.capability
                .resolved_by(explicit_check, explicit_test || !config.require_tests)
        })
        .collect::<Vec<_>>();

    let mut results = Vec::new();
    if let Some(command) = config
        .commands
        .check
        .as_ref()
        .filter(|command| !command.trim().is_empty())
    {
        results.push(synaptic_sandbox::run_command_with_policy(
            "configured build/check",
            command,
            root,
            Duration::from_secs(300),
            100,
            execution,
        ));
    } else {
        for project in &projects {
            let project_root = root.join(&project.root);
            for command in &project.checks {
                results.push(synaptic_sandbox::run_command_with_policy(
                    &format!(
                        "{} build/check ({})",
                        project.ecosystem,
                        display_project_root(&project.root)
                    ),
                    command,
                    &project_root,
                    Duration::from_secs(300),
                    100,
                    execution,
                ));
            }
        }
    }

    if let Some(command) = config
        .commands
        .test
        .as_ref()
        .filter(|command| !command.trim().is_empty())
    {
        let command = command.replace(
            "{files}",
            &required_tests
                .iter()
                .map(|file| shell_quote(Path::new(file)))
                .collect::<Vec<_>>()
                .join(" "),
        );
        results.push(synaptic_sandbox::run_command_with_policy(
            "configured selected tests",
            &command,
            root,
            Duration::from_secs(300),
            100,
            execution,
        ));
    } else {
        for project in &projects {
            let project_root = root.join(&project.root);
            for command in &project.tests {
                results.push(synaptic_sandbox::run_command_with_policy(
                    &format!(
                        "{} tests ({})",
                        project.ecosystem,
                        display_project_root(&project.root)
                    ),
                    command,
                    &project_root,
                    Duration::from_secs(300),
                    100,
                    execution,
                ));
            }
        }
    }
    if results.is_empty() && unresolved.is_empty() {
        return gate(
            gate_name,
            if config.require_tests {
                synaptic_api::GateOutcome::Inconclusive
            } else {
                synaptic_api::GateOutcome::Passed
            },
            "no build or test command was configured/detected".into(),
        );
    }
    let command_status = command_outcome(&results);
    let outcome = if command_status == synaptic_api::GateOutcome::Failed {
        command_status
    } else if !unresolved.is_empty() {
        synaptic_api::GateOutcome::Inconclusive
    } else {
        command_status
    };
    let mut detail = results
        .iter()
        .map(|result| format!("{}={:?}: {}", result.label, result.status, result.output))
        .collect::<Vec<_>>();
    detail.extend(unresolved.iter().map(|gap| {
        format!(
            "{} ({}) unresolved {:?}: {}",
            gap.ecosystem,
            display_project_root(&gap.root),
            gap.capability,
            gap.reason
        )
    }));
    gate(gate_name, outcome, detail.join("; "))
}

fn display_project_root(root: &Path) -> String {
    if root.as_os_str().is_empty() {
        ".".into()
    } else {
        root.display().to_string()
    }
}

fn run_policy_gate(
    root: &Path,
    config: &synaptic_api::ApiMaintenanceConfig,
    execution: &synaptic_sandbox::ExecutionPolicy,
) -> synaptic_api::GateResult {
    if config.commands.policy.is_empty() {
        return gate(
            "repository_policy",
            synaptic_api::GateOutcome::Passed,
            "no additional project policy commands configured".into(),
        );
    }
    let results = config
        .commands
        .policy
        .iter()
        .map(|command| {
            synaptic_sandbox::run_command_with_policy(
                "repository policy",
                command,
                root,
                Duration::from_secs(300),
                100,
                execution,
            )
        })
        .collect::<Vec<_>>();
    gate(
        "repository_policy",
        command_outcome(&results),
        results
            .iter()
            .map(|result| format!("{:?}: {}", result.status, result.output))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

fn command_outcome(results: &[synaptic_sandbox::CommandResult]) -> synaptic_api::GateOutcome {
    if results.iter().any(|result| {
        matches!(
            result.status,
            synaptic_sandbox::CommandStatus::Failed | synaptic_sandbox::CommandStatus::TimedOut
        )
    }) {
        synaptic_api::GateOutcome::Failed
    } else if results
        .iter()
        .any(|result| result.status == synaptic_sandbox::CommandStatus::Skipped)
    {
        synaptic_api::GateOutcome::Inconclusive
    } else {
        synaptic_api::GateOutcome::Passed
    }
}

fn dependency_consistency(
    root: &Path,
    inspection: &synaptic_api::PatchInspection,
) -> std::result::Result<(), String> {
    let changed = inspection
        .changed_files
        .iter()
        .map(|file| file.to_ascii_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    for (manifest, locks) in [
        (
            "package.json",
            &["package-lock.json", "pnpm-lock.yaml", "yarn.lock"][..],
        ),
        ("pyproject.toml", &["poetry.lock", "uv.lock"][..]),
        ("cargo.toml", &["cargo.lock"][..]),
        ("go.mod", &["go.sum"][..]),
    ] {
        if changed.iter().any(|path| path.ends_with(manifest)) {
            let existing_lock = locks.iter().find(|lock| root.join(lock).exists());
            if let Some(lock) = existing_lock {
                if !changed.iter().any(|path| path.ends_with(lock)) {
                    return Err(format!(
                        "{manifest} changed without its existing lockfile {lock}"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn load_registry(
    root: &std::path::Path,
    config_path: Option<PathBuf>,
    vendor: Option<&str>,
) -> Result<synaptic_api::VendorRegistry> {
    let config_path =
        config_path.unwrap_or_else(|| root.join(".synaptic").join("api-maintenance.toml"));
    let source = std::fs::read_to_string(&config_path).with_context(|| {
        format!(
            "reading API maintenance config {} (run `synaptic api init` or pass --config)",
            config_path.display()
        )
    })?;
    let mut config = synaptic_api::ApiMaintenanceConfig::parse(&source)
        .with_context(|| format!("parsing {}", config_path.display()))?;
    if let Some(requested) = vendor {
        config
            .vendors
            .retain(|candidate| candidate.id.eq_ignore_ascii_case(requested));
        if config.vendors.is_empty() {
            bail!(
                "vendor {requested:?} is not configured in {}",
                config_path.display()
            );
        }
    }
    Ok(synaptic_api::VendorRegistry::new(config)?)
}

fn run_directory(root: &Path, run_id: &str) -> Result<PathBuf> {
    if run_id.is_empty()
        || !run_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid API run id {run_id:?}");
    }
    Ok(root.join("synaptic-out/api-maintenance").join(run_id))
}

fn write_pretty(path: PathBuf, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    std::fs::write(path, bytes)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<T> {
    serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("reading {}", path.display()))
}

fn emit_json_or_text(json: bool, value: &serde_json::Value, text: &str) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{text}");
    }
    Ok(())
}

fn git_head(root: &Path) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        bail!("repository has no resolvable HEAD commit")
    }
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    if cfg!(windows) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn relevant_memory(root: &Path, vendor: &str) -> Vec<synaptic_api::MemoryEvidence> {
    let store = synaptic_memory::MemoryStore::open(root.join(".synaptic/memory"));
    store
        .search(&synaptic_memory::MemoryQuery {
            text: format!("{vendor} API migration"),
            kinds: vec![
                synaptic_memory::MemoryKind::FailedAttempt,
                synaptic_memory::MemoryKind::Regression,
                synaptic_memory::MemoryKind::ArchitectureDecision,
                synaptic_memory::MemoryKind::Convention,
                synaptic_memory::MemoryKind::Procedure,
                synaptic_memory::MemoryKind::PullRequest,
            ],
            symbol: None,
            include_superseded: false,
            limit: 20,
        })
        .unwrap_or_default()
        .into_iter()
        .map(|hit| synaptic_api::MemoryEvidence {
            kind: hit.record.kind.as_str().into(),
            summary: hit.record.summary,
            source: hit
                .record
                .sources
                .first()
                .map(|source| source.uri.clone())
                .unwrap_or_default(),
            digest: hit.record.id,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn record_run_memory(
    root: &Path,
    context: &ImpactContext,
    run: &synaptic_api::ApiRunRecord,
    kind: synaptic_memory::MemoryKind,
    verification: synaptic_memory::VerificationStatus,
    summary: &str,
    pr_url: Option<&str>,
) {
    let store = synaptic_memory::MemoryStore::open(root.join(".synaptic/memory"));
    let commands = run
        .verification
        .as_ref()
        .into_iter()
        .flat_map(|report| report.gates.iter().map(|gate| gate.gate.clone()))
        .collect();
    let branch = run_directory(root, &run.id)
        .ok()
        .and_then(|directory| read_json::<RepairManifest>(directory.join("run.json")).ok())
        .map(|manifest| manifest.branch);
    let _ = synaptic_memory::record_api_maintenance_memory(
        &store,
        kind,
        &synaptic_memory::ApiMaintenanceMemory {
            repository: run.repository_identity.clone(),
            vendor: context.event.vendor.clone(),
            event_id: context.event.id.clone(),
            run_id: Some(run.id.clone()),
            occurred_at: context.event.occurred_at,
            source_uri: context.event.source.uri.clone(),
            source_revision: context.event.source.revision.clone(),
            source_digest: context.event.source.content_digest.clone(),
            base_sha: Some(run.base_sha.clone()),
            branch,
            pull_request_url: pr_url.map(str::to_string),
            summary: summary.into(),
            commands,
            verification,
        },
    );
}

fn record_scan_events(root: &Path, report: &synaptic_api::ScanReport) -> Result<()> {
    if report.events.is_empty() {
        return Ok(());
    }
    let repository = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let store = synaptic_memory::MemoryStore::open(root.join(".synaptic/memory"));
    for event in &report.events {
        let summary = event
            .changes
            .iter()
            .map(|change| change.migration_summary.as_str())
            .take(20)
            .collect::<Vec<_>>()
            .join("; ");
        synaptic_memory::record_api_maintenance_memory(
            &store,
            synaptic_memory::MemoryKind::Release,
            &synaptic_memory::ApiMaintenanceMemory {
                repository: repository.clone(),
                vendor: event.vendor.clone(),
                event_id: event.id.clone(),
                run_id: None,
                occurred_at: event.occurred_at,
                source_uri: event.source.uri.clone(),
                source_revision: event.source.revision.clone(),
                source_digest: event.source.content_digest.clone(),
                base_sha: None,
                branch: None,
                pull_request_url: None,
                summary,
                commands: vec!["synaptic api scan".into()],
                verification: synaptic_memory::VerificationStatus::Unknown,
            },
        )?;
    }
    Ok(())
}

fn render_inventory(report: &synaptic_api::ApiInventory) {
    println!(
        "API dependency inventory: {} matched / {} total",
        report.matched.len(),
        report.dependencies.len()
    );
    let mut current_vendor: Option<&str> = None;
    for entry in &report.matched {
        if current_vendor != Some(&entry.vendor_id) {
            current_vendor = Some(&entry.vendor_id);
            println!("\n{}:", entry.vendor_id);
        }
        let dependency = &entry.dependency;
        let version = dependency
            .resolved_version
            .as_deref()
            .or(dependency.declared_requirement.as_deref())
            .unwrap_or("version unknown");
        println!(
            "  {} {} ({})",
            dependency.package, version, dependency.source_file
        );
    }
    if !report.ambiguous.is_empty() {
        println!("\nAmbiguous (automation disabled):");
        for entry in &report.ambiguous {
            println!(
                "  {} -> {}",
                entry.dependency.package,
                entry.vendor_ids.join(", ")
            );
        }
    }
    if !report.unmatched.is_empty() {
        println!("\nUnmatched dependencies: {}", report.unmatched.len());
    }
}

fn render_coverage(report: &synaptic_api::ApiCoverageReport) {
    println!(
        "API coverage: {} observation(s), {} gap(s), {} inventoried dependency/dependencies ({} development negative control(s))",
        report.observations.len(),
        report.gaps.len(),
        report.dependency_inventory,
        report.development_dependencies.len()
    );
    for observation in &report.observations {
        println!(
            "  {:?} {:?}: {} ({})",
            observation.state, observation.kind, observation.identity, observation.source_file
        );
        if !observation.gaps.is_empty() {
            println!("    missing: {:?}", observation.gaps);
        }
    }
    if report.complete {
        println!("Coverage ledger is complete for the evidence present in this graph.");
    } else {
        println!(
            "Coverage ledger is incomplete; unresolved surfaces remain visible and are not repair-eligible."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution() -> synaptic_sandbox::ExecutionPolicy {
        synaptic_sandbox::ExecutionPolicy {
            network: synaptic_sandbox::NetworkPolicy::Allow,
            network_guard: None,
            scrub_credentials: false,
        }
    }

    #[test]
    fn baseline_project_gate_fails_and_incomplete_detection_is_inconclusive() {
        let repo = tempfile::tempdir().unwrap();
        let failing = synaptic_api::ApiMaintenanceConfig::parse(
            r#"
schema = 1
require_tests = true
[commands]
check = "exit 7"
test = "exit 0"
"#,
        )
        .unwrap();
        let gate = run_project_gate(
            "baseline_tests_and_build",
            repo.path(),
            &failing,
            &[],
            &[],
            &execution(),
        );
        assert_eq!(gate.outcome, synaptic_api::GateOutcome::Failed);

        std::fs::write(repo.path().join("package.json"), r#"{"scripts":{}}"#).unwrap();
        std::fs::write(repo.path().join("client.ts"), "export const value = 1;\n").unwrap();
        let detected =
            synaptic_api::ApiMaintenanceConfig::parse("schema = 1\nrequire_tests = true\n")
                .unwrap();
        let gate = run_project_gate(
            "baseline_tests_and_build",
            repo.path(),
            &detected,
            &[],
            &["client.ts".into()],
            &execution(),
        );
        assert_eq!(gate.outcome, synaptic_api::GateOutcome::Inconclusive);
        assert!(gate.detail.contains("unresolved"));
    }
}
