# Telegram Frontend Integration Plan

**日期**: 2026-05-13  
**范围**: `crates/koda-agent-frontends` — Telegram IM 前端完整集成  
**上游基准**: `lsdefine/GenericAgent` → `frontends/tgapp.py` (917行)  
**Rust基准**: lib.rs L1682-1869 (190行基础实现)

---

## 1. 现状分析

### 1.1 当前 Rust 实现 (lib.rs L1682-1869)

| 模块 | 行数 | 能力 |
|------|------|------|
| TelegramResponse/Update/Message/Chat | 22行 | 基础反序列化结构 |
| `run_telegram()` | 30行 | getUpdates 长轮询，单 chat_id，文本消息 |
| `send_telegram_message()` | 16行 | sendMessage + 分段 (3500字节) |
| `handle_chat_text()` | 24行 | 命令路由 /help /status /stop /llm /new |
| 辅助函数 | 66行 | split_text, extract_file_markers, looks_like_local_file |

**缺失特性**: 流式编辑、inline buttons、callback query、ask_user 菜单、MarkdownV2 转义、图片/文件发送、BotCommand 菜单注册、reply_to 错误处理、多 chat_id 并发、typing 指示器。

### 1.2 上游 tgapp.py 能力清单 (917行, 39个函数/类)

| 类别 | 函数 | 行号 | 行为 |
|------|------|------|------|
| MD转义 | `_to_markdown_v2` | L208 | 完整MarkdownV2解析器：正则识别code block/quote/link/inline code结构，各段分别转义 |
| MD转义 | `_escape_pre/_escape_code/_escape_link_target` | L195-203 | 按entity_type不同转义规则 |
| MD转义 | `_quote_to_markdown_v2` | L204 | quote文本专用转义 |
| MD转义 | `_is_not_modified_error` | L239 | 识别400 "message is not modified" 错误 |
| 流式 | `_make_draft_id` | L71 | 生成草稿ID |
| 流式 | `_visible_segments/_markdown_safe_segments` | L74-111 | 按行/代码围栏切分可见段 |
| 流式 | `_line_complete/_maybe_partial_code_fence` | L112-128 | 行完整性检测、代码围栏追踪 |
| 流式 | `_TelegramStreamSession` (L371-619) | L371 | 核心流式会话类：segment buffer、draft_id生命周期、编辑节流(1s最小间隔)、finalize/send_files分离 |
| 流式 | `_TelegramTurnStreamCoordinator` | L620-708 | 多turn标记处理、行缓冲、代码围栏追踪、turn summary注入 |
| 流式 | `_extract_turn_summary/_inject_turn_summary/_quote_tag` | L129-155 | turn总结提取/注入 |
| 流式 | editMessageText + retry | - | 编辑消息，not_modified时忽略 |
| 流式 | sendChatAction typing heartbeat | - | 任务期间每5s发送typing |
| ask_user | `_extract_ask_user_event` | L242 | 从ctx提取ask_user事件 |
| ask_user | `_register_ask_user_hook` | L269 | 注册运行时hook |
| ask_user | `_drain_latest_ask_user_event` | L278-285 | 从事件队列取出最新事件(幂等排空) |
| ask_user | `_build_ask_user_markup` | L287 | 构建InlineKeyboardMarkup |
| ask_user | `_parse_ask_callback_data` | L297 | 解析inline button的callback_data(格式: `ask:INDEX`) |
| ask_user | `_normalize_ask_menu_event` | L309-321 | 无candidates时文本fallback |
| ask_user | `_render_ask_user_result` | L323 | 渲染ask结果(用户选择/超时/取消) |
| ask_user | `_clear_ask_reply_markup` | L340 | 清除inline keyboard |
| ask_user | `_edit_ask_user_result` | L346 | 更新ask结果消息 |
| ask_user | `_send_ask_user_menu` | L356 | 发送菜单(30s超时自动关闭) |
| ask_user | `_build_text_prompt` | L306 | 纯文本提示构建 |
| 文件 | `sendDocument/sendPhoto` | - | multipart文件上传 |
| 文件 | `_send_files/_send_files_from_text` | L177-193 | 发送文件列表 |
| 文件 | `_resolve_files` | L156 | 路径解析(相对/绝对/展开) |
| 文件 | `_render_file_markers` | L168 | 文件标记渲染为用户友好文本 |
| 文件 | `_files_from_text` | L173 | 从agent输出提取文件路径 |
| 命令 | `cmd_abort` | L807 | /abort 终止当前任务 |
| 命令 | `handle_command` | L847 | /continue 继续上次中断 |
| 命令 | `cmd_llm` | L812 | /btw 旁白注释 |
| 命令 | `_normalized_command` | L749 | 别名映射表(如 /stop→/abort) |
| 命令 | `handle_msg` | L763 | 主消息处理入口 |
| 命令 | `handle_ask_callback` | L772 | callback query处理入口 |
| 基建 | ALLOWED whitelist | - | 用户白名单过滤 |
| 基建 | Proxy support | - | HTTP代理配置 |
| 基建 | Error handler | - | 全局错误处理+用户通知 |
| 基建 | Restart loop + backoff | - | 断线重连指数退避 |
| 基建 | drop_pending_updates | - | 启动时清除积压消息 |
| 基建 | ensure_single_instance | - | 防止多实例运行 |
| 基建 | ctx.user_data['stream_task'] | L756 | 流式任务JoinHandle管理 |
| 基建 | 429 rate limit retry | - | API限流自动重试 |
| 基建 | FILE_HINT constant | - | 文件大小/类型提示常量 |

