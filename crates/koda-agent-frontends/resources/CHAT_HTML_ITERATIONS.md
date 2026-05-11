# chat.html — 物理级聊天终端 迭代说明书

> 文件位置：`crates/koda-agent-frontends/resources/chat.html`
> 引用方式：`include_str!("../resources/chat.html")` (lib.rs:472)
> 当前版本：**v2.7**

---

## 一、架构概述

chat.html 是 koda-agent HTTP 前端的主页面，运行在 `http://localhost:8787`。
它不是一个静态页面，而是一个**完整的交互式终端**，通过 SSE 流式接口与后端 LLM 引擎通信。

### 通信架构

```
浏览器 (chat.html)                    Rust 后端 (lib.rs)
      │                                      │
      │  POST /message/stream                │
      │  { prompt: "用户输入" }               │
      │ ──────────────────────────────────→  │
      │                                      │  put_task_with_events()
      │                                      │
      │  SSE event: agent                    │
      │  data: {"type":"agent_message_chunk", │
      │         "content":[{"text":"..."}]}   │
      │ ←────────────────────────────────── │
      │                                      │
      │  SSE event: agent                    │
      │  data: {"type":"tool_call",          │
      │         "name":"code_run",           │
      │         "args":{...}}                │
      │ ←────────────────────────────────── │
      │                                      │
      │  SSE event: agent                    │
      │  data: {"type":"tool_result",        │
      │         "name":"code_run",           │
      │         "content":{...}}             │
      │ ←────────────────────────────────── │
      │                                      │
      │  SSE event: agent                    │
      │  data: {"type":"agent_turn_done"}    │
      │ ←────────────────────────────────── │
```

### 后端 SSE 事件类型 (lib.rs `acp_update_from_event`)

| SSE type | 含义 | 前端处理 |
|----------|------|---------|
| `agent_message_chunk` | AI 文本输出片段 | 累加后 Markdown 渲染 |
| `tool_call` | AI 调用工具 | 创建终端日志卡片(参数默认隐藏) |
| `tool_result` | 工具执行结果 | 更新卡片状态(✓/✗) |
| `agent_turn_done` | 本轮结束 | 停止流式光标 |
| `final` | 最终完整输出 | 回填最终文本 |
| `error` | 错误 | 红色错误提示 |

---

## 二、迭代历史

### v1.0 (原始版)
- 纯前端 mock 聊天，无后端接入
- 使用 `responses` 对象内的硬编码回复
- 基础 Matrix 雨背景、扫描线效果

### v2.0 — 视觉融合
- 融合 `index.html` 的设计语言
- 新增：glitch 标题 RGB 分离特效 (`::before`/`::after`)
- 新增：状态栏(系统/网络/权限三色灯)
- 新增：终端窗口(systemctl status 动画)
- 新增：核心能力卡片(4 张)
- 新增：技能精通度进度条(3 类 9 条)
- 新增：系统规格表(8 项)
- 新增：鼠标轨迹 Canvas 粒子特效
- 新增：入侵检测 banner + 屏幕抖动

### v2.1 — 后端 API 接入
- 移除 mock 数据，接入 `POST /message` 接口
- 本地命令(glitch/matrix/clear)保留不走网络
- 错误处理：网络失败/后端报错红色提示
- 加载状态锁防止重复提交

### v2.2 — SSE 流式 + Markdown
- **SSE 流式**：`fetch POST /message/stream` + `ReadableStream`
- 流式闪烁光标指示进行中
- `AbortController` 支持清屏中断
- **Markdown 渲染**：自实现 `renderMarkdown()` 函数
- AI 字体优化：`#c8e6c8` 柔和矩阵绿 + 微发光

### v2.3 — tool_call 透明化
- `tool_call` 事件 → 创建工具卡片(显示工具名+参数)
- `tool_result` 事件 → 更新卡片 ✓完成(绿)/✗失败(红)
- 通过 `name:index` 键值匹配调用与结果

### v2.4 — 终端日志风格
- 从 Web 卡片改为终端命令行执行日志样式
- `⫸ $ tool_name` 命令提示符前缀
- 无边框/无背景块，纯等宽字体
- `⟳` 旋转动画表示执行中

### v2.5 — summary 折叠 + 参数格式化
- `<summary>` 标签检测 → 可折叠行 `▶ 阶段：xxx`
- 点击展开全文(灰色左边框)
- `formatToolArgs()`：智能格式化参数
  - string: `"前40字..."`
  - array: `[3 items]`
  - object: `{5 keys}`

### v2.6 — 表格渲染
- markdown 表格 `|col1|col2|` → `<table><thead><th>` + `<tbody><td>`
- 检测 `---|---` 分隔线，自动分割表头/表体
- 无分隔线时第一行当表头
- 修复 `__TABLE_SEP__` 残留 bug

