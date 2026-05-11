use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use super::render::{summarize_tool_result, trim_chars};

pub(super) fn render_tool_call_card(name: &str, args: &str) -> Vec<Line<'static>> {
    let parsed = parse_json(args);
    let icon = tool_icon(name);
    let mut lines = vec![Line::from(vec![
        Span::styled("▣ 工具 ", Style::default().fg(Color::Magenta)),
        Span::styled(name.to_string(), Style::default().fg(Color::LightMagenta)),
        Span::styled(
            format!("  {}", tool_title(name)),
            Style::default().fg(Color::DarkGray),
        ),
    ])];
    let detail = match name {
        "code_run" => {
            let kind = json_str(&parsed, "type").unwrap_or("python");
            let timeout = json_i64(&parsed, "timeout")
                .map(|n| format!("{n}s"))
                .unwrap_or_else(|| "默认超时".into());
            format!(
                "{icon} 运行 {kind} | {} | cwd={}",
                timeout,
                json_str(&parsed, "cwd").unwrap_or("工作区/默认")
            )
        }
        "file_read" => format!(
            "{icon} 读取 {} | start={} count={} keyword={} linenos={}",
            json_str(&parsed, "path").unwrap_or("<未指定路径>"),
            json_i64(&parsed, "start")
                .map(|n| n.to_string())
                .unwrap_or_else(|| "auto".into()),
            json_i64(&parsed, "count")
                .map(|n| n.to_string())
                .unwrap_or_else(|| "200".into()),
            json_str(&parsed, "keyword").unwrap_or("-"),
            json_bool(&parsed, "show_linenos")
                .map(|b| b.to_string())
                .unwrap_or_else(|| "true".into())
        ),
        "file_patch" => format!(
            "{icon} 修改 {} | old={} | new={}",
            json_str(&parsed, "path").unwrap_or("<未指定路径>"),
            json_preview(&parsed, "old_content", 34).unwrap_or_else(|| "-".into()),
            json_preview(&parsed, "new_content", 34).unwrap_or_else(|| "-".into())
        ),
        "file_write" => format!(
            "{icon} 写入 {} | mode={} | content={}",
            json_str(&parsed, "path").unwrap_or("<默认 index.html>"),
            json_str(&parsed, "mode").unwrap_or("overwrite"),
            json_preview(&parsed, "content", 52).unwrap_or_else(|| "<来自回复代码块>".into())
        ),
        "web_scan" => format!(
            "{icon} 扫描浏览器 | tab={} tabs_only={} text_only={} cutlist={}",
            json_str(&parsed, "switch_tab_id").unwrap_or("当前"),
            json_bool(&parsed, "tabs_only").unwrap_or(false),
            json_bool(&parsed, "text_only").unwrap_or(false),
            json_bool(&parsed, "cutlist").unwrap_or(false)
        ),
        "web_execute_js" => format!(
            "{icon} 执行 JS | tab={} save={} monitor={} | {}",
            json_str(&parsed, "switch_tab_id").unwrap_or("当前"),
            json_str(&parsed, "save_to_file").unwrap_or("-"),
            !json_bool(&parsed, "no_monitor").unwrap_or(false),
            json_preview(&parsed, "script", 58).unwrap_or_else(|| "<来自回复代码块>".into())
        ),
        "ask_user" => format!(
            "{icon} 询问用户 | {} | candidates={}",
            json_preview(&parsed, "question", 70).unwrap_or_else(|| "请提供输入".into()),
            parsed
                .get("candidates")
                .and_then(serde_json::Value::as_array)
                .map(|a| a.len().to_string())
                .unwrap_or_else(|| "0".into())
        ),
        "update_working_checkpoint" => format!(
            "{icon} 更新短期记忆 | key_info={} | sop={}",
            json_preview(&parsed, "key_info", 58).unwrap_or_else(|| "-".into()),
            json_str(&parsed, "related_sop").unwrap_or("-")
        ),
        "start_long_term_update" => format!("{icon} 启动长期记忆沉淀"),
        _ => format!("{icon} 参数 {}", trim_chars(args, 96)),
    };
    lines.push(Line::styled(detail, Style::default().fg(Color::Gray)));
    lines
}

