# LLM 配置与 CLI Setup 落地方案

## 目标

让普通用户完成安装后，只需要执行一个配置命令并粘贴一个 API Key，就能启动 Koda Agent：

```bash
koda-agent config setup mimo
koda-agent tui --full
```

高级用户仍可直接编辑 `~/.koda-agent/config/llms.toml`，配置多模型、故障转移、Claude Messages API、OpenAI Responses API、中转服务、代理、thinking/reasoning 等高级选项。

## 实施状态（已完成）

截至 `v0.1.3`，本方案的 Iteration 0-7 已实现并通过验收。当前文档作为设计记录和验收追溯保留；用户操作入口以 `README.md`、`docs/configuration.md`、`docs/installation.md` 和 `docs/release-notes.md` 为准。

已落地能力：

- `llms.toml` profile-first 主配置路径。
- `config setup/path/validate/list/show/use/secret/add/set/remove/migrate` 配置管理闭环。
- `auth_scheme/auth_header` provider 认证模型，覆盖 Bearer、`api-key` header、Claude `x-api-key`。
- Runtime `--profile`、`--llm-no`、`/llm <name>` profile 切换。
- Full TUI Inspector 展示 active profile 和 model。
- 旧 `OPENAI_*` 配置迁移提示与 `config migrate` 自助迁移。
- release/install 配套、真实 LLM smoke、TUI 非 TTY smoke、secret/history audit。

验收证据：

- `cargo fmt --all --check` passed。
- `cargo test --workspace --all-features` passed。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` passed。
- `make release-dry-run` passed。
- `scripts/audit-secrets.sh` / `scripts/audit-history.sh` exited 0，命中项仅为 placeholder/test fixture 或 ignored runtime 文件。

## 背景与原则

原版 GenericAgent 使用 `mykey.py` / `mykey.json` 作为主配置，变量名决定 Session 类型，例如 `native_oai_config`、`native_claude_config`、`oai_config`、`claude_config`、`mixin_config`。Rust 版应保留这个语义，但用更安全、更结构化的 `llms.toml + .env + CLI setup` 替代手写 Python 配置。

核心原则：

- `llms.toml` 是 LLM profile 的事实来源。
- `.env` 只保存密钥和当前选择，不保存 base URL / model 等结构化配置。
- 普通用户不需要打开配置文件，只需通过 CLI 粘贴 key。
- 高级用户可以编辑 TOML，组合多个 profile 和 mixin failover。
- 不长期兼容旧的 `OPENAI_BASE_URL` / `OPENAI_MODEL` runtime fallback；仅提供迁移提示或迁移命令。
- 不把真实 API Key 写入 `llms.toml`，默认写入 `~/.koda-agent/.env`，权限设置为 `0600`。

## 文件布局

默认运行时布局：

```text
~/.koda-agent/
  .env                         # secret + active profile
  config/
    llms.toml                  # 主配置，CLI setup 自动创建
    llms.example.toml          # 完整模板，随资源安装
  temp/
  memory/
  logs/
  sessions/
  browser/
```

`.env` 示例：

```bash
KODA_LLM_PROFILE=mimo
MIMO_API_KEY=粘贴的真实key
```

`llms.toml` 示例：

```toml
[selector]
default = "mimo"

[defaults]
stream = true
timeout_secs = 600
connect_timeout_secs = 30
verify_tls = true
temperature = 1.0
max_tokens = 8192
failover = true

