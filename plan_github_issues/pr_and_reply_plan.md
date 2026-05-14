# 提交 & 回复 & 版本更新计划 (v2 - 直推主仓库)

## 📊 现状总结

| 项目 | 状态 |
|------|------|
| 当前分支 | `fix/issue-4-claude-parser-tolerance` |
| 对比 main | 5 个 commits (7a055eb → 4134608) |
| 测试 | 83 + 12 = 95 passed, 0 failed |
| 当前 CLI 版本 | 0.1.7 |
| GitHub Issues | #2-#7 全部 open |

### Issues 修复映射

| Issue | 标题 | 修复 Commit | 改动文件 |
|-------|------|------------|---------|
| #2 | native_oai /v1 硬编码 | 8a9cdd2 | `llm/src/lib.rs` |
| #3 | config validate 阻断启动 | 10a0efd | `core/src/lib.rs` |
| #4 | native_claude 无限循环 | 7a055eb | `llm/src/lib.rs` |
| #5 | --max-turns CLI 选项 | 10a0efd | `cli/src/main.rs` |
| #6 | TUI 显示 turn/max_turns | 10a0efd | `cli/src/tui_full/render.rs` |
| #7 | TUI 计时器 | 91cd17e | `cli/src/tui_full/state.rs` + 4 files |

---

## Step 1: 版本更新 0.1.7 → 0.1.8

需要修改的文件：
```
crates/koda-agent-cli/Cargo.toml     version = "0.1.7" → "0.1.8"
```

---

## Step 2: Git 合并到 main

```bash
# 切回 main
git checkout main

# Squash 合并 feature 分支
git merge --squash fix/issue-4-claude-parser-tolerance

# 提交（带 Closes 关键字，GitHub 自动关联 issues）
git commit -m "fix: resolve #2 #3 #4 bugs + feat #5 #6 #7 TUI enhancements

- #2: remove hardcoded /v1 path in auto_make_url for native_oai
- #3: validate only active profile when --profile is specified
- #4: tolerate non-standard content block types in native_claude parser
- #5: add --max-turns CLI option to config set
- #6: display turn/max_turns in TUI status bar
- #7: add session & turn elapsed timer in TUI

Closes #2, Closes #3, Closes #4, Closes #5, Closes #6, Closes #7"

# 推送到 origin
git push origin main

# 打 tag
git tag v0.1.8
git push origin v0.1.8

# 删除已合并的 feature 分支（本地 + 远程如果有）
git branch -d fix/issue-4-claude-parser-tolerance fix/issue-2-native-oai-url fix/issue-2-native-oai-url-hardcoded fix/issue-3-validate-per-profile
```

---

## Step 3: GitHub Issues 回复

推送后通过 GitHub API 在每个 issue 下留修复说明评论：

### Issue #2
> ✅ **Fixed in [v0.1.8](https://github.com/koda-claw/koda-agent/commit/COMMIT_HASH)**
> 
> Removed the hardcoded `/v1` path concatenation in `auto_make_url()`. Now intelligently detects whether `base_url` already contains a path component, avoiding double-prefix for endpoints like ZhipuAI (`https://open.bigmodel.cn/api/paas/v4/...`).

### Issue #3
> ✅ **Fixed in [v0.1.8](link)**
> 
> Modified `config validate` to only validate the active/selected profile when `--profile` is specified, instead of failing on all profiles including those with missing API keys.

### Issue #4
> ✅ **Fixed in [v0.1.8](link)**
> 
> Added tolerance in the native_claude parser for non-standard content block types returned by third-party Anthropic-compatible APIs (e.g. ZhipuAI). Previously, unknown block types caused an infinite retry loop; now they are gracefully skipped with a warning log.

### Issue #5
> ✅ **Fixed in [v0.1.8](link)**
> 
> Added `--max-turns` CLI option support. Can now be set via:
> ```bash
> koda-agent config set --profile <name> max_turns 20
> ```
> or via environment variable `KODA_MAX_TURNS`.

### Issue #6
> ✅ **Fixed in [v0.1.8](link)**
> 
> TUI status bar now displays turn count: `Turn 3/20` showing current turn and configured max_turns limit.

### Issue #7
> ✅ **Fixed in [v0.1.8](link)**
> 
> TUI status bar now shows:
> - **Session timer**: elapsed time since session start
> - **Turn timer**: per-turn elapsed time alongside each turn

---

## Step 4: 确认

- [ ] GitHub 上 issues #2-#7 自动关闭 (commit message 中 `Closes #N`)
- [ ] 每个 issue 有修复说明评论
- [ ] v0.1.8 tag 存在
- [ ] main 分支包含所有修复

---

## 执行顺序

1. ⬜ 版本更新 0.1.7 → 0.1.8
2. ⬜ git squash merge 到 main
3. ⬜ git push origin main
4. ⬜ git tag v0.1.8 + push tag
5. ⬜ 清理本地 feature 分支
6. ⬜ GitHub API 回复 #2-#7 每个 issue
7. ⬜ 确认 issues 自动关闭
