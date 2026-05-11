use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};
use unicode_width::UnicodeWidthChar;

use super::markdown::render_markdown_lines;
use super::state::{
    AppLayout, FocusPane, LayoutMode, Overlay, SessionStatus, TimelineItem, TimelineRenderCache,
    TimelineSignature, ToolDetail, TuiAppState, TuiSessionState,
};
use super::tool_cards::{render_tool_call_card, render_tool_result_card};

fn compute_layout(area: Rect) -> AppLayout {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(7),
            Constraint::Length(1),
        ])
        .split(area);

    let mode = layout_mode(area);
    let (sidebar, timeline, inspector) = match mode {
        LayoutMode::Wide => {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(28),
                    Constraint::Min(40),
                    Constraint::Length(34),
                ])
                .split(vertical[1]);
            (Some(chunks[0]), chunks[1], Some(chunks[2]))
        }
        LayoutMode::Medium => {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(26), Constraint::Min(32)])
                .split(vertical[1]);
            (Some(chunks[0]), chunks[1], None)
        }
        LayoutMode::Narrow => (None, vertical[1], None),
    };

    AppLayout {
        header: vertical[0],
        sidebar,
        timeline,
        inspector,
        composer: vertical[2],
        status: vertical[3],
    }
}

fn layout_mode(area: Rect) -> LayoutMode {
    match area.width {
        0..=79 => LayoutMode::Narrow,
        80..=119 => LayoutMode::Medium,
        _ => LayoutMode::Wide,
    }
}

pub(super) fn render_app(frame: &mut Frame<'_>, state: &mut TuiAppState) {
    state.layout_mode = layout_mode(frame.area());
    let layout = compute_layout(frame.area());
    state.last_layout = Some(layout);
    reconcile_active_timeline_scroll(state);
    render_header(frame, layout.header, state);
    if let Some(sidebar) = layout.sidebar {
        render_sessions(frame, sidebar, state);
    }
    render_timeline(frame, layout.timeline, state);
    if let Some(inspector) = layout.inspector {
        render_inspector(frame, inspector, state);
    }
    render_composer(frame, layout.composer, state);
    render_status(frame, layout.status, state);
    render_overlay(frame, state);
}

fn reconcile_active_timeline_scroll(state: &mut TuiAppState) {
    let viewport = timeline_viewport_lines(state);
    let width = timeline_content_width(state);
    if let Some(session) = state.active_session_mut() {
        let max_scroll = max_timeline_scroll_for_width(session, viewport, width);
        if session.timeline_follow_tail {
            session.timeline_scroll = max_scroll;
            session.timeline_unseen = 0;
        } else {
            session.timeline_scroll = session.timeline_scroll.min(max_scroll);
        }
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &TuiAppState) {
    let title = Line::from(vec![
        Span::styled(
            " Koda Agent ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  full-screen preview  "),
        Span::styled("模型: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            trim_middle(&state.model_label, 30),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::styled("目录: ", Style::default().fg(Color::DarkGray)),
        Span::raw(trim_middle(&state.root_label, 48)),
    ]);
    frame.render_widget(
        Paragraph::new(title)
            .block(Block::default().borders(Borders::ALL))
            .alignment(Alignment::Left),
        area,
    );
}

fn render_sessions(frame: &mut Frame<'_>, area: Rect, state: &TuiAppState) {
    let items = state
        .sessions
        .values()
        .map(|session| {
            let marker = if session.id == state.active { ">" } else { " " };
            let unread = if session.unread_events > 0 {
                format!(" +{}", session.unread_events)
            } else {
                String::new()
            };
            let activity = if matches!(session.status, SessionStatus::Running) {
                format!(" t{}", session.active_turn.unwrap_or_default())
            } else {
                unread
            };
            let line = Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::LightGreen)),
                Span::raw(format!(" #{} ", session.id)),
                Span::styled(
                    trim_chars(&session.name, 12),
                    Style::default().fg(Color::White),
                ),
                Span::raw(" "),
                Span::styled(session.status.label(), session.status.style()),
                Span::styled(activity, Style::default().fg(Color::LightYellow)),
            ]);
            ListItem::new(line)
        })
        .collect::<Vec<_>>();
    let border = focused_block("会话 Sessions", state.focus == FocusPane::Sessions);
    frame.render_widget(List::new(items).block(border), area);
}

