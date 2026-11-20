set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

ci_cargo := if env("RADIA_CI", "") == "true" { "--locked --profile ci" } else { "" }
ci_nextest := if env("RADIA_CI", "") == "true" { "--locked --cargo-profile ci" } else { "" }

default:
    @just --list

[doc('Format the workspace with the import grouping rustfmt accepts only on the command line')]
fmt:
    cargo fmt-all

[doc('Check formatting')]
fmt-check:
    cargo ci-fmt

[doc('Clippy on all targets and features, warnings denied')]
lint *flags:
    cargo clippy --workspace --all-targets --all-features {{ ci_cargo }} {{ flags }} -- -D warnings

[doc('Fail on unused dependencies')]
machete:
    cargo machete

[doc('Build the workspace with all features')]
build *flags:
    cargo build --workspace --all-features {{ ci_cargo }} {{ flags }}

[doc('Run the workspace suite through Nextest')]
test *flags:
    cargo test-all {{ ci_nextest }} {{ flags }}

[doc('Run GPU tests across the workspace; they need a Vulkan adapter')]
test-gpu:
    cargo nextest run --workspace --all-features --release --run-ignored only -E 'not (package(network) & test(udp_burst_measurement))'

[doc('Check licenses, bans, sources, and RustSec advisories')]
security:
    cargo deny check

[doc('Run the checks CI runs, stopping at the first failure')]
ci: fmt-check lint machete build test

[doc('Enable the repository git hooks in this checkout')]
hooks:
    git config --local core.hooksPath .githooks

[doc('Run workspace benchmarks with the thorough settings')]
perf:
    cargo bench-all

[doc('Run workspace benchmarks with the quick settings')]
perf-quick:
    cargo bench-quick

[doc('Run the viewer profiling build with Tracy')]
profile:
    cargo profile
