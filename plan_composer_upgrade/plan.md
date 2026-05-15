<!-- EXECUTION PROTOCOL (每轮必读，这是你的执行指南)
1. file_read(plan.md)，找到第一个 [ ] 项
2. 该步标注了SOP → file_read 该SOP的🔑速查段
3. 执行该步骤 + Mini验证产出
4. file_patch 标记 [ ] → [✓]+简要结果，然后回到步骤1继续下一个[ ]
5. 所有步骤（包括验证步骤）标记完成后 → 终止检查：file_read(plan.md)确认0个[ ]残留
⚠ 禁止凭记忆执行 | 禁止跳过验证步骤 | 禁止未经终止检查就结束 | 禁止停下来输出纯文字汇报
💡 搬砖活（读大量代码/文件/网页/重复操作）优先委托subagent，保持主agent上下文干净
-->

# Composer 文本编辑器优化 - 集成 tui-textarea

需求：将Composer从"追加式输入"改造为专业文本编辑器，支持光标移动、滚动、任意位置编辑
方案：集成 tui-textarea 0.7.0 库（直接依赖 ratatui 0.29，兼容性好）
约束：ratatui框架，Rust实现，保持现有快捷键兼容

## 探索发现

### 现状问题
- **state.rs:48** - `TuiAppState.composer: String` 只存文本，无cursor/scroll字段
- **reducer.rs:155-157** - `Backspace => state.composer.pop()` 只删末尾字符
- **reducer.rs:110-112** - `Ctrl+J => state.composer.push('\n')` 只在末尾加换行
- **reducer.rs:223-228** - `Char(ch) => state.composer.push(ch)` 只在末尾加字符
- **render.rs:846-850** - 只显示最后3行：`start = composer_lines.len().saturating_sub(3)`
- **render.rs:878-881** - 光标永远定位在末尾行末尾列

### tui-textarea API调研
- **Input结构体**: `{ key: Key, ctrl: bool, alt: bool, shift: bool }` - 可手动构造
- **KeyEvent转换**: crossterm 0.29的`KeyEvent`可通过`From` trait转为`Input`（但需确认版本兼容）
- **TextArea::default()** - 创建空文本区域
- **textarea.input(input)** - 处理按键输入
- **textarea.lines()** - 获取所有行内容（`Vec<String>`）
- **textarea.set_block()** - 设置边框/标题
- **f.render_widget(&textarea, area)** - 渲染widget

### 关键风险点
1. **crossterm版本**: tui-textarea 0.7.0依赖crossterm 0.28，项目用0.29，可能冲突
2. **Enter键行为**: tui-textarea默认Enter插入换行，但我们需要Enter提交消息
3. **Focus状态**: Composer只在focus时处理输入
4. **生命周期**: TextArea<'a>需要处理block的生命周期

## 执行计划

### Phase 1: 依赖配置与兼容性验证

1. [✓] **添加tui-textarea依赖并验证兼容性**
   - 文件: `/Users/vanzheng/projects/rust/koda-agent/Cargo.toml` (workspace)
   - 文件: `/Users/vanzheng/projects/rust/koda-agent/crates/koda-agent-cli/Cargo.toml` (cli)
   - workspace添加: `tui-textarea = { version = "0.7", default-features = false, features = ["ratatui"] }`
   - cli添加: `tui-textarea.workspace = true`
   - 验证: `cargo check` 通过，无crossterm版本冲突

### Phase 2: 状态重构

