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

Each push to `main` queues a release build for that commit. GitHub Actions
allocates the next version, builds and tests the supported platforms, packages
notices, generates SHA256SUMS and signed provenance, and then tags the tested
commit and publishes. Update the base version in both `Cargo.toml` and
`Cargo.lock` when starting a new release series.
