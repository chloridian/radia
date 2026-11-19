set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

ci_cargo := if env("RADIA_CI", "") == "true" { "--locked --profile ci" } else { "" }
ci_nextest := if env("RADIA_CI", "") == "true" { "--locked --cargo-profile ci" } else { "" }

default:
    @just --list

[doc('Format the workspace with the import grouping rustfmt accepts only on the command line')]
fmt:
    cargo xfmt

[doc('Check formatting')]
fmt-check:
    cargo xfmt --check

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
    cargo xtest --workspace --no-fail-fast {{ ci_nextest }} {{ flags }}

[doc('Run the render GPU tests; they need a Vulkan adapter')]
render-gpu-tests:
    cargo nextest run -p render --release --run-ignored only

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
    cargo perf

[doc('Run workspace benchmarks with the quick settings')]
perf-quick:
    cargo perf-quick

[doc('Run the viewer profiling build with Tracy')]
profile:
    cargo profile
