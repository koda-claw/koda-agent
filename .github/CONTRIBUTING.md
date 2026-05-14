# Contributing to koda-agent

## 开发环境

```bash
# 克隆
git clone https://github.com/koda-claw/koda-agent.git
cd koda-agent

# 构建
cargo build

# 运行（开发版，不是发布版 koda-agent）
cargo run -p koda-agent-cli -- <args>
```

> ⚠️ `koda-agent` 是已发布的二进制，`cargo run -p koda-agent-cli` 才是当前代码的开发版。

## 推送前验证清单

**每次推送前必须全部通过，不要逐项修逐项推：**

```bash
# 1. 格式检查
cargo fmt --check

# 2. 静态分析
cargo clippy -- -D warnings

# 3. 测试（大项目超时时先 cargo check --workspace，再 cargo test -p <crate>）
cargo test
```

### 常见反模式

| ❌ 反模式 | ✅ 正确做法 |
|----------|-----------|
| 只跑 `cargo fmt` 就 push，等 CI 告诉你 clippy 挂 | 三项全跑通过再 push |
| CI 报了 3 个 job 失败，修一个推一次 | 先看完所有失败项，一次性修完 |
| `git add .` 把临时文件带进去 | `git add <具体文件>`，提交前 `git status` 确认 |

## Git 卫生

- **临时文件**（`.tmp*`、`plan_*`、`*.log` 等）必须在 `.gitignore` 中声明
- 误提交后不要只 `git rm`，**同时更新 `.gitignore`** 防复发
- 提交前必做：
  ```bash
  git status          # 确认没有不该提交的文件
  git diff --cached   # 确认暂存区内容干净
  ```

## 跨平台注意

CI 在 **Linux / macOS / Windows** 三平台运行。涉及以下代码时自问：Windows 上会怎样？

- **路径处理**：Unix 用 `/`，Windows 用 `\`，尽量用 `std::path::Path` 或 `PathBuf`
- **文件系统操作**：权限、大小写敏感性、路径长度限制
- **进程/命令**：shell 语法、可执行文件扩展名

## CI 流程

推送后 CI 自动运行 4 个 job：

| Job | 内容 |
|-----|------|
| `ubuntu` | fmt + clippy + test |
| `macos` | fmt + clippy + test |
| `windows` | fmt + clippy + test |
| `public-audit` | 安全审计 |

- ✅ 全过 → 可以合并/发布
- ❌ 有失败 → 本地修好，**验证全部通过后**再推一次

```bash
# 查看失败详情
gh run list --limit 1
gh run view <run-id> --log-failed
```

## Commit 规范

```
<type>: <简短描述>

type: fix / feat / docs / refactor / test / chore
```

## 关闭 Issue

在 commit message 中使用关键字自动关闭：
```
Fixes #123
Closes #123
```

或在 issue 页面手动关闭 + 说明对应的 commit。
