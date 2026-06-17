//! Orchestration: materialize a proposed change in a throwaway worktree, run the
//! build/type-check and the at-risk tests, and assemble a ground-truth report.
//!
//! The graph forecast narrows *what to check* (the at-risk test files, risk
//! ordered); this module *confirms* it by actually running. Pure helpers
//! (template expansion, status classification, command detection) live in their
//! own modules and are unit-tested there; here we wire them to real git + process
//! IO, exercised by tests against a real temp repo.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use codegraph_history::git;

use crate::detect::{detect_commands, DetectedCommands};
use crate::run::{expand_template, run_command, CommandResult, CommandStatus};
use crate::worktree::Worktree;
use crate::SandboxError;

/// On-disk schema version for a speculate report.
pub const SPECULATE_VERSION: u32 = 1;

/// The change to evaluate.
pub enum Change {
    /// The current working-tree changes (vs `base`), applied in isolation.
    /// `paths` (if non-empty) scopes the captured diff to those files, so the
    /// applied change matches the file set the at-risk tests were selected for.
    WorkingTree { base: String, paths: Vec<String> },
    /// A supplied unified diff, applied onto `base`.
    Patch { base: String, diff: String },
}

impl Change {
    fn base(&self) -> &str {
        match self {
            Change::WorkingTree { base, .. } | Change::Patch { base, .. } => base,
        }
    }
}

/// The overall verdict of a speculative run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Everything that ran passed.
    Passed,
    /// The build/type-check or at least one test failed (or timed out).
    Failed,
    /// Nothing conclusive ran (no change, or no command to run).
    Inconclusive,
}

/// Options controlling a speculative run.
#[derive(Debug, Clone)]
pub struct SpeculateOptions {
    /// Explicit test command (template; `{files}` expands to the at-risk files).
    pub test_cmd: Option<String>,
    /// Explicit build/type-check command.
    pub check_cmd: Option<String>,
    /// The at-risk test files to run, highest-risk first.
    pub test_files: Vec<String>,
    /// Auto-detect commands from project markers when one is not given.
    pub auto_detect: bool,
    /// Per-command wall-clock budget.
    pub timeout: Duration,
    /// Cap on the number of at-risk test files run.
    pub max_tests: usize,
    /// Stop after the first failing test.
    pub fail_fast: bool,
    /// Lines of command output retained (tail) per command.
    pub max_output_lines: usize,
}

impl Default for SpeculateOptions {
    fn default() -> Self {
        SpeculateOptions {
            test_cmd: None,
            check_cmd: None,
            test_files: Vec::new(),
            auto_detect: true,
            timeout: Duration::from_secs(300),
            max_tests: 20,
            fail_fast: false,
            max_output_lines: 40,
        }
    }
}

/// The ground-truth report of a speculative run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeculateReport {
    pub version: u32,
    /// The resolved base commit the change was applied onto.
    pub base: String,
    /// Whether the proposed change applied cleanly in the worktree.
    pub applied: bool,
    /// A short description of the change source.
    pub change_summary: String,
    /// What command detection found, when it was used.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detected: Option<DetectedCommands>,
    /// The build/type-check result, if a check command ran.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub check: Option<CommandResult>,
    /// Per at-risk test results (or one whole-suite result).
    pub tests: Vec<CommandResult>,
    /// How many at-risk tests existed before the `max_tests` cap. Meaningful only
    /// when `tests_scoped` is true (a whole-suite command runs everything).
    pub tests_total_at_risk: usize,
    /// Whether the tests were narrowed to the at-risk files (a `{files}` template)
    /// rather than run as a whole suite.
    pub tests_scoped: bool,
    pub outcome: Outcome,
    pub summary: String,
}

