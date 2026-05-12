# koda-agent

koda-agent 是一个 Rust 实现的 GenericAgent 兼容运行时，目标是对齐上游 `lsdefine/GenericAgent` `9024af7` 的核心行为，同时提供更工程化的安装、配置、更新、资源管理、TUI 和浏览器桥接能力。

## 一键安装

安装脚本会自动识别当前平台，下载 GitHub Releases 中匹配的二进制包，尽量校验 `SHA256SUMS`，安装 `koda-agent`，同步内置 prompts、tool schema、memory SOP、浏览器桥接资源到 `~/.koda-agent/resources`，并执行 `koda-agent init` 初始化本机目录。

### macOS / Linux

安装最新 release：

```bash
curl -fsSL https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.sh \
  | KODA_AGENT_REPO=koda-claw/koda-agent sh
```

安装指定版本：

```bash
curl -fsSL https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.sh \
  | KODA_AGENT_REPO=koda-claw/koda-agent KODA_AGENT_VERSION=v0.1.5 sh
```

默认安装到 `~/.local/bin/koda-agent`。如果命令不可用，把 `~/.local/bin` 加入 `PATH`，或打开一个新终端后再试。

### Windows PowerShell

安装最新 release：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=$env:TEMP+'\koda-agent-install.ps1'; iwr -UseBasicParsing https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.ps1 -OutFile $s; & $s -Repo koda-claw/koda-agent"
```

安装指定版本：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=$env:TEMP+'\koda-agent-install.ps1'; iwr -UseBasicParsing https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.ps1 -OutFile $s; & $s -Repo koda-claw/koda-agent -Version v0.1.5"
```

默认安装到 `%LOCALAPPDATA%\koda-agent\bin`。脚本会把该目录加入用户级 `Path`；如果当前终端还找不到命令，打开一个新终端。

### 从源码安装

适合开发者或想测试当前 checkout 的用户：

```bash
scripts/install.sh --from-source
```

Windows：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/install.ps1 -FromSource
```

## 安装后验证

```bash
koda-agent --version
koda-agent doctor
```

运行时数据默认在 `~/.koda-agent`：

- `~/.koda-agent/config/llms.toml`：LLM profile 和模型别名配置。
- `~/.koda-agent/.env`：API Key、当前 profile / model 选择等本机私密配置。
- `~/.koda-agent/resources`：安装包内置资源，例如 prompts、tool schema、memory SOP、浏览器插件资源、Python helper requirements。
- 当前命令执行目录仍然是 file tools 的默认 workspace。

如需重新修复资源：

```bash
koda-agent resources install --repair
koda-agent resources doctor --json
```

## 更新

无需保留源码 checkout，可以直接从 GitHub Releases 更新：

```bash
koda-agent update --repo koda-claw/koda-agent --version latest
koda-agent update --repo koda-claw/koda-agent --version v0.1.5
koda-agent update --check
koda-agent update --check --json
```

当前 release 资产覆盖 Linux / macOS / Windows 的 amd64 和 arm64。更新器会选择当前平台资产，替换本地二进制，并默认修复 `~/.koda-agent/resources`。

## 快速开始

先初始化配置和资源：

```bash
koda-agent init
koda-agent doctor
```

配置一个 LLM profile。以 MiMo 为例：

```bash
koda-agent config setup mimo --yes
koda-agent config secret MIMO_API_KEY --from-stdin
koda-agent config use mimo
koda-agent config validate
```

启动 TUI：

```bash
koda-agent tui
koda-agent tui --full
```

一次性任务：

```bash
koda-agent --input "用一句话介绍你自己"
koda-agent --task demo --input "读取 README.md 并总结"
```

常用命令：

```bash
koda-agent config list
koda-agent config model list mimo
koda-agent config model use deepseek flash
koda-agent memory settle
koda-agent memory audit
koda-agent frontend tmwebdriver
koda-agent serve-acp
koda-agent goal "持续优化当前项目" --budget 30m
```

## LLM 配置模型

koda-agent 以 `llms.toml` 为主配置。一个 profile 表示一个 provider / endpoint / account，一个 profile 下可以挂多个模型别名：

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
```

