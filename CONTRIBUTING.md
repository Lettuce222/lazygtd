# Contributing

Use the repository's pinned Rust toolchain and a C compiler for bundled SQLite.
Run these checks on the branch before opening a pull request:

    cargo fmt --all -- --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
    cargo run --locked -- --demo

Use synthetic fixtures and temporary databases in tests. Do not add private
task data, agent settings, credentials, built executables, or local DB files.
Include reproduction steps and the platform when reporting a bug. See
[SECURITY.md](SECURITY.md) for vulnerability reports.

Release tags must match the Cargo version prefixed with v. GitHub Actions tests
and builds the supported operating-system and CPU matrix, packages notices,
generates SHA256SUMS and signed provenance, then publishes the Release.
