mod lifecycle;
mod load;
mod pose;
mod relations;
mod support;
mod updates;

fn main() {
    lifecycle::report_linked_diagnostics();
    updates::report_diagnostics();
    divan::main();
}
