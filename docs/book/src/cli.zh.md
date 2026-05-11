# CLI 命令手册

本文档按 `koda-agent --help` 的命令面组织，说明每个命令的用途、常用参数和典型场景。

## 全局参数

```bash
koda-agent [OPTIONS] [COMMAND]
```

常用全局参数：

| 参数 | 说明 |
| --- | --- |
| `--home <HOME>` | 覆盖用户级 Koda home，默认 `~/.koda-agent`。 |
| `--workspace <WORKSPACE>` | 覆盖工作区，默认当前目录。文件工具只应在工作区内操作。 |
| `--resource-dir <RESOURCE_DIR>` | 覆盖静态资源目录。通常不需要手动指定。 |
| `--input <INPUT>` | 一次性执行一个 prompt。 |
| `--task <TASK>` | 使用 upstream 风格的文件 I/O 任务模式。 |
| `--reflect <RULE>` | 启动 reflect 轮询模式，支持 Python 脚本和原生 JSON rule。 |
| `--profile <PROFILE>` | 本次进程临时指定 LLM profile。 |
| `--model <MODEL>` | 本次进程临时指定当前 profile 下的 model alias。 |
| `--llm <PROFILE:MODEL>` | 本次进程临时指定完整 LLM selector。 |
| `--verbose` | 输出更详细的运行信息。 |
| `--version` | 输出当前 CLI 版本。 |

## 顶层命令

| 命令 | 用途 |
| --- | --- |
| `init` | 初始化 Koda home 配置、静态资源和运行期目录。 |
| `doctor` | 检查路径、LLM 配置、resources 和 Python helper 状态。 |
| `bootstrap-python` | 创建或修复可选的托管 Python helper 环境。 |
| `python-env` | 管理可选 Python helper 环境。 |
| `resources` | 安装、修复或检查静态资源。 |
| `config` | 管理 LLM profiles、model aliases、secrets 和配置校验。 |
| `update` | 从 GitHub Releases 更新已安装二进制。 |
| `tui` | 启动交互式终端 UI。 |
| `serve-acp` | 启动 ACP JSON-RPC-over-JSONL bridge。 |
| `frontend` | 启动指定 frontend adapter，例如 `tmwebdriver`。 |
| `memory` | 审计、沉淀、召回和归档长期记忆。 |

## init

```bash
koda-agent init
koda-agent init --json
koda-agent init --force
koda-agent init --dry-run
```

`init` 负责创建用户目录和默认配置，并把源码或安装包里的静态资源安装到 `~/.koda-agent/resources`。

建议新用户第一步运行：

```bash
koda-agent init
koda-agent doctor
```

## doctor

```bash
koda-agent doctor
koda-agent doctor --json
```

用于排查：

- 当前 workspace 是否符合预期。
- Koda home 是否存在。
- `llms.toml` 和 `.env` 是否能解析。
- resources 是否完整。
- 可选 Python helper 是否可用。

## config

`config` 是普通用户配置 LLM 的主入口，优先使用 `~/.koda-agent/config/llms.toml`。

```bash
koda-agent config path
koda-agent config list
koda-agent config show
koda-agent config setup mimo --yes
koda-agent config secret MIMO_API_KEY --from-stdin
koda-agent config use mimo
koda-agent config validate
```

### config path

打印配置文件路径。适合排查 CLI 到底读取了哪份配置。

```bash
koda-agent config path
koda-agent config path --json
```

### config list / show

```bash
koda-agent config list
koda-agent config show
koda-agent config show mimo
```

`list` 用于快速看 profile 和 model alias；`show` 用于看当前配置摘要或指定 profile。

### config setup

```bash
koda-agent config setup mimo --yes
koda-agent config setup openai --base-url https://api.openai.com/v1 --model gpt-5.2 --yes
```

`setup` 用 preset 创建或更新 provider profile。密钥不会写进 `llms.toml`，只记录 `api_key_env`。

### config secret

