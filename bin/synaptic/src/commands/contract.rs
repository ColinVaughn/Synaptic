//! CLI lifecycle for evidence-backed change contracts.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use synaptic_change::{
    ChangeContract, ContractSnapshot, ContractStore, RecoveryOptions, VerificationInput,
    VerificationReport, VerificationState, historical_constraints_from_memory, recover_contract,
    verify_contract,
};
use synaptic_memory::MemoryPrincipal;
use synaptic_query::{DEFAULT_AFFECTED_RELATIONS, QueryIndex, ReverseImpactIndex};

use crate::cli::ContractAction;
use crate::commands::common::{changed_files_from_git, default_graph_path, load_scoped_graph};
use crate::commands::memory::memory_store;

pub(crate) fn run_contract(action: ContractAction) -> Result<()> {
    match action {
        ContractAction::Recover {
            task,
            graph,
            root,
            base,
            max_anchors,
            depth,
            approve,
            json,
        } => {
            let kg = load_scoped_graph(&default_graph_path(graph), None)?;
            let root = canonical_root(&root)?;
            let base_revision = base.map(Ok).unwrap_or_else(|| git_revision(&root))?;
            let snapshot = ContractSnapshot {
                repository: root.to_string_lossy().replace('\\', "/"),
                base_revision,
                graph_revision: kg.built_at_commit.clone(),
            };
            let query_index = QueryIndex::build(&kg);
            let affected_index = ReverseImpactIndex::build(&kg, DEFAULT_AFFECTED_RELATIONS);
            let store = ContractStore::under(&root);
            let mut draft = recover_contract(
                &kg,
                &query_index,
                &affected_index,
                &task,
                snapshot,
                &[],
                &RecoveryOptions {
                    max_anchors,
                    depth,
                    ..RecoveryOptions::default()
                },
            )?;
            let memory = memory_store(&root);
            if memory.root().exists() {
                let historical = historical_constraints_from_memory(
                    &memory,
                    &MemoryPrincipal::operator(),
                    &draft.scope.anchors,
                    8,
                )?;
                draft.add_historical_constraints(&historical)?;
            } else {
                draft.note_unknown(
                    "Repository memory is not configured; historical decisions and invariants were not evaluated",
                )?;
            }
            let draft = store.prepare_revision(draft)?;
            store.save(&draft)?;
            let contract = if approve {
                let approved = draft.approve()?;
                store.save(&approved)?;
                approved
            } else {
                draft
            };
            print_contract(&contract, &store_path(&root, &contract), json)
        }
        ContractAction::Approve {
            contract,
            root,
            json,
        } => {
            let root = canonical_root(&root)?;
            let store = ContractStore::under(&root);
            let approved = load_contract(&store, &root, &contract)?.approve()?;
            let path = store.save(&approved)?;
            print_contract(&approved, &path, json)
        }
        ContractAction::Verify {
            contract,
            paths,
            graph,
            root,
            base,
            passed_proofs,
            json,
        } => {
            let root = canonical_root(&root)?;
            let store = ContractStore::under(&root);
            let contract = load_contract(&store, &root, &contract)?;
            let kg = load_scoped_graph(&default_graph_path(graph), None)?;
            let changed_files = if paths.is_empty() {
                changed_files_from_git(&root, &contract.snapshot.base_revision)?
            } else {
                paths
                    .iter()
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
                    .collect()
            };
            let report = verify_contract(
                &contract,
                &kg,
                &VerificationInput {
                    base_revision: base.map(Ok).unwrap_or_else(|| git_revision(&root))?,
                    changed_files,
                    passed_proofs,
                },
            );
            print_report(&report, json)?;
            if matches!(
                report.state,
                VerificationState::Satisfied | VerificationState::SatisfiedWithWarnings
            ) {
                Ok(())
            } else {
                bail!("change contract verification: {}", report.summary)
            }
        }
    }
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    root.canonicalize()
        .with_context(|| format!("resolving repository root {}", root.display()))
}

fn git_revision(root: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("running git rev-parse HEAD")?;
    if !output.status.success() {
        bail!(
            "git rev-parse HEAD failed: {} (pass --base explicitly outside git)",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn load_contract(store: &ContractStore, root: &Path, reference: &str) -> Result<ChangeContract> {
    let direct = PathBuf::from(reference);
    if direct.is_file() {
        return Ok(ContractStore::load_path(direct)?);
    }
    let under_root = root.join(reference);
    if under_root.is_file() {
        return Ok(ContractStore::load_path(under_root)?);
    }
    Ok(store.load_latest(reference)?)
}

fn store_path(root: &Path, contract: &ChangeContract) -> PathBuf {
    root.join(".synaptic")
        .join("contracts")
        .join(&contract.id)
        .join(format!("v{}.json", contract.revision))
}

fn print_contract(contract: &ChangeContract, path: &Path, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(contract)?);
        return Ok(());
    }
    println!(
        "Change contract {} v{} ({:?})",
        contract.id, contract.revision, contract.state
    );
    println!("  saved: {}", path.display());
    println!("  anchors: {}", contract.scope.anchors.len());
    for requirement in &contract.requirements {
        println!("  {:?}: {}", requirement.strength, requirement.statement);
    }
    if !contract.proofs.is_empty() {
        println!("  proofs to pass:");
        for proof in &contract.proofs {
            println!("    {}  {}", proof.id, proof.description);
        }
    }
    Ok(())
}

fn print_report(report: &VerificationReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    println!("Contract verification: {:?}", report.state);
    println!("  {}", report.summary);
    for proof in &report.proofs {
        println!("  {:?} {}: {}", proof.status, proof.proof_id, proof.detail);
    }
    for warning in &report.warnings {
        println!("  warning: {warning}");
    }
    Ok(())
}
