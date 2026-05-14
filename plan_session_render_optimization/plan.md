# Plan: 会话历史渲染优化

## 背景

当前TUI Sessions功能存在两个问题：
1. **列表重复**：同一session有多个快照文件（增量保存），未去重
2. **渲染粗糙**：只用`history`文本行渲染，未使用结构化的`messages`数据

## 目标

用`messages`（结构化ChatMessage）替代`history`（文本行）渲染会话内容，同时解决列表重复问题。

---

## 技术分析

### 数据结构对比

```
session_*.json:
├── session: String          # session ID
├── created_at: String       # 创建时间
├── history: Vec<String>     # 文本行（当前渲染用）
└── messages: Vec<ChatMessage>  # 结构化数据（未使用）
    └── ChatMessage { role, content }
        ├── role: "system" | "user" | "assistant" | "tool"
        └── content: String | Object
            ├── user: 纯文本 或 {content: "..."}
            ├── assistant: {text, thinking, tool_calls}
            └── tool: {name, content}
```

### 当前渲染问题

```rust
// history.rs 第83-92行 - 只用history
timeline.extend(
    raw.history.iter().rev().take(MAX_HISTORY_LINES)
        .map(|line| history_line_to_timeline(line)),
);
```

- 文本解析易出错（格式变化）
- 丢失`thinking`字段
- 工具调用依赖文本格式`[ToolCall]: {...}`

---

## 执行计划

### Step 1: 分析messages数据样本 [✓]

**状态**: 已完成（探索态）

**发现**:
- `user`消息：纯文本 或 JSON对象{content: "含系统提示的复杂文本"}
- `assistant`消息：JSON对象{text, thinking, tool_calls}
- `system`消息：系统提示（应跳过）
- 无独立`role=tool`消息，工具结果嵌入在assistant的tool_calls中

### Step 2: 实现messages_to_timeline函数 [✓]

**位置**: `crates/koda-agent-cli/src/tui_full/history.rs`

**逻辑**:
```rust
fn messages_to_timeline(messages: &[ChatMessage]) -> Vec<TimelineItem> {
    messages.iter().filter_map(|msg| {
        match msg.role.as_str() {
            "system" => None,  // 跳过
            
            "user" => {
                let text = extract_user_text(&msg.content)?;
                if text.is_empty() { None }
                else { Some(TimelineItem::User(text)) }
            }
            
            "assistant" => {
                let resp: AgentResponse = serde_json::from_value(msg.content.clone()).ok()?;
                let mut items = Vec::new();
                
                // 工具调用
                for tc in &resp.tool_calls {
                    items.push(TimelineItem::ToolCall {
                        name: tc.name.clone(),
                        args: tc.args.to_string(),
                    });
                }
                
                // 回复文本
                if !resp.text.is_empty() {
                    items.push(TimelineItem::Assistant(resp.text));
                }
                
                Some(items)
            }
            
            _ => None,
        }
    }).flatten().collect()
}
```

**辅助函数**:
```rust
fn extract_user_text(content: &Value) -> Option<String> {
    // 纯字符串
    if let Some(s) = content.as_str() {
        return Some(s.trim().to_string());
    }
    
    // JSON对象：提取content字段，过滤系统提示
    if let Some(obj) = content.as_object() {
        if let Some(text) = obj.get("content").and_then(|v| v.as_str()) {
            let filtered: String = text.lines()
                .filter(|line| !line.contains("[SYSTEM]") && !line.contains("WORKING MEMORY"))
                .collect::<Vec<_>>()
                .join("\n");
            return Some(filtered.trim().to_string());
        }
    }
    
    None
}
```

### Step 3: 修改history_session_to_tui函数 [✓]

**位置**: `crates/koda-agent-cli/src/tui_full/history.rs` 第70-125行

**改动**:
```rust
fn history_session_to_tui(raw: RawSessionFile, id: usize) -> LoadedHistorySession {
    let title = history_prompt_title(&raw.history).unwrap_or_else(|| {
        raw.session.as_deref()
            .map(history_session_title)
            .unwrap_or_else(|| format!("history-{id}"))
    });
    
    // 优先用messages，fallback到history
    let timeline = if !raw.messages.is_empty() 
        && raw.messages.iter().any(|m| m.role != "system") 
    {
        messages_to_timeline(&raw.messages)
    } else {
        history_to_timeline(&raw.history)
    };
    
    // ... 其余不变
}
```

### Step 4: 实现列表去重 [✓]

**位置**: `crates/koda-agent-cli/src/tui_full/history.rs` 第43-55行

**改动**:
```rust
pub(super) fn load_recent_history_sessions() -> Vec<LoadedHistorySession> {
    // ... 现有代码 ...
    
    let mut sessions: Vec<LoadedHistorySession> = files
        .iter()
        .enumerate()
        .filter_map(|(idx, path)| {
            let raw = load_history_session_file(path).ok()?;
            if raw.history.is_empty() && raw.messages.is_empty() {
                return None;
            }
            Some(history_session_to_tui(raw, idx))
        })
        .collect();
    
    // 按session ID去重，保留最新的
    let mut seen = std::collections::HashSet::new();
    sessions.retain(|s| seen.insert(s.session.name.clone()));
    
    sessions.into_iter().rev().take(8).collect()
}
```

### Step 5: 编译测试 [✓]

**验证**:
1. `cargo build -p koda-agent-cli` 编译通过
2. 运行TUI，检查Sessions列表
3. 点击历史会话，验证渲染正确
4. 检查thinking字段是否保留（可选在Inspector展示）

---

## 风险与兜底

| 风险 | 应对 |
|------|------|
| 旧文件只有history | fallback到history_to_timeline |
| messages格式异常 | 捕获解析错误，fallback |
| tool_calls结构变化 | serde_json容错处理 |

---

## 交付物

- [ ] 修改后的`history.rs`
- [ ] 编译通过
- [ ] 功能验证截图/说明