fn render_timeline(frame: &mut Frame<'_>, area: Rect, state: &mut TuiAppState) {
    let viewport = area.height.saturating_sub(2) as usize;
    let width = content_width_for_area(area);
    let focus = state.focus == FocusPane::Timeline;
    let (visible_lines, content_len, scroll) = if let Some(session) = state.active_session_mut() {
        let scroll = session.timeline_scroll;
        let wrapped_lines = ensure_timeline_cache(session, width);
        let start = usize::from(scroll).min(wrapped_lines.len().saturating_sub(viewport));
        (
            wrapped_lines
                .iter()
                .skip(start)
                .take(viewport)
                .cloned()
                .collect::<Vec<_>>(),
            wrapped_lines.len(),
            scroll,
        )
    } else {
        (vec![Line::raw("")], 1, 0)
    };
    frame.render_widget(
        Paragraph::new(visible_lines)
            .block(focused_block("时间线 Timeline", focus))
            .wrap(Wrap { trim: false }),
        area,
    );
    render_vertical_scrollbar(frame, area, content_len, scroll);
}

pub(super) fn timeline_viewport_lines(state: &TuiAppState) -> usize {
    state
        .last_layout
        .map(|layout| layout.timeline.height.saturating_sub(2) as usize)
        .unwrap_or(10)
        .max(1)
}

pub(super) fn timeline_content_width(state: &TuiAppState) -> usize {
    state
        .last_layout
        .map(|layout| content_width_for_area(layout.timeline))
        .unwrap_or(80)
        .max(1)
}

pub(super) fn max_timeline_scroll_for_width(
    session: &mut TuiSessionState,
    viewport: usize,
    width: usize,
) -> u16 {
    timeline_line_count_for_width(session, width)
        .saturating_sub(viewport)
        .min(u16::MAX as usize) as u16
}

fn timeline_line_count_for_width(session: &mut TuiSessionState, width: usize) -> usize {
    ensure_timeline_cache(session, width).len()
}

fn ensure_timeline_cache(session: &mut TuiSessionState, width: usize) -> &[Line<'static>] {
    let width = width.max(1);
    let signature = timeline_signature(session);
    let cache_valid = session.timeline_cache.as_ref().is_some_and(|cache| {
        cache.revision == session.timeline_revision
            && cache.width == width
            && cache.fold == session.fold
            && cache.signature == signature
    });
    if !cache_valid {
        let lines = wrap_lines(timeline_lines(session), width);
        session.timeline_cache = Some(TimelineRenderCache {
            revision: session.timeline_revision,
            width,
            fold: session.fold,
            signature,
            lines,
        });
    }
    session
        .timeline_cache
        .as_ref()
        .map(|cache| cache.lines.as_slice())
        .unwrap_or(&[])
}

fn timeline_signature(session: &TuiSessionState) -> TimelineSignature {
    let text_len = session
        .timeline
        .iter()
        .map(|item| match item {
            TimelineItem::User(text)
            | TimelineItem::Assistant(text)
            | TimelineItem::Thinking(text)
            | TimelineItem::System(text)
            | TimelineItem::Error(text) => text.len(),
            TimelineItem::ToolCall { name, args } => name.len() + args.len(),
            TimelineItem::ToolResult { name, args, data } => name.len() + args.len() + data.len(),
        })
        .sum();
    TimelineSignature {
        items: session.timeline.len(),
        text_len,
    }
}

