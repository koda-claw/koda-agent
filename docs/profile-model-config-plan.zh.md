# Profile / Model 配置重构方案

## 1. 背景

当前旧配置把 `profile` 和 `model` 混在一起：

```toml
[[profiles]]
name = "deepseek"
kind = "native_oai"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
model = "deepseek-v4-pro"
```

这个写法在只有一个模型时够用，但同一个 provider/account/endpoint 下存在多个模型时会产生明显问题：

- 同一个 provider 的 `base_url`、`api_key_env`、`auth_scheme`、`api_mode` 被重复配置。
- `/llm deepseek` 的含义不清楚：它到底是 provider，还是某个具体模型。
- CLI/TUI 无法自然表达“同一个 profile 下切换 pro / flash / reasoner / vision”。
- 后续做模型级参数覆盖、failover 顺序、视觉模型、reasoning 模型时会越来越绕。

因此重构为两层语义：

- `profile`：provider/account/endpoint，例如 `deepseek`、`mimo`、`openai-compat`。
- `model`：某个 profile 下的本地模型别名，例如 `pro`、`flash`、`reasoner`、`vision`。
- Runtime 使用统一扁平 key：`profile:model`。

本方案不保留旧 `[[profiles]] model = "..."` 写法作为正式 schema；遇到旧写法时给出清晰迁移提示或由 `config migrate` 转换。

## 2. 目标

1. `llms.toml` 清晰表达 provider 与 model 的层级关系。
2. 用户可以通过 CLI 完成常用配置，不必须手动打开 TOML。
3. 支持同一个 profile 下多个模型，并可用 `profile:model` 精确选择。
4. Runtime 继续复用现有 Multi LLM / failover / switch 机制，避免重写协议层。
5. Slash command 和 TUI 与 CLI 使用同一套选择语义。
6. Secret 仍然只来自 `.env` / 环境变量，不写入 TOML，不打印密钥。
7. 初始化后的用户目录形态稳定：`~/.koda-agent/config/llms.toml` 是实际配置，`llms.example.toml` 只是参考模板。

## 3. 非目标

- 不重写 OpenAI Chat、OpenAI Responses、Claude Messages、tool calling、SSE streaming 协议实现。
- 不把多个 provider 的 API key 写进 `llms.toml`。
- 不为旧 schema 做长期双轨维护；旧配置只允许迁移，不作为推荐路径。
- 不在本阶段处理 IM/GUI 前端。

## 4. 新配置 Schema

推荐结构：

```toml
[selector]
default_profile = "deepseek"
default_model = "pro"

[defaults]
stream = true
timeout_secs = 1200
connect_timeout_secs = 30
verify_tls = true
temperature = 1.0
max_tokens = 16384
failover = true

[[profiles]]
name = "deepseek"
kind = "native_oai"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
auth_scheme = "bearer"
api_mode = "chat_completions"

[[profiles.models]]
name = "pro"
id = "deepseek-v4-pro"
reasoning_effort = "medium"
max_tokens = 16384

[[profiles.models]]
name = "flash"
id = "deepseek-v4-flash"
max_tokens = 32768

[[profiles.models]]
name = "reasoner"
id = "deepseek-reasoner"
reasoning_effort = "high"
max_tokens = 16384
```

字段约定：

- `[selector].default_profile`：默认 provider/profile。
- `[selector].default_model`：默认 model alias。
- `[[profiles]].name`：provider/account/endpoint 名称。
- `[[profiles]].kind`：协议类型，继续使用现有值，例如 `native_oai`、`native_claude`、`oai`、`claude`。
- `[[profiles]].base_url`：provider endpoint。
- `[[profiles]].api_key_env`：读取 secret 的环境变量名。
- `[[profiles]].auth_scheme` / `auth_header`：鉴权方式。
- `[[profiles.models]].name`：本地 model alias。
- `[[profiles.models]].id`：真实发给 provider API 的模型名。

## 5. 参数继承规则

继承优先级：

```text
model 字段 > profile 字段 > defaults 字段 > 代码默认值
```

Profile 级字段由所有 models 继承：

- `kind`
- `base_url`
- `api_key_env`
- `auth_scheme`
- `auth_header`
- `api_mode`
- `headers`
- `proxy`
- `stream`
- `timeout_secs`
- `connect_timeout_secs`
- `verify_tls`

Model 级可覆盖字段：

