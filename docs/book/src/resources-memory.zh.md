# Resources 与 Memory

Koda Agent 有两个容易混淆但职责不同的目录：`resources/memory` 和运行期 `memory`。

## 目录职责

```text
~/.koda-agent/
  resources/
    memory/      # 静态 SOP/helper/template，随版本安装或修复
  memory/        # 运行期可变记忆，随用户使用增长
```

## resources/memory

`resources/memory` 是静态资源，来源于安装包或源码 assets/memory。它应该可以被重新安装、覆盖和修复。

典型内容：

- `memory_management_sop.md`
- `plan_sop.md`
- `verify_sop.md`
- `vision_sop.md`
- `tmwebdriver_sop.md`
- `ocr_utils.py`
- `vision_api.py`
- `skill_search/*`

检查 resources：

```bash
koda-agent resources doctor
koda-agent resources install --repair
```

## runtime memory

`~/.koda-agent/memory` 是用户运行期记忆，不能当作静态资源覆盖。

典型内容：

- `global_mem.txt`
- `global_mem_insight.txt`
- `long_term_updates.jsonl`
- `pending_long_term_updates.md`
- L4 session history

常用命令：

```bash
koda-agent memory audit
koda-agent memory settle
koda-agent memory recall "关键词"
koda-agent memory l4-archive --run
```

## 工作区与用户目录

在任意项目目录运行 `koda-agent` 时：

- 当前目录是 workspace，文件工具默认面向这里。
- 用户级配置、resources、memory 来自 `~/.koda-agent`。
- 这避免把运行期记忆和配置写进项目仓库。

如果需要确认当前解析结果：

```bash
koda-agent doctor --json
```