/// Evaluate a proposed change: apply it in a disposable worktree, run the
/// build/type-check and the at-risk tests, and report what actually happened.
///
/// Returns `Err` only for setup failures the caller must surface (the base is not
/// a commit, the worktree could not be created, or the change would not apply);
/// a failing build or test is a normal result captured in the report.
pub fn speculate(
    repo_root: &Path,
    change: &Change,
    opts: &SpeculateOptions,
) -> Result<SpeculateReport, SandboxError> {
    let base_sha =
        git::rev_parse(repo_root, change.base()).map_err(|e| SandboxError::Git(e.to_string()))?;

    // Obtain the patch bytes and a label for the change source.
    let (patch, change_summary) = match change {
        Change::WorkingTree { paths, .. } => {
            let mut args = vec!["diff", "--binary", base_sha.as_str()];
            if !paths.is_empty() {
                args.push("--");
                args.extend(paths.iter().map(String::as_str));
            }
            (
                git_capture(repo_root, &args)?,
                format!("working-tree changes vs {}", short(&base_sha)),
            )
        }
        Change::Patch { diff, .. } => (
            diff.clone().into_bytes(),
            format!("supplied patch onto {}", short(&base_sha)),
        ),
    };

    // No diff -> nothing to speculate. Not an error: report it plainly.
    if patch.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(SpeculateReport {
            version: SPECULATE_VERSION,
            base: base_sha,
            applied: false,
            change_summary,
            detected: None,
            check: None,
            tests: Vec::new(),
            tests_total_at_risk: opts.test_files.len(),
            tests_scoped: false,
            outcome: Outcome::Inconclusive,
            summary: "no changes to speculate".to_string(),
        });
    }

    let mut wt = Worktree::create(repo_root, &base_sha)?;
    git_apply(wt.path(), &patch)?;

    // Resolve commands: an explicit command always wins; otherwise auto-detect.
    let need_detect = opts.auto_detect && (opts.test_cmd.is_none() || opts.check_cmd.is_none());
    let detected = need_detect.then(|| detect_commands(&root_file_names(wt.path())));
    let check_cmd = opts
        .check_cmd
        .clone()
        .or_else(|| detected.as_ref().and_then(|d| d.check.clone()));
    let test_cmd = opts
        .test_cmd
        .clone()
        .or_else(|| detected.as_ref().and_then(|d| d.test.clone()));

    // Build / type-check first: there is no point testing code that will not build.
    let check = check_cmd
        .as_deref()
        .filter(|c| !c.trim().is_empty())
        .map(|c| run_command("check", c, wt.path(), opts.timeout, opts.max_output_lines));
    let check_failed = check
        .as_ref()
        .is_some_and(|r| matches!(r.status, CommandStatus::Failed | CommandStatus::TimedOut));

    let mut tests = Vec::new();
    let mut tests_scoped = false;
    if let Some(tcmd) = test_cmd.as_deref().filter(|c| !c.trim().is_empty()) {
        if check_failed {
            tests.push(CommandResult::skipped(
                "tests",
                "build/type-check failed; tests not run",
            ));
        } else {
            tests_scoped = run_tests(wt.path(), tcmd, opts, &mut tests);
        }
    }

    let outcome = verdict(check.as_ref(), &tests);
    let summary = summarize(&change_summary, outcome, check.as_ref(), &tests);

    let report = SpeculateReport {
        version: SPECULATE_VERSION,
        base: base_sha,
        applied: true,
        change_summary,
        detected,
        check,
        tests,
        tests_total_at_risk: opts.test_files.len(),
        tests_scoped,
        outcome,
        summary,
    };
    wt.remove();
    Ok(report)
}

/// Run the at-risk tests. A `{files}`-templated command runs once per at-risk
/// file (risk ordered, capped, fail-fast aware) so each test is attributed; a
/// command with no placeholder runs once as a whole-suite check. Returns whether
/// the run was narrowed to the at-risk files (the `{files}` branch).
fn run_tests(
    dir: &Path,
    tcmd: &str,
    opts: &SpeculateOptions,
    out: &mut Vec<CommandResult>,
) -> bool {
    if tcmd.contains("{files}") {
        if opts.test_files.is_empty() {
            out.push(CommandResult::skipped(
                "tests",
                "no at-risk tests identified for this change",
            ));
            return true;
        }
        let n = opts.test_files.len().min(opts.max_tests);
        for file in &opts.test_files[..n] {
            let cmd = expand_template(tcmd, std::slice::from_ref(file));
            let r = run_command(file, &cmd, dir, opts.timeout, opts.max_output_lines);
            let failed = matches!(r.status, CommandStatus::Failed | CommandStatus::TimedOut);
            out.push(r);
            if failed && opts.fail_fast {
                break;
            }
        }
        true
    } else {
        out.push(run_command(
            "tests",
            tcmd,
            dir,
            opts.timeout,
            opts.max_output_lines,
        ));
        false
    }
}

fn verdict(check: Option<&CommandResult>, tests: &[CommandResult]) -> Outcome {
    let failed = |s: CommandStatus| matches!(s, CommandStatus::Failed | CommandStatus::TimedOut);
    let any_failed =
        check.is_some_and(|r| failed(r.status)) || tests.iter().any(|t| failed(t.status));
    if any_failed {
        return Outcome::Failed;
    }
    let any_passed = check.is_some_and(|r| r.status == CommandStatus::Passed)
        || tests.iter().any(|t| t.status == CommandStatus::Passed);
    if any_passed {
        Outcome::Passed
    } else {
        Outcome::Inconclusive
    }
}