- `id` / `model`
- `temperature`
- `max_tokens`
- `reasoning_effort`
- `thinking_type`
- `thinking_budget_tokens`
- `service_tier`
- `stream`
- `timeout_secs`
- `connect_timeout_secs`
- `verify_tls`
- `proxy`
- `headers`

设计原因：provider 级字段描述“怎么连到服务”，model 级字段描述“这次模型怎么生成”。允许少量传输字段在 model 级覆盖，是为了支持视觉模型、长上下文模型、慢 reasoning 模型使用不同 timeout/proxy/header。

## 6. Runtime 展开规则

加载配置时，把嵌套结构展开成扁平 LLM 列表：

```text
deepseek:pro      -> deepseek-v4-pro
deepseek:flash    -> deepseek-v4-flash
deepseek:reasoner -> deepseek-reasoner
mimo:pro          -> mimo-v2.5-pro
```

内部建议结构：

```rust
ResolvedLlmModel {
    key: "deepseek:pro",
    profile_name: "deepseek",
    model_alias: "pro",
    model_id: "deepseek-v4-pro",
    config: LlmModelConfig,
}
```

现有 `MultiLlmClient` 继续吃扁平列表：

- `list_llms()` 返回 `deepseek:pro (deepseek-v4-pro)`。
- `switch_llm(n)` 继续按序号切换。
- `switch_llm_by_name("deepseek:pro")` 精确切换。
- `switch_llm_by_name("deepseek")` 切换到该 profile 的默认模型。
- `switch_llm_model_by_name("flash")` 在当前 profile 下切换模型 alias。

这样协议层、tool calling、history trimming、SSE、usage 统计不需要知道配置原本是嵌套的。

## 7. 选择优先级

CLI：

```bash
koda-agent --profile deepseek --model flash --input "..."
koda-agent --llm deepseek:flash --input "..."
koda-agent --profile deepseek --input "..."
```

优先级：

```text
--llm profile:model
> --profile + --model
> KODA_LLM_PROFILE + KODA_LLM_MODEL
> selector.default_profile + selector.default_model
> 第一个 profile 的第一个 model
```

环境变量：

```bash
KODA_LLM_PROFILE=deepseek
KODA_LLM_MODEL=flash
```

约束：

- `--llm` 只接受 `profile:model`。
- `--profile` 可以单独使用；此时使用该 profile 的默认 model 或第一个 model。
- `--model` 单独使用时只在默认 profile 下选择 model。
- 如果 selector 指向不存在的 profile/model，必须报清晰错误。

## 8. Slash / TUI 语义

支持命令：

```text
/llm
/llms
/llm deepseek
/llm deepseek:flash
/models
/model flash
/status
```

建议输出：

```text
LLMs:
-> [0] deepseek:pro      deepseek-v4-pro
   [1] deepseek:flash    deepseek-v4-flash
   [2] mimo:pro          mimo-v2.5-pro
```

`/models` 只显示当前 profile 下的 models：

```text
Models for deepseek:
-> pro       deepseek-v4-pro
   flash     deepseek-v4-flash
   reasoner  deepseek-reasoner
```

命令语义：

- `/llm deepseek`：切换到 deepseek profile 的默认 model。
- `/llm deepseek:flash`：精确切换到 deepseek 的 flash model。
- `/model flash`：在当前 profile 内切换到 flash。
- `/models`：展示当前 profile 的所有 model alias。

## 9. Config CLI 设计

### 9.1 初始化

`koda-agent init`：

- 创建 `~/.koda-agent/config/llms.toml`，这是实际配置文件。
- 创建 `~/.koda-agent/config/llms.example.toml`，这是参考模板。
- 创建 `~/.koda-agent/.env`，只写环境变量名和空值/占位值，不写真实 secret。
- 默认 selector 写入 `default_profile` 和 `default_model`。

### 9.2 Provider/Profile 命令

保留并调整：

```bash
koda-agent config list
koda-agent config show
koda-agent config show deepseek
koda-agent config validate
koda-agent config setup
koda-agent config add <profile> --kind native_oai --base-url ... --api-key-env ...
koda-agent config use deepseek
koda-agent config use deepseek:flash
koda-agent config migrate --force
```

行为要求：

- `list` 展示 profile 及其 models。
- `show` 展示 provider 字段、models、selector，不展示密钥值。
- `validate` 检查：profile 存在、model 存在、secret env 存在、旧 schema 是否需要迁移。
- `use deepseek:flash` 同时更新 `KODA_LLM_PROFILE` 和 `KODA_LLM_MODEL`。
- `migrate` 将旧 `.env` / 旧 `llms.toml` 转成新 schema。

