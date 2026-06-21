//! Microbench for the deterministic, CPU-bound parts of a speculative run:
//! command detection and Markdown rendering. The worktree/process execution
//! itself is IO-bound and not a meaningful criterion target.

use criterion::{criterion_group, criterion_main, Criterion};
use synaptic_sandbox::{
    detect_commands, render_markdown, CommandResult, CommandStatus, Outcome, SpeculateReport,
};

fn big_report() -> SpeculateReport {
    let tests: Vec<CommandResult> = (0..200)
        .map(|i| CommandResult {
            label: format!("tests/test_{i}.py"),
            command: format!("pytest tests/test_{i}.py"),
            status: if i % 7 == 0 {
                CommandStatus::Failed
            } else {
                CommandStatus::Passed
            },
            exit_code: Some(if i % 7 == 0 { 1 } else { 0 }),
            output: "line one\nline two\nAssertionError: boom".repeat(3),
            duration_ms: 12,
        })
        .collect();
    SpeculateReport {
        version: 1,
        base: "0123456789abcdef0123456789abcdef01234567".into(),
        applied: true,
        change_summary: "working-tree changes vs 01234567".into(),
        detected: None,
        check: Some(CommandResult {
            label: "check".into(),
            command: "cargo build".into(),
            status: CommandStatus::Passed,
            exit_code: Some(0),
            output: String::new(),
            duration_ms: 100,
        }),
        tests,
        tests_total_at_risk: 200,
        tests_scoped: true,
        outcome: Outcome::Failed,
        summary: "FAILED".into(),
    }
}

fn bench(c: &mut Criterion) {
    let report = big_report();
    let markers: Vec<String> = ["package.json", "tsconfig.json", "README.md"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    c.bench_function("render_markdown_200_tests", |b| {
        b.iter(|| render_markdown(std::hint::black_box(&report)))
    });
    c.bench_function("detect_commands", |b| {
        b.iter(|| detect_commands(std::hint::black_box(&markers)))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
