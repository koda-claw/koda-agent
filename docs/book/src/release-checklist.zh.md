# 发布验收清单

本清单用于 tag / push 前的本机验收。原则是先验证本机安装和真实命令，再提交、打 tag、推送。

## 1. 完整门禁

```bash
cargo fmt --all --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
scripts/audit-secrets.sh
scripts/audit-history.sh
make release-dry-run
```

可选专项 smoke：

```bash
make smoke-tui
cargo run -q -p xtask -- memory-parity-smoke
make smoke-tmwd-static-parity
```

浏览器真实矩阵需要本机 Edge/Chrome 插件和 bridge 环境，不作为无条件阻塞项：

```bash
make tmwebdriver
make smoke-tmwd-extension
make smoke-tmwd-matrix
```

## 2. 本机安装与真实验收

```bash
scripts/install.sh --from-source
koda-agent --version
koda-agent --help
koda-agent doctor
koda-agent resources doctor
koda-agent config validate
```

如果本机已配置可用 profile，做一次真实 LLM smoke：

```bash
koda-agent --profile mimo --input "用一句话回复：配置验证成功"
```

如果 profile 有多个模型：

```bash
koda-agent --llm mimo:pro --input "用一句话回复：配置验证成功"
koda-agent --llm mimo:flash --input "用一句话回复：配置验证成功"
```

## 3. Diff 与隐私检查

```bash
git status --short
git diff --stat
git diff --cached --stat
git diff -- . ':(exclude).env'
```

必须确认：

- 没有提交 `.env`、真实 API key、用户 token、cookie、浏览器 profile 数据。
- 没有提交 `temp/`、`logs/`、运行期 `memory/` 里的用户产出。
- README、中文文档、CLI help、实际命令输出一致。

## 4. Commit / Tag / Push

```bash
git add <files>
git commit -m "..."
git tag v0.1.0
git push origin main
git push origin v0.1.0
```

如果 tag 后发现问题，不要覆盖已发布 tag。修复后发新 patch tag。