### 9.3 Model 命令

新增：

```bash
koda-agent config model list <profile>
koda-agent config model add <profile> <alias> --id <provider-model-id>
koda-agent config model set <profile> <alias> <key> <value>
koda-agent config model use <profile> <alias>
koda-agent config model remove <profile> <alias> [--force]
```

字段限制：

- `add` 至少需要 alias 和真实 model id。
- `set` 只允许 model 级字段，例如 `id`、`max_tokens`、`temperature`、`reasoning_effort`、`stream`、`timeout_secs`。
- `remove` 不允许删除 profile 的唯一 model；删除当前 active model 时必须先切换到其他 model，或使用 `--force` 自动切到剩余的第一个 model。

## 10. 迁移策略

旧 schema：

```toml
[[profiles]]
name = "openai-compat"
model = "gpt-5.2"
```

迁移后：

```toml
[selector]
default_profile = "openai-compat"
default_model = "default"

[[profiles]]
name = "openai-compat"
kind = "native_oai"
base_url = "..."
api_key_env = "OPENAI_API_KEY"

[[profiles.models]]
name = "default"
id = "gpt-5.2"
```

`.env` 迁移：

```bash
OPENAI_BASE_URL=...
OPENAI_API_KEY=...
OPENAI_MODEL=gpt-5.2
```

迁移后补充：

```bash
KODA_LLM_PROFILE=openai-compat
KODA_LLM_MODEL=default
```

保留已有 provider secret 变量名，不复制 secret 到 TOML。

## 11. 文档与模板同步

必须同步修改：

- `config/llms.example.toml`
- `README.md`
- `docs/configuration.md`
- `docs/installation.md`
- `docs/release-notes.md`
- CLI help text
- init 生成模板

必须避免：

- 文档里继续出现推荐写法 `model = "..."` 放在 profile 级。
- 初始化只生成 `llms.example.toml` 而不生成实际 `llms.toml`。
- CLI 示例仍使用 `--profile openai-compat` 但不说明 model 选择规则。

## 12. 迭代计划

### Phase 1：Schema 与 Runtime 主干

实现内容：

- 新 `[[profiles.models]]` schema 解析。
- 旧 `profile.model` 检测和迁移提示。
- 展开为 `profile:model` 扁平 LLM 列表。
- selector 支持 `default_profile/default_model`。
- `KODA_LLM_PROFILE/KODA_LLM_MODEL` 生效。
- `next_llm_by_name("profile:model")` 与 `next_llm_by_name("profile")` 生效。

验收：

```bash
cargo test -p koda-agent-core
```

### Phase 2：CLI 选择入口

实现内容：

- 全局参数 `--profile`、`--model`、`--llm`。
- `--llm profile:model` 拆分成 profile/model 环境选择。
- `--profile` 单独使用时选该 profile 默认 model。
- 错误信息不打印密钥。

验收：

```bash
cargo test -p koda-agent-cli cli_accepts_profile_and_llm_no_alias
```

### Phase 3：Config CLI 全量适配

实现内容：

- `init/setup/migrate/list/show/validate` 适配新 schema。
- `config use profile:model` 更新 env selector。
- `config set <profile> model ...` 明确拒绝，并提示使用 `config model`。
- `config model list/add/set/use/remove`。

验收：

```bash
cargo test -p koda-agent-cli config_
cargo test -p koda-agent-cli init_
```

### Phase 4：Slash/TUI 入口

实现内容：

- `/llm profile:model`。
- `/llm profile`。
- `/models`。
- `/model alias`。
- TUI Inspector/Timeline 中显示当前 `profile:model` 和真实 model id。

验收：

```bash
cargo test -p koda-agent-core runtime_slash_switches_llm_by_profile_model_and_model_alias
```

### Phase 5：模板、文档、发布检查

实现内容：

- 更新根模板和用户 home 初始化模板。
- 更新 README / configuration / installation / release notes。
- 检查旧 schema 示例残留。
- 检查 secret 泄漏。

验收：

```bash
rg -n 'model = "|default = "' README.md docs config crates || true
scripts/audit-secrets.sh
scripts/audit-history.sh
```

说明：`rg` 结果需要人工区分旧 schema 迁移测试、错误提示和正常 model 级字段。

### Phase 6：全量质量门禁与真实 smoke

实现内容：

