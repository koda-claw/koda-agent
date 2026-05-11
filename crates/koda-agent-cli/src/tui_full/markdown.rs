use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

pub(super) fn render_markdown_lines(text: &str, base_color: Color) -> Vec<Line<'static>> {
    let renderer = MarkdownRenderer::new(base_color);
    renderer.render(text)
}

struct MarkdownRenderer {
    base_color: Color,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    list_stack: Vec<Option<u64>>,
    blockquote_depth: usize,
    in_code_block: bool,
    strong: bool,
    emphasis: bool,
    strike: bool,
    table_row: Vec<String>,
    in_table_cell: bool,
}

impl MarkdownRenderer {
    fn new(base_color: Color) -> Self {
        Self {
            base_color,
            lines: Vec::new(),
            current: Vec::new(),
            list_stack: Vec::new(),
            blockquote_depth: 0,
            in_code_block: false,
            strong: false,
            emphasis: false,
            strike: false,
            table_row: Vec::new(),
            in_table_cell: false,
        }
    }

    fn render(mut self, text: &str) -> Vec<Line<'static>> {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_FOOTNOTES);
        options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
        for event in Parser::new_ext(text, options) {
            self.handle_event(event);
        }
        self.flush_current();
        if self.in_code_block {
            self.close_code_block();
        }
        if self.lines.is_empty() {
            self.lines.push(Line::raw(""));
        }
        self.lines
    }

    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.inline_code(&code),
            Event::InlineMath(code) => self.inline_code(&format!("${code}$")),
            Event::DisplayMath(code) => self.code_lines("math", &code),
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html),
            Event::FootnoteReference(label) => self.text(&format!("[^{label}]")),
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.flush_current(),
            Event::Rule => {
                self.flush_current();
                self.lines.push(Line::styled(
                    "────────────",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }),
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_current();
                self.current.push(Span::styled(
                    heading_prefix(level),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Tag::BlockQuote(_) => {
                self.flush_current();
                self.blockquote_depth += 1;
                self.current.push(Span::styled(
                    "┃ ".repeat(self.blockquote_depth),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Tag::CodeBlock(kind) => {
                self.flush_current();
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.trim().is_empty() => lang.to_string(),
                    _ => String::new(),
                };
                self.open_code_block(&lang);
            }
            Tag::List(start) => self.list_stack.push(start),
            Tag::Item => {
                self.flush_current();
                let prefix = self.next_list_prefix();
                self.current.push(Span::styled(
                    prefix,
                    Style::default().fg(Color::LightYellow),
                ));
            }
            Tag::Emphasis => self.emphasis = true,
            Tag::Strong => self.strong = true,
            Tag::Strikethrough => self.strike = true,
            Tag::Table(_) => {
                self.flush_current();
                self.lines.push(Line::styled(
                    "┌─ 表格 Table",
                    Style::default().fg(Color::LightBlue),
                ));
            }
            Tag::TableRow => self.table_row.clear(),
            Tag::TableCell => {
                self.in_table_cell = true;
                self.current.clear();
            }
            Tag::Link { .. } => self.text("["),
            Tag::Image { dest_url, .. } => self.text(&format!("![image]({dest_url})")),
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::TableHead
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => self.flush_current(),
            TagEnd::BlockQuote(_) => {
                self.flush_current();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => self.close_code_block(),
            TagEnd::List(_) => {
                self.flush_current();
                self.list_stack.pop();
            }
            TagEnd::Emphasis => self.emphasis = false,
            TagEnd::Strong => self.strong = false,
            TagEnd::Strikethrough => self.strike = false,
            TagEnd::TableCell => {
                self.in_table_cell = false;
                self.table_row.push(line_plain_text(&self.current));
                self.current.clear();
            }
            TagEnd::TableRow => {
                let row = self.table_row.join(" │ ");
                if !row.trim().is_empty() {
                    self.lines.push(Line::styled(
                        format!("│ {row}"),
                        Style::default().fg(self.base_color),
                    ));
                }
            }
            TagEnd::TableHead => {
                let row = self.table_row.join(" │ ");
                if !row.trim().is_empty() {
                    self.lines.push(Line::styled(
                        format!("│ {row}"),
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    ));
                    self.lines.push(Line::styled(
                        "├─".to_string(),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                self.table_row.clear();
            }
            TagEnd::Table => self.lines.push(Line::styled(
                "└".to_string(),
                Style::default().fg(Color::DarkGray),
            )),
            TagEnd::Link => self.text("]"),
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::Image
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.in_code_block {
            self.code_text(text);
            return;
        }
        let style = self.inline_style();
        for (idx, part) in text.split('\n').enumerate() {
            if idx > 0 {
                self.flush_current();
            }
            if !part.is_empty() {
                self.current.push(Span::styled(part.to_string(), style));
            }
        }
    }

    fn inline_code(&mut self, code: &str) {
        self.current.push(Span::styled(
            format!(" {code} "),
            Style::default().fg(Color::Black).bg(Color::LightYellow),
        ));
    }

    fn inline_style(&self) -> Style {
        let mut style = Style::default().fg(if self.emphasis {
            Color::LightCyan
        } else {
            self.base_color
        });
        if self.strong {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.strike {
            style = style.fg(Color::DarkGray);
        }
        style
    }

    fn open_code_block(&mut self, lang: &str) {
        self.in_code_block = true;
        let title = if lang.is_empty() {
            "代码".to_string()
        } else {
            format!("代码 · {lang}")
        };
        self.lines.push(Line::styled(
            format!("┌─ {title}"),
            Style::default().fg(Color::LightBlue),
        ));
    }

    fn close_code_block(&mut self) {
        self.in_code_block = false;
        self.lines.push(Line::styled(
            "└".to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }

    fn code_lines(&mut self, lang: &str, text: &str) {
        self.flush_current();
        self.open_code_block(lang);
        self.code_text(text);
        self.close_code_block();
    }

    fn code_text(&mut self, text: &str) {
        for line in text.trim_end_matches('\n').lines() {
            self.lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(line.to_string(), Style::default().fg(Color::LightCyan)),
            ]));
        }
    }

    fn next_list_prefix(&mut self) -> String {
        let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
        match self.list_stack.last_mut() {
            Some(Some(n)) => {
                let cur = *n;
                *n = n.saturating_add(1);
                format!("{indent} {cur}. ")
            }
            _ => format!("{indent}  • "),
        }
    }

    fn flush_current(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.current);
        if self.in_table_cell {
            self.current = spans;
        } else {
            self.lines.push(Line::from(spans));
        }
    }
}

fn heading_prefix(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "▌ ",
        HeadingLevel::H2 => "▌ ",
        HeadingLevel::H3 => "▸ ",
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => "· ",
    }
}

fn line_plain_text(spans: &[Span<'_>]) -> String {
    spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(text: &str) -> String {
        render_markdown_lines(text, Color::White)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_common_markdown_blocks() {
        let output = rendered("# 标题\n- item `x`\n> quote\n```rust\nfn main() {}\n```");
        assert!(output.contains("▌ 标题"));
        assert!(output.contains("• item"));
        assert!(output.contains("x"));
        assert!(output.contains("┃ quote"));
        assert!(output.contains("代码 · rust"));
        assert!(output.contains("fn main"));
    }

    #[test]
    fn renders_commonmark_tables_tasks_and_inline_styles() {
        let output = rendered("- [x] done\n\n| A | B |\n|---|---|\n| **x** | ~~y~~ |\n");
        assert!(output.contains("[x] done"));
        assert!(output.contains("表格 Table"));
        assert!(output.contains("A │ B"));
        assert!(output.contains("x │ y"));
    }
}
