//! The strict block rules: one final, complete, unquoted block.
//!
//! An agent ends its report with one tagged block. [`parse_block`] reads
//! the body of that block and refuses every other shape, so a truncated
//! or duplicated block never reaches a parser.

/// Why [`parse_block`] found no usable block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    /// The text holds no block with this tag.
    Absent,
    /// The text holds a block that breaks one strict rule; the message
    /// names the rule.
    Malformed(&'static str),
}

/// Parse the one strict block of `tag` from the end of `text`.
///
/// The rules are the ticket-proposal rules: the text holds exactly one
/// opening and one closing tag, no code fence, the block starts after a
/// line break, its body starts on its own line, and the closing tag ends
/// the text. The body between the tags may hold any text; the caller
/// parses it.
pub fn parse_block(tag: &str, text: &str) -> Result<String, BlockError> {
    let close = crate::theory::records::close_tag(tag);
    let text = text.trim_end();
    let open = match text.find(tag) {
        None => return Err(BlockError::Absent),
        Some(at) => at,
    };
    if text.match_indices(tag).count() > 1 {
        return Err(BlockError::Malformed("the text holds a second block"));
    }
    if text.match_indices(close.as_str()).count() > 1 {
        return Err(BlockError::Malformed("the text holds a second closing tag"));
    }
    if text.contains("```") {
        return Err(BlockError::Malformed("the block sits in a code fence"));
    }
    if !text.ends_with(close.as_str()) {
        return Err(BlockError::Malformed("the block does not end the text"));
    }
    if open > 0 && text.as_bytes().get(open - 1) != Some(&b'\n') {
        return Err(BlockError::Malformed(
            "the block does not start after a line break",
        ));
    }
    let body = text[open..]
        .strip_prefix(tag)
        .and_then(|rest| rest.strip_prefix('\n'))
        .and_then(|rest| rest.strip_suffix(close.as_str()))
        .and_then(|rest| rest.strip_suffix('\n'))
        .ok_or(BlockError::Malformed(
            "the block body does not start on its own line",
        ))?;
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: &str = "<aif-ticket-proposal-v1>";
    const CLOSE: &str = "</aif-ticket-proposal-v1>";

    fn block(body: &str) -> String {
        format!("{TAG}\n{body}\n{CLOSE}")
    }

    #[test]
    fn parse_block_reads_the_body_of_one_final_block() {
        let text = format!("The title needs work.\n\n{}", block(r#"{"title":"A"}"#));
        assert_eq!(parse_block(TAG, &text), Ok(r#"{"title":"A"}"#.to_string()));
        assert_eq!(parse_block(TAG, &block("body")), Ok("body".to_string()));
    }

    #[test]
    fn parse_block_reports_absent_text_without_a_block() {
        assert_eq!(parse_block(TAG, "no blocks here"), Err(BlockError::Absent));
        assert_eq!(parse_block(TAG, ""), Err(BlockError::Absent));
        assert_eq!(parse_block(TAG, "```\ncode\n```"), Err(BlockError::Absent));
        assert_eq!(
            parse_block(TAG, &format!("a stray {CLOSE} with no opening tag")),
            Err(BlockError::Absent)
        );
    }

    #[test]
    fn parse_block_rejects_a_fenced_block() {
        let fenced = format!(
            "```\n{}\n```",
            block(r#"{"title":"A"}"#).trim_end_matches('\n')
        );
        assert_eq!(
            parse_block(TAG, &fenced),
            Err(BlockError::Malformed("the block sits in a code fence"))
        );
    }

    #[test]
    fn parse_block_rejects_a_second_block() {
        let text = format!("{}\n{}", block("first"), block("second"));
        assert_eq!(
            parse_block(TAG, &text),
            Err(BlockError::Malformed("the text holds a second block"))
        );
    }

    #[test]
    fn parse_block_rejects_a_block_not_at_the_end() {
        let text = format!("{}\nmore text", block(r#"{"title":"A"}"#));
        assert_eq!(
            parse_block(TAG, &text),
            Err(BlockError::Malformed("the block does not end the text"))
        );
    }

    #[test]
    fn parse_block_rejects_an_incomplete_block() {
        let incomplete = format!("{TAG}\n{{\"title\":\"A\"}}");
        assert_eq!(
            parse_block(TAG, &incomplete),
            Err(BlockError::Malformed("the block does not end the text"))
        );
        let doubled = format!("{}\n{CLOSE}", block("body"));
        assert_eq!(
            parse_block(TAG, &doubled),
            Err(BlockError::Malformed("the text holds a second closing tag"))
        );
    }

    #[test]
    fn parse_block_rejects_a_block_off_a_line_boundary() {
        let text = format!("> {TAG}\n{{}}\n> {CLOSE}");
        assert_eq!(
            parse_block(TAG, &text),
            Err(BlockError::Malformed(
                "the block does not start after a line break"
            ))
        );
        let glued = format!("prose{TAG}\n{{}}\n{CLOSE}");
        assert_eq!(
            parse_block(TAG, &glued),
            Err(BlockError::Malformed(
                "the block does not start after a line break"
            ))
        );
    }

    #[test]
    fn parse_block_rejects_a_body_that_shares_its_opening_line() {
        let text = format!("{TAG}{{}}\n{CLOSE}");
        assert_eq!(
            parse_block(TAG, &text),
            Err(BlockError::Malformed(
                "the block body does not start on its own line"
            ))
        );
    }
}
