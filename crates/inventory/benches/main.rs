use criterion::{Criterion, criterion_group, criterion_main};
use test_support::allocation::CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

mod inventory;

fn bench_inventory(c: &mut Criterion) {
    inventory::bench(c);
}

criterion_group!(benches, bench_inventory);
criterion_main!(benches);