pub(super) fn render_tool_result_card(
    name: &str,
    args: &str,
    data: &str,
    folded: bool,
) -> Vec<Line<'static>> {
    let parsed_args = parse_json(args);
    let parsed = parse_json(data);
    let status = json_str(&parsed, "status")
        .or_else(|| json_str(&parsed, "code"))
        .unwrap_or("result");
    let ok = status.eq_ignore_ascii_case("success")
        || status.eq_ignore_ascii_case("ok")
        || parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(true);
    let marker = if ok { "✓" } else { "▸" };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{marker} 结果 "),
            Style::default().fg(result_color(ok)),
        ),
        Span::styled(name.to_string(), Style::default().fg(Color::LightMagenta)),
        Span::styled(
            format!("  {}", tool_result_summary(name, &parsed, data)),
            Style::default().fg(Color::DarkGray),
        ),
    ])];
    if !folded {
        lines.extend(tool_result_detail_lines(name, &parsed_args, &parsed, data));
    }
    lines
}

fn tool_result_detail_lines(
    name: &str,
    args: &serde_json::Value,
    parsed: &serde_json::Value,
    data: &str,
) -> Vec<Line<'static>> {
    match name {
        "code_run" => code_run_detail_lines(parsed),
        "file_patch" => file_patch_detail_lines(args, parsed),
        "file_write" => file_write_detail_lines(args, parsed),
        "file_read" => file_read_detail_lines(parsed, data),
        "web_scan" | "web_execute_js" => browser_detail_lines(parsed, data),
        _ => generic_detail_lines(parsed, data),
    }
}

fn code_run_detail_lines(parsed: &serde_json::Value) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(exit) = json_i64(parsed, "exit_code") {
        lines.push(detail_line(format!("  exit_code: {exit}")));
    }
    if let Some(stdout) = json_str(parsed, "stdout") {
        lines.extend(section_lines("stdout", stdout, Color::Gray));
    }
    if let Some(stderr) = json_str(parsed, "stderr") {
        lines.extend(section_lines("stderr", stderr, Color::LightRed));
    }
    if lines.is_empty() {
        generic_detail_lines(parsed, &parsed.to_string())
    } else {
        lines
    }
}

fn file_patch_detail_lines(
    args: &serde_json::Value,
    parsed: &serde_json::Value,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(path) = json_str(parsed, "path").or_else(|| json_str(args, "path")) {
        lines.push(detail_line(format!("  path: {path}")));
    }
    if let Some(count) = json_i64(parsed, "replacements").or_else(|| json_i64(parsed, "count")) {
        lines.push(detail_line(format!("  replacements: {count}")));
    }
    let old = json_str(args, "old_content").unwrap_or_default();
    let new = json_str(args, "new_content").unwrap_or_default();
    if !old.is_empty() || !new.is_empty() {
        lines.push(Line::styled(
            "  diff preview:",
            Style::default().fg(Color::DarkGray),
        ));
        if !old.is_empty() {
            lines.push(Line::styled(
                format!("  - {}", trim_chars(&first_non_empty_line(old), 110)),
                Style::default().fg(Color::LightRed),
            ));
        }
        if !new.is_empty() {
            lines.push(Line::styled(
                format!("  + {}", trim_chars(&first_non_empty_line(new), 110)),
                Style::default().fg(Color::LightGreen),
            ));
        }
    }
    lines
}

fn file_write_detail_lines(
    args: &serde_json::Value,
    parsed: &serde_json::Value,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(path) = json_str(parsed, "path").or_else(|| json_str(args, "path")) {
        lines.push(detail_line(format!("  path: {path}")));
    }
    if let Some(bytes) =
        json_i64(parsed, "writed_bytes").or_else(|| json_i64(parsed, "written_bytes"))
    {
        lines.push(detail_line(format!("  bytes: {bytes}")));
    }
    if let Some(content) = json_str(args, "content") {
        lines.extend(section_lines("content preview", content, Color::Gray));
    }
    lines
}

fn file_read_detail_lines(parsed: &serde_json::Value, data: &str) -> Vec<Line<'static>> {
    let content = json_str(parsed, "content")
        .or_else(|| json_str(parsed, "data"))
        .unwrap_or(data);
    section_lines("content", content, Color::Gray)
}

fn browser_detail_lines(parsed: &serde_json::Value, data: &str) -> Vec<Line<'static>> {
    let content = json_str(parsed, "data")
        .or_else(|| json_str(parsed, "text"))
        .or_else(|| json_str(parsed, "html"))
        .unwrap_or(data);
    section_lines("browser output", content, Color::Gray)
}