fn summarize(
    change_summary: &str,
    outcome: Outcome,
    check: Option<&CommandResult>,
    tests: &[CommandResult],
) -> String {
    let word = match outcome {
        Outcome::Passed => "PASSED",
        Outcome::Failed => "FAILED",
        Outcome::Inconclusive => "INCONCLUSIVE",
    };
    let passed = tests
        .iter()
        .filter(|t| t.status == CommandStatus::Passed)
        .count();
    let ran = tests
        .iter()
        .filter(|t| t.status != CommandStatus::Skipped)
        .count();
    let check_word = match check.map(|r| r.status) {
        Some(CommandStatus::Passed) => "check passed",
        Some(CommandStatus::Failed) => "check failed",
        Some(CommandStatus::TimedOut) => "check timed out",
        _ => "no check",
    };
    format!("{word}: {change_summary}; {check_word}, {passed}/{ran} test(s) passed")
}

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// File names directly under `dir` (for command detection).
fn root_file_names(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Run git in `dir`, returning stdout bytes on success.
fn git_capture(dir: &Path, args: &[&str]) -> Result<Vec<u8>, SandboxError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| SandboxError::Git(format!("spawning git: {e}")))?;
    if !out.status.success() {
        return Err(SandboxError::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// Apply a unified diff onto the worktree by piping it to `git apply` (no temp
/// file). `--whitespace=nowarn` keeps benign whitespace differences from failing
/// (a deliberate fidelity-vs-robustness lean).
fn git_apply(worktree: &Path, patch: &[u8]) -> Result<(), SandboxError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["apply", "--whitespace=nowarn"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SandboxError::Git(format!("spawning git apply: {e}")))?;
    // Write the patch on a thread while the main thread drains git's stdout/stderr
    // via wait_with_output. A single blocking write of a large patch could
    // otherwise deadlock: git blocks emitting output we are not reading while we
    // block writing stdin it is not reading. A broken pipe here (git rejected the
    // patch and closed stdin early) is swallowed; the real reason is on stderr.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let owned = patch.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&owned);
        drop(stdin);
    });
    let out = child
        .wait_with_output()
        .map_err(|e| SandboxError::Apply(format!("waiting on git apply: {e}")))?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(SandboxError::Apply(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .expect("git")
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A repo with a committed source file and a committed test file. Returns the
    /// repo root tempdir; the caller dirties the working tree to create a change.
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_run(root, &["init", "-q"]);
        std::fs::write(root.join("src.py"), b"def f():\n    return 1\n").unwrap();
        std::fs::write(root.join("tracked_test.py"), b"# a test\n").unwrap();
        git_run(root, &["add", "-A"]);
        git_run(root, &["commit", "-q", "-m", "init", "--no-gpg-sign"]);
        tmp
    }

    fn opts(test_cmd: &str, check_cmd: &str, files: &[&str]) -> SpeculateOptions {
        SpeculateOptions {
            test_cmd: Some(test_cmd.into()),
            check_cmd: Some(check_cmd.into()),
            test_files: files.iter().map(|s| s.to_string()).collect(),
            auto_detect: false,
            timeout: Duration::from_secs(60),
            ..Default::default()
        }
    }

    // `git ls-files --error-unmatch <p>` exits 0 iff <p> is tracked in the
    // worktree -> a deterministic, dependency-free per-file pass/fail signal.
    const LS: &str = "git ls-files --error-unmatch {files}";

    #[test]
    fn passes_when_check_and_tests_pass() {
        let tmp = repo();
        let root = tmp.path();
        std::fs::write(root.join("src.py"), b"def f():\n    return 2\n").unwrap();
        let report = speculate(
            root,
            &Change::WorkingTree {
                base: "HEAD".into(),
                paths: vec![],
            },
            &opts("git --version", "git --version", &["tracked_test.py"]),
        )
        .unwrap();
        assert!(report.applied, "{report:?}");
        assert_eq!(report.outcome, Outcome::Passed, "{report:?}");
        assert_eq!(report.check.as_ref().unwrap().status, CommandStatus::Passed);
        assert_eq!(report.tests.len(), 1);
        assert_eq!(report.tests[0].status, CommandStatus::Passed);
    }

    #[test]
    fn fails_when_a_test_fails() {
        let tmp = repo();
        let root = tmp.path();
        std::fs::write(root.join("src.py"), b"def f():\n    return 2\n").unwrap();
        // missing_test.py is not tracked -> ls-files exits non-zero -> Failed.
        let report = speculate(
            root,
            &Change::WorkingTree {
                base: "HEAD".into(),
                paths: vec![],
            },
            &opts(LS, "git --version", &["missing_test.py"]),
        )
        .unwrap();
        assert_eq!(report.outcome, Outcome::Failed, "{report:?}");
        assert_eq!(report.tests[0].status, CommandStatus::Failed);
    }

    #[test]
    fn check_failure_skips_tests() {
        let tmp = repo();
        let root = tmp.path();
        std::fs::write(root.join("src.py"), b"def f():\n    return 2\n").unwrap();
        let report = speculate(
            root,
            &Change::WorkingTree {
                base: "HEAD".into(),
                paths: vec![],
            },
            &opts(LS, "git not-a-real-subcommand", &["tracked_test.py"]),
        )
        .unwrap();
        assert_eq!(report.outcome, Outcome::Failed, "{report:?}");
        assert_eq!(report.check.as_ref().unwrap().status, CommandStatus::Failed);
        assert_eq!(report.tests.len(), 1);
        assert_eq!(
            report.tests[0].status,
            CommandStatus::Skipped,
            "tests skipped after a failed check"
        );
    }

    #[test]
    fn inconclusive_when_no_changes() {
        let tmp = repo();
        let root = tmp.path();
        // Working tree is clean: nothing to speculate.
        let report = speculate(
            root,
            &Change::WorkingTree {
                base: "HEAD".into(),
                paths: vec![],
            },
            &opts("git --version", "git --version", &["tracked_test.py"]),
        )
        .unwrap();
        assert_eq!(report.outcome, Outcome::Inconclusive, "{report:?}");
        assert!(!report.applied);
    }

    #[test]
    fn degrades_when_no_command_is_available() {
        let tmp = repo();
        let root = tmp.path();
        std::fs::write(root.join("src.py"), b"def f():\n    return 2\n").unwrap();
        // No explicit commands, auto-detect on, but no recognized markers commited
        // (only .py files) -> nothing detected -> nothing runs -> inconclusive.
        let report = speculate(
            root,
            &Change::WorkingTree {
                base: "HEAD".into(),
                paths: vec![],
            },
            &SpeculateOptions {
                auto_detect: true,
                test_files: vec!["tracked_test.py".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, Outcome::Inconclusive, "{report:?}");
        assert!(report.check.is_none());
    }

    #[test]
    fn respects_the_max_tests_cap() {
        let tmp = repo();
        let root = tmp.path();
        std::fs::write(root.join("a_test.py"), b"# a\n").unwrap();
        std::fs::write(root.join("b_test.py"), b"# b\n").unwrap();
        git_run(root, &["add", "-A"]);
        git_run(root, &["commit", "-q", "-m", "more tests", "--no-gpg-sign"]);
        std::fs::write(root.join("src.py"), b"def f():\n    return 2\n").unwrap();
        let mut o = opts(
            LS,
            "git --version",
            &["tracked_test.py", "a_test.py", "b_test.py"],
        );
        o.max_tests = 2;
        let report = speculate(
            root,
            &Change::WorkingTree {
                base: "HEAD".into(),
                paths: vec![],
            },
            &o,
        )
        .unwrap();
        assert_eq!(report.tests.len(), 2, "capped to max_tests");
        assert_eq!(report.tests_total_at_risk, 3, "true count preserved");
        assert!(report.tests_scoped, "per-file run is scoped");
    }

    #[test]
    fn fail_fast_stops_after_first_failure() {
        let tmp = repo();
        let root = tmp.path();
        std::fs::write(root.join("src.py"), b"def f():\n    return 2\n").unwrap();
        let mut o = opts(LS, "git --version", &["missing_test.py", "tracked_test.py"]);
        o.fail_fast = true;
        let report = speculate(
            root,
            &Change::WorkingTree {
                base: "HEAD".into(),
                paths: vec![],
            },
            &o,
        )
        .unwrap();
        assert_eq!(report.tests.len(), 1, "stopped after the first failure");
        assert_eq!(report.tests[0].status, CommandStatus::Failed);
    }

    #[test]
    fn applies_a_supplied_patch_onto_a_clean_base() {
        let tmp = repo();
        let root = tmp.path();
        // Capture a diff, then revert the tree so the base is clean.
        std::fs::write(root.join("src.py"), b"def f():\n    return 3\n").unwrap();
        let diff = String::from_utf8(git_capture(root, &["diff", "HEAD"]).unwrap()).unwrap();
        git_run(root, &["checkout", "--", "src.py"]);
        let report = speculate(
            root,
            &Change::Patch {
                base: "HEAD".into(),
                diff,
            },
            &opts("git --version", "git --version", &["tracked_test.py"]),
        )
        .unwrap();
        assert!(report.applied, "{report:?}");
        assert_eq!(report.outcome, Outcome::Passed);
    }

    #[test]
    fn errors_when_a_patch_does_not_apply() {
        let tmp = repo();
        let root = tmp.path();
        let bogus = "diff --git a/nope.txt b/nope.txt\n--- a/nope.txt\n+++ b/nope.txt\n@@ -1,1 +1,1 @@\n-was here\n+now here\n".to_string();
        let err = speculate(
            root,
            &Change::Patch {
                base: "HEAD".into(),
                diff: bogus,
            },
            &opts("git --version", "git --version", &[]),
        );
        assert!(matches!(err, Err(SandboxError::Apply(_))), "{err:?}");
    }
}
