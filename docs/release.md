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
cargo fmt --all --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
make audit-secrets
make audit-history
make release-dry-run
```

## Local install acceptance

Run this before committing and tagging so the installed CLI path is verified, not
only the workspace binary:

```bash
scripts/install.sh --from-source
koda-agent --version
koda-agent --help
koda-agent doctor
koda-agent resources doctor
koda-agent config validate
```

If a real profile is configured locally, run at least one provider smoke:

```bash
koda-agent --profile mimo --input "用一句话回复：配置验证成功"
```

Optional targeted smoke checks:

```bash
cargo run -q -p xtask -- tui-smoke
cargo run -q -p xtask -- memory-parity-smoke
cargo run -q -p xtask -- tmwd-static-parity-smoke
```

Check the final diff and staged set after local install acceptance:

```bash
git status --short
git diff --stat
git diff --cached --stat
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
GenericAgent. Keep the current draft in `docs/release-notes.md`.
