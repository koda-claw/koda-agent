# TUI 使用

Koda Agent 提供两种终端交互模式：稳定 line-mode 和实验性 full-screen TUI。

## 启动

```bash
koda-agent tui
koda-agent tui --line
koda-agent tui --full
```

- `tui`：默认稳定 line-mode。
- `tui --line`：强制 line-mode。
- `tui --full`：启动全屏 TUI。

如果终端不是 TTY，全屏 TUI 会返回清晰错误，不会输出鼠标控制序列污染终端。

## 快捷键

全屏 TUI 使用终端可移植的 `Ctrl-*` 快捷键。macOS 的 `Command-*` 通常被 Terminal/iTerm/WezTerm 自身截获，CLI 程序一般收不到。

| 快捷键 | 行为 |
| --- | --- |
| `Enter` | 提交当前输入。 |
| `Ctrl-J` | 在 Composer 中换行。 |
| `Ctrl-S` | 停止当前运行。 |
| `Ctrl-N` / `F3` | 新会话。 |
| `Ctrl-B` / `F4` | 从当前上下文分支。 |
| `Ctrl-W` / `F6` | 关闭当前会话。 |
| `Ctrl-L` / `F5` | 清空当前 timeline 视图。 |
| `Ctrl-P` / `F2` | 打开命令面板。 |
| `?` / `F1` | 帮助。 |
| `PageUp` / `PageDown` | Timeline 翻页。 |
| `End` | 回到最新输出并恢复自动跟随。 |
| `Esc` | 关闭 overlay 或退出。 |
| `Ctrl-Q` | 立即退出。 |

## 本地命令

```text
/branch [name]
/switch <id|name>
/rename <name>
/sessions
/clear
/close
/help
/commands
```

## Runtime 命令

这些命令会传给 Agent runtime：

```text
/status
/llms
/llm <n|profile|profile:model>
/models
/model <alias>
/continue
/btw <question>
```

## 滚动与输出

- Timeline 默认自动跟随最新输出。
- 用户手动滚动后会暂停自动跟随，按 `End` 回到最新输出。
- 鼠标滚轮作用于鼠标所在 pane。
- Timeline 渲染轻量 Markdown，并使用中文优先的角色标签。

## 使用建议

如果要稳定跑任务或排查问题，优先用 line-mode 或 `--input`。如果要连续对话、多会话切换、查看工具轨迹和 Inspector 信息，使用 `tui --full`。
