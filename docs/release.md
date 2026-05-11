# Release Process

## Local release dry run

```bash
make release-dry-run
```

This builds the release binary, assembles a local archive containing
`koda-agent` plus `resources/`, writes `dist/SHA256SUMS`, and checks that the
binary starts and can emit `doctor --json` against the packaged resources.

## Pre-release gates

```bash
make audit-secrets
make audit-history
cargo fmt --all --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run -q -p xtask -- tui-smoke
cargo run -q -p xtask -- memory-parity-smoke
cargo run -q -p xtask -- tmwd-static-parity-smoke
```

## GitHub release artifact names

- `koda-agent-aarch64-apple-darwin.tar.gz`
- `koda-agent-x86_64-apple-darwin.tar.gz`
- `koda-agent-x86_64-unknown-linux-gnu.tar.gz`
- `koda-agent-aarch64-unknown-linux-gnu.tar.gz`
- `koda-agent-x86_64-pc-windows-msvc.zip`
- `koda-agent-aarch64-pc-windows-msvc.zip`
- `SHA256SUMS`

## Tagging

Use semantic version tags:

```bash
git tag v0.1.0
git push origin v0.1.0
```

Release notes should include compatibility changes, LLM protocol changes,
security-sensitive fixes, installer changes, and known gaps versus upstream
GenericAgent.