- 全 workspace 格式、测试、clippy。
- release dry run。
- 本机安装版本 smoke。
- 真实 provider smoke：`--profile --model` 与 `--llm profile:model`。

验收：

```bash
cargo fmt --all --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
make release-dry-run
koda-agent config validate
koda-agent --profile openai-compat --model default --input "用一句话回复：配置验证成功"
koda-agent --llm openai-compat:default --input "用一句话回复：模型选择成功"
```

## 13. 风险与处理

### 风险 1：旧配置用户启动失败

处理：启动失败信息必须指出：旧 `profile.model` 已废弃，运行 `koda-agent config migrate --force` 或改成 `[[profiles.models]]`。

### 风险 2：`profile` 和 `profile:model` 混用导致歧义

处理：

- 文档明确：精确选择用 `profile:model`。
- `profile` 只表示“该 provider 的默认 model”。
- `/models` 与 `config model list` 让用户看到实际 alias。

### 风险 3：CLI/TUI/Runtime 显示不一致

处理：所有展示都来自 runtime 展平后的 `list_llms()`，不要在 TUI 单独拼装模型列表。

### 风险 4：`config model` 让 CLI 复杂度上升

处理：命令范围限制在模型 alias 的增删改查和默认选择，不做 provider 级 secret 写入，不做复杂交互式 UI。

### 风险 5：model 级覆盖字段过多

处理：只允许已有 `LlmModelConfig` 支持的字段，新增字段必须先有协议层需求和测试。

## 14. 自审

### 14.1 需求闭环

- 已覆盖“同一个 profile 多个模型”的核心问题。
- 已覆盖用户不想手写配置的需求：`config setup/add/use/model ...` 可以完成主要路径。
- 已覆盖初始化问题：实际配置文件必须是 `llms.toml`，example 只作参考。
- 已覆盖运行时选择：CLI、环境变量、selector、slash 语义一致。
- 已覆盖 secret 安全：TOML 只保存 env var 名，不保存 key。

结论：需求闭环成立。

### 14.2 工程复杂度

- 改动主要集中在配置解析、CLI config 命令、runtime selector。
- LLM 协议层不需要大改。
- Multi LLM 继续使用扁平列表，能避免把嵌套 schema 扩散到 tool/agent loop/TUI。
- `config model` 增加 CLI 面积，但比让用户手动编辑 TOML 更可靠。

结论：复杂度可控，且长期复杂度低于旧 schema 兼容双轨。

### 14.3 兼容性取舍

- 不长期兼容 profile 级 `model` 是合理取舍，否则 profile 语义会继续混乱。
- 需要保留 `config migrate` 作为过渡工具，否则现有用户初始化后会卡住。
- 测试中可以保留 legacy fixture，但正式文档和模板不能继续推荐旧写法。

结论：迁移工具可以有，正式 schema 不双轨。

### 14.4 验收充分性

自动化验收覆盖：

- core schema parsing。
- runtime LLM switch。
- CLI 参数解析。
- config init/setup/migrate/list/show/validate/model。
- workspace test/clippy/fmt。

仍需要真实验收：

- 使用本机 `~/.koda-agent` 配置跑 `config validate`。
- 使用真实 OpenAI-compatible provider 跑 `--profile --model`。
- 使用真实 OpenAI-compatible provider 跑 `--llm profile:model`。

结论：自动化门禁足够防回归；真实 provider smoke 是最终完成前硬门槛。

### 14.5 文档一致性

必须在实现完成后复查：

```bash
rg -n 'profile.*model|model = "|default = "' README.md docs config crates
```

允许存在：

- model 级 `[[profiles.models]] id/model` 示例。
- 迁移章节中的旧 schema 示例。
- 测试 fixture 和错误提示。

不允许存在：

- 推荐用户在 `[[profiles]]` 里写 `model = "..."`。
- 初始化说明只生成 `llms.example.toml`。
- CLI 文档不说明 `--model` 或 `--llm profile:model`。

结论：文档必须作为 Phase 5 的硬门槛，不然会再次造成初始化后不可用的问题。

## 15. 最终建议

按 Phase 1 到 Phase 6 顺序推进。每个阶段完成后先跑对应测试，最后统一跑全量门禁和真实 provider smoke。实现上不要把嵌套 profile/model 概念扩散到 LLM 协议层；协议层只接收最终解析好的扁平 `LlmModelConfig`。

如果后续要支持更复杂的路由，例如“同一任务自动选择 vision/reasoner/flash”，应该基于 `profile:model` key 做策略层，而不是再改变配置 schema。
