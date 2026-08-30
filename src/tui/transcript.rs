//! Turns raw log lines into display lines for the session view.
//!
//! The task log holds one NDJSON line per protocol event. The claude runner
//! and the opencode runner write two different dialects; both shapes are
//! recorded in `docs/v0.5/SPEC.md` under "Verified external protocol facts".
//!
//! [`parse`] maps one raw line to zero or more [`Entry`] values. [`render`]
//! maps one entry to wrapped [`Line`] values for one pane width. Both
//! functions are pure: they touch no terminal, no file, and no clock, so
//! tests run offline.
//!
//! A line that parses to nothing known renders as a dim raw line. Verified
//! claude subagent output is excluded. No input can panic the renderer.

use super::theme::THEME;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

/// The style of ordinary assistant text.
pub fn assistant_style() -> Style {
    Style::default().fg(THEME.text)
}

/// The style of dimmed lines: tools, system notes, and raw fallbacks.
pub fn dim_style() -> Style {
    Style::default().fg(THEME.dim).add_modifier(Modifier::DIM)
}

/// The style of text the human typed into the session.
pub fn user_style() -> Style {
    Style::default()
        .fg(THEME.accent)
        .add_modifier(Modifier::BOLD)
}

/// The style of a successful result.
pub fn ok_style() -> Style {
    Style::default().fg(THEME.ok)
}

/// The style of every marked failure.
pub fn fail_style() -> Style {
    Style::default().fg(THEME.error)
}

/// The style of a pending ask for this task.
pub fn ask_style() -> Style {
    Style::default().fg(THEME.warn)
}

/// One parsed item of the transcript.
///
/// One raw log line can hold several items, because one claude assistant
/// line can carry several content blocks.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// Assistant prose from either runner.
    Assistant {
        /// The text the agent wrote.
        text: String,
    },
    /// One tool call. Rendered dim and prefixed.
    Tool {
        /// The tool name, for example `Bash` or `read`.
        name: String,
        /// A one-line summary of the call.
        summary: String,
        /// True when the call failed.
        failed: bool,
    },
    /// The result of one tool call. A failed result is marked.
    ToolResult {
        /// A one-line summary of the result content.
        summary: String,
        /// True when the result carried an error.
        failed: bool,
    },
    /// Text the human sent into the session.
    User {
        /// The message text.
        text: String,
    },
    /// The end of one turn or one step.
    Result {
        /// True when the turn or step succeeded.
        ok: bool,
        /// The outcome text.
        text: String,
        /// The reported cost in dollars, when the line carries one.
        cost_usd: Option<f64>,
    },
    /// A system note: session init, a control line, a step marker.
    System {
        /// The note text.
        text: String,
    },
    /// A line this module cannot parse. Rendered dim, never dropped.
    Raw {
        /// The raw line text.
        text: String,
    },
}

/// The longest summary the renderer builds before it cuts the text.
const SUMMARY_CHARS: usize = 120;

/// Parse one raw log line into transcript items.
///
/// An empty line yields no items. A line that is not JSON, or a JSON line
/// with a shape this module does not know, yields one [`Entry::Raw`] item.
pub fn parse(raw: &str) -> Vec<Entry> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => parse_value(&value, trimmed),
        Err(_) => vec![Entry::Raw {
            text: trimmed.to_string(),
        }],
    }
}

/// Parse one decoded JSON log line.
///
/// `raw` is the original line text for the raw fallback.
fn parse_value(value: &Value, raw: &str) -> Vec<Entry> {
    if value.pointer("/part/type").and_then(Value::as_str) == Some("tool") {
        return opencode_tool(value, raw);
    }
    let kind = value.get("type").and_then(Value::as_str);
    match kind {
        // The claude dialect.
        Some("assistant") => claude_assistant(value, raw),
        Some("user") => claude_user(value, raw),
        Some("result") => vec![claude_result(value)],
        Some("system") => vec![claude_system(value)],
        Some("control_request") => vec![claude_control_request(value)],
        Some("control_response") => vec![Entry::System {
            text: "control reply".to_string(),
        }],
        Some("rate_limit_event") => vec![Entry::System {
            text: "rate limit event".to_string(),
        }],
        // The opencode dialect.
        Some("text") => opencode_text(value, raw),
        Some("tool_use") => opencode_tool(value, raw),
        Some("step_start") => vec![Entry::System {
            text: "step start".to_string(),
        }],
        Some("step_finish") => vec![opencode_step_finish(value)],
        // A JSON line of no known shape still shows, dim and raw.
        _ => vec![Entry::Raw {
            text: raw.to_string(),
        }],
    }
}

