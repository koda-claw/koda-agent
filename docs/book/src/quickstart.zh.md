# 快速开始

本教程面向已经安装或准备安装 `koda-agent` 的用户。Koda Agent 是一个 Rust 实现的 GenericAgent-compatible CLI，默认把用户级配置和运行期数据放在 `~/.koda-agent`，把当前 shell 所在目录作为工作区。

## 1. 安装

从源码目录安装到本机：

```bash
scripts/install.sh --from-source
```

安装后确认版本和帮助文本：

```bash
koda-agent --version
koda-agent --help
```

如果你使用 Windows PowerShell：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/install.ps1 -FromSource
```

## 2. 初始化用户目录

```bash
koda-agent init
koda-agent doctor
koda-agent resources doctor
```

初始化会创建或修复：

- `~/.koda-agent/.env`：保存 secret 和当前 profile/model selector。
- `~/.koda-agent/config/llms.toml`：主 LLM 配置文件。
- `~/.koda-agent/resources`：静态资源、SOP、工具 schema、浏览器桥接资源。
- `~/.koda-agent/memory`：运行期可变记忆。

## 3. 配置模型

推荐使用 `llms.toml` 作为主配置，`.env` 只保存密钥和当前选择。

以 MiMo 兼容接口为例：

```bash
koda-agent config setup mimo --yes
printf '%s' "$MIMO_API_KEY" | koda-agent config secret MIMO_API_KEY --from-stdin
koda-agent config use mimo
koda-agent config validate
```

DeepSeek / OpenAI / GLM 也可以直接用内置 preset：

```bash
koda-agent config setup deepseek --yes
koda-agent config setup openai --yes
koda-agent config setup glm --yes
```

如果同一个 provider/profile 下有多个模型：

```bash
koda-agent config model list mimo
koda-agent config model add mimo flash --id mimo-v2.5
koda-agent config model use mimo flash
```

临时指定模型，不改默认配置：

```bash
koda-agent --profile mimo --model flash --input "用一句话回复：配置验证成功"
koda-agent --llm mimo:flash --input "用一句话回复：配置验证成功"
```

## 4. 第一次运行

```bash
koda-agent --input "用一句话介绍你自己"
```

进入 TUI：

```bash
koda-agent tui
koda-agent tui --full
```

`koda-agent tui` 是稳定 line-mode，`koda-agent tui --full` 是全屏 TUI。

## 5. 常用诊断

```bash
koda-agent doctor --json
koda-agent config list
koda-agent config show
koda-agent config validate
koda-agent resources doctor --json
```

如果缺少 Python helper，但你需要 OCR、Vision helper、reflect Python 脚本或上游 Python SOP：

```bash
koda-agent bootstrap-python --extras core --repair
```

Python 是可选依赖，核心 CLI、LLM、工具和 memory 不依赖用户系统已有 Python。
