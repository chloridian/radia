#![expect(clippy::indexing_slicing)]

mod packets;
mod receive;

fn main() {
    divan::main();
}
