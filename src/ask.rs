//! The question block an agent appends to a `needs-human` comment.
//!
//! The agent ends its question comment with one strict block. The format
//! copies the ticket proposal block of [`crate::ticket`]:
//!
//! ```text
//! <aif-ask-v1>
//! {"question":"Which workload mode ships first?","options":[{"label":"Fast","description":"deterministic only"},{"label":"Full"}]}
//! </aif-ask-v1>
//! ```
//!
//! The parser accepts a block under these rules. Any other text produces
//! no ask. Unlike [`crate::ticket::parse_ticket_proposal`], the parser does
//! not refuse a fenced block, because a comment often holds a real code
//! fence beside the ask.
//!
//! - The text holds exactly one open tag and exactly one close tag.
//! - The open tag starts a line. The close tag ends a line.
//! - The JSON sits on one line between the two tags.
//! - The JSON has the fields `question` and `options` and no other field.
//! - Each option has `label` and an optional `description`.
//! - `question` is not empty after a trim. `options` holds 1 to 9 entries.
//!   Each `label` is not empty after a trim.

use serde::{Deserialize, Serialize};

/// The open tag of one ask block.
const OPEN: &str = "<aif-ask-v1>";

/// The close tag of one ask block.
const CLOSE: &str = "</aif-ask-v1>";

/// One named answer an agent offers with its question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskOption {
    /// The short answer text a pick posts back to GitHub.
    pub label: String,
    /// The one-line explanation of the answer, when the agent gave one.
    #[serde(default)]
    pub description: String,
}

/// One question an agent asks the human through a `needs-human` comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    /// The question text.
    pub question: String,
    /// The offered answers, in file order.
    pub options: Vec<AskOption>,
}

/// Parse the strict ask block out of one comment body.
///
/// The call returns `None` when the text holds no valid block. The option
/// list keeps the file order of the JSON array.
pub fn parse_ask_block(text: &str) -> Option<Ask> {
    if text.match_indices(OPEN).count() != 1 || text.match_indices(CLOSE).count() != 1 {
        return None;
    }
    let open = text.find(OPEN)?;
    // The open tag starts a line.
    if open > 0 && text.as_bytes().get(open - 1) != Some(&b'\n') {
        return None;
    }
    let close = text.find(CLOSE)?;
    if close < open + OPEN.len() {
        return None;
    }
    // The close tag ends a line.
    let after = close + CLOSE.len();
    if after < text.len() && text.as_bytes().get(after) != Some(&b'\n') {
        return None;
    }
    // The JSON sits on one line between the two tags.
    let json = text[open + OPEN.len()..close]
        .strip_prefix('\n')?
        .strip_suffix('\n')?;
    if json.is_empty() || json.contains('\n') {
        return None;
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct AskWire {
        question: String,
        options: Vec<AskOption>,
    }

    let ask: AskWire = serde_json::from_str(json).ok()?;
    if ask.question.trim().is_empty() {
        return None;
    }
    if ask.options.is_empty() || ask.options.len() > 9 {
        return None;
    }
    if ask
        .options
        .iter()
        .any(|option| option.label.trim().is_empty())
    {
        return None;
    }
    Some(Ask {
        question: ask.question,
        options: ask.options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap one JSON line into a full block comment.
    fn block(json: &str) -> String {
        format!("Please decide.\n{OPEN}\n{json}\n{CLOSE}\nThanks.")
    }

    #[test]
    fn a_valid_block_gives_the_question_and_the_options_in_file_order() {
        let text = block(
            r#"{"question":"Which workload mode ships first?","options":[{"label":"Fast","description":"deterministic only"},{"label":"Full"}]}"#,
        );

        let ask = parse_ask_block(&text).expect("a valid block must parse");

        assert_eq!(ask.question, "Which workload mode ships first?");
        assert_eq!(
            ask.options,
            vec![
                AskOption {
                    label: "Fast".to_string(),
                    description: "deterministic only".to_string(),
                },
                AskOption {
                    label: "Full".to_string(),
                    description: String::new(),
                },
            ]
        );
    }

    #[test]
    fn a_block_inside_a_longer_comment_parses() {
        let text = format!("prose\n{OPEN}\n{{\"question\":\"Go on?\",\"options\":[{{\"label\":\"Yes\"}}]}}\n{CLOSE}\nmore prose");

        let ask = parse_ask_block(&text).expect("a mid-comment block must parse");

        assert_eq!(ask.question, "Go on?");
        assert_eq!(ask.options.len(), 1);
    }

    #[test]
    fn a_comment_without_a_block_gives_no_ask() {
        assert!(parse_ask_block("plain prose only").is_none());
        assert!(parse_ask_block("").is_none());
    }

    #[test]
    fn a_fenced_block_still_parses() {
        let text = format!("```\n{OPEN}\n{{\"question\":\"Still works?\",\"options\":[{{\"label\":\"Yes\"}}]}}\n{CLOSE}\n```");

        assert!(
            parse_ask_block(&text).is_some(),
            "a real code fence beside the ask must not reject the comment"
        );
    }

    #[test]
    fn a_broken_block_gives_no_ask() {
        let good = r#"{"question":"Q","options":[{"label":"A"}]}"#;
        let cases = vec![
            // Ten options exceed the digit keys.
            block(
                r#"{"question":"Q","options":[{"label":"1"},{"label":"2"},{"label":"3"},{"label":"4"},{"label":"5"},{"label":"6"},{"label":"7"},{"label":"8"},{"label":"9"},{"label":"10"}]}"#,
            ),
            // An empty question.
            block(r#"{"question":"   ","options":[{"label":"A"}]}"#),
            // An empty option label.
            block(r#"{"question":"Q","options":[{"label":" "}]}"#),
            // An unknown JSON field.
            block(r#"{"question":"Q","extra":1,"options":[{"label":"A"}]}"#),
            // An unknown option field.
            block(r#"{"question":"Q","options":[{"label":"A","extra":1}]}"#),
            // A multi-line JSON body.
            format!("{OPEN}\n{{\n\"question\":\"Q\"\n}}\n{CLOSE}"),
            // Two open tags.
            format!("{OPEN}\n{good}\n{CLOSE}\n{OPEN}\n{good}\n{CLOSE}"),
            // Two close tags.
            format!("{OPEN}\n{good}\n{CLOSE}\n{CLOSE}"),
            // The open tag does not start a line.
            format!("text {OPEN}\n{good}\n{CLOSE}"),
            // The close tag does not end a line.
            format!("{OPEN}\n{good}\n{CLOSE} tail"),
            // The JSON shares a line with a tag.
            format!("{OPEN}{good}\n{CLOSE}"),
            // The close tag precedes the open tag.
            format!("{CLOSE}\n{good}\n{OPEN}"),
            // No options.
            block(r#"{"question":"Q","options":[]}"#),
        ];
        for case in cases {
            assert!(
                parse_ask_block(&case).is_none(),
                "this comment must give no ask: {case}"
            );
        }
    }
}
