Run commands from the repository root unless stated otherwise. Windows is the primary development platform; use WSL2 for Linux builds. CI checks Windows, macOS, and Linux. The Rust toolchain follows the stable channel rather than a fixed release. CI installs Nextest 0.9; other development tools may vary by checkout.

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
cargo fmt --all
cargo fmt --all -- --check

# Lint
cargo lint
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p <crate> --all-targets -- -D warnings

# Test
cargo xtest --workspace
cargo xtest -p <crate>
cargo nextest list --workspace

# Run specific tests
cargo nextest run -p <crate> -E 'test(<name>)'
cargo nextest run -p <crate> --run-ignored only
cargo test -p <crate> <name> -- --nocapture

# Render GPU tests (need a Vulkan adapter)
cargo nextest run -p render --release --run-ignored only

# Repeat affected tests under parallel test load
cargo nextest archive --workspace --all-features --archive-file target/timing-tests.tar.zst
mkdir target/timing-load
cargo nextest run --archive-file target/timing-tests.tar.zst --extract-to target/timing-load --stress-duration 20m
# In a second terminal
mkdir target/timing-repeat
cargo nextest run --archive-file target/timing-tests.tar.zst --extract-to target/timing-repeat --stress-count 200 -E 'test(<name>)'
```

- `cargo lint` checks all targets and features with warnings denied.
- `cargo xtest` runs Nextest with all features, but does not explicitly select the workspace. Add `--workspace` when needed.
- Replace `<crate>`, `<name>`, and `<args>` with the relevant values.
- Optional features may require platform-specific dependencies.
- Ignored tests do not run in `cargo xtest`. The `render` GPU tests need a Vulkan adapter; run them with the command above on render changes.
- Stress runs use the same production constants as ordinary tests. Keep the workspace load running throughout the focused repetition and record both commands and their results.
- Build the archive after edits are complete and use fresh extraction directories for each validation run. Both runs use the archived executables, so subsequent Cargo builds cannot overwrite running test binaries on Windows.

## Git hooks

Enable the repository hooks once in each checkout:

```bash
git config --local core.hooksPath .githooks
```

This command changes the local Git configuration for the current checkout.

The pre-commit hook checks formatting. There is no pre-push hook. CI runs workspace Clippy with warnings denied, then the all-feature workspace suite through Nextest without stopping at the first failure.

## Test coverage

```bash
# Summary across workspace crates
cargo llvm-cov --workspace

# Show uncovered lines across the workspace
cargo llvm-cov --workspace --text --show-missing-lines

# Inspect uncovered lines in one package
cargo llvm-cov -p <crate> --text --show-missing-lines

# Generate an HTML report
cargo llvm-cov --workspace --html

# Generate an HTML report with Nextest and all features
cargo llvm-cov nextest --workspace --all-features --html

# Machine-readable per-file summary for scripts
cargo llvm-cov --workspace --json --summary-only
```

Report: `target/llvm-cov/html/index.html`

Start with the affected package when investigating a test gap. Workspace missing-line reports can be large. Coverage reflects the features and test targets selected by the command; `--summary-only` requires an export format such as `--json`, `--lcov`, or `--cobertura`.

## Benchmarks

```bash
# Build the single benchmark target for these crates
cargo bench -p network -p protocol --no-run

# Smoke run each crate's benchmarks
cargo bench -p network -- --test
cargo bench -p protocol -- --test

# Quick
cargo bench -p <crate> -- <filter> --sample-size 30 --warm-up-time 1 --measurement-time 3 --noplot

# Thorough
cargo bench -p <crate> -- <filter> --sample-size 200 --warm-up-time 5 --measurement-time 15
```

Each crate has one `main` benchmark target. Replace the package and Criterion filter as needed. The inventory package name is version-qualified (`-p inventory@0.0.1`) because a dependency also uses `inventory`. For example, `cargo bench -p render -- reconcile` selects reconciliation benchmarks. Run timing baselines without concurrent tests or builds; the parallel workspace load above is for flakiness validation.

- `cargo perf` runs workspace benchmarks with 200 samples, a 5-second warm-up, and a 15-second measurement. `cargo perf-quick` uses 30 samples, a 1-second warm-up, a 3-second measurement, and no plot. Use `cargo bench -p <crate> -- <filter> ...` for a focused run.
- `cargo profile`: viewer profiling build with Tracy.
- `cargo flamegraph`: requires a configured profiler backend; WSL2 may restrict Linux `perf`.

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
