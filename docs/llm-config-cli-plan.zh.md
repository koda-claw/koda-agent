# LLM Config CLI 方案状态

本文件原先记录的是早期 “一个 profile 绑定一个 model” 的配置 CLI 方案。该方案已经被新的 Profile / Model 分层方案取代，不再作为实现依据。

当前权威方案：`docs/profile-model-config-plan.zh.md`。

## 当前配置原则

- `profile` 表示 provider/account/endpoint，例如 `mimo`、`deepseek`、`openai-compat`。
- `model` 表示 profile 下的模型 alias，例如 `pro`、`flash`、`reasoner`、`default`。
- Runtime 使用扁平 key：`profile:model`。
- Secret 只保存在 `.env` 或外部环境变量里，不能写入 `llms.toml`。
- `~/.koda-agent/config/llms.toml` 是实际配置文件，`llms.example.toml` 只是参考模板。

## 当前推荐命令

```bash
koda-agent init
koda-agent config setup mimo --yes
koda-agent config secret MIMO_API_KEY --from-stdin
koda-agent config list
koda-agent config show mimo
koda-agent config model list mimo
koda-agent config model add deepseek flash --id deepseek-v4-flash
koda-agent config model use deepseek flash
koda-agent config model remove deepseek flash --force
koda-agent config use deepseek:flash
koda-agent config validate
koda-agent --profile deepseek --model flash --input "hi"
koda-agent --llm deepseek:flash --input "hi"
```

## 当前 TOML 示例

```toml
[selector]
default_profile = "mimo"
default_model = "pro"

[[profiles]]
name = "mimo"
kind = "native_oai"
base_url = "https://api.xiaomimimo.com/v1"
api_key_env = "MIMO_API_KEY"
auth_scheme = "header"
auth_header = "api-key"
api_mode = "chat_completions"

[[profiles.models]]
name = "pro"
id = "mimo-v2.5-pro"
```

## 旧方案处理

旧的 profile 级 `model` 字段不再推荐：

```toml
# legacy only; do not use in new config
[[profiles]]
name = "openai-compat"
model = "旧 OPENAI_MODEL"
```

处理方式：

```bash
koda-agent config migrate --force
```

迁移后应该得到：

```toml
[[profiles.models]]
name = "default"
id = "旧 OPENAI_MODEL"
```

## 验收

以 `docs/profile-model-config-plan.zh.md` 的 Phase 1 到 Phase 6 为准。最小门禁：

```bash
cargo fmt --all --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
make release-dry-run
scripts/audit-secrets.sh
scripts/audit-history.sh
```
