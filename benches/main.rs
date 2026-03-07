mod storage;

use criterion::{criterion_group, criterion_main};

criterion_group!(benches, storage::bench_encryption);
criterion_main!(benches);
