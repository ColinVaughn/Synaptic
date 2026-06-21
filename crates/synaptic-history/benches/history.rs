use synaptic_history::report::module_of;
use criterion::{criterion_group, criterion_main, Criterion};

fn bench_module_of(c: &mut Criterion) {
    c.bench_function("module_of", |b| {
        b.iter(|| module_of("crates/foo/src/bar/baz.rs", 1))
    });
}

criterion_group!(benches, bench_module_of);
criterion_main!(benches);