fn generic_detail_lines(parsed: &serde_json::Value, data: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for key in ["stdout", "stderr", "msg", "error", "path", "exit_code"] {
        if let Some(value) = parsed.get(key) {
            lines.push(detail_line(format!(
                "  {key}: {}",
                trim_chars(&value_to_string(value), 120)
            )));
        }
    }
    if lines.is_empty() {
        lines.push(detail_line(format!("  {}", trim_chars(data, 140))));
    }
    lines
}

fn section_lines(title: &str, text: &str, color: Color) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        format!("  {title}:"),
        Style::default().fg(Color::DarkGray),
    )];
    let mut body = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .peekable();
    if body.peek().is_none() {
        lines.push(Line::styled(
            "    <empty>",
            Style::default().fg(Color::DarkGray),
        ));
        return lines;
    }
    lines.extend(body.take(4).map(|line| {
        Line::styled(
            format!("    {}", trim_chars(line.trim(), 116)),
            Style::default().fg(color),
        )
    }));
    lines
}

fn detail_line(text: String) -> Line<'static> {
    Line::styled(text, Style::default().fg(Color::Gray))
}

fn first_non_empty_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
        .trim()
        .to_string()
}

fn tool_result_summary(name: &str, parsed: &serde_json::Value, data: &str) -> String {
    match name {
        "code_run" => format!(
            "status={} exit={} stdout={} stderr={}",
            json_str(parsed, "status").unwrap_or("-"),
            json_i64(parsed, "exit_code")
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
            json_preview(parsed, "stdout", 42).unwrap_or_else(|| "-".into()),
            json_preview(parsed, "stderr", 32).unwrap_or_else(|| "-".into())
        ),
        "file_read" => json_preview(parsed, "content", 88)
            .or_else(|| json_preview(parsed, "data", 88))
            .unwrap_or_else(|| summarize_tool_result(data)),
        "file_write" | "file_patch" => format!(
            "{} {} bytes={}",
            json_str(parsed, "status").unwrap_or("result"),
            json_str(parsed, "path").unwrap_or(""),
            json_i64(parsed, "writed_bytes")
                .or_else(|| json_i64(parsed, "written_bytes"))
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into())
        ),
        "web_scan" | "web_execute_js" => json_preview(parsed, "data", 88)
            .or_else(|| json_preview(parsed, "html", 88))
            .or_else(|| json_preview(parsed, "text", 88))
            .unwrap_or_else(|| summarize_tool_result(data)),
        "ask_user" => {
            json_preview(parsed, "data", 88).unwrap_or_else(|| summarize_tool_result(data))
        }
        "update_working_checkpoint" | "start_long_term_update" => summarize_tool_result(data),
        _ => summarize_tool_result(data),
    }
}

fn tool_title(name: &str) -> &'static str {
    match name {
        "code_run" => "代码执行",
        "file_read" => "读取文件",
        "file_patch" => "补丁修改",
        "file_write" => "写入文件",
        "web_scan" => "浏览器扫描",
        "web_execute_js" => "浏览器执行",
        "ask_user" => "请求用户输入",
        "update_working_checkpoint" => "短期记忆",
        "start_long_term_update" => "长期记忆",
        _ => "自定义工具",
    }
}

fn tool_icon(name: &str) -> &'static str {
    match name {
        "code_run" => "⌘",
        "file_read" => "R",
        "file_patch" => "✎",
        "file_write" => "✚",
        "web_scan" => "◉",
        "web_execute_js" => "⚡",
        "ask_user" => "?",
        "update_working_checkpoint" => "◇",
        "start_long_term_update" => "◆",
        _ => "•",
    }
}

fn result_color(ok: bool) -> Color {
    if ok {
        Color::LightGreen
    } else {
        Color::DarkGray
    }
}

fn parse_json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()))
}

fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

fn json_i64(value: &serde_json::Value, key: &str) -> Option<i64> {
    value.get(key).and_then(serde_json::Value::as_i64)
}

fn json_bool(value: &serde_json::Value, key: &str) -> Option<bool> {
    value.get(key).and_then(serde_json::Value::as_bool)
}

fn json_preview(value: &serde_json::Value, key: &str, max: usize) -> Option<String> {
    value
        .get(key)
        .map(value_to_string)
        .map(|s| trim_chars(&s, max))
}

fn value_to_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
        .replace(['\n', '\r', '\t'], " ")
}