/// Parse one claude `assistant` line into its content blocks.
///
/// A line with a non-null `parent_tool_use_id` is subagent output. The
/// verified protocol says to skip its content blocks.
fn claude_assistant(value: &Value, raw: &str) -> Vec<Entry> {
    if value
        .get("parent_tool_use_id")
        .is_some_and(|parent| !parent.is_null())
    {
        return Vec::new();
    }
    let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
        return raw_fallback(raw);
    };
    let mut items = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    items.push(Entry::Assistant {
                        text: text.to_string(),
                    });
                }
            }
            Some("tool_use") => {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                items.push(Entry::Tool {
                    name: name.to_string(),
                    summary: claude_tool_summary(name, &input),
                    failed: false,
                });
            }
            // `thinking` blocks and unknown block types stay hidden. The
            // runner skips them too; the raw line stays in the log file.
            _ => {}
        }
    }
    if items.is_empty() {
        raw_fallback(raw)
    } else {
        items
    }
}

/// Parse one claude `user` line.
///
/// A string content is a message the human sent into the session. An array
/// content holds tool results; a result with `is_error` is marked.
fn claude_user(value: &Value, raw: &str) -> Vec<Entry> {
    let Some(content) = value.pointer("/message/content") else {
        return raw_fallback(raw);
    };
    if let Some(text) = content.as_str() {
        return vec![Entry::User {
            text: text.to_string(),
        }];
    }
    let Some(blocks) = content.as_array() else {
        return raw_fallback(raw);
    };
    let mut items = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
        items.push(Entry::ToolResult {
            summary: tool_result_summary(block),
            failed,
        });
    }
    if items.is_empty() {
        raw_fallback(raw)
    } else {
        items
    }
}

/// Parse one claude `result` line.
fn claude_result(value: &Value) -> Entry {
    Entry::Result {
        ok: value.get("subtype").and_then(Value::as_str) == Some("success"),
        text: value
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("turn ended")
            .to_string(),
        cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
    }
}

/// Parse one claude `system` line into a dim note.
fn claude_system(value: &Value) -> Entry {
    let subtype = value
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("system");
    if subtype == "init" {
        let model = value.get("model").and_then(Value::as_str).unwrap_or("");
        if model.is_empty() {
            return Entry::System {
                text: "session init".to_string(),
            };
        }
        return Entry::System {
            text: format!("session init ({model})"),
        };
    }
    Entry::System {
        text: format!("system/{subtype}"),
    }
}

/// Parse one claude `control_request` line into a dim note.
fn claude_control_request(value: &Value) -> Entry {
    let subtype = value
        .pointer("/request/subtype")
        .and_then(Value::as_str)
        .unwrap_or("request");
    if subtype == "can_use_tool" {
        let tool = value
            .pointer("/request/tool_name")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        return Entry::System {
            text: format!("ask: {tool}"),
        };
    }
    Entry::System {
        text: format!("control/{subtype}"),
    }
}

/// Parse one opencode `text` line.
///
/// The text lives at `part.text`. A text line without `part.text` falls
/// back to the raw line.
fn opencode_text(value: &Value, raw: &str) -> Vec<Entry> {
    match value.pointer("/part/text").and_then(Value::as_str) {
        Some(text) => vec![Entry::Assistant {
            text: text.to_string(),
        }],
        None => raw_fallback(raw),
    }
}

/// Parse one opencode `tool_use` line.
///
/// The line type is `tool_use` while the part type is `tool`; the spec says
/// to match on the part. The call state lives under `part.state`.
fn opencode_tool(value: &Value, raw: &str) -> Vec<Entry> {
    let part = value.get("part");
    if part
        .and_then(|part| part.get("type"))
        .and_then(Value::as_str)
        != Some("tool")
    {
        return raw_fallback(raw);
    }
    let part = part.unwrap_or(&Value::Null);
    let name = part
        .get("tool")
        .or_else(|| part.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("tool");
    vec![Entry::Tool {
        name: name.to_string(),
        summary: opencode_tool_summary(part),
        failed: part.pointer("/state/status").and_then(Value::as_str) == Some("error"),
    }]
}

/// Parse one opencode `step_finish` line.
///
/// One run emits several steps, so several of these can appear before the
/// process exit. A step failed only when its reason names an error.
fn opencode_step_finish(value: &Value) -> Entry {
    let reason = value.pointer("/part/reason").and_then(Value::as_str);
    Entry::Result {
        ok: reason != Some("error"),
        text: reason.unwrap_or("step finished").to_string(),
        cost_usd: value.pointer("/part/cost").and_then(Value::as_f64),
    }
}

/// Build the raw fallback for a known line type with an unusable payload.
fn raw_fallback(raw: &str) -> Vec<Entry> {
    vec![Entry::Raw {
        text: raw.to_string(),
    }]
}

/// Summarize the result content of one claude `tool_result` block.
///
/// The content can be a plain string or an array of text blocks.
fn tool_result_summary(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => truncate(text),
        Some(Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            truncate(&text)
        }
        _ => truncate(&block.to_string()),
    }
}

