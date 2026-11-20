mod cache;
mod store;
mod texture;

fn main() {
    store::report_diagnostics();
    divan::main();
}