### 1.3 差异汇总: 16项遗漏

**P0关键(6项)**: MarkdownV2解析器、StreamSession类、TurnStreamCoordinator、is_not_modified_error、drain_ask_event、normalize_ask_menu  
**命令(5项)**: /abort、/continue、/btw、/debug、_normalized_command别名表  
**基建(5项)**: Proxy、stream_task句柄、callback_data解析、ask结果渲染、文件路径解析链

---

## 2. 目标分层架构

```
┌─────────────────────────────────────────────────┐
│  Telegram Bot Layer (run_telegram)              │
│  ┌─────────────────┐  ┌─────────────────────┐  │
│  │ Command Router   │  │ Message Dispatcher  │  │
│  │ /help /status    │  │ handle_msg          │  │
│  │ /stop /llm /new  │  │ handle_ask_callback │  │
│  │ [新增] /abort    │  │                     │  │
│  │ [新增] /continue │  │                     │  │
│  │ [新增] /btw      │  │                     │  │
│  │ [新增] /debug    │  │                     │  │
│  └────────┬────────┘  └──────────┬──────────┘  │
│           └──────────┬───────────┘              │
│  ┌───────────────────▼──────────────────────┐   │
│  │  Streaming Session Layer [新增]           │   │
│  │  _TelegramStreamSession                   │   │
│  │  _TelegramTurnStreamCoordinator           │   │
│  │  ┌──────────┐ ┌──────────────┐            │   │
│  │  │ Segment  │ │ Draft ID     │            │   │
│  │  │ Buffer   │ │ Lifecycle    │            │   │
│  │  └──────────┘ └──────────────┘            │   │
│  │  ┌──────────┐ ┌──────────────┐            │   │
│  │  │ Edit     │ │ Typing       │            │   │
│  │  │ Throttle │ │ Heartbeat    │            │   │
│  │  │ (1s min) │ │ (5s interval)│            │   │
│  │  └──────────┘ └──────────────┘            │   │
│  └───────────────────────────────────────────┘   │
│  ┌───────────────────────────────────────────┐   │
│  │  ask_user Layer [新增]                    │   │
│  │  _extract/_drain/_normalize               │   │
│  │  _build_markup/_parse_callback_data       │   │
│  │  _render_result/_clear/_edit              │   │
│  │  _send_menu (30s timeout)                 │   │
│  └───────────────────────────────────────────┘   │
│  ┌───────────────────────────────────────────┐   │
│  │  Shared Layer                             │   │
│  │  split_text  extract_file_markers         │   │
│  │  [新增] MarkdownV2 parser                 │   │
│  │  [新增] _resolve_files/_files_from_text   │   │
│  │  [新增] Proxy config                      │   │
│  │  [新增] ensure_single_instance            │   │
│  └───────────────────────────────────────────┘   │
│                                                  │
│  AgentRuntime (shared across all chat_ids)       │
│  put_task / event stream / session write         │
└─────────────────────────────────────────────────┘
```

### 2.1 分阶段交付