[[profiles]]
name = "mimo"
kind = "native_oai"
base_url = "https://api.xiaomimimo.com/v1"
api_key_env = "MIMO_API_KEY"
api_key_header = "api-key"
model = "mimo-v2.5-pro"
api_mode = "chat_completions"
stream = true
```

## 配置语义

`kind` 对齐原版 `mykey.py` 变量名规则：

| Rust `kind` | 原版配置名语义 | 协议与工具调用 |
| --- | --- | --- |
| `native_oai` | `native_oai_config` | OpenAI-compatible + native function/tool calling |
| `native_claude` | `native_claude_config` | Anthropic Messages + native tools |
| `oai` | `oai_config` | OpenAI-compatible + text tool protocol |
| `claude` | `claude_config` | Claude Messages + text tool protocol |

默认推荐 `native_oai` / `native_claude`。`oai` / `claude` 主要用于对齐原版和兼容弱 provider。

字段说明：

```toml
[[profiles]]
name = "deepseek"                  # profile 名，/llm deepseek 和 mixin 引用使用
kind = "native_oai"                # session 类型
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"   # 从 .env 或系统环境变量读取
api_key_header = "Authorization"   # 可选；默认 Authorization Bearer，MiMo 用 api-key
model = "deepseek-chat"
api_mode = "chat_completions"      # chat_completions 或 responses
stream = true
timeout_secs = 600
connect_timeout_secs = 30
verify_tls = true
reasoning_effort = "medium"
thinking_type = "adaptive"
proxy = ""
```

`api_key` 字段可以作为高级逃生口保留，但 CLI 不主动生成，`doctor` 需要提示：建议改用 `api_key_env`。

## 内置 Preset

第一阶段内置这些 preset：

| preset | kind | base_url | model | key env | 特殊点 |
| --- | --- | --- | --- | --- | --- |
| `mimo` | `native_oai` | `https://api.xiaomimimo.com/v1` | `mimo-v2.5-pro` | `MIMO_API_KEY` | `api_key_header = "api-key"` |
| `deepseek` | `native_oai` | `https://api.deepseek.com/v1` | `deepseek-chat` | `DEEPSEEK_API_KEY` | chat completions |
| `openai` | `native_oai` | `https://api.openai.com/v1` | `gpt-4.1-mini` | `OPENAI_API_KEY` | 默认可选 responses |
| `claude` | `native_claude` | `https://api.anthropic.com/v1` | `claude-3-5-sonnet-latest` | `ANTHROPIC_API_KEY` | thinking adaptive |
| `openrouter` | `native_oai` | `https://openrouter.ai/api/v1` | `anthropic/claude-3.5-sonnet` | `OPENROUTER_API_KEY` | provider/model 格式 |
| `custom-oai` | `native_oai` | 用户输入 | 用户输入 | 用户输入 | OpenAI-compatible 自定义 |
| `custom-claude` | `native_claude` | 用户输入 | 用户输入 | 用户输入 | Claude-compatible 自定义 |

## 用户体验

### 最短路径

```bash
koda-agent config setup mimo
```

交互：

```text
Koda Agent LLM Setup

Provider: MiMo
Profile name [mimo]:
Model [mimo-v2.5-pro]:
Paste API key: ********
Set as default? [Y/n]:
Run validation? [Y/n]:
```

用户一路回车，只需要粘贴 key。

CLI 写入：

```text
~/.koda-agent/config/llms.toml
~/.koda-agent/.env
```

然后自动运行等价于：

```bash
koda-agent config validate
```

成功后提示：

```text
Configured profile: mimo
Key: MIMO_API_KEY found
Next: koda-agent tui --full
```

### 非交互模式

```bash
koda-agent config setup mimo --api-key "$MIMO_API_KEY" --yes
```

或使用已导出的环境变量，不把 key 通过命令行参数传入：

```bash
export MIMO_API_KEY=xxx
koda-agent config setup mimo --from-env --yes
```

### 切换模型

```bash
koda-agent config use deepseek
KODA_LLM_PROFILE=claude koda-agent tui --full
koda-agent --profile openai --input "hello"
```

TUI 内：

```text
/llms
/llm mimo
/llm 1
```

## CLI 命令设计

第一阶段必须实现：

```bash
koda-agent config setup [preset]
koda-agent config list
koda-agent config show [profile]
koda-agent config use <profile>
koda-agent config validate
koda-agent config path
koda-agent config secret <ENV_NAME>
```

第二阶段增强：

```bash
koda-agent config add <profile> --kind ... --base-url ... --model ...
koda-agent config set <profile> <key> <value>
koda-agent config remove <profile>
koda-agent config remove-secret <ENV_NAME>
koda-agent config migrate
```

### `config path`

输出：

```text
home: /Users/vanzheng/.koda-agent
env: /Users/vanzheng/.koda-agent/.env
llms: /Users/vanzheng/.koda-agent/config/llms.toml
example: /Users/vanzheng/.koda-agent/config/llms.example.toml
```

### `config list`

输出：

```text
→ mimo       native_oai      mimo-v2.5-pro       key:MIMO_API_KEY found
  deepseek   native_oai      deepseek-chat       key:DEEPSEEK_API_KEY missing
  claude     native_claude   claude-sonnet       key:ANTHROPIC_API_KEY found
```

### `config show`

输出隐藏密钥：