fn timeline_lines(session: &TuiSessionState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for item in &session.timeline {
        match item {
            TimelineItem::User(text) => {
                lines.push(Line::from(vec![
                    Span::styled("● ", Style::default().fg(Color::LightYellow)),
                    Span::styled("用户 You", Style::default().fg(Color::LightYellow)),
                ]));
                lines.extend(render_markdown_lines(text, Color::White));
            }
            TimelineItem::Assistant(text) => {
                lines.push(Line::from(vec![
                    Span::styled("◆ ", Style::default().fg(Color::LightGreen)),
                    Span::styled(
                        "助手 Assistant",
                        Style::default()
                            .fg(Color::LightGreen)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.extend(render_markdown_lines(text, Color::White));
            }
            TimelineItem::Thinking(text) => {
                lines.push(Line::from(vec![
                    Span::styled("◇ ", Style::default().fg(Color::Cyan)),
                    Span::styled("思考 Thinking", Style::default().fg(Color::Cyan)),
                ]));
                lines.extend(render_markdown_lines(text, Color::DarkGray));
            }
            TimelineItem::ToolCall { name, args } => {
                lines.extend(render_tool_call_card(name, args));
            }
            TimelineItem::ToolResult { name, args, data } => {
                lines.extend(render_tool_result_card(name, args, data, session.fold));
            }
            TimelineItem::System(text) => {
                lines.push(Line::from(vec![
                    Span::styled("• ", Style::default().fg(Color::DarkGray)),
                    Span::styled(text.clone(), Style::default().fg(Color::DarkGray)),
                ]));
            }
            TimelineItem::Error(text) => {
                lines.push(Line::from(vec![
                    Span::styled("✖ ", Style::default().fg(Color::Red)),
                    Span::styled("错误 Error", Style::default().fg(Color::Red)),
                ]));
                lines.extend(render_markdown_lines(text, Color::LightRed));
            }
        }
        lines.push(Line::raw(""));
    }
    lines
}

fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for line in lines {
        let mut current = Vec::<Span<'static>>::new();
        let mut current_width = 0usize;
        for span in line.spans {
            let style = span.style;
            let mut chunk = String::new();
            for ch in span.content.chars() {
                let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                if current_width > 0 && current_width.saturating_add(ch_width) > width {
                    if !chunk.is_empty() {
                        current.push(Span::styled(std::mem::take(&mut chunk), style));
                    }
                    wrapped.push(Line::from(std::mem::take(&mut current)));
                    current_width = 0;
                }
                chunk.push(ch);
                current_width = current_width.saturating_add(ch_width);
            }
            if !chunk.is_empty() {
                current.push(Span::styled(chunk, style));
            }
        }
        wrapped.push(Line::from(current));
    }
    if wrapped.is_empty() {
        wrapped.push(Line::raw(""));
    }
    wrapped
}

fn content_width_for_area(area: Rect) -> usize {
    area.width.saturating_sub(3).max(1) as usize
}

fn render_inspector(frame: &mut Frame<'_>, area: Rect, state: &TuiAppState) {
    let lines = inspector_lines(state);
    frame.render_widget(
        Paragraph::new(lines)
            .block(focused_block(
                "检查器 Inspector",
                state.focus == FocusPane::Inspector,
            ))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn inspector_lines(state: &TuiAppState) -> Vec<Line<'static>> {
    let active = state.active_session();
    let mut lines = vec![
        Line::styled("运行时 Runtime", Style::default().fg(Color::LightBlue)),
        Line::from(vec![
            Span::raw("状态 "),
            Span::styled(
                active.map(|s| s.status.label()).unwrap_or("missing"),
                active
                    .map(|s| s.status.style())
                    .unwrap_or_else(|| Style::default().fg(Color::DarkGray)),
            ),
            Span::raw(format!(
                "  Turn {}",
                active
                    .and_then(|s| s.active_turn)
                    .map(|turn| turn.to_string())
                    .unwrap_or_else(|| "-".into())
            )),
        ]),
        Line::raw(format!(
            "会话 #{}  模型 {}",
            state.active,
            trim_chars(&state.model_label, 20)
        )),
        Line::raw(format!(
            "API {}  stream:{}  mouse:{}",
            state.api_mode,
            if state.stream_enabled { "on" } else { "off" },
            if state.mouse_capture { "on" } else { "off" }
        )),
        Line::raw(format!(
            "渲染 {}  Thinking {}",
            active.map(|s| s.stream_state.label()).unwrap_or("idle"),
            active
                .map(|s| s.thinking_state.label())
                .unwrap_or("unavailable")
        )),
        Line::raw(format!(
            "SSE c:{} t:{} u:{} last:{}",
            active
                .map(|s| s.stream_metrics.content_chunks)
                .unwrap_or_default(),
            active
                .map(|s| s.stream_metrics.thinking_chunks)
                .unwrap_or_default(),
            active
                .map(|s| s.stream_metrics.usage_chunks)
                .unwrap_or_default(),
            active
                .and_then(|s| s.stream_metrics.last_delta_tick)
                .map(|tick| format!("{}t", state.tick.saturating_sub(tick)))
                .unwrap_or_else(|| "-".into())
        )),
        Line::raw(""),
        Line::styled("Token Usage", Style::default().fg(Color::LightBlue)),
    ];
    lines.extend(usage_lines(active.map(|s| &s.usage)));
    lines.extend([
        Line::raw(""),
        Line::styled("会话概览 Session", Style::default().fg(Color::LightBlue)),
    ]);
    lines.extend(session_lines(active));
    lines.push(Line::raw(""));
    if let Some(tool) = active.and_then(|s| s.last_tool.as_ref()) {
        lines.extend(tool_lines(tool));
        lines.push(Line::raw(""));
    }
    lines.extend([
        Line::styled("提示 Hint", Style::default().fg(Color::LightBlue)),
        Line::raw("F7/Ctrl-M 切换交互/复制模式；复制模式下可直接选中文字"),
    ]);
    lines
}

fn tool_lines(tool: &ToolDetail) -> Vec<Line<'static>> {
    let done = tool.result.is_some();
    let status_style = if done {
        Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    };
    vec![
        Line::styled("工具状态 Tool", Style::default().fg(Color::LightBlue)),
        Line::from(vec![
            Span::styled(if done { "✓ done " } else { "… running " }, status_style),
            Span::styled(tool.name.clone(), Style::default().fg(Color::White)),
            Span::raw(format!("  #{}.{}", tool.turn, tool.index)),
        ]),
        Line::from(vec![
            Span::styled("参数 ", Style::default().fg(Color::DarkGray)),
            Span::raw(tool_arg_summary(&tool.name, &tool.args)),
        ]),
        Line::from(vec![
            Span::styled("结果 ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                tool.result
                    .as_deref()
                    .map(tool_result_summary)
                    .unwrap_or_else(|| "等待工具返回...".into()),
                if done {
                    Style::default().fg(Color::LightGreen)
                } else {
                    Style::default().fg(Color::LightYellow)
                },
            ),
        ]),
    ]
}

fn tool_arg_summary(name: &str, args: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(args) else {
        return trim_chars(args, 46);
    };
    let key = match name {
        "file_read" | "file_write" | "file_patch" => ["path", "file_path", "filename"]
            .into_iter()
            .find_map(|k| value.get(k).and_then(serde_json::Value::as_str)),
        "code_run" => ["language", "cmd", "command"]
            .into_iter()
            .find_map(|k| value.get(k).and_then(serde_json::Value::as_str)),
        "web_scan" | "web_execute_js" => ["url", "tab", "selector"]
            .into_iter()
            .find_map(|k| value.get(k).and_then(serde_json::Value::as_str)),
        _ => None,
    };
    key.map(|s| trim_chars(s, 46))
        .unwrap_or_else(|| trim_chars(&compact_json_value(&value), 46))
}

fn tool_result_summary(result: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(result) else {
        return trim_chars(result, 46);
    };
    ["summary", "message", "title", "path", "error"]
        .into_iter()
        .find_map(|k| value.get(k).and_then(serde_json::Value::as_str))
        .map(|s| trim_chars(s, 46))
        .unwrap_or_else(|| trim_chars(&compact_json_value(&value), 46))
}

fn compact_json_value(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn usage_lines(usage: Option<&super::state::UsageStats>) -> Vec<Line<'static>> {
    let Some(stats) = usage else {
        return vec![Line::styled(
            "暂无 usage",
            Style::default().fg(Color::DarkGray),
        )];
    };
    let Some(last) = stats.current_turn.as_ref() else {
        let label = if stats.unavailable {
            "本轮 usage 不可用（供应商未返回）"
        } else {
            "本轮 usage 待返回..."
        };
        return vec![
            Line::styled(label, Style::default().fg(Color::DarkGray)),
            usage_metric_line(
                "会话",
                stats.session_input_tokens,
                stats.session_output_tokens,
                stats.session_total_tokens,
                stats.session_cached_tokens,
            ),
        ];
    };
    let input = last.input_tokens.unwrap_or_default();
    let output = last.output_tokens.unwrap_or_default();
    let total = last
        .total_tokens
        .unwrap_or_else(|| input.saturating_add(output));
    let cached = last.cached_tokens.unwrap_or_default();
    vec![
        usage_metric_line("本轮", input, output, total, cached),
        usage_metric_line(
            "会话",
            stats.session_input_tokens,
            stats.session_output_tokens,
            stats.session_total_tokens,
            stats.session_cached_tokens,
        ),
    ]
}

fn usage_metric_line(
    label: &'static str,
    input: u64,
    output: u64,
    total: u64,
    cached: u64,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label} "), Style::default().fg(Color::DarkGray)),
        Span::styled("in ", Style::default().fg(Color::DarkGray)),
        Span::styled(format_compact_u64(input), Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled("out ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format_compact_u64(output),
            Style::default().fg(Color::LightGreen),
        ),
        Span::raw("  "),
        Span::styled("total ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format_compact_u64(total),
            Style::default().fg(Color::LightYellow),
        ),
        Span::raw("  "),
        Span::styled("cached ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format_compact_u64(cached),
            Style::default().fg(Color::LightBlue),
        ),
    ])
}

fn session_lines(active: Option<&TuiSessionState>) -> Vec<Line<'static>> {
    let Some(session) = active else {
        return vec![Line::styled(
            "无活动会话",
            Style::default().fg(Color::DarkGray),
        )];
    };
    let follow_style = if session.timeline_follow_tail {
        Style::default().fg(Color::LightGreen)
    } else {
        Style::default().fg(Color::LightYellow)
    };
    vec![
        Line::from(vec![
            Span::styled("消息 ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                session.timeline.len().to_string(),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("未读 ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                session.unread_events.to_string(),
                Style::default().fg(Color::LightYellow),
            ),
            Span::raw("  "),
            Span::styled("折叠 ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if session.fold { "on" } else { "off" },
                Style::default().fg(Color::LightBlue),
            ),
        ]),
        Line::from(vec![
            Span::styled("视图 ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if session.timeline_follow_tail {
                    "跟随最新"
                } else {
                    "查看历史"
                },
                follow_style,
            ),
            Span::raw("  "),
            Span::styled("scroll ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                session.timeline_scroll.to_string(),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("unseen ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                session.timeline_unseen.to_string(),
                Style::default().fg(Color::LightYellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("任务 ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "成功 {} / 失败 {}",
                    session.completed_tasks, session.failed_tasks
                ),
                if session.failed_tasks > 0 {
                    Style::default().fg(Color::LightRed)
                } else {
                    Style::default().fg(Color::LightGreen)
                },
            ),
        ]),
        Line::raw(format!(
            "最近: {}",
            session
                .last_notice
                .as_deref()
                .map(|s| trim_chars(s, 28))
                .or(session.last_error.as_deref().map(|s| trim_chars(s, 28)))
                .unwrap_or_else(|| "-".into())
        )),
    ]
}

