//! Render a `SpeculateReport` as agent-readable Markdown, mirroring the style of
//! the predict/refactor reports.

use crate::run::{CommandResult, CommandStatus};
use crate::speculate::{Outcome, SpeculateReport};

fn status_word(s: CommandStatus) -> &'static str {
    match s {
        CommandStatus::Passed => "passed",
        CommandStatus::Failed => "failed",
        CommandStatus::TimedOut => "timed out",
        CommandStatus::Skipped => "skipped",
    }
}

fn outcome_word(o: Outcome) -> &'static str {
    match o {
        Outcome::Passed => "PASSED",
        Outcome::Failed => "FAILED",
        Outcome::Inconclusive => "INCONCLUSIVE",
    }
}

fn render_command(out: &mut String, r: &CommandResult) {
    out.push_str(&format!("- **{}**: {}", r.label, status_word(r.status)));
    if let Some(code) = r.exit_code {
        out.push_str(&format!(" (exit {code})"));
    }
    out.push('\n');
    if !r.command.is_empty() {
        out.push_str(&format!("  - `{}`\n", r.command));
    }
    // Only surface output for non-passing commands: that is where the agent needs
    // to read why it failed.
    if r.status != CommandStatus::Passed && !r.output.trim().is_empty() {
        out.push_str("  - output:\n\n```\n");
        out.push_str(r.output.trim_end());
        out.push_str("\n```\n");
    }
}

/// Render the report to Markdown.
pub fn render_markdown(report: &SpeculateReport) -> String {
    let mut out = String::new();
    out.push_str("# Speculative run\n\n");
    out.push_str(&format!(
        "**{}** — {}\n\n",
        outcome_word(report.outcome),
        report.summary
    ));
    out.push_str(&format!("- base: `{}`\n", report.base));
    out.push_str(&format!("- change: {}\n", report.change_summary));
    out.push_str(&format!(
        "- applied: {}\n",
        if report.applied { "yes" } else { "no" }
    ));
    if let Some(d) = &report.detected {
        if let Some(lang) = &d.language {
            out.push_str(&format!("- detected: {lang}\n"));
        }
    }
    out.push('\n');

    out.push_str("## Build / type-check\n\n");
    match &report.check {
        Some(r) => render_command(&mut out, r),
        None => out.push_str("- no build/type-check command\n"),
    }
    out.push('\n');

    out.push_str("## At-risk tests\n\n");
    if report.tests.is_empty() {
        out.push_str("- none run\n");
    } else {
        for r in &report.tests {
            render_command(&mut out, r);
        }
        let ran = report
            .tests
            .iter()
            .filter(|t| t.status != CommandStatus::Skipped)
            .count();
        if !report.tests_scoped && ran > 0 {
            out.push_str("\n_ran the whole test suite; at-risk narrowing not applied._\n");
        } else if report.tests_scoped && report.tests_total_at_risk > ran && ran > 0 {
            out.push_str(&format!(
                "\n_ran {} of {} at-risk test(s) (capped at --max-tests)._\n",
                ran, report.tests_total_at_risk
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::CommandResult;
    use crate::speculate::SpeculateReport;

    fn cmd(label: &str, status: CommandStatus, output: &str) -> CommandResult {
        CommandResult {
            label: label.into(),
            command: format!("run {label}"),
            status,
            exit_code: Some(if status == CommandStatus::Passed {
                0
            } else {
                1
            }),
            output: output.into(),
            duration_ms: 5,
        }
    }

    #[test]
    fn renders_outcome_check_and_tests() {
        let report = SpeculateReport {
            version: 1,
            base: "abc1234".into(),
            applied: true,
            change_summary: "working-tree changes vs abc1234".into(),
            detected: None,
            check: Some(cmd("check", CommandStatus::Passed, "ok")),
            tests: vec![cmd(
                "t_login.py",
                CommandStatus::Failed,
                "AssertionError: boom",
            )],
            tests_total_at_risk: 1,
            tests_scoped: true,
            outcome: Outcome::Failed,
            summary: "FAILED: ...".into(),
        };
        let md = render_markdown(&report);
        assert!(md.starts_with("# Speculative run"), "{md}");
        assert!(md.contains("**FAILED**"), "{md}");
        assert!(md.contains("## Build / type-check"), "{md}");
        assert!(md.contains("## At-risk tests"), "{md}");
        assert!(md.contains("t_login.py"), "{md}");
        // The failing test's output is surfaced for the agent to read.
        assert!(md.contains("AssertionError: boom"), "{md}");
        // The passing check's output is not dumped.
        assert!(!md.contains("\nok\n"), "passing output suppressed: {md}");
    }

    #[test]
    fn notes_no_commands() {
        let report = SpeculateReport {
            version: 1,
            base: "abc".into(),
            applied: true,
            change_summary: "x".into(),
            detected: None,
            check: None,
            tests: vec![],
            tests_total_at_risk: 0,
            tests_scoped: false,
            outcome: Outcome::Inconclusive,
            summary: "INCONCLUSIVE".into(),
        };
        let md = render_markdown(&report);
        assert!(md.contains("no build/type-check command"), "{md}");
        assert!(md.contains("none run"), "{md}");
    }
}