```bash
printf '%s' "$MIMO_API_KEY" | koda-agent config secret MIMO_API_KEY --from-stdin
```

密钥写入 `~/.koda-agent/.env`。不要把真实密钥提交到 Git。

### config use

```bash
koda-agent config use mimo
koda-agent config use mimo:flash
```

选择默认 profile 或默认 profile:model。临时覆盖请用全局参数 `--profile`、`--model` 或 `--llm`。

### config add / set / remove

```bash
koda-agent config add custom --base-url https://example.com/v1 --api-key-env CUSTOM_API_KEY --model my-model
koda-agent config set custom timeout_secs 600
koda-agent config remove custom --yes
```

用于高级用户手动维护 custom provider。

### config migrate

```bash
koda-agent config migrate
```

把旧的 `OPENAI_BASE_URL` / `OPENAI_API_KEY` / `OPENAI_MODEL` 风格迁移成 `llms.toml` profile。迁移后建议用 `config validate` 检查。

## config model

同一个 profile 可以配置多个 model alias，用于 `pro`、`flash`、`reasoner`、`vision` 等模型切换。

```bash
koda-agent config model list mimo
koda-agent config model add mimo flash --id mimo-v2.5
koda-agent config model set deepseek flash max_tokens 32768
koda-agent config model use mimo flash
koda-agent config model remove mimo flash --yes
```

运行时选择优先级：

1. CLI 参数：`--llm` / `--profile` / `--model`
2. 环境 selector：`KODA_LLM_PROFILE` / `KODA_LLM_MODEL`
3. `llms.toml` 的 `[selector]`
4. profile 自身默认模型

## resources

```bash
koda-agent resources install --repair
koda-agent resources doctor
koda-agent resources doctor --json
```

resources 是静态资源目录，包含：

- system prompt 和 tool schema
- memory SOP/helper/template
- browser bridge 静态资源
- Python helper requirements

正常情况下 `koda-agent init` 和安装脚本会自动安装 resources。只有资源缺失或升级后需要修复时才手动运行 `resources install --repair`。

## memory

```bash
koda-agent memory settle
koda-agent memory settle --assisted
koda-agent memory audit
koda-agent memory cleanup
koda-agent memory cleanup --run
koda-agent memory recall "关键词"
koda-agent memory l4-archive
koda-agent memory l4-archive --run
```

`memory` 操作的是 `~/.koda-agent/memory` 运行期记忆，不是 `~/.koda-agent/resources/memory` 静态 SOP。

## tui

```bash
koda-agent tui
koda-agent tui --full
koda-agent tui --line
```

- `tui`：稳定 line-mode。
- `tui --full`：全屏 Ratatui UI。
- `tui --line`：强制 line-mode。

## update

```bash
koda-agent update --check
koda-agent update --check --json
koda-agent update --repo koda-claw/koda-agent --version latest
koda-agent update --repo koda-claw/koda-agent --version v0.1.0
```

`update` 会从 GitHub Releases 选择当前平台资产，校验 `SHA256SUMS`，替换二进制，并默认修复 resources。

## bootstrap-python / python-env

```bash
koda-agent bootstrap-python --extras core --repair
koda-agent bootstrap-python --extras ocr,automation --repair
koda-agent python-env remove
```

Python helper 是可选能力，只在 reflect Python 脚本、OCR、Vision helper 或 upstream Python SOP 场景需要。

## serve-acp

```bash
koda-agent serve-acp
```

启动 ACP JSON-RPC-over-JSONL bridge，供外部客户端通过 JSONL 与 Koda Agent 会话交互。

## frontend

```bash
koda-agent frontend tmwebdriver
koda-agent frontend http
koda-agent frontend webhook
```

常用 frontend：

- `tmwebdriver`：浏览器插件桥接 master。
- `http`：HTTP frontend smoke / webhook。
- `webhook`：stdin webhook 调试入口。

具体可用 frontend 以当前构建和 feature 为准。