fn format_compact_u64(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 10_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, state: &TuiAppState) {
    let mut lines = Vec::new();
    let width = area.width.saturating_sub(4) as usize;
    if state.composer.is_empty() {
        lines.push(Line::styled(
            "Ask Koda Agent... (Enter submit, Ctrl-J newline)",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        let composer_lines = state.composer.lines().collect::<Vec<_>>();
        let start = composer_lines.len().saturating_sub(3);
        for line in &composer_lines[start..] {
            lines.push(Line::raw(trim_chars(line, width)));
        }
    }
    lines.push(Line::styled(
        "Ctrl-P commands | ? help | Ctrl-N new | Ctrl-B branch | Ctrl-W close | Ctrl-L clear",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(focused_block(
                "输入 Composer",
                state.focus == FocusPane::Composer,
            ))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_overlay(frame: &mut Frame<'_>, state: &TuiAppState) {
    let (title, lines) = match state.overlay {
        Overlay::None => return,
        Overlay::Help => ("Help", help_lines()),
        Overlay::Commands => ("Command Palette", command_palette_lines()),
    };
    let area = centered_rect(72, 60, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightGreen)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

fn help_lines() -> Vec<Line<'static>> {
    vec![
        Line::styled("导航 Navigation", Style::default().fg(Color::LightBlue)),
        Line::raw("Tab focus | j/k or Up/Down switch sessions when composer is empty"),
        Line::raw("PageUp/PageDown scroll timeline | Home reset timeline scroll"),
        Line::raw("F7/Ctrl-M toggle interactive/copy mode; copy mode allows text selection"),
        Line::raw(""),
        Line::styled("输入 Composer", Style::default().fg(Color::LightBlue)),
        Line::raw("Enter submit | Ctrl-J newline | Backspace delete | Ctrl-S stop"),
        Line::raw("Ctrl-P/ F2 command palette | ?/ F1 help | Esc close overlay / quit"),
        Line::raw(""),
        Line::styled("会话 Sessions", Style::default().fg(Color::LightBlue)),
        Line::raw("Ctrl-N/F3 new | Ctrl-B/F4 branch | Ctrl-W/F6 close | Ctrl-L/F5 clear"),
        Line::raw("/branch [name] | /switch <id|name> | /rename <name> | /sessions"),
        Line::raw(""),
        Line::styled("macOS Notes", Style::default().fg(Color::LightBlue)),
        Line::raw(
            "Command key is usually intercepted by Terminal/iTerm/Warp before TUI apps see it.",
        ),
        Line::raw("Use Ctrl (shown as ^ in macOS menus) or the F-key alternatives above."),
        Line::raw(""),
        Line::styled(
            "Runtime Slash Commands",
            Style::default().fg(Color::LightBlue),
        ),
        Line::raw("/status | /llm <n> | /llms | /continue | /btw <question> pass through"),
    ]
}

fn command_palette_lines() -> Vec<Line<'static>> {
    vec![
        Line::styled(
            "Press a number to insert a command template. Esc closes.",
            Style::default().fg(Color::LightBlue),
        ),
        Line::raw("1  /branch <name>"),
        Line::raw("2  /switch <id|name>"),
        Line::raw("3  /rename <name>"),
        Line::raw("4  /sessions"),
        Line::raw("5  /clear"),
        Line::raw("6  /close"),
        Line::raw("7  /status"),
        Line::raw("8  /llms"),
        Line::raw("9  /continue"),
    ]
}

fn render_status(frame: &mut Frame<'_>, area: Rect, state: &TuiAppState) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ", Style::default().bg(Color::DarkGray)),
            Span::raw(&state.status),
            Span::raw(format!(
                " | {:?} | stream:{} | mouse:{}",
                state.layout_mode,
                if state.stream_enabled { "on" } else { "off" },
                if state.mouse_capture { "on" } else { "off" }
            )),
        ])),
        area,
    );
}

