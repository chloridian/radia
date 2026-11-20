Run commands from the repository root unless stated otherwise. Windows is the primary development platform; use WSL2 for Linux builds. CI checks Windows, macOS, and Linux. The Rust toolchain follows the stable channel rather than a fixed release. Cargo aliases in `.cargo/config.toml` cover the everyday commands, and [just](https://github.com/casey/just) recipes wrap them for CI and multi-step tasks; run `just` to list them. CI installs just, Nextest 0.9, cargo-llvm-cov, and cargo-machete; other development tools may vary by checkout.

Network HTTPS uses Reqwest 0.13 with rustls and aws-lc. Building aws-lc requires a C/C++ compiler. Windows x86-64 assembly uses NASM when available or the crate's enabled prebuilt objects. The selected non-FIPS build does not require CMake, bindgen, or Go. See the [aws-lc-rs build guide](https://github.com/aws/aws-lc-rs#build) for supported toolchains. Certificate verification follows the operating system trust configuration.

## Development

```bash
# Build
cargo check --workspace --all-targets
cargo build --workspace --locked
cargo build --workspace --release --locked
cargo build -p <crate>
cargo build -p viewer --bin viewer

# Run viewer
cargo run
cargo run --release
cargo run -p viewer --bin viewer
cargo run -p <package-with-binary> --release
cargo run -p <package-with-binary> -- <args>

# Build diagnostics
cargo build --workspace --timings

# Format
cargo fmt-all
cargo ci-fmt

# Lint
cargo lint
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p <crate> --all-targets -- -D warnings

# Test
cargo test-all
cargo nextest run -p <crate> --all-features
cargo nextest list --workspace

# Run specific tests
cargo nextest run -p <crate> -E 'test(<name>)'
cargo nextest run -p <crate> --run-ignored only
cargo test -p <crate> <name> -- --nocapture

# GPU tests across the workspace (need a Vulkan adapter)
just test-gpu

# Repeat affected tests under parallel test load
cargo nextest archive --workspace --all-features --archive-file target/timing-tests.tar.zst
mkdir target/timing-load
cargo nextest run --archive-file target/timing-tests.tar.zst --extract-to target/timing-load --stress-duration 20m
# In a second terminal
mkdir target/timing-repeat
cargo nextest run --archive-file target/timing-tests.tar.zst --extract-to target/timing-repeat --stress-count 200 -E 'test(<name>)'
```

- `cargo lint` checks all targets and features with warnings denied. `just lint` runs the same check and accepts extra Clippy arguments.
- With `RADIA_CI=true`, which CI sets, `just lint`, `just build`, and `just test` also pass `--locked` and the `ci` Cargo profile.
- `cargo fmt-all` runs rustfmt with the import grouping and merging options, which stable rustfmt accepts only on the command line. Plain `cargo fmt` leaves imports ungrouped, and the hook and CI reject the result. `cargo ci-fmt` is the check-only form.
- `cargo test-all` runs Nextest on the whole workspace with all features and without stopping at the first failure. `just test` runs the same command. Scope a run with `cargo nextest run -p <crate>`.
- `just ci` runs the formatting, Clippy, cargo-machete, build, and test checks in order and stops at the first failure.
- `just security` runs `cargo deny check`, which also covers RustSec advisories. CI does not run it. CodeQL scans the Rust code on pull requests, on pushes to main, and weekly. Dependabot opens grouped weekly updates for Cargo and GitHub Actions.
- Replace `<crate>`, `<name>`, and `<args>` with the relevant values.
- Optional features may require platform-specific dependencies.
- Ignored tests do not run in `cargo test-all`. GPU tests need a Vulkan adapter; run them with the command above after changes to GPU code.
- Stress runs use the same production constants as ordinary tests. Keep the workspace load running throughout the focused repetition and record both commands and their results.
- Build the archive after edits are complete and use fresh extraction directories for each validation run. Both runs use the archived executables, so subsequent Cargo builds cannot overwrite running test binaries on Windows.

## Git hooks

Enable the repository hooks once in each checkout:

```bash
git config --local core.hooksPath .githooks
```

`just hooks` runs the same command. It changes the local Git configuration for the current checkout.

The pre-commit hook checks formatting. The commit-msg hook rejects messages with more than one line and validates the header with `git-conventional-commits`; Node.js and `npx` must be available. There is no pre-push hook. CI runs formatting, cargo-machete, and Linux workspace Clippy with warnings denied as separate checks, then builds and runs the all-feature workspace suite through Nextest on each platform without stopping at the first failure. Jobs that only matter for source changes are skipped when a change touches nothing relevant. CI uses the `ci` Cargo profile.

## Test coverage

```bash
# Generate the same all-feature LCOV report that CI uploads to Codecov
just coverage

# Generate a local HTML report with the same test selection
just coverage-html

# Show uncovered lines across the workspace
cargo llvm-cov --workspace --text --show-missing-lines

# Inspect uncovered lines in one package
cargo llvm-cov -p <crate> --text --show-missing-lines

# Machine-readable per-file summary for scripts
cargo llvm-cov --workspace --json --summary-only
```

HTML report: `target/llvm-cov/html/index.html`
LCOV report: `lcov.info`

`just coverage` runs the all-feature workspace suite through Nextest and writes the LCOV report consumed by Codecov. `just coverage-html` uses the same test selection and writes a local HTML report. Both exclude `test-support` from the report while still running its tests. CI adds the locked dependencies and `ci` Cargo profile through the existing `RADIA_CI` settings.

Start with the affected package when investigating a test gap. Workspace missing-line reports can be large. Coverage reflects the features and test targets selected by the command; `--summary-only` requires an export format such as `--json`, `--lcov`, or `--cobertura`. Linux CI uploads the LCOV report to Codecov with OIDC; project and patch statuses are informational.

## Benchmarks

```bash
# Build benchmark targets
cargo bench --workspace --no-run

# Run a crate's benchmarks
cargo bench -p network

# Filter benchmark names with a regular expression
cargo bench -p render -- 'scene_(reconcile|prim_mesh)'

# Quick and thorough local runs
cargo bench -p world -- --sample-count 20 --sample-size 5 --min-time 1 --max-time 3
cargo bench -p world -- --sample-count 100 --sample-size 10 --min-time 5
```

Each benchmark target uses CodSpeed's Divan compatibility package and `divan::main()`. Divan takes benchmark names from function identifiers; prefixes distinguish related operations and formats. Parameterized workloads use `args`, and `with_inputs` keeps fixture setup outside the measured operation. Throughput uses Divan's byte and item counters. CodSpeed's current compatibility package accepts these counters but does not include them in results. The inventory package name is version-qualified (`-p inventory@0.0.1`) because a dependency also uses `inventory`.

- Bench CI maps changed paths to package shards and benchmark name filters. A shard runs all its benchmarks when a change has no narrower filter.
- `cargo bench-all` runs the workspace with 100 samples, 10 iterations per sample, and a 5-second minimum per benchmark. `cargo bench-quick` uses 20 samples, 5 iterations per sample, a 1-second minimum, and a 3-second maximum.
- `just perf` and `just perf-quick` run the thorough and quick workspace configurations. Use `cargo bench -p <crate> -- <regex>` for focused runs.
- `just codspeed-build` and `just codspeed-run` build and run the same Divan targets under CodSpeed simulation. Install `cargo-codspeed` and the `codspeed` CLI for local CodSpeed runs.
- Larger workload benchmarks execute one complete workload operation per Divan iteration. Their scenarios and diagnostic reporting remain in the benchmark code.
- GPU preparation benchmarks are compiled out under `cfg(codspeed)`; they require a GPU adapter and are not represented by CPU simulation. Texture compression remains limited to the 512 and 1024 sizes under CodSpeed to keep simulation time reasonable.
- Run timing baselines without concurrent tests or builds. The parallel workspace load described above is for flakiness validation.

## Dependencies

```bash
# Dependency trees
cargo tree --workspace --duplicates
cargo tree -p <crate>
cargo tree -i <crate>

# Security and policies
cargo audit
cargo deny check

# Maintenance
cargo outdated --workspace
cargo +nightly udeps --workspace
```

- `cargo deny` requires `deny.toml`.
- `cargo udeps` may report false positives for optional features.
- Duplicate dependencies are not necessarily problems; inspect before consolidating.

## Code analysis

```bash
# Binary size
cargo bloat --release -p <crate> --bin <bin> --crates
cargo bloat --release -p <crate> --bin <bin> -n <count>

# Macro expansion
cargo expand -p <crate> --lib

# Undefined behavior checks
cargo +nightly miri test -p <crate>

# Mutation testing
cargo mutants -p <crate>
cargo mutants -f <file-path>
```

- `cargo bloat` measures linked code size, not memory usage.
- Miri may fail on unsupported OS operations; prefer focused tests.
- Mutation testing is expensive; start with individual files.
- `rust-analyzer-mcp` is available on Windows for editor and agent integration.

## Platforms

The CI workflow builds, lints, and tests on Windows, macOS, and Linux.

Prefer `~/Radia` over `/mnt/c/Radia` for Linux builds under WSL2. Keep build artifacts separate between platforms. Windows `.exe` tools exposed through WSL's `PATH` do not replace Linux-native installations.

On Linux, remembered login storage uses Secret Service and requires a running provider such as GNOME Keyring or KWallet.