**P0 — 流式编辑 (核心循环)** ✅
- ✅ MarkdownV2转义解析器 (`_to_markdown_v2`)
- ✅ `_TelegramStreamSession` 类 (segment buffer, draft_id, 编辑节流)
- ✅ `_TelegramTurnStreamCoordinator` 类 (turn标记, 行缓冲, 代码围栏)
- ✅ editMessageText 定时刷新 + `is_not_modified_error` 容错
- ✅ parse_mode fallback (MarkdownV2失败→纯文本)

**P1 — Inline Buttons + ask_user** ✅
- ✅ InlineKeyboardMarkup 动态按钮
- ✅ answerCallbackQuery 处理
- ✅ ask_user完整事件系统 (6个函数)
- ✅ `/abort` 命令终止流式任务

**P2 — 文件/图片/BotCommand/typing/基建** ✅
- ✅ sendDocument / sendPhoto
- ✅ BotCommand 菜单注册
- ✅ typing 心跳 (每5s)
- ✅ 文件路径解析链
- ✅ Proxy支持
- ✅ stream_task句柄管理
- ✅ /continue /btw /debug 命令

### 2.2 接口设计

```rust
// 现有接口保持不变
pub trait AgentRuntime: Send + Sync + 'static {
    async fn put_task(&self, text: String) -> Result<String>;
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RuntimeEvent>;
    fn session(&self) -> &Session;
}

// 流式执行使用 subscribe() 获取事件流
// RuntimeEvent::ToolDelta { delta, .. } → 文本增量
// RuntimeEvent::AssistantDelta { delta, .. } → 文本增量
// RuntimeEvent::ToolCallStart { .. } → 工具调用开始
// RuntimeEvent::TurnComplete { summary, .. } → 轮次完成

// ask_user 事件类型
enum AskUserEvent {
    Menu { text: String, candidates: Vec<String>, menu_id: String },
    Result { selected: Option<String>, cancelled: bool },
}
```

### 2.3 MarkdownV2 Parser 设计

上游 `_to_markdown_v2` 是完整的结构化解析器，非简单字符转义：