```text
profile: mimo
kind: native_oai
base_url: https://api.xiaomimimo.com/v1
model: mimo-v2.5-pro
api_mode: chat_completions
api_key_env: MIMO_API_KEY
api_key: found
api_key_header: api-key
stream: true
```

### `config secret`

```bash
koda-agent config secret MIMO_API_KEY
```

交互式隐藏输入，写入 `.env`：

```text
Paste value for MIMO_API_KEY: ********
Saved to /Users/vanzheng/.koda-agent/.env
```

### `config validate`

检查：

- `llms.toml` 是否存在。
- active profile 是否存在。
- `kind` 是否合法。
- `base_url` 是否非空。
- `model` 是否非空。
- `api_key_env` 对应密钥是否存在。
- `api_key_header` 是否合理。
- `mixin.llm_nos` 引用是否存在。
- mixin 是否混用 native 与 non-native。
- `api_mode` / `thinking_type` / `reasoning_effort` 是否合法。

## 加载优先级

最终运行时配置选择顺序：

```text
1. CLI --profile <name>
2. KODA_LLM_PROFILE
3. llms.toml [selector].default
4. llms.toml 第一个 [[profiles]]
```

如果找不到 `llms.toml`，不再自动使用 `OPENAI_BASE_URL` / `OPENAI_MODEL` 作为 runtime fallback。改为提示：

```text
LLM config missing.

Quick setup:
  koda-agent config setup mimo

If you have legacy OPENAI_* variables:
  koda-agent config migrate
```

## 迁移策略

不做长期兼容，只做迁移辅助。

`koda-agent config migrate` 行为：

- 检测 `.env` 或系统环境变量里的旧字段：
  - `OPENAI_BASE_URL`
  - `OPENAI_MODEL`
  - `OPENAI_API_KEY`
  - `OPENAI_API_STYLE`
- 生成 profile：

```toml
[selector]
default = "openai-compat"

[[profiles]]
name = "openai-compat"
kind = "native_oai"
base_url = "旧 OPENAI_BASE_URL"
api_key_env = "OPENAI_API_KEY"
model = "旧 OPENAI_MODEL"
api_mode = "chat_completions"
stream = true
```

- 不把 key 写入 TOML。
- 如果 `llms.toml` 已存在，默认不覆盖，除非 `--force`。

## `doctor` 输出调整

有配置时：

```text
LLM
  config: /Users/vanzheng/.koda-agent/config/llms.toml
  active profile: mimo
  kind: native_oai
  model: mimo-v2.5-pro
  base_url: https://api.xiaomimimo.com/v1
  api_mode: chat_completions
  stream: true
  api_key_env: MIMO_API_KEY found
```

缺 key 时：

```text
LLM
  active profile: mimo
  api_key_env: MIMO_API_KEY missing
  fix: koda-agent config secret MIMO_API_KEY
```

缺配置时：

```text
LLM config missing.
Quick setup: koda-agent config setup mimo
```

## 安全要求

- CLI 输入 key 时默认不回显。
- `.env` 权限设置为 `0600`。
- `config show` / `doctor` / logs 不打印 key。
- `llms.toml` 默认只写 `api_key_env`，不写 `api_key`。
- `audit-secrets.sh` 继续禁止提交：
  - `.env`
  - `config/llms.toml`
  - runtime logs
  - browser runtime config
  - memory runtime files
- `--api-key` 非交互模式可支持，但文档优先推荐交互粘贴或 `--from-env`。

## 实现阶段

### Phase 1：配置模型重构

- 新增 TOML schema：`selector`、`defaults`、`profiles`、`kind`、`api_key_env`、`api_key_header`、`api_mode`。
- `llms.toml` 成为主配置。
- `.env` 只加载 secret 和 `KODA_LLM_PROFILE`。
- 删除 `OPENAI_BASE_URL` / `OPENAI_MODEL` runtime fallback，改为迁移提示。
- 保留内部 `AgentConfig` 字段兼容现有 LLM 层，加载时从 active profile 映射过去。

验收：

```bash
cargo test -p koda-agent-core config_
cargo test -p koda-agent-llm
```

### Phase 2：CLI 配置命令

- 实现：
  - `config path`
  - `config setup <preset>`
  - `config list`
  - `config show`
  - `config use`
  - `config secret`
  - `config validate`