### v2.7 — 参数隐藏 + 资源文件正式化
- 工具参数默认隐藏，`▶ args` 点击展开
- 展开后显示：格式化摘要 + 分隔线 + 完整 JSON
- chat.html 从 `temp/` 移至 `resources/` 正式目录
- `include_str` 路径更新

---

## 三、当前功能清单 (v2.7)

### 视觉层

| 功能 | 说明 |
|------|------|
| Matrix 雨 | Canvas 片假名+数字，定时刷新 |
| 鼠标轨迹 | 绿色粒子拖尾+连线 |
| CRT 扫描线 | `repeating-linear-gradient` 叠加 |
| Glitch 干扰 | RGB 分离 + 随机屏幕撕裂动画 |
| Glitch 标题 | Orbitron 字体 + `::before`(粉) `::after`(青) 偏移 |
| 状态栏 | 系统(绿)、网络(黄)、权限(红) 三色脉冲灯 |
| 入侵检测 | 屏幕抖动 + 红色 banner，每 3 次弹出 |
| 响应式 | 移动端适配，隐藏第 4 个状态项 |

### 交互层

| 功能 | 说明 |
|------|------|
| SSE 流式聊天 | 实时增量渲染 |
| Markdown 渲染 | 粗/斜/代码/代码块/链接/列表/标题/引用/分隔线 |
| 表格渲染 | thead/th + tbody/td 分离 |
| `<summary>` 折叠 | 点击 ▶ 展开全文 |
| 工具调用透明化 | 终端日志样式卡片 |
| 工具参数隐藏 | `▶ args` 点击展开，含格式化摘要+完整 JSON |
| 本地命令 | glitch/matrix/clear 不走网络 |
| 错误提示 | 红色边框 + `#ff6666` |

### 后端集成

| 接口 | 方法 | 请求格式 | 响应格式 |
|------|------|---------|---------|
| `/message` | POST | `{prompt:string}` | `{ok:bool, output?:string, error?:string}` |
| `/message/stream` | POST | `{prompt:string}` | SSE `event:agent` + JSON data |
| `/health` | GET | - | `{ok:true}` |
| `/status` | GET | - | LLM 列表 |
| `/stop` | POST | - | 停止当前任务 |

---

## 四、关键技术决策

### 为什么自实现 Markdown 渲染而非使用库？
- 无外部依赖，文件自包含
- 精确控制渲染样式(矩阵绿主题)
- 避免 npm/webpack 等构建工具
- 轻量：~80 行 JS，覆盖 90% 常用语法

### 为什么 SSE 而非 WebSocket？
- 后端已有 axum SSE 支持
- SSE 天然兼容 HTTP/2，无需额外握手
- 一请求一响应模式更简单可靠
- 浏览器原生 EventSource 支持(但此处用 fetch stream 支持 POST)

### 为什么用 `include_str!` 而非独立文件服务？
- 零部署成本，HTML 随二进制编译
- 避免额外文件读取和路径问题
- 适合单个页面的场景

---

## 五、开发指南

### 修改 chat.html
```bash
# 编辑资源文件
vim crates/koda-agent-frontends/resources/chat.html

# 验证编译
cargo check -p koda-agent-frontends

# 运行测试
cargo run -p koda-agent -- http
# 访问 http://localhost:8787
```

### 添加新的 Markdown 语法
在 `renderMarkdown()` 函数中按顺序添加：
1. 在 HTML 转义之后、行内代码之前添加全局替换
2. 添加对应的 CSS 样式到 Markdown 区块
3. 验证流式渲染效果

### 添加新的 SSE 事件处理
在 SSE 解析循环的 `updateType` 判断分支中添加：
1. 事件类型匹配
2. DOM 操作(插入/更新元素)
3. 在 `addToolCard` 或 `updateToolCard` 中注册状态管理

---

## 六、未来路线图 (Roadmap)

> ⚠️ **上版 Roadmap 已废弃** — 此版为 v2.7 代码审计后重排。
> 核心变化：↑↓ 输入历史、欢迎页优化、工具卡片布局修复升为 P0；流式闪烁难度调高为 6h+；新增 Ctrl+L 清屏、长对话性能预警。

### P0 — 高频痛点 · 低风险 · 纯前端

| 优先级 | 项目 | 说明 | 预估工作量 | 风险 |
|:------:|------|------|:--------:|:----:|
| 🔴 | ✅ **↑↓ 输入历史导航** | 命令行肌肉记忆，按 ↑ 回填上一条 prompt；纯前端环形缓冲区 | 0.5h | 🟢 |
| 🔴 | ✅ **欢迎页只显示一次** | `localStorage` 标记已读，clear 后不再弹出 terminal-window 大块内容；保留能力卡片可手动唤起 | 0.5h | 🟢 |
| 🔴 | ✅ **工具卡片 DOM 独立化** | 工具卡片从 `aiMsgDiv` 内移出，改为独立的消息级 `.tool-cards` 容器，避免 AI 文本与工具卡片 DOM 错乱 | 1.5h | 🟢 |

