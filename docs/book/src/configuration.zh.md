# LLM 配置

Koda Agent 的推荐配置模型是：`llms.toml` 保存 provider/profile/model 结构，`.env` 保存密钥和当前 selector。

## 文件位置

默认用户目录：

```text
~/.koda-agent/
  .env
  config/
    llms.toml
  resources/
  memory/
```

查看实际路径：

```bash
koda-agent config path
koda-agent doctor --json
```

## Profile 与 Model

- profile 表示一个 provider/account/endpoint，例如 `mimo`、`openai`、`deepseek`。
- model alias 表示 profile 下的模型别名，例如 `pro`、`flash`、`vision`。
- 运行时可以选择 `profile:model`，例如 `mimo:flash`。

示例：

```toml
[selector]
default_profile = "mimo"
default_model = "pro"

[defaults]
stream = true
timeout_secs = 1200
max_tokens = 16384
failover = true

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

[[profiles.models]]
name = "flash"
id = "mimo-v2.5"
```


## 常用 Provider 模板

```bash
koda-agent config setup deepseek --yes
koda-agent config model use deepseek flash

koda-agent config setup openai --yes
koda-agent config model use openai default

koda-agent config setup glm --yes
koda-agent config model use glm default
```

- DeepSeek 默认只配置 `deepseek-v4-pro` 与 `deepseek-v4-flash`。
- OpenAI 默认配置 GPT-5 系列：`gpt-5.2`、`gpt-5.2-pro`、`gpt-5-mini`、`gpt-5.2-codex`。
- GLM 默认配置 `glm-5.1`，并保留 `glm-4.7` stable alias 与 `glm-4.5-flash` flash alias。

## 密钥管理

密钥只写入 `~/.koda-agent/.env`：

```bash
printf '%s' "$MIMO_API_KEY" | koda-agent config secret MIMO_API_KEY --from-stdin
```

不要把 `.env` 提交到 Git。日志和工具输出会尽量脱敏，但不要主动让模型读取本地 secret 文件。

## 选择模型

持久选择：

```bash
koda-agent config use mimo
koda-agent config model use mimo flash
koda-agent config use mimo:flash
```

临时选择：

```bash
koda-agent --profile mimo --model flash --input "hello"
koda-agent --llm mimo:flash --input "hello"
```

TUI / slash command：

```text
/llms
/llm mimo:flash
/models
/model pro
```

## OpenAI-compatible、Responses、Claude Messages

配置里的 `api_mode` 决定请求协议：

- `chat_completions`：OpenAI-compatible `/chat/completions`。
- `responses`：OpenAI Responses 风格。
- `claude_messages`：Claude `/v1/messages` 风格。

具体 provider 是否可用取决于供应商兼容程度。`koda-agent config validate` 只检查配置和必要 secret，不等价于真实模型调用成功。真实验收建议运行：

```bash
koda-agent --llm mimo:pro --input "用一句话回复：配置验证成功"
```

## 超时与长上下文

长上下文、慢模型、工具调用链较长时，建议把 profile 或 defaults 的 `timeout_secs` 设置得更长：

```bash
koda-agent config set deepseek timeout_secs 1800
```

如果 provider 对 streaming、thinking、tool call adjacency 有特殊要求，优先用当前 provider 已验证的 `api_mode` 和模型 alias。DeepSeek V4 模板只保留 `deepseek-v4-pro` / `deepseek-v4-flash`；OpenAI 模板使用 GPT-5 系列；GLM 模板走 BigModel OpenAI-compatible Chat Completions，并对 GLM thinking 发送 `thinking = { type = "enabled" }`。
