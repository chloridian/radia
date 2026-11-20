#![cfg_attr(codspeed, allow(dead_code))]

#[cfg(not(codspeed))]
mod prepare;
mod prim;
mod reconcile;
mod support;
mod texture;

use criterion::{Criterion, criterion_group, criterion_main};

fn bench_render(c: &mut Criterion) {
    prim::bench(c);
    #[cfg(not(codspeed))]
    prepare::bench(c);
    reconcile::bench(c);
    texture::bench(c);
}

criterion_group!(benches, bench_render);
criterion_main!(benches);
