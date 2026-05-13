# Telegram Integration - Exploration Findings

**日期**: 2026-05-13  
**基准**: upstream tgapp.py (917行) + 现有Rust实现 (lib.rs L1682-1869, 190行)

---

## 环境现状

### 上游 tgapp.py 能力清单 (39个函数/类)

| 类别 | 函数 | 行号 | Rust状态 |
|------|------|------|----------|
| MD转义 | `_to_markdown_v2` | L208 | ❌ 缺失 |
| MD转义 | `_escape_pre/_escape_code/_escape_link_target` | L208-237 | ❌ 缺失 |
| MD转义 | `_quote_to_markdown_v2` | L208-237 | ❌ 缺失 |
| MD转义 | `_is_not_modified_error` | L239 | ❌ 缺失 |
| 流式 | `_TelegramStreamSession` | L371-619 | ❌ 缺失 |
| 流式 | `_TelegramTurnStreamCoordinator` | L620-708 | ❌ 缺失 |
| 流式 | `_visible_segments/_markdown_safe_segments` | - | ✅ 有概念 |
| 流式 | `_line_complete/_maybe_partial_code_fence` | - | ✅ 有概念 |
| 流式 | `_extract_turn_summary/_inject_turn_summary` | - | ✅ 有概念 |
| 流式 | `editMessageText + retry` | - | ✅ 方案有 |
| 流式 | `sendChatAction typing heartbeat` | - | ✅ 方案有 |
| ask_user | `_extract_ask_user_event` | - | ✅ 方案有 |
| ask_user | `_register_ask_user_hook` | - | ✅ 方案有 |
| ask_user | `_drain_latest_ask_user_event` | L278-285 | ❌ 缺失 |
| ask_user | `_build_ask_user_markup` | - | ✅ 方案有 |
| ask_user | `_parse_ask_callback_data` | L309 | ❌ 缺失 |
| ask_user | `_normalize_ask_menu_event` | L309-321 | ❌ 缺失 |
| ask_user | `_render_ask_user_result` | - | ❌ 缺失 |
| ask_user | `_clear_ask_reply_markup/_edit_ask_user_result` | - | ✅ 方案有 |
| ask_user | `_send_ask_user_menu + 30s timeout` | - | ✅ 方案有 |
| 文件 | `sendDocument/sendPhoto` | - | ✅ 方案有 |
| 文件 | `_send_files/_send_files_from_text` | - | ✅ 方案有 |
| 文件 | `_resolve_files` | - | ❌ 缺失 |
| 文件 | `_render_file_markers` | - | ❌ 缺失 |
| 文件 | `_files_from_text` | - | ❌ 缺失 |
| 命令 | `/abort` | L807 | ❌ 缺失 |
| 命令 | `/continue` | - | ❌ 缺失 |
| 命令 | `/btw` | - | ❌ 缺失 |
| 命令 | `/debug` | - | ❌ 缺失 |
| 命令 | `_normalized_command` (别名) | L749 | ❌ 缺失 |
| 基建 | ALLOWED whitelist | - | ✅ 方案有 |
| 基建 | Proxy support | - | ❌ 缺失 |
| 基建 | Error handler | - | ✅ 方案有 |
| 基建 | Restart loop + backoff | - | ✅ 方案有 |
| 基建 | drop_pending_updates | - | ✅ 方案有 |
| 基建 | ensure_single_instance | - | ❌ 缺失 |
| 基建 | ctx.user_data['stream_task'] | - | ❌ 缺失 |
| 基建 | 429 rate limit retry | - | ✅ 方案有 |
| 基建 | FILE_HINT constant | - | ✅ 方案有 |

### Rust现有实现 (lib.rs L1682-1869)

| 能力 | 状态 |
|------|------|
| getUpdates长轮询 | ✅ |
| sendMessage + 分段(3500字节) | ✅ |
| 命令路由 /help /status /stop /llm /new | ✅ |
| split_text, extract_file_markers | ✅ |

### 缺失汇总: 16项

**P0关键(6项)**: MarkdownV2解析器、StreamSession类、TurnStreamCoordinator、is_not_modified_error、drain_ask_event、normalize_ask_menu  
**命令(5项)**: /abort、/continue、/btw、/debug、_normalized_command  
**基建(5项)**: Proxy、stream_task句柄、callback_data解析、ask结果渲染、文件路径解析链

---

## 关键发现

1. **MarkdownV2不是简单字符转义** — 上游用正则先识别code block/quote/link/inline code等结构，再对各段分别转义，entity_type不同转义规则不同
2. **StreamSession是核心类** — 管理segment buffer、draft_id生命周期、编辑节流(1s最小间隔)、finalize/send_files分离
3. **TurnStreamCoordinator独立于Session** — 处理多turn标记、行缓冲、代码围栏追踪，生成turn summary
4. **ask_user是事件驱动** — hook写入event queue → drain读取 → 发送menu，需要队列排空机制

## 风险/不确定点

- Rust regex crate的命名捕获组语法 vs Python regex
- Telegram API "not modified" 错误码是否一致
- Proxy在reqwest中的配置方式(可能需要环境变量 vs 显式配置)