- 内置 presets：mimo、deepseek、openai、claude、openrouter、custom-oai、custom-claude。
- `init` 生成 `.env` 模板与 `llms.toml` 初始文件，保留 `llms.example.toml`。

验收：

```bash
cargo test -p koda-agent-cli config_
koda-agent --home /tmp/koda-home config setup mimo --api-key test --yes
koda-agent --home /tmp/koda-home config validate
```

### Phase 3：Runtime / TUI 切换

- 新增全局参数：`--profile <name>`。
- `/llm <name>` 支持 profile 名称。
- `/llms` 显示 profile、kind、model、key 状态。
- 保留数字切换，继续支持 `--llm-no` / 当前 `--llm_no`。

验收：

```bash
cargo test -p koda-agent-cli tui_full::tests::full_tui_
cargo test -p koda-agent-core multi_llm
```

### Phase 4：迁移与文档

- 实现 `config migrate`。
- 更新：
  - `README.md`
  - `docs/configuration.md`
  - `docs/installation.md`
  - `config/llms.example.toml`
- 明确旧 `OPENAI_*` 不再是主路径。

验收：

```bash
cargo fmt --all --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
make release-dry-run
```

## 风险与处理

### 风险：破坏现有 `.env` 用户

处理：提供清晰错误和 `config migrate`，不静默 fallback。

### 风险：CLI setup 写坏用户配置

处理：

- 写文件前备份：`llms.toml.bak.<timestamp>`。
- 默认 merge profile，不覆盖其他 profile。
- `--force` 才覆盖同名 profile。

### 风险：preset 过时

处理：preset 只提供默认值，用户可通过 `config set` 或编辑 TOML 修改。

### 风险：key 出现在 shell history

处理：文档主推交互粘贴；`--api-key` 仅用于 CI/自动化。

## 最终用户路径

普通用户：

```bash
koda-agent config setup mimo
koda-agent doctor
koda-agent tui --full
```

高级用户：

```bash
cp ~/.koda-agent/config/llms.example.toml ~/.koda-agent/config/llms.toml
$EDITOR ~/.koda-agent/config/llms.toml
koda-agent config validate
```

迁移用户：

```bash
koda-agent config migrate
koda-agent config validate
koda-agent tui --full
```

## 自审记录

### 结论

方案方向成立：`llms.toml` 主配置、`.env` 只放 secret、普通用户通过 `config setup <preset>` 粘贴 key，是当前阶段最适合的产品化路径。它比原版 `mykey.py` 更安全，也比继续兼容 `OPENAI_BASE_URL` / `OPENAI_MODEL` 更清晰。

但实施前需要补齐以下细节，避免落地时产生歧义。

### 需要澄清或修正的点

1. `api_key_header` 语义需要更精确。

当前文档示例写了：

```toml
api_key_header = "Authorization"
```

这不够严谨，因为标准 Bearer 鉴权不是简单写 `Authorization: <key>`，而是：

```text
Authorization: Bearer <key>
```

落地时应拆成两个字段或定义明确枚举：

```toml
auth_scheme = "bearer"       # 默认：Authorization: Bearer <key>
api_key_header = "api-key"   # 自定义 header：api-key: <key>
```

建议规则：

- 默认 `auth_scheme = "bearer"`。
- MiMo preset 使用 `auth_scheme = "header"` + `api_key_header = "api-key"`。
- Anthropic 官方可以用 `auth_scheme = "x-api-key"`，或由 `kind = "native_claude"` + key 前缀自动判断。
- 不推荐让用户手写 `Authorization` header，避免漏掉 `Bearer`。

2. `kind` 与 `api_mode` 的边界要固定。

建议最终定义：

- `kind` 决定 Session/工具协议类型：`native_oai`、`native_claude`、`oai`、`claude`。
- `api_mode` 只对 `native_oai` / `oai` 有效：`chat_completions` 或 `responses`。
- `native_claude` / `claude` 固定走 `/v1/messages`，忽略 `api_mode`，`validate` 如发现则 warning。

3. `init` 是否直接创建 `llms.toml` 需要明确。

文档目前写“`init` 生成 `.env` 模板与 `llms.toml` 初始文件”。为了实现“粘贴 key 即可”，建议这样定：

- `koda-agent init` 创建 `llms.example.toml` 和一个可用但缺 key 的 `llms.toml`，默认 profile 为 `mimo`。
- `.env` 创建：

```bash
KODA_LLM_PROFILE=mimo
MIMO_API_KEY=
```