/// Derive a one-line summary for a claude tool input.
///
/// The key per tool mirrors the claude runner: the command for `Bash`, the
/// path for `Write` and `Edit`, the whole input otherwise.
pub fn claude_tool_summary(name: &str, input: &Value) -> String {
    let key = match name {
        "Bash" => Some("command"),
        "Write" | "Edit" => Some("file_path"),
        _ => None,
    };
    match key.and_then(|key| input.get(key)).and_then(Value::as_str) {
        Some(detail) => detail.to_string(),
        None => truncate(&input.to_string()),
    }
}

/// Derive a one-line summary for an opencode tool part.
///
/// The state title wins, then the whole part, mirroring the opencode runner.
pub fn opencode_tool_summary(part: &Value) -> String {
    match part
        .pointer("/state/title")
        .or_else(|| part.get("title"))
        .and_then(Value::as_str)
    {
        Some(title) => title.to_string(),
        None => truncate(&part.to_string()),
    }
}

/// Cut `text` to at most [`SUMMARY_CHARS`] characters.
fn truncate(text: &str) -> String {
    if text.chars().count() <= SUMMARY_CHARS {
        text.to_string()
    } else {
        text.chars().take(SUMMARY_CHARS).collect()
    }
}

/// Render one entry into wrapped display lines for a pane `width`.
///
/// The width uses terminal display columns. The call never returns an empty
/// vector and never panics, even at width 0.
pub fn render(entry: &Entry, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    match entry {
        Entry::Assistant { text } => prefixed_lines("▌ ", assistant_style(), text, width),
        Entry::Tool {
            name,
            summary,
            failed,
        } => {
            let text = format!("{name} {summary}");
            if *failed {
                prefixed_lines("✗ ", fail_style(), &text, width)
            } else {
                prefixed_lines("· ", dim_style(), &text, width)
            }
        }
        Entry::ToolResult { summary, failed } => {
            if *failed {
                prefixed_lines("✗ ", fail_style(), summary, width)
            } else {
                prefixed_lines("↳ ", dim_style(), summary, width)
            }
        }
        Entry::User { text } => prefixed_lines("› ", user_style(), text, width),
        Entry::Result { ok, text, cost_usd } => {
            let mut shown = text.clone();
            if let Some(cost) = cost_usd {
                shown.push_str(&format!(" (${cost:.2})"));
            }
            if *ok {
                prefixed_lines("✓ ", ok_style(), &shown, width)
            } else {
                prefixed_lines("✗ ", fail_style(), &shown, width)
            }
        }
        Entry::System { text } => prefixed_lines("* ", dim_style(), text, width),
        Entry::Raw { text } => prefixed_lines("", dim_style(), text, width),
    }
}

/// Render `text` with a two-column prefix and a hanging indent.
fn prefixed_lines(prefix: &str, style: Style, text: &str, width: usize) -> Vec<Line<'static>> {
    let lead_width = display_width(prefix);
    if lead_width >= width {
        let prefix_end = prefix_at_width(prefix, width);
        let mut lines = Vec::new();
        if prefix_end > 0 {
            lines.push(Line::from(Span::styled(
                prefix[..prefix_end].to_string(),
                style,
            )));
        }
        if !text.is_empty() {
            lines.extend(
                wrap(text, width)
                    .into_iter()
                    .map(|chunk| Line::from(Span::styled(chunk, style))),
            );
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(String::new(), style)));
        }
        return lines;
    }
    let body_width = width.saturating_sub(lead_width).max(1);
    let indent = " ".repeat(lead_width);
    wrap(text, body_width)
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let lead = if index == 0 { prefix } else { indent.as_str() };
            let mut content = String::with_capacity(lead.len() + chunk.len());
            content.push_str(lead);
            content.push_str(&chunk);
            Line::from(vec![Span::styled(content, style)])
        })
        .collect()
}