fn focused_block(title: &'static str, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(Color::LightGreen)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(style)
}

fn render_vertical_scrollbar(frame: &mut Frame<'_>, area: Rect, content_len: usize, position: u16) {
    let viewport = area.height.saturating_sub(2) as usize;
    if content_len <= viewport || viewport == 0 {
        return;
    }
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("▲"))
        .thumb_symbol("█")
        .track_symbol(Some("│"))
        .end_symbol(Some("▼"))
        .thumb_style(Style::default().fg(Color::LightGreen))
        .track_style(Style::default().fg(Color::DarkGray));
    let mut state = ScrollbarState::new(content_len)
        .position(position as usize)
        .viewport_content_length(viewport);
    frame.render_stateful_widget(
        scrollbar,
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut state,
    );
}

pub(super) fn summarize_tool_result(text: &str) -> String {
    let compact = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    trim_chars(if compact.is_empty() { text } else { &compact }, 88)
}

pub(super) fn trim_chars(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        text.to_string()
    } else {
        format!(
            "{}...",
            text.chars().take(max.saturating_sub(3)).collect::<String>()
        )
    }
}

fn trim_middle(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_string();
    }
    if max <= 3 {
        return ".".repeat(max);
    }
    let head = (max - 3) / 2;
    let tail = max - 3 - head;
    let start = text.chars().take(head).collect::<String>();
    let end = text
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{start}...{end}")
}
