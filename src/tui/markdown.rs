//! Renders markdown source as styled terminal lines.
//!
//! The ticket and pull request views receive GitHub bodies as markdown
//! source. This module turns that source into [`Line`] values for the
//! ratatui paragraph widget, so headings, lists, code, and emphasis show
//! with styles instead of raw syntax characters.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::THEME;

/// The text of one list indent level.
const LIST_INDENT: &str = "  ";
/// The prefix that marks a block quote line.
const QUOTE_PREFIX: &str = "│ ";
/// The width of the horizontal rule.
const RULE_WIDTH: usize = 32;

/// One open list frame. `Some` counts the items of an ordered list.
#[derive(Debug)]
struct ListFrame {
    /// The next number of an ordered list, or `None` for bullets.
    next: Option<u64>,
}

/// Converts one markdown document into terminal lines.
#[derive(Debug, Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<ListFrame>,
    quotes: usize,
    code_depth: usize,
    images: usize,
    links: Vec<String>,
    link_emitted: bool,
    marker: Option<String>,
    pending_indent: usize,
    in_table: bool,
    head_row: bool,
    cell: Option<Vec<Span<'static>>>,
    cells: Vec<Vec<Span<'static>>>,
}

/// Converts one markdown document into styled terminal lines.
pub(super) fn markdown_lines(input: &str) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_TABLES);
    let mut renderer = Renderer {
        styles: vec![Style::default().fg(THEME.text)],
        ..Renderer::default()
    };
    for event in Parser::new_ext(input, options) {
        renderer.event(event);
    }
    renderer.finish()
}

