#![cfg_attr(codspeed, allow(dead_code))]
#![expect(clippy::unwrap_used, clippy::indexing_slicing)]

#[cfg(not(codspeed))]
mod prepare;
mod prim;
mod reconcile;
mod support;
mod texture;

fn main() {
    divan::main();
}
