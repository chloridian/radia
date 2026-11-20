#![cfg_attr(codspeed, allow(dead_code))]

#[cfg(not(codspeed))]
mod prepare;
mod prim;
mod reconcile;
mod support;
mod texture;

fn main() {
    divan::main();
}