- `doctor` 看到空 key 时提示：`koda-agent config secret MIMO_API_KEY`。
- `config setup mimo` 会补齐 key，并可覆盖/更新同名 profile。

这样用户不需要先复制 example。

4. 需要定义 workspace 配置和 home 配置的优先级。

当前项目支持 workspace/source/home/resource 多路径搜索。新方案建议：

```text
CLI --config /path/to/llms.toml    # 后续可选
当前 workspace/config/llms.toml   # 项目级覆盖，适合团队项目，但禁止提交真实 key
~/.koda-agent/config/llms.toml     # 用户默认
~/.koda-agent/resources/config/llms.example.toml # 只作为模板，不作为运行配置
```

`.env` 搜索建议：

```text
当前目录/.env
workspace/.env
~/.koda-agent/.env
系统环境变量
```

但要在 `doctor` 中显示“实际使用了哪个 llms.toml”，否则排查会困难。

5. `OPENAI_*` 迁移提示需要可控。

既然决定不做长期兼容，启动时报错要足够友好：

- 若缺 `llms.toml` 且发现旧 `OPENAI_*`：提示 `koda-agent config migrate`。
- 若缺 `llms.toml` 且没有旧变量：提示 `koda-agent config setup mimo`。
- `doctor` 不应因为缺 LLM config 直接失败，应报告 `llm.status = missing_config`。

6. 需要补充 Windows 安全权限策略。

Unix 可用 `0600`。Windows 没有同样语义，落地可以先：

- 默认写入 `%USERPROFILE%\.koda-agent\.env`。
- 不打印 key。
- 文档说明 Windows ACL hardening 是后续增强。
- 如果实现成本可控，再用 Windows ACL 限制当前用户读写。

7. 需要补充 atomic write 与备份策略。

所有会改写配置的命令都应：

- 写临时文件。
- fsync 或尽量保证 rename 原子替换。
- 覆盖同名 profile 前生成 `llms.toml.bak.<timestamp>`。
- `.env` 更新时保留未知行和注释，只替换目标 key。

8. 需要补充 OpenRouter / 中转常见 headers。

OpenRouter 常见需要可选 headers：

```toml
[profiles.headers]
HTTP-Referer = "https://koda-agent.local"
X-Title = "Koda Agent"
```

现有 Rust 版已有 `custom_headers`，新 schema 应继续支持。

9. 需要补充 Claude Code relay 相关字段。

原版 `NativeClaudeSession` 支持：

- `fake_cc_system_prompt`
- `user_agent`
- `[1m]` 模型后缀触发 beta

新 schema 应保留：

```toml
fake_cc_system_prompt = true
user_agent = "claude-cli/2.1.113 (external, cli)"
```

这对一些 Claude Code relay / switch 很重要。

10. `--api-key` 风险需要更强提示。

文档已提醒 shell history 风险。实现上建议：

- `config setup <preset> --api-key ...` 支持但 help 文案标注“不推荐”。
- 更推荐 `--from-env` 或交互隐藏输入。
- 测试日志不要打印参数值。

### 修订后的关键决策

最终落地时按以下决策执行：

```text
主配置：~/.koda-agent/config/llms.toml
模板：~/.koda-agent/config/llms.example.toml
密钥：~/.koda-agent/.env
普通配置入口：koda-agent config setup <preset>
默认 preset：mimo
默认 active profile：KODA_LLM_PROFILE 或 [selector].default
旧 OPENAI_*：不 runtime fallback，只迁移提示
```

`auth` 字段建议采用：

```toml
auth_scheme = "bearer"      # bearer | header | x-api-key
auth_header = "api-key"     # auth_scheme=header 时使用
api_key_env = "MIMO_API_KEY"
```

为了兼容已写进文档的字段，实现时也可以接受 `api_key_header` 作为 `auth_header` alias。

### 建议调整实施顺序

原 Phase 顺序基本可行，但建议先做最小闭环：

1. `config setup mimo --api-key test --yes` 能生成 `.env + llms.toml`。
2. `AgentConfig` 能只依赖 `llms.toml + MIMO_API_KEY` 启动，不需要 `OPENAI_BASE_URL`。
3. `doctor` 能展示 active profile 和 key found/missing。
4. 再扩展 `config list/show/use/secret/validate`。
5. 最后做 `migrate` 和 TUI `/llm name`。

这样每一步都能测试验收，不会一次性改太大。