/// Wrap `text` to `width` terminal display columns.
///
/// The function splits on newlines first, then word-wraps each paragraph on
/// spaces. A word wider than the width is split hard. A width of 0 counts
/// as 1. An empty text yields one empty line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        lines.extend(wrap_paragraph(paragraph, width));
    }
    lines
}

/// Word-wrap one paragraph with no newlines in it.
fn wrap_paragraph(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_len = 0usize;
    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let word_len = display_width(word);
            let space = usize::from(line_len > 0);
            if line_len + space + word_len <= width {
                if space == 1 {
                    line.push(' ');
                    line_len += 1;
                }
                line.push_str(word);
                line_len += word_len;
                break;
            }
            if line_len > 0 {
                lines.push(std::mem::take(&mut line));
                line_len = 0;
                continue;
            }
            // The word alone is wider than the width: split it hard.
            let take = prefix_at_width(word, width);
            let (head, tail) = word.split_at(take);
            lines.push(head.to_string());
            word = tail;
        }
    }
    lines.push(line);
    lines
}

/// Return the terminal display width of `text`.
fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

/// Return the byte end of the longest non-empty prefix within `width`.
fn prefix_at_width(text: &str, width: usize) -> usize {
    let mut end = 0;
    let mut used = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        let character_width = display_width(&text[index..next]);
        if used + character_width > width {
            break;
        }
        used += character_width;
        end = next;
    }
    if end == 0 {
        text.chars().next().map_or(0, char::len_utf8)
    } else {
        end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    /// The sum of the span widths of one rendered line.
    fn line_width(line: &Line<'_>) -> usize {
        line.width()
    }

    /// The text of one rendered line.
    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    #[test]
    fn a_claude_assistant_text_line_renders_as_plain_text() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Refining the ticket."}]}}"#;
        let entries = parse(line);

        assert_eq!(
            entries,
            vec![Entry::Assistant {
                text: "Refining the ticket.".to_string()
            }]
        );

        let lines = render(&entries[0], 80);
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("Refining the ticket."));
        assert!(
            !lines[0].spans[0].style.add_modifier.contains(Modifier::DIM),
            "assistant text must not render dim"
        );
    }

    #[test]
    fn a_claude_tool_use_line_renders_dim_and_prefixed() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"gh issue view 7"}}]}}"#;
        let entries = parse(line);

        assert_eq!(entries.len(), 1);
        let Entry::Tool {
            name,
            summary,
            failed,
        } = &entries[0]
        else {
            panic!("expected a tool entry");
        };
        assert_eq!(name, "Bash");
        assert_eq!(summary, "gh issue view 7");
        assert!(!failed);

        let lines = render(&entries[0], 80);
        let text = line_text(&lines[0]);
        assert!(text.starts_with("· "), "prefix: {text}");
        assert!(text.contains("Bash"));
        assert!(text.contains("gh issue view 7"));
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn an_opencode_text_line_renders_the_text() {
        let line = r#"{"type":"text","timestamp":1788091541442,"sessionID":"ses_1","part":{"type":"text","text":"Build passes."}}"#;
        let entries = parse(line);

        assert_eq!(
            entries,
            vec![Entry::Assistant {
                text: "Build passes.".to_string()
            }]
        );
        let lines = render(&entries[0], 80);
        assert!(line_text(&lines[0]).contains("Build passes."));
    }

    #[test]
    fn an_opencode_tool_line_matches_on_the_part_and_marks_errors() {
        let line = r#"{"type":"tool_use","timestamp":1788091541442,"sessionID":"ses_1","part":{"type":"tool","tool":"read","callID":"call_1","state":{"status":"completed","input":{"filePath":"src/main.rs"},"output":"ok","title":"src/main.rs"}}}"#;
        let entries = parse(line);

        let Entry::Tool {
            name,
            summary,
            failed,
        } = &entries[0]
        else {
            panic!("expected a tool entry");
        };
        assert_eq!(name, "read");
        assert_eq!(summary, "src/main.rs");
        assert!(!failed);

        let failed_line = r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","tool":"bash","state":{"status":"error","output":"exit 1","title":"make test"}}}"#;
        let entries = parse(failed_line);
        let Entry::Tool { failed, .. } = &entries[0] else {
            panic!("expected a tool entry");
        };
        assert!(failed);

        let lines = render(&entries[0], 80);
        let text = line_text(&lines[0]);
        assert!(text.starts_with("✗ "), "a failed tool is marked: {text}");
        assert!(text.contains("make test"));
    }

    #[test]
    fn an_opencode_tool_part_does_not_depend_on_the_outer_line_type() {
        let line = r#"{"type":"future_type","part":{"type":"tool","tool":"read","state":{"status":"completed","title":"src/main.rs"}}}"#;

        let entries = parse(line);

        assert_eq!(
            entries,
            vec![Entry::Tool {
                name: "read".to_string(),
                summary: "src/main.rs".to_string(),
                failed: false,
            }]
        );
    }

    #[test]
    fn a_claude_failed_tool_result_is_marked() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"boom"}]}}"#;
        let entries = parse(line);

        assert_eq!(
            entries,
            vec![Entry::ToolResult {
                summary: "boom".to_string(),
                failed: true
            }]
        );

        let lines = render(&entries[0], 80);
        let text = line_text(&lines[0]);
        assert!(text.starts_with("✗ "), "a failed result is marked: {text}");
        assert!(text.contains("boom"));
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));

        // A clean result stays dim and unmarked.
        let ok_line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}"#;
        let entries = parse(ok_line);
        let Entry::ToolResult { failed, .. } = &entries[0] else {
            panic!("expected a tool result entry");
        };
        assert!(!failed);
        let lines = render(&entries[0], 80);
        assert!(line_text(&lines[0]).starts_with("↳ "));
    }

    #[test]
    fn a_malformed_line_renders_as_a_dim_raw_line_and_is_never_dropped() {
        let entries = parse("this is not json {");

        assert_eq!(
            entries,
            vec![Entry::Raw {
                text: "this is not json {".to_string()
            }]
        );

        let lines = render(&entries[0], 80);
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("this is not json {"));
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::DIM));

        // A JSON line of no known shape falls back the same way.
        let entries = parse(r#"{"type":"mystery","payload":42}"#);
        assert_eq!(entries.len(), 1);
        let lines = render(&entries[0], 80);
        assert!(line_text(&lines[0]).contains("mystery"));
    }

    #[test]
    fn a_very_narrow_width_wraps_every_line_inside_the_width() {
        let entry = Entry::Assistant {
            text: "alpha beta gamma delta epsilon zeta".to_string(),
        };

        let lines = render(&entry, 5);

        assert!(lines.len() >= 2, "the text must wrap, got {}", lines.len());
        for line in &lines {
            assert!(
                line_width(line) <= 5,
                "line too wide: {:?}",
                line_text(line)
            );
        }
        // Strip the two-column prefix and indent, then join the chunks.
        // Hard-split words come back adjacent, so every word must appear.
        let joined: String = lines
            .iter()
            .map(|line| line_text(line).chars().skip(2).collect::<String>())
            .collect();
        for word in ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"] {
            assert!(joined.contains(word), "lost word {word}: {joined}");
        }

        // Width 0 must not panic and must behave like width 1.
        let lines = render(&entry, 0);
        assert!(!lines.is_empty());
        for line in &lines {
            assert!(line_width(line) <= 1, "a zero width uses one column");
        }
    }

    #[test]
    fn a_claude_result_line_shows_the_outcome_and_the_cost() {
        let line = r#"{"type":"result","subtype":"success","result":"Ticket refined.","session_id":"s1","total_cost_usd":0.21,"usage":{}}"#;
        let entries = parse(line);

        let Entry::Result { ok, text, cost_usd } = &entries[0] else {
            panic!("expected a result entry");
        };
        assert!(ok);
        assert_eq!(text, "Ticket refined.");
        assert_eq!(*cost_usd, Some(0.21));

        let lines = render(&entries[0], 80);
        let text = line_text(&lines[0]);
        assert!(text.starts_with("✓ "));
        assert!(text.contains("Ticket refined."));
        assert!(text.contains("$0.21"));
    }

    #[test]
    fn a_failed_claude_result_line_is_marked() {
        let line =
            r#"{"type":"result","subtype":"error_max_turns","result":"gave up","is_error":true}"#;
        let entries = parse(line);

        let Entry::Result { ok, .. } = &entries[0] else {
            panic!("expected a result entry");
        };
        assert!(!ok);

        let lines = render(&entries[0], 80);
        assert!(line_text(&lines[0]).starts_with("✗ "));
    }

    #[test]
    fn a_claude_user_message_renders_as_the_human_voice() {
        let line = r#"{"type":"user","message":{"role":"user","content":"use sqlite instead"}}"#;
        let entries = parse(line);

        assert_eq!(
            entries,
            vec![Entry::User {
                text: "use sqlite instead".to_string()
            }]
        );

        let lines = render(&entries[0], 80);
        let text = line_text(&lines[0]);
        assert!(text.starts_with("› "));
        assert!(text.contains("use sqlite instead"));
    }

    #[test]
    fn a_subagent_line_is_skipped() {
        let line = r#"{"type":"assistant","parent_tool_use_id":"toolu_9","message":{"content":[{"type":"text","text":"nested work"}]}}"#;

        assert!(parse(line).is_empty());
    }

    #[test]
    fn system_and_control_lines_render_as_dim_notes() {
        let entries = parse(
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-5"}"#,
        );
        let Entry::System { text } = &entries[0] else {
            panic!("expected a system note");
        };
        assert!(text.contains("session init"));
        assert!(text.contains("claude-opus-5"));

        let entries = parse(
            r#"{"type":"control_request","request_id":"u1","request":{"subtype":"can_use_tool","tool_name":"Write"}}"#,
        );
        let Entry::System { text } = &entries[0] else {
            panic!("expected a system note");
        };
        assert!(text.contains("ask: Write"));

        let entries = parse(r#"{"type":"step_start","sessionID":"ses_1"}"#);
        assert!(matches!(entries[0], Entry::System { .. }));
    }

    #[test]
    fn an_opencode_step_finish_renders_per_step() {
        let line = r#"{"type":"step_finish","sessionID":"ses_1","part":{"type":"step-start","reason":"ok","cost":0.5}}"#;
        let entries = parse(line);

        let Entry::Result { ok, text, cost_usd } = &entries[0] else {
            panic!("expected a result entry");
        };
        assert!(ok);
        assert_eq!(text, "ok");
        assert_eq!(*cost_usd, Some(0.5));

        let failing = r#"{"type":"step_finish","part":{"reason":"error"}}"#;
        let entries = parse(failing);
        let Entry::Result { ok, .. } = &entries[0] else {
            panic!("expected a result entry");
        };
        assert!(!ok);
    }

    #[test]
    fn an_empty_line_parses_to_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("   \t ").is_empty());
    }

    #[test]
    fn a_known_type_with_an_unusable_payload_falls_back_to_raw() {
        // An assistant line without a content array.
        let entries = parse(r#"{"type":"assistant"}"#);
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], Entry::Raw { .. }));

        // A text line without part.text.
        let entries = parse(r#"{"type":"text","part":{}}"#);
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], Entry::Raw { .. }));

        // A tool_use line without a tool part.
        let entries = parse(r#"{"type":"tool_use","part":{"type":"text"}}"#);
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], Entry::Raw { .. }));
    }

    #[test]
    fn wrap_respects_the_width_and_splits_long_words() {
        assert_eq!(wrap("hello world", 80), vec!["hello world".to_string()]);
        assert_eq!(
            wrap("hello world", 5),
            vec!["hello".to_string(), "world".to_string()]
        );
        assert_eq!(
            wrap("abcdef", 2),
            vec!["ab".to_string(), "cd".to_string(), "ef".to_string()]
        );
        assert_eq!(wrap("", 5), vec![String::new()]);
        assert_eq!(
            wrap("a\n\nb", 5),
            vec!["a".to_string(), String::new(), "b".to_string()]
        );
        // A width of 0 counts as 1 and never hangs.
        assert_eq!(
            wrap("abc", 0),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn prefixed_and_wide_text_stays_inside_the_pane_width() {
        let narrow = render(
            &Entry::Assistant {
                text: "ab".to_string(),
            },
            1,
        );
        assert!(narrow.iter().all(|line| line_width(line) <= 1));
        assert_eq!(line_text(&narrow[0]), "▌");

        let wide = render(
            &Entry::Assistant {
                text: "界界".to_string(),
            },
            4,
        );
        assert_eq!(wide.len(), 2);
        assert!(wide.iter().all(|line| line_width(line) <= 4));
        assert_eq!(line_text(&wide[0]), "▌ 界");
        assert_eq!(line_text(&wide[1]), "  界");
    }
}
