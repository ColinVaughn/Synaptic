//! Benchmarks for the deterministic parts of a speculative run. The recursive
//! command planner intentionally includes a warm filesystem walk; worktree and
//! child-process execution remain outside Criterion because their cost belongs to
//! Git and the repository's own toolchain.

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use synaptic_sandbox::{
    CommandResult, CommandStatus, Outcome, SpeculateReport, detect_command_plan, detect_commands,
    render_markdown,
};
use tempfile::TempDir;

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

    bench_recursive_command_plan(c);
}

fn command_plan_fixture(directories: usize, project_stride: usize) -> TempDir {
    let fixture = tempfile::tempdir().expect("create command-plan benchmark fixture");
    for index in 0..directories {
        let directory = fixture.path().join(format!("project-{index:05}"));
        std::fs::create_dir(&directory).expect("create fixture directory");
        if index % project_stride == 0 {
            std::fs::write(
                directory.join("go.mod"),
                format!("module benchmark.test/project{index}\n"),
            )
            .expect("write fixture manifest");
            std::fs::write(directory.join("main.go"), "package main\n")
                .expect("write fixture source");
        } else {
            std::fs::write(directory.join("README.md"), "benchmark fixture\n")
                .expect("write fixture filler");
        }
    }
    fixture
}

fn bench_recursive_command_plan(c: &mut Criterion) {
    let fixtures = [
        (100_usize, command_plan_fixture(100, 10)),
        (1_000, command_plan_fixture(1_000, 10)),
        (5_000, command_plan_fixture(5_000, 10)),
    ];
    let mut group = c.benchmark_group("command_plan/scaling");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));

    for (directories, fixture) in &fixtures {
        let plan = detect_command_plan(fixture.path()).expect("detect fixture command plan");
        assert_eq!(plan.projects.len(), directories / 10);
        assert!(plan.gaps.is_empty());
        assert!(!plan.truncated);
        group.throughput(Throughput::Elements(plan.directories_scanned as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(directories),
            fixture,
            |bencher, fixture| {
                bencher.iter(|| {
                    detect_command_plan(std::hint::black_box(fixture.path()))
                        .expect("detect fixture command plan")
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