```
解析流程:
1. 用正则识别结构: code block (```)、inline code (`)、quote (>)、link ([text](url))
2. 对各段分别应用不同转义规则:
   - 纯文本段: 转义 _*[]()~`>#+-=|{}.! 字符
   - code block: 仅转义 \` 和 \\
   - inline code: 仅转义 \` 和 \\
   - link target: 仅转义 ) 和 \\
3. 保持MarkdownV2 entity结构完整
```

**Rust实现策略**:
- 使用 `regex` crate 的命名捕获组识别结构
- 分段处理，每段应用不同 `_escape_*` 函数
- 返回 `String` (已转义的MarkdownV2文本)

```rust
fn to_markdown_v2(text: &str) -> String {
    // 1. 分割: code fence / inline code / link / quote / plain text
    // 2. 各段分别转义
    // 3. 拼接返回
}

fn escape_pre(text: &str) -> String { /* code block: 转义 \` \\ */ }
fn escape_code(text: &str) -> String { /* inline code: 转义 \` \\ */ }
fn escape_link_target(text: &str) -> String { /* link: 转义 ) \\ */ }
fn quote_to_markdown_v2(text: &str) -> String { /* quote: 专用规则 */ }
fn is_not_modified_error(exc: &str) -> bool { /* 400 "message is not modified" */ }
```

### 2.4 StreamSession 类设计

上游 `_TelegramStreamSession` (L371-619, ~250行) 是流式编辑的核心:

```rust
struct StreamSession {
    client: Client,
    token: String,
    chat_id: i64,
    message_id: Option<i64>,       // editMessageText 的目标消息
    draft_id: String,              // _make_draft_id() 生成
    buffer: Vec<String>,           // segment buffer
    last_edit: Instant,            // 编辑节流 (1s最小间隔)
    typing_handle: Option<JoinHandle<()>>,  // typing心跳
}

impl StreamSession {
    // 核心方法
    async fn push_delta(&mut self, delta: &str);     // 追加文本增量
    async fn maybe_edit(&mut self);                    // 节流编辑 (≥1s间隔)
    async fn finalize(&mut self) -> Result<()>;        // 最终编辑
    async fn send_files(&mut self, files: &[PathBuf]); // 文件发送
    async fn start_typing(&mut self);                  // 启动typing心跳
    async fn stop_typing(&mut self);                   // 停止typing心跳
}
```

**关键行为**:
- `push_delta`: 追加到buffer，如果距上次编辑≥1s则调用`maybe_edit`
- `maybe_edit`: 将buffer内容转MarkdownV2，调用editMessageText，处理is_not_modified
- `finalize`: 无条件编辑最终内容，清理draft_id
- 编辑失败时fallback到纯文本重试

### 2.5 TurnStreamCoordinator 设计

上游 `_TelegramTurnStreamCoordinator` (L620-708, ~90行) 处理多turn:

```rust
struct TurnStreamCoordinator {
    session: StreamSession,
    line_buffer: String,           // 未完成行缓冲
    code_fence_depth: usize,       // 代码围栏嵌套深度
    turn_count: usize,             // 已完成turn数
    current_summary: Option<String>, // 当前turn summary
}

impl TurnStreamCoordinator {
    async fn feed_delta(&mut self, delta: &str);        // 喂入文本增量
    fn line_complete(&self, line: &str) -> bool;         // 行完整性检测
    fn maybe_partial_code_fence(&self, line: &str) -> bool; // 代码围栏检测
    fn extract_turn_summary(&self, text: &str) -> Option<String>; // 提取turn summary
    fn inject_turn_summary(&self, body: &str, summary: &str) -> String; // 注入summary
    async fn finalize(&mut self) -> Result<()>;          // 最终化
}
```

### 2.6 ask_user 事件系统设计

上游 ask_user 是事件驱动架构 (L242-370, ~130行):

```
事件流:
hook写入event queue → drain_latest取出 → build_markup构建按钮
→ send_menu发送(30s超时) → callback → parse_callback_data
→ render_result → edit_result → clear_markup
```

**6个核心函数**:

```rust
// 1. 从事件队列取出最新事件 (幂等排空)
fn drain_latest_ask_user_event(events: &mut VecDeque<AskUserEvent>) -> Option<AskUserEvent> {
    let mut latest = None;
    while let Some(event) = events.pop_front() {
        latest = Some(event);
    }
    latest
}

// 2. 解析callback_data (格式: "ask:INDEX")
fn parse_ask_callback_data(data: &str) -> Option<usize> {
    data.strip_prefix("ask:").and_then(|s| s.parse().ok())
}

// 3. 无candidates时文本fallback
fn normalize_ask_menu_event(stored: &AskUserEvent) -> AskUserEvent { ... }

// 4. 构建InlineKeyboardMarkup
fn build_ask_user_markup(menu_id: &str, candidates: &[String]) -> serde_json::Value { ... }

// 5. 渲染ask结果
fn render_ask_user_result(event: &AskUserEvent, selected: Option<&str>, cancelled: bool) -> String { ... }

// 6. 发送菜单 (30s超时自动关闭)
async fn send_ask_user_menu(client: &Client, token: &str, chat_id: i64, event: &AskUserEvent) -> Result<i64> { ... }
```

### 2.7 文件处理设计

```rust
// 路径解析: 相对路径 → 绝对路径, ~展开, 校验存在性
fn resolve_files(paths: &[String]) -> Vec<PathBuf> { ... }

// 从agent输出提取文件路径标记 [FILE: path]
fn files_from_text(text: &str) -> Vec<String> { ... }

// 渲染文件标记为用户友好文本
fn render_file_markers(text: &str) -> String { ... }

// 发送文件 (sendDocument / sendPhoto)
async fn send_files(client: &Client, token: &str, chat_id: i64, files: &[PathBuf]) -> Result<()> { ... }

// 从文本中提取并发送文件
async fn send_files_from_text(client: &Client, token: &str, chat_id: i64, text: &str) -> Result<()> { ... }
```

### 2.8 命令路由扩展

```rust
// 别名映射表
fn normalized_command(text: &str) -> Option<&str> {
    match text {
        "/stop" | "/cancel" => Some("/abort"),
        "/cont" => Some("/continue"),
        "/aside" | "/note" => Some("/btw"),
        other => Some(other),
    }
}

// 新增命令
async fn cmd_abort(ctx, update) { /* 终止当前流式任务 */ }
async fn cmd_continue(ctx, update) { /* 继续上次中断的任务 */ }
async fn cmd_btw(ctx, update) { /* 旁白注释，不触发agent */ }
async fn cmd_debug(ctx, update) { /* 显示debug信息: stream状态、token统计、工具调用历史 */ }
```

### 2.9 基础设施设计

```rust
// Proxy配置: 从环境变量读取
fn build_client_with_proxy() -> Result<Client> {
    let mut builder = Client::builder();
    if let Ok(proxy_url) = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("HTTP_PROXY")) {
        builder = builder.proxy(reqwest::Proxy::all(&proxy_url)?);
    }
    Ok(builder.build()?)
}

// stream_task句柄管理
struct UserState {
    stream_task: Option<JoinHandle<()>>,
    ask_events: VecDeque<AskUserEvent>,
}

// /abort时终止
fn cancel_stream_task(state: &mut UserState) {
    if let Some(handle) = state.stream_task.take() {
        handle.abort();
    }
}

// ensure_single_instance: 文件锁
fn ensure_single_instance() -> Result<FileLock> { ... }
```

---

## 3. 文件结构决策

**决策: 保持单文件结构** (用户明确要求)

理由:
1. 用户明确要求"不拆分独立rs文件"
2. 当前190行 + 新增~500行 = ~690行，仍在可维护范围
3. 模块内用 `// === Section ===` 注释分隔
4. 未来如需拆分，结构化注释可作为拆分边界参考

### 3.1 新增代码区域规划

```
lib.rs 当前结构 (2412行):
├── imports (1-50)
├── ACP JSONL (51-486)
├── run_frontend 分发 (487-509)
├── HTTP/Web UI (510-1151)
├── placeholder (1152-1163)
├── tmwebdriver (1164-1678)
├── telegram (1682-1869)      ← 现有190行
│   // === Telegram Data Types ===
│   // === Telegram Helper Functions ===
│   // === Telegram MarkdownV2 Parser === [新增]
│   // === Telegram StreamSession === [新增]
│   // === Telegram TurnStreamCoordinator === [新增]
│   // === Telegram ask_user System === [新增]
│   // === Telegram File Handling === [新增]
│   // === Telegram Command Router === [扩展]
│   // === Telegram Infrastructure === [新增]
│   // === Telegram Main Loop ===
└── tests (1871-2412)
```

---

## 4. 实施步骤

| 步骤 | 内容 | 估时 | 完成判据 |
|------|------|------|----------|
| Phase A ✅ | 更新集成方案文档(本文档) | 0.5h | 文档包含所有16项遗漏的Rust设计 |
| Phase B ✅ | MarkdownV2解析器 | 1h | `cargo build`通过 + 6个单元测试通过 |
| Phase C ✅ | StreamSession + Coordinator | 2h | `cargo build`通过 + 编辑行为测试 |
| Phase D ✅ | 命令路由 + ask_user | 1.5h | `cargo build`通过 + ask_user流程测试 |
| Phase E ✅ | 文件处理 + 基建 | 1h | `cargo build`通过 |
| Phase F ✅ | 集成验证 | 0.5h | `cargo test`全量通过 + 功能对照0遗漏 |

---

## 5. 风险与决策点

| 风险 | 缓解 |
|------|------|
| Telegram API rate limit (30 msg/s per chat) | 流式编辑节流≥1s，分段发送间隔500ms |
| MarkdownV2转义遗漏导致发送失败 | 带parse_mode发送失败时fallback到纯文本重试 |
| callback_query超时 (Telegram默认5s) | 立即answerCallbackQuery，实际处理异步进行 |
| Rust regex命名捕获组 vs Python regex | 测试覆盖各entity类型 |
| "not modified" 错误码一致性 | `_is_not_modified_error` 检查status code + body |
| Proxy在reqwest中的配置 | 支持HTTPS_PROXY/HTTP_PROXY环境变量 |
| 多chat_id并发runtime访问 | AgentRuntime已是Arc，tokio::spawn每个chat独立任务 |
| StreamSession编辑竞争 | 单chat单session，消息ID原子更新 |

---

## 6. 对齐 upstream-parity-alignment.md

当前状态: ✅ `Done+/P2` (Phase A-F全部完成，65测试)

已更新位置:
- ✅ L180: 标记 Telegram streaming edit + inline buttons + file support 完成
- ✅ L188: 更新 Telegram 进度为 Done