### 必须补的测试

- 没有 `OPENAI_BASE_URL`，只有 `llms.toml + MIMO_API_KEY`，`AgentConfig` 成功。
- `KODA_LLM_PROFILE=deepseek` 能选择 deepseek profile。
- active profile 缺 key 时错误信息包含 `config secret <KEY>`。
- MiMo preset 使用 `api-key` header。
- Bearer preset 使用 `Authorization: Bearer`。
- `config setup` 不把 key 写入 `llms.toml`。
- `.env` 更新保留未知变量和注释。
- `doctor --json` 不包含真实 key。
- 缺 `llms.toml` 且存在旧 `OPENAI_*` 时提示 migrate。
- `mixin.llm_nos` 引用不存在时报 validation error。

### 自审结论

文档可以作为实施蓝图，但落地前应先按本自审记录修订 auth 字段、init 行为、配置搜索优先级、Windows 权限、atomic write、custom headers 和 Claude relay 字段。完成这些修订后再开始编码，风险会明显降低。

## 二次自审：迭代化落地规划

### 二次自审结论

第一次方案已经明确了方向，但仍偏“大方案”。为了避免一次性重构过大、影响现有 TUI/LLM/工具链，应按可验收的小迭代推进。每个迭代都必须满足：

- 代码可编译。
- 单元测试覆盖新增行为。
- 不泄露 `.env` 密钥。
- `cargo fmt --all --check` 通过。
- 该迭代相关测试通过。
- 完成后先暂存，等统一提交。

### 迭代边界原则

- 每个迭代只改一层主要职责，避免配置 schema、CLI、TUI、doctor 同时大改。
- 先实现“最小可启动闭环”，再扩展用户体验。
- Runtime 破坏性切换必须有迁移提示，不能让用户看到底层 panic 或 `OPENAI_BASE_URL missing`。
- 文档、example、doctor 输出必须跟代码同步，否则验收不算完成。
- IM/GUI 不进入本阶段，仍按既定规则最后做。

## 迭代计划

### Iteration 0：冻结现状与准备测试夹具

目标：在改配置核心前，先建立基线和测试辅助，确保后续能判断是否破坏现有行为。

范围：

- 保留当前已暂存的 init/resource/example 改动。
- 新增测试辅助函数，用临时 home/workspace 构造 `.env`、`config/llms.toml`。
- 梳理当前 `AgentConfig::from_env_with_path_options` 中旧配置分支，标注待替换点。
- 不改变 runtime 行为。

验收：

```bash
cargo fmt --all --check
cargo test -p koda-agent-core agent_config_
cargo test -p koda-agent-cli init_
```

完成标准：

- 没有行为变化。
- 后续迭代可以复用测试夹具。

---

### Iteration 1：新增 `profiles` schema，只解析不切主路径

目标：让 Rust 能解析新的 `llms.toml` schema，但暂不改变现有配置优先级。

范围：

- 在 core 增加：
  - `LlmSelectorToml`
  - `LlmDefaultsToml`
  - `LlmProfileToml`
  - `auth_scheme`
  - `auth_header`
  - `api_key_env`
  - `kind`
  - `api_mode`
  - `headers/custom_headers`
  - `fake_cc_system_prompt`
  - `user_agent`
- 支持 `api_key_header` 作为 `auth_header` alias，仅为了读旧 example/文档，不作为主推字段。
- 新增 profile normalize/validate helper，但只在测试中使用。
- 更新 `config/llms.example.toml` 为新 schema。

不做：

- 不删除 `OPENAI_*` fallback。
- 不修改 TUI `/llm`。
- 不修改实际 HTTP header 发送逻辑，除非已有字段能无损映射。

验收：

```bash
cargo test -p koda-agent-core llm_profile_
cargo test -p koda-agent-core config_loads_
cargo fmt --all --check
```

必须测试：

- 解析 MiMo profile。
- 解析 DeepSeek profile。
- 解析 Claude profile。
- `auth_scheme = "header"` + `auth_header = "api-key"` 正常。
- `api_key_header = "api-key"` alias 正常。
- invalid `kind` 报 validation error。

完成标准：

- 新 schema 可解析、可验证。
- 当前用户现有配置仍可运行。

---

### Iteration 2：实现 `config setup/path/validate` 最小闭环

目标：普通用户可以通过 CLI 生成 `.env + llms.toml`，但 runtime 仍可暂时旧逻辑运行。