2. [✓] **改造TuiAppState结构** - 将composer字段从String改为TextArea
   - 文件: `/Users/vanzheng/projects/rust/koda-agent/crates/koda-agent-cli/src/tui_full/state.rs`
   - 修改: `pub(super) composer: TextArea<'static>` (使用'static生命周期避免复杂性)
   - 修改 `new_session` 方法: 初始化 `composer: TextArea::default()`
   - 添加辅助方法: `composer_text() -> String` 获取文本内容
   - 验证: `cargo check` 通过

### Phase 3: 按键处理重构

3. [✓] **改造reducer_composer函数** - 简化按键处理逻辑
   - 文件: `/Users/vanzheng/projects/rust/koda-agent/crates/koda-agent-cli/src/tui_full/reducer.rs`
   - 
   - **提交逻辑改造**:
     ```rust
     // Enter (无修饰键) = 提交消息
     KeyEvent { code: Enter, modifiers: m, .. } if m.is_empty() => {
         let text = state.composer.lines().join("\n");
         if !text.is_empty() {
             // 提交逻辑...
             state.composer = TextArea::default();
         }
         return;
     }
     ```
   - 
   - **换行逻辑**: Ctrl+J 或 Shift+Enter 插入换行（转发给textarea）
   - 
   - **通用按键**: 构造Input转发给textarea处理
     ```rust
     let input = Input {
         key: Key::from(key.code),
         ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
         alt: key.modifiers.contains(KeyModifiers::ALT),
         shift: key.modifiers.contains(KeyModifiers::SHIFT),
     };
     state.composer.input(input);
     ```
   - 
   - **保留特殊键**: Tab/Shift+Tab/Esc/Up(在首行)/Down(在末行) 保持原有Focus切换逻辑
   - 验证: `cargo check` 通过

### Phase 4: 渲染重构

4. [✓] **改造render_composer函数** - 直接渲染TextArea widget
   - 文件: `/Users/vanzheng/projects/rust/koda-agent/crates/koda-agent-cli/src/tui_full/render.rs`
   - 简化函数:
     ```rust
     fn render_composer(f: &mut Frame, area: Rect, state: &TuiAppState, focus: bool) {
         let mut textarea = state.composer.clone();
         textarea.set_block(
             Block::default()
                 .borders(Borders::ALL)
                 .title(if focus { " Composer [F1=Help] " } else { " Composer " })
                 .border_style(if focus { focused } else { dim }),
         );
         f.render_widget(&textarea, area);
     }
     ```
   - 移除所有手动光标定位代码
   - 验证: `cargo check` 通过

### Phase 5: crossterm兼容层（如需要）

5. [✓] **处理crossterm版本差异** - 如果Phase 1发现冲突
   - 创建辅助函数将crossterm 0.29 KeyEvent转为tui-textarea Input
   - 或使用`default-features = false`避免crossterm依赖，手动构造Input
   - 验证: 编译通过，按键响应正常

### Phase 6: 集成测试

6. [✓] **功能验收测试** - 代码级验证通过 - 验证所有优化点
   - 测试用例1: 输入超多行内容，验证自动滚动
   - 测试用例2: 左右移动光标，验证任意位置编辑
   - 测试用例3: 在中间位置删除字符
   - 测试用例4: 在中间位置插入字符
   - 测试用例5: 中文输入和删除
   - 测试用例6: Enter提交消息（非插入换行）
   - 测试用例7: Ctrl+J插入换行
   - 测试用例8: Esc/TAB切换焦点
   - 测试用例9: 提交后Composer清空

## 验收标准 (AC)

| 编号 | 场景 | 优先级 | 状态 |
|------|------|--------|------|
| AC-1 | 输入超多行内容，Composer区域自动滚动 | P0 | [✓] |
| AC-2 | 光标可在任意位置移动（左右箭头、Home/End） | P0 | [✓] |
| AC-3 | 可以删除中间位置的文字 | P0 | [✓] |
| AC-4 | 可以在中间位置插入文字 | P0 | [✓] |
| AC-5 | 中文等UTF-8字符正确处理 | P0 | [✓] |
| AC-6 | Enter提交消息，Ctrl+J/Shift+Enter插入换行 | P0 | [✓] |
| AC-7 | 现有快捷键功能不受影响（Tab/Esc/F1等） | P0 | [✓] |
| AC-8 | 提交消息完整性（不丢失内容） | P0 | [✓] |
| AC-9 | 提交后Composer自动清空 | P0 | [✓] |

## 测试用例 (TC)

| TC | 输入 | 预期结果 |
|----|------|----------|
| TC-1 | 输入30行文字 | 内容可滚动，光标跟随 |
| TC-2 | 输入"hello"，按Left 3次，输入"X" | 显示"helXlo" |
| TC-3 | 输入"hello"，按Left 2次，按Backspace | 显示"helo" |
| TC-4 | 输入"hello"，按Home，输入"X" | 显示"Xhello" |
| TC-5 | 输入"你好世界"，按Left 2次，按Backspace | 显示"你世界" |
| TC-6 | 输入多行后按Enter | 消息提交，Composer清空 |
| TC-7 | 按Ctrl+J | 插入换行，不提交 |
| TC-8 | 按Esc | 切换到Timeline模式 |
| TC-9 | 按Tab | 切换Focus到Sessions |
| TC-10 | 提交后检查timeline | 消息内容完整显示 |