impl Renderer {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if self.code_depth > 0 {
                    self.code_text(&text);
                    return;
                }
                if !self.links.is_empty() {
                    self.link_emitted = true;
                }
                let style = if self.images > 0 {
                    self.top().add_modifier(Modifier::ITALIC).fg(THEME.dim)
                } else {
                    self.top()
                };
                self.push_span(&text, style);
            }
            Event::Code(code) => {
                self.link_emitted = true;
                self.push_span(&code, Style::default().fg(THEME.ok));
            }
            Event::Html(html) => {
                for line in html.lines() {
                    self.push_span(line, Style::default().fg(THEME.dim));
                    self.flush_line();
                }
            }
            Event::InlineHtml(html) => {
                self.push_span(&html, Style::default().fg(THEME.dim));
            }
            Event::SoftBreak | Event::HardBreak => self.break_line(),
            Event::Rule => self.rule(),
            Event::TaskListMarker(done) => {
                let box_char = if done { "☑" } else { "☐" };
                self.marker = Some(self.item_indent() + box_char + " ");
            }
            Event::FootnoteReference(name) => {
                self.push_span(&format!("[^{name}]"), Style::default().fg(THEME.dim));
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                self.push_span(&math, Style::default().fg(THEME.ok));
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.blank();
                self.styles.push(self.heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.quotes += 1;
            }
            Tag::CodeBlock(_) => {
                self.flush_line();
                self.code_depth += 1;
            }
            Tag::List(start) => {
                self.lists.push(ListFrame { next: start });
            }
            Tag::Item => {
                let marker = match self.lists.last().and_then(|frame| frame.next) {
                    Some(number) => {
                        if let Some(frame) = self.lists.last_mut() {
                            frame.next = Some(number + 1);
                        }
                        format!("{number}. ")
                    }
                    None => "• ".to_string(),
                };
                self.flush_line();
                self.marker = Some(self.item_indent() + &marker);
            }
            Tag::Emphasis => self.styles.push(self.top().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(self.top().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(self.top().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                self.link_emitted = false;
                self.styles.push(
                    self.top()
                        .fg(THEME.accent)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { .. } => {
                self.images += 1;
                self.link_emitted = true;
            }
            Tag::Table(_) => {
                self.flush_line();
                self.blank();
                self.in_table = true;
            }
            Tag::TableHead => {
                self.head_row = true;
                self.cells.clear();
            }
            Tag::TableRow => {}
            Tag::TableCell => {
                self.cell = Some(Vec::new());
            }
            Tag::HtmlBlock | Tag::FootnoteDefinition(_) | Tag::MetadataBlock(_) => {}
            Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.blank();
            }
            TagEnd::Heading(_) => {
                self.flush_line();
                self.styles.pop();
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quotes = self.quotes.saturating_sub(1);
                self.blank();
            }
            TagEnd::CodeBlock => {
                self.code_depth = self.code_depth.saturating_sub(1);
                self.flush_line();
                self.blank();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.flush_line();
                self.blank();
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::Link => {
                self.close_link();
                self.styles.pop();
            }
            TagEnd::Image => {
                self.images = self.images.saturating_sub(1);
                self.styles.pop();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Table => {
                self.in_table = false;
                self.head_row = false;
                self.blank();
            }
            TagEnd::TableHead => {
                self.table_row_line();
                self.head_row = false;
            }
            TagEnd::TableRow => self.table_row_line(),
            TagEnd::TableCell => {
                if let Some(cell) = self.cell.take() {
                    self.cells.push(cell);
                }
            }
            TagEnd::HtmlBlock | TagEnd::FootnoteDefinition | TagEnd::MetadataBlock(_) => {}
            TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    fn close_link(&mut self) {
        if let Some(dest) = self.links.pop() {
            if !self.link_emitted {
                let style = self.top();
                self.push_span(&dest, style);
            }
            self.link_emitted = true;
        }
    }

    fn heading_style(&self, level: HeadingLevel) -> Style {
        match level {
            HeadingLevel::H1 | HeadingLevel::H2 => {
                self.top().fg(THEME.accent).add_modifier(Modifier::BOLD)
            }
            HeadingLevel::H3 | HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => {
                self.top().add_modifier(Modifier::BOLD)
            }
        }
    }

    fn top(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn item_indent(&self) -> String {
        LIST_INDENT.repeat(self.lists.len().saturating_sub(1))
    }

    fn code_text(&mut self, text: &str) {
        let style = Style::default().fg(THEME.dim).bg(THEME.selected_bg);
        let trimmed = text.strip_suffix('\n').unwrap_or(text);
        if trimmed.is_empty() {
            return;
        }
        for line in trimmed.split('\n') {
            self.push_span(line, style);
            self.flush_line();
        }
    }

    fn rule(&mut self) {
        self.flush_line();
        self.lines.push(Line::from(Span::styled(
            "─".repeat(RULE_WIDTH),
            Style::default().fg(THEME.dim),
        )));
        self.blank();
    }

    fn break_line(&mut self) {
        self.flush_line();
        if !self.lists.is_empty() {
            self.pending_indent = self.item_indent().len() + 2;
        }
    }

    fn table_row_line(&mut self) {
        let cells = std::mem::take(&mut self.cells);
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (index, cell) in cells.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled("  │  ", Style::default().fg(THEME.dim)));
            }
            spans.extend(cell.into_iter().map(|mut span| {
                if self.head_row {
                    span.style = span.style.add_modifier(Modifier::BOLD);
                }
                span
            }));
        }
        if !spans.is_empty() {
            self.lines.push(Line::from(spans));
        }
    }

    fn push_span(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(cell) = self.cell.as_mut() {
            cell.push(Span::styled(text.to_string(), style));
            return;
        }
        if self.spans.is_empty() {
            if let Some(marker) = self.marker.take() {
                self.spans
                    .push(Span::styled(marker, Style::default().fg(THEME.dim)));
            } else if self.pending_indent > 0 {
                let indent = " ".repeat(self.pending_indent);
                self.pending_indent = 0;
                self.spans.push(Span::raw(indent));
            }
        }
        self.spans.push(Span::styled(text.to_string(), style));
    }

    fn flush_line(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let mut spans = std::mem::take(&mut self.spans);
        if self.quotes > 0 {
            spans.insert(
                0,
                Span::styled(
                    QUOTE_PREFIX.repeat(self.quotes),
                    Style::default().fg(THEME.dim),
                ),
            );
        }
        self.lines.push(Line::from(spans));
    }

    fn blank(&mut self) {
        if self.is_last_blank() {
            return;
        }
        self.lines.push(Line::from(""));
    }

    fn is_last_blank(&self) -> bool {
        self.lines
            .last()
            .is_none_or(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        while self.is_last_blank() {
            self.lines.pop();
        }
        while self
            .lines
            .first()
            .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
        {
            self.lines.remove(0);
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    /// Joins the span texts of one rendered line.
    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Renders markdown and returns the plain text of every line.
    fn render_text(input: &str) -> Vec<String> {
        markdown_lines(input).iter().map(line_text).collect()
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(render_text("just a note"), vec!["just a note"]);
    }

    #[test]
    fn heading_renders_bold_with_accent() {
        let lines = markdown_lines("# Title");
        assert_eq!(line_text(&lines[0]), "Title");
        let span = &lines[0].spans[0];
        assert_eq!(span.style.fg, Some(Color::Rgb(0x55, 0xE6, 0xFF)));
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn bullets_prefix_each_item() {
        let texts = render_text("- first\n- second");
        assert_eq!(texts, vec!["• first", "• second"]);
    }

    #[test]
    fn ordered_lists_keep_numbers() {
        let texts = render_text("1. one\n2. two");
        assert_eq!(texts, vec!["1. one", "2. two"]);
    }

    #[test]
    fn task_lists_show_boxes() {
        let texts = render_text("- [ ] open\n- [x] done");
        assert_eq!(texts, vec!["☐ open", "☑ done"]);
    }

    #[test]
    fn nested_lists_indent() {
        let texts = render_text("- a\n  - b");
        assert_eq!(texts, vec!["• a", "  • b"]);
    }

    #[test]
    fn strong_and_code_receive_styles() {
        let lines = markdown_lines("a **bold** and `code`");
        let spans = &lines[0].spans;
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[3].style.fg, Some(THEME.ok));
    }

    #[test]
    fn links_render_text_with_underline() {
        let lines = markdown_lines("[gh](https://example.com)");
        let span = &lines[0].spans[0];
        assert_eq!(span.content, "gh");
        assert_eq!(span.style.fg, Some(THEME.accent));
        assert!(span.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn bare_links_fall_back_to_the_url() {
        let texts = render_text("<https://example.com>");
        assert_eq!(texts, vec!["https://example.com"]);
    }

    #[test]
    fn code_blocks_get_the_panel_style() {
        let lines = markdown_lines("```\nfn main() {}\n```");
        assert_eq!(line_text(&lines[0]), "fn main() {}");
        assert_eq!(lines[0].spans[0].style.fg, Some(THEME.dim));
        assert_eq!(lines[0].spans[0].style.bg, Some(THEME.selected_bg));
    }

    #[test]
    fn blockquotes_prefix_their_lines() {
        let texts = render_text("> quoted");
        assert_eq!(texts, vec!["│ quoted"]);
    }

    #[test]
    fn tables_render_cells_with_separators() {
        let texts = render_text("| a | b |\n|---|---|\n| 1 | 2 |");
        assert_eq!(texts, vec!["a  │  b", "1  │  2"]);
        assert!(
            markdown_lines("| a | b |\n|---|---|\n| 1 | 2 |")[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn blank_lines_do_not_pile_up() {
        let texts = render_text("one\n\n\n\n\ntwo");
        assert_eq!(texts, vec!["one", "", "two"]);
    }

    #[test]
    fn soft_breaks_keep_author_line_breaks() {
        let texts = render_text("alpha\nbeta");
        assert_eq!(texts, vec!["alpha", "beta"]);
    }
}