范围：

- 新增 `koda-agent config` 子命令组。
- 实现：
  - `config path`
  - `config setup <preset>`
  - `config validate`
- 内置 presets：
  - `mimo`
  - `deepseek`
  - `openai`
  - `claude`
  - `openrouter`
- 支持非交互参数：
  - `--api-key <value>`，help 标注不推荐。
  - `--from-env`。
  - `--yes`。
  - `--set-active` 默认 true。
- 交互隐藏输入可以放到 Iteration 3；本迭代先支持非交互和空 key 模板。
- 写入 `.env` 时保留未知行和注释，只 upsert 目标 key。
- 写入 `llms.toml` 前生成 `.bak.<timestamp>`。
- 写入文件使用临时文件 + rename。

不做：

- 不要求 runtime 立即使用新 schema。
- 不做 `config list/show/use/secret`。

验收：

```bash
cargo test -p koda-agent-cli config_setup_
cargo test -p koda-agent-cli config_validate_
TMP_HOME=$(mktemp -d)
koda-agent --home "$TMP_HOME" config setup mimo --api-key test --yes
koda-agent --home "$TMP_HOME" config validate
rg -n "test" "$TMP_HOME/config/llms.toml" && exit 1 || true
rg -n "MIMO_API_KEY=test" "$TMP_HOME/.env"
```

必须测试：

- `setup mimo --api-key test --yes` 创建 `llms.toml` 和 `.env`。
- key 只进入 `.env`，不进入 `llms.toml`。
- `validate` 能检测 key found。
- 第二次 setup 同名 profile 默认不覆盖，除非 `--force`。
- `.env` 注释和未知变量保留。

完成标准：

- 用户可通过 CLI 生成新配置。
- 不泄露 key。

---

### Iteration 3：Runtime 切到 `llms.toml profiles` 主路径

目标：`AgentConfig` 不再依赖 `OPENAI_BASE_URL` / `OPENAI_MODEL`，只要有 `llms.toml + key env` 就能启动。

范围：

- `AgentConfig::from_env_with_path_options` 优先加载 `profiles`。
- 选择顺序：

```text
1. CLI profile option 注入的 KODA_LLM_PROFILE 或 path option 扩展字段
2. KODA_LLM_PROFILE
3. [selector].default
4. 第一个 [[profiles]]
```

- 将 active profile 映射到现有 `AgentConfig` / `LlmModelConfig`：
  - `kind/native_oai/oai` + `api_mode=chat_completions` → `api_style = "chat"`
  - `kind/native_oai/oai` + `api_mode=responses` → `api_style = "responses"`
  - `kind/native_claude/claude` → `api_style = "claude"`
- 将 `auth_scheme/auth_header` 映射到现有 custom header 能力；如现有 LLM 层不足，需要补最小支持。
- 缺 `llms.toml` 时不再 fallback 到旧 `OPENAI_*`，而是返回可读错误：
  - 有旧变量：提示 `koda-agent config migrate`。
  - 无旧变量：提示 `koda-agent config setup mimo`。
- `doctor` 不因缺配置失败，而是报告 status。

不做：

- 暂不做 `config migrate`。
- 暂不做 `/llm name`。

验收：

```bash
cargo test -p koda-agent-core profile_config_
cargo test -p koda-agent-llm
cargo test -p koda-agent-cli doctor_
```

必须测试：

- 无 `OPENAI_BASE_URL`，只有 `llms.toml + MIMO_API_KEY`，`AgentConfig` 成功。
- `KODA_LLM_PROFILE=deepseek` 选择 deepseek。
- active profile 缺 key，错误包含 `koda-agent config secret KEY` 或 `config setup` 指引。
- 缺 `llms.toml` 且存在旧 `OPENAI_*`，错误包含 `config migrate`。
- `doctor --json` 不包含真实 key。

完成标准：

- 新配置成为主路径。
- 旧 `.env OPENAI_*` 不再静默生效。

---

### Iteration 4：补齐用户配置 CLI

目标：用户日常不打开文件也能管理配置。

范围：

- 实现：
  - `config list`
  - `config show [profile]`
  - `config use <profile>`
  - `config secret <ENV_NAME>`
  - `config add <profile>`
  - `config set <profile> <key> <value>`
  - `config remove <profile>`