### P1 — 快速见效 · 交互优化

| 优先级 | 项目 | 说明 | 预估工作量 | 风险 |
|:------:|------|------|:--------:|:----:|
| 🟡 | ✅ **Ctrl+L 清屏** | 终端用户直觉快捷键，替代手工点 clear/输入 clear 命令 | 0.3h | 🟢 |
| 🟡 | ✅ **代码块复制按钮** | hover 代码块时右上角显示「📋 复制」按钮，`navigator.clipboard.writeText()` + 降级 fallback | 1h | 🟢 |
| 🟡 | ✅ **工具调用耗时** | `performance.now()` 记录起止差，状态栏追加 `(230ms)` / `(1.23s)` 耗时标签 | 1h | 🟢 |
| 🟡 | ✅ **停止响应按钮** | UI 按钮触发 `abortController.abort()` + 调用 `POST /stop` 确保后端也停 | 2h | 🟡 |
| 🟡 | ✅ **消息时间戳分组** | 连续 5min 内的消息合并显示一个时间标签，减少视觉噪音 | 1h | 🟢 |

### P2 — 功能增强

| 优先级 | 项目 | 说明 | 预估工作量 | 风险 |
|:------:|------|------|:--------:|:----:|
| 🔵 | ✅ **输入框多行支持** | `<input>` → `<textarea>`，Shift+Enter 换行 / Enter 发送（可配置） | 0.5h | 🟢 |
| 🔵 | ✅ **对话历史持久化** | `localStorage` 存储消息 DOM 快照，刷新页面不丢失（`persistMessages`/`restoreMessages`） | 1h | 🟢 |
| 🔵 | ✅ **SSE 断线重连** | fetch 失败后指数退避重连（1s→2s→4s→8s→max 30s），用 `fullText` 缓存恢复现场 | 3h | 🟡 |
| 🔵 | ✅ **Enter/Ctrl+Enter 切换** | 设置项：Enter 发送 / Ctrl+Enter 发送 | 1h | 🟢 |

### P3 — 架构级改造

| 优先级 | 项目 | 说明 | 预估工作量 | 风险 |
|:------:|------|------|:--------:|:----:|
| 🟢 | **流式 Markdown 增量渲染** | 当前每次 chunk 全量 `innerHTML` 替换 → 改为 append-only 增量渲染，保留 DOM 状态 | 6h+ | 🔴 |
| 🟢 | **长对话虚拟滚动** | 消息 >200 条时启用 IntersectionObserver 虚拟滚动，避免 DOM 节点爆炸 | 5h | 🔴 |
| 🟢 | ✅ **~~删除线~~ Markdown 支持** | `~~text~~` → `<del>` | 0.5h | 🟢 |
| 🟢 | ✅ **工具参数 JSON 语法高亮** | JSON key 绿色、string 黄色、number 青色 | 1h | 🟢 |
| 🟢 | ✅ **代码块语法高亮** | 代码块按语言着色(js/python/rust/shell)，替代纯绿字 | 4h | 🟡 |

### P4 — 长线规划

| 优先级 | 项目 | 说明 | 预估工作量 |
|:------:|------|------|:--------:|
| ⚪ | **图片/文件拖拽上传** | 拖拽到输入区，自动 base64 嵌入发送 | 3h |
| ⚪ | ✅ **对话导出** | 导出 Markdown / JSON / TXT（⤓ 按钮 + 下拉面板 + 三种格式下载） | 2h |
| ⚪ | **多会话 Tab** | 左侧边栏多会话管理 | 5h |
| ⚪ | **消息搜索** | Ctrl+F 搜索高亮 | 2h |
| ⚪ | **消息编辑/删除** | 右键菜单编辑/删除单条 | 2h |
| ⚪ | **移动端手势优化** | 滑动返回、长按快捷命令 | 2h |
| ⚪ | **WebSocket 回退** | SSE 不可用时降级 | 3h |
| ⚪ | **Service Worker 缓存** | 离线缓存页面 | 3h |
| ⚪ | **i18n 多语言** | 英文/日文界面 | 4h |
| ⚪ | **E2E 加密** | 端到端加密 | 8h |

---

## 七、设计理念与视觉系统

### 7.1 核心设计哲学

```
「物理级全能执行者」—— 不推诿，不空想。
```

每一个 UI 元素都在讲述「终端执行者」的身份故事。设计决策围绕三条主线：