运行时可以切换模型：

- `/llms`：查看可用模型。
- `/llm <n|profile|profile:model>`：切换 profile 或具体模型。
- `/model <alias>`：在当前 profile 内切换模型别名。

兼容说明：旧的 `OPENAI_BASE_URL` / `OPENAI_MODEL` 环境变量主要作为迁移输入，不是推荐的新配置方式。密钥应放在 `~/.koda-agent/.env`，日志中会脱敏。

## TUI

默认 `koda-agent tui` 是稳定的行模式。`koda-agent tui --full` 是全屏 Ratatui 界面，包含多 session、Timeline、Inspector、Composer 和工具卡片。

常用快捷键：

- `Enter`：提交。
- `Ctrl-J`：Composer 换行。
- `Ctrl-S`：停止当前任务。
- `Ctrl-N`：新建 session。
- `Ctrl-B`：分支 session。
- `Ctrl-W`：关闭 session。
- `Ctrl-L`：清空 Timeline。
- `Ctrl-P`：命令面板。
- `?`：帮助。
- `PageUp` / `PageDown` / 鼠标滚轮：滚动 Timeline。
- `End`：回到最新输出。
- `F7` / `Ctrl-M`：切换交互模式和复制模式。

macOS 的 Terminal / iTerm / Warp 通常会先拦截 `Command-*`，所以 TUI 使用跨终端更稳定的 `Ctrl-*` 和 F-key：`F1` 帮助，`F2` 命令面板，`F3` 新 session，`F4` 分支，`F5` 清空，`F6` 关闭。

`ask_user` 在全屏 TUI 中会进入专门的等待用户状态：候选项可以按 `1-9` 选择，也可以输入自由文本，或使用 `/answer <text>`、`/choose <n>`、`/cancel`。

## 浏览器桥接

启动 Rust TMWebDriver-compatible master：

```bash
koda-agent frontend tmwebdriver
```

浏览器插件资源位于：

```text
~/.koda-agent/browser/tmwd_cdp_bridge
```

插件通过 `ws://127.0.0.1:18765` 和 `127.0.0.1:18766` 与本机 master 通信。`web_execute_js` 支持直接发送 JSON bridge 命令，例如 `tabs`、`cookies`、`cdp Runtime.evaluate`、`batch`、`management`、`contentSettings`，否则会回退为普通页面 JavaScript。

## Python helper

核心运行时是 Rust 原生实现。Python helper 是可选能力，主要用于 reflect 脚本、OCR、vision helper 或上游 Python SOP。

需要时再安装：

```bash
koda-agent bootstrap-python --extras core --repair
```

可选 extras 见：

```bash
koda-agent python-env --help
```

## 开发与验收

推荐本地质量门禁：

```bash
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

项目 Makefile 封装了常用检查：

```bash
make check
make docs
make release-dry-run
make audit-secrets
make audit-history
```

浏览器和 TUI 冒烟：

```bash
make smoke-tui
make smoke-browser
make smoke-rich-monitor
make smoke-tmwd-extension
make smoke-tmwd-matrix
```

部分浏览器 smoke 需要先启动 Chrome/Edge CDP 或安装并加载 `tmwd_cdp_bridge` 插件。

## 中文文档

更详细的教程可以用 mdBook 渲染：

```bash
make docs-serve
```

常用入口：

- 快速开始：`docs/book/src/quickstart.zh.md`
- CLI 命令手册：`docs/book/src/cli.zh.md`
- LLM 配置：`docs/book/src/configuration.zh.md`
- TUI 使用：`docs/book/src/tui.zh.md`
- Resources 与 Memory：`docs/book/src/resources-memory.zh.md`
- 发布验收清单：`docs/book/src/release-checklist.zh.md`

## 安全说明

- 不要提交 `.env`、真实 API Key、日志、浏览器插件运行时 `config.js`、memory L4 原始会话等本机数据。
- `koda-agent doctor`、日志和配置展示会尽量脱敏密钥。
- 发布前建议运行 `make audit-secrets` 和 `make audit-history`。