- `config secret` 使用隐藏输入；非 TTY 下要求 `--value` 或从 stdin 读取。
- `config use` 默认写 `.env` 的 `KODA_LLM_PROFILE`，不改 `llms.toml`。
- `config show` 和 `list` 只显示 key found/missing，不显示真实 key。

验收：

```bash
cargo test -p koda-agent-cli config_list_
cargo test -p koda-agent-cli config_secret_
cargo test -p koda-agent-cli config_use_
```

必须测试：

- `config list` active marker 正确。
- `config use deepseek` 更新 `.env` 且保留其他变量。
- `config secret MIMO_API_KEY --value test` 写入 `.env`，权限保持安全。
- `config show` 不泄露 key。
- `config remove` 不自动删除 secret。

完成标准：

- 普通用户完整配置管理闭环可用。

---

### Iteration 5：TUI / Runtime profile 切换增强

目标：运行时切换从数字扩展到 profile 名称。

范围：

- 新增全局参数：`--profile <name>`。
- 保留 `--llm_no`，新增更 Rust 风格 alias `--llm-no`。
- `/llm <name>` 支持 profile 名称。
- `/llms` 显示：index、active、profile name、kind、model、key 状态。
- Full TUI Inspector 显示 active profile/kind/model。

验收：

```bash
cargo test -p koda-agent-core multi_llm_
cargo test -p koda-agent-cli tui_full::tests::full_tui_
cargo test -p koda-agent-cli slash_
```

必须测试：

- `/llm mimo` 能切换。
- `/llm 1` 仍可用。
- `--profile deepseek` 优先于 `.env KODA_LLM_PROFILE`。
- TUI 不显示 key。

完成标准：

- CLI/TUI 模型切换体验完成。

---

### Iteration 6：迁移命令与旧配置移除收口

目标：给旧 `.env OPENAI_*` 用户提供一次性迁移路径，并彻底移除旧 fallback 的歧义。

范围：

- 实现 `config migrate`。
- 如果检测到旧 `.env`：生成 `openai-compat` profile。
- 不把 `OPENAI_API_KEY` 的值写入 TOML，只写 `api_key_env = "OPENAI_API_KEY"`。
- `migrate --force` 才覆盖已有 `llms.toml` 或同名 profile。
- 更新 `docs/configuration.md`、`README.md`、`docs/installation.md`。

验收：

```bash
cargo test -p koda-agent-cli config_migrate_
cargo fmt --all --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
make release-dry-run
```

必须测试：

- 旧 `.env` 迁移为 `openai-compat`。
- TOML 不包含真实 key。
- 已存在 `llms.toml` 时默认拒绝覆盖。
- 缺旧变量时 migrate 给出可读错误。

完成标准：

- 破坏性重构可被用户自助迁移。

---

### Iteration 7：真实端到端验证与 release 准备

目标：用本机真实配置验证新路径可用，并准备发布。

范围：

- 用 MiMo profile 真实跑：

```bash
koda-agent config validate
koda-agent --profile mimo --input "用一句话回复：配置验证成功"
```

- 用 TUI 非 TTY smoke 验证不再报 `OPENAI_BASE_URL missing`。
- 更新 release notes。
- 检查 secret/history：

```bash
scripts/audit-secrets.sh
scripts/audit-history.sh
```

完整验收：

```bash
cargo fmt --all --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
make release-dry-run
```

完成标准：

- 新用户路径可用。
- 旧配置迁移路径可用。
- 可以进入 commit / tag / release 流程。

## 每轮迭代交付格式

每个迭代完成后汇报：

```text
完成内容：
- ...

关键文件：
- ...

验收：
- cargo ... passed
- ...

风险/遗留：
- ...

已暂存：是/否
```

## 二次自审风险结论

- 最大风险不是 schema 本身，而是“一次性切换 runtime 主路径”导致 TUI/CLI/doctor 同时坏。因此 Iteration 1-2 必须先做到“可生成新配置但不改变运行时”。
- 第二大风险是 key 泄露。所有测试都必须检查 TOML、doctor、config show 不包含真实 key。
- 第三大风险是用户不知道为什么旧 `.env` 不能用了。必须在错误消息中明确给出 `config setup` 和 `config migrate`。
- 第四大风险是 header/auth 设计不清。落地前必须采用 `auth_scheme/auth_header`，不要用含糊的 `api_key_header = "Authorization"`。

按以上迭代推进，可以保证每一步都有可运行状态，不会把配置系统、TUI 和 release 包同时打散。
