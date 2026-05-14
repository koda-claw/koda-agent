# GitHub Issues 修复计划

## Issues 分析

### Bug Issues (高优先级)

| # | Issue | 价值 | 原因 | 复杂度 |
|---|-------|------|------|--------|
| 1 | #2 native_oai 硬编码 /v1 前缀 | ⭐⭐⭐ 高 | 影响所有非标准端点用户(如ZhipuAI) | 中 |
| 2 | #3 config validate 阻断启动 | ⭐⭐⭐ 高 | 多profile用户无法使用指定profile | 低 |
| 3 | #4 native_claude 无限循环 | ⭐⭐⭐ 高 | 第三方Anthropic兼容提供商完全不可用 | 中 |

### Feature Issues (中优先级)

| # | Issue | 价值 | 原因 | 复杂度 |
|---|-------|------|------|--------|
| 4 | #5 添加 --max-turns 选项 | ⭐⭐ 中 | 已有变通方案(env var)，但CLI支持更友好 | 低 |
| 5 | #6 TUI 显示 turn/max_turns | ⭐⭐ 中 | UX改善，依赖#5 | 低 |
| 6 | #7 TUI 计时器 | ⭐ 低 | nice-to-have，优先级最低 | 中 |

## 修复计划

### Phase 1: Bug 修复 (必须完成)

#### Issue #3: config validate 阻断启动 (最简单，先做)
- **分支**: `fix/issue-3-validate-per-profile`
- **改动**: 修改validate逻辑，`--profile`指定时只校验该profile
- **文件**: 待探索 `crates/koda-agent-core/src/` 或 CLI相关
- **测试**: ���profile配置下指定profile启动

#### Issue #2: native_oai 路径硬编码
- **分支**: `fix/issue-2-oai-path-prefix`
- **改动**: 智能检测base_url是否已含路径，避免重复拼接
- **文件**: 待探索 native_oai 实现处
- **测试**: ZhipuAI端点配置测试

#### Issue #4: native_claude 无限循环
- **分支**: `fix/issue-4-claude-parser-tolerance`
- **改动**: 增加响应格式容错，添加verbose日志
- **文件**: 待探索 native_claude parser
- **测试**: 第三方Anthropic端点测试

### Phase 2: Feature 实现 (时间允许)

#### Issue #5: 添加 max_turns 到 config set
- **分支**: `feat/issue-5-max-turns-cli`
- **改动**: config set允许max_turns字段
- **测试**: config set/get max_turns

#### Issue #6: TUI 显示 turn 计数
- **分支**: `feat/issue-6-tui-turn-display`
- **改动**: TUI状态栏显示 Turn X/max_turns
- **测试**: TUI模式验证显示

#### Issue #7: TUI 计时器 (可选)
- **分支**: `feat/issue-7-tui-timer`
- **改动**: 状态栏session timer + per-message耗时
- **测试**: TUI模式验证计时

## 执行规则

1. **每个issue一个分支一个commit**
2. **修复后运行测试** (`cargo test` 或项目测试命令)
3. **不推送**，仅本地提交
4. **遇到困难跳过**，记录原因，继续下一个

## 进度追踪

- [x] Phase 1.1: Issue #3 - config validate (10a0efd)
- [x] Phase 1.2: Issue #2 - native_oai path (10a0efd)
- [x] Phase 1.3: Issue #4 - native_claude parser (10a0efd)
- [x] Phase 2.1: Issue #5 - max_turns config (10a0efd)
- [x] Phase 2.2: Issue #6 - TUI turn display (10a0efd)
- [x] Phase 2.3: Issue #7 - TUI timer (91cd17e)