**① 终端即身份 (Terminal as Identity)**
- 等宽字体 `Share Tech Mono` — 每一行文字都是终端输出
- Orbitron 标题 — 科幻感、未来感、控制台气质
- `⫸ $` 命令提示符 — 所有操作都是「在执行命令」
- 无圆角、无毛玻璃、无拟物 — 克制、精确、纯粹

**② 透明即信任 (Transparency as Trust)**
- tool_call 实时显示 — 用户看到 Agent 在「做什么」
- 参数可展开审查 — 不隐藏细节，不黑箱操作
- 错误红色高亮 — 不美化失败，如实反馈
- 流式逐字输出 — 每一 token 都可被追溯

**③ 氛围即功能 (Atmosphere as Function)**
- Matrix 雨 → 代表「系统活跃、数据流动」
- Glitch 干扰 → 象征「数字世界的边界」
- CRT 扫描线 → 唤起「老旧但可靠的硬件感」
- 入侵检测 → 暗示「安全边界被触及」

### 7.2 视觉系统规范

```
🎨 调色板
  ┌────────────────────────────────────────────┐
  │ 背景    #0a0a0a  ████  深空黑               │
  │ 主色    #00ff41  ████  霓虹绿 (执行/活跃)    │
  │ 辅色    #00cc33  ████  矩阵绿 (次要/静止)    │
  │ 强调    #00d4ff  ████  对话蓝 (用户消息)     │
  │ 警告    #ff0040  ████  血红色 (错误/入侵)    │
  │ 文字    #c8e6c8  ████  柔和绿 (AI回复)       │
  │ 高亮    #ffdd00  ████  高亮黄 (代码/数值)    │
  └────────────────────────────────────────────┘

🔤 字体层级
  ┌───────────┬──────────────┬──────────────────┐
  │ 用途      │ 字体          │ 特征              │
  ├───────────┼──────────────┼──────────────────┤
  │ 标题      │ Orbitron 900 │ 大写、字距大、未来感 │
  │ 界面      │ Share Tech   │ 等宽、清晰、终端感  │
  │ 正文      │ Share Tech   │ 统一性优先         │
  └───────────┴──────────────┴──────────────────┘

✨ 特效规范
  ┌──────────────┬─────────────────────────────────────┐
  │ 特效          │ 实现                                 │
  ├──────────────┼─────────────────────────────────────┤
  │ Matrix 雨    │ Canvas, opacity 0.25, interval 55ms  │
  │ 扫描线        │ repeating-linear-gradient 2px 间隔   │
  │ Glitch 标题   │ ::before 偏移 #ff00c1, ::after #00fff7 │
  │ 入侵抖动      │ translate(dx,dy) + RGB 通道分离      │
  │ 鼠标轨迹      │ arc() + shadowBlur 15px 衰减粒子     │
  │ 流式光标      │ blink-cursor 0.8s step-end           │
  └──────────────┴─────────────────────────────────────┘
```

### 7.3 设计决策记录 (ADR)

| 决策 | 选择 | 放弃 | 理由 |
|------|------|------|------|
| UI 框架 | 原生 DOM | React/Vue/Svelte | 单页、零构建、38KB 全包 |
| Markdown | 自实现 80 行 | marked.js | 无依赖、精确控制配色 |
| 通信 | SSE (fetch stream) | WebSocket | HTTP/2 兼容、无需握手 |
| 打包 | `include_str!` | 静态文件服务 | 零部署成本、编译即打包 |
| 响应式 | CSS media query | Tailwind | 轻量、无需工具链 |
| 图标 | Unicode/Emoji | Font Awesome | 零请求、零依赖 |

### 7.4 关键追问

**Q: 为什么不做成更像 ChatGPT 那样的 UI？**
因为这不是 ChatGPT。这是「物理级全能执行者」——一个命令行工具的人格化界面。它应该看起来像一个**有态度的终端**，而不是一个**友好的聊天框**。

**Q: 为什么不把 tool_call 完全隐藏，只给用户看最终结果？**
因为信任来自于透明。你看到 Agent 在执行 `code_run`、`web_scan`，你就知道它在「做事」，不是在「编故事」。这符合「不推诿，不空想」的承诺。

**Q: 为什么 Matrix 雨不做得更明显？**
因为它只是背景氛围，不是主体。opacity 0.25 是经过反复调试的值——太低没存在感，太高干扰阅读。氛围是**衬托**，**不是**主角。

**Q: 为什么 AI 回复不用纯白色提高可读性？**
因为 `#c8e6c8` 柔和绿比纯白更符合矩阵主题，同时兼顾可读性。白色 `#ffffff` 在暗色背景下反而刺眼，尤其长时间阅读。主题一致性优先于「最大化对比度」。
