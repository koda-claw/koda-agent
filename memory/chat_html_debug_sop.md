# chat.html 前端调试 SOP

## 工具卡片 DOM 结构（经 file_read 验证）

修改 `addToolCard()` / `updateToolCard()` 时，DOM 类名必须与 CSS 精确匹配：

```
div.tool-exec[data-tool-name="..."]  ← 容器，accent 色用 CSS 变量 --tool-accent
  div.tool-exec-line                  ← header 行（flex 布局）
    span.tool-icon                    ← emoji 图标
    span.tool-name                    ← 友好名称
    span.tool-args-inline             ← 参数摘要
    span.tool-args-toggle             ← 展开/折叠按钮
  div.tool-exec-status.running/.done  ← 状态行
    span.tool-exec-summary            ← 结果摘要（动态更新）
```

⚠️ **坑点**：CSS 用 `.tool-exec-line` 而非 `.tool-header`，用 `.tool-exec-status` 而非 `.tool-status`。注入 DOM 做测试时**必须先读 CSS 确认类名**，否则样式不生效且难以排查。

## 浏览器验证方法

- **file:// 协议缓存**：修改文件后浏览器不会自动刷新，必须 CDP `Page.reload(ignoreCache=true)`
- **截图不可用时**：用 `getComputedStyle(el)` 检查关键 CSS 属性（display、border-left、background）验证规则是否生效
- **注入测试卡片**：可直接调用 `addToolCard(data)` 和 `updateToolCard(data, result)` 注入模拟数据

## 关键函数

- `addToolCard(data)` — 创建卡片 DOM
- `updateToolCard(data, result)` — 更新状态和结果
- `formatToolArgs(data)` / `formatToolArgsSmart(data)` — 参数格式化
- `toggleToolArgs(el)` — 展开/折叠
- `TOOL_CONFIG` — 9 工具配置表（icon、name、color、formatArgs）
