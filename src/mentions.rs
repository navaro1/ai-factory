//! Scans GitHub ticket mentions and maps their live statuses.
//!
//! The daemon and the UI share this module. The daemon scans issue and pull
//! request bodies to plan its status fetches. The UI scans rendered text to
//! place one status icon before each mention it can resolve. The scanner is
//! pure text work with no style and no I/O, so both sides agree on what a
//! mention is.

use anyhow::bail;
use anyhow::Result;

use crate::sock::MentionStatus;

/// The color role of one mention status icon.
///
/// The UI maps each role onto its own palette, so this module stays free of
/// style constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionTone {
    /// Healthy state, such as an open ticket.
    Ok,
    /// Secondary state, such as a closed ticket or a draft.
    Dim,
    /// The merged state.
    Repo,
    /// The failed state, such as a closed pull request.
    Error,
}

/// One mention found in text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// The owner and repository of the mention, lowercased. `None` targets
    /// the repository of the body that holds the mention.
    pub repo: Option<String>,
    /// The issue or pull request number.
    pub number: u64,
    /// The byte offset of the first character of the mention.
    pub start: usize,
    /// The byte offset one past the last character of the mention.
    pub end: usize,
}

/// Scan text and return every mention in document order.
///
/// The grammar is an ordered list of per-form matchers. At each byte offset
/// the scanner tries the URL form, the `owner/repo#N` form, and the bare
/// `#N` form. A bare `#N` counts only when the character before it is
/// neither a word character nor `/`, `&`, or `#`; this keeps `abc#12`, URL
/// paths, HTML entities, and `##` headings out. Matched text is consumed,
/// so overlapping forms cannot double-count.
pub fn scan(text: &str) -> Vec<Mention> {
    let mut found = Vec::new();
    let mut at = 0usize;
    while at < text.len() {
        if !text.is_char_boundary(at) {
            at += 1;
            continue;
        }
        if let Some((consumed, mention)) = match_url(text, at)
            .or_else(|| match_repo_hash(text, at))
            .or_else(|| match_bare_hash(text, at))
        {
            found.push(mention);
            at += consumed;
        } else {
            at += 1;
        }
    }
    found
}

/// Classify one GitHub object into its mention status.
///
/// `is_pr` reports the `pull_request` key, `merged` reports a non-null
/// `pull_request.merged_at`, and `state` reports the raw state string.
pub fn classify(state: &str, merged: bool, draft: bool, is_pr: bool) -> Result<MentionStatus> {
    if is_pr {
        if merged {
            return Ok(MentionStatus::MergedPr);
        }
        match state {
            "open" if draft => Ok(MentionStatus::DraftPr),
            "open" => Ok(MentionStatus::OpenPr),
            "closed" => Ok(MentionStatus::ClosedPr),
            other => bail!("GitHub object has unknown state \"{other}\""),
        }
    } else {
        match state {
            "open" => Ok(MentionStatus::OpenIssue),
            "closed" => Ok(MentionStatus::ClosedIssue),
            other => bail!("GitHub object has unknown state \"{other}\""),
        }
    }
}

/// The icon text of one status. A not-found status shows nothing.
pub fn glyph(status: MentionStatus) -> &'static str {
    match status {
        MentionStatus::OpenIssue => "●",
        MentionStatus::ClosedIssue => "○",
        MentionStatus::DraftPr | MentionStatus::OpenPr | MentionStatus::ClosedPr => "◇",
        MentionStatus::MergedPr => "◆",
        MentionStatus::Missing => "",
    }
}

/// The color role of one status icon.
pub fn tone(status: MentionStatus) -> MentionTone {
    match status {
        MentionStatus::OpenIssue | MentionStatus::OpenPr => MentionTone::Ok,
        MentionStatus::ClosedIssue | MentionStatus::DraftPr => MentionTone::Dim,
        MentionStatus::MergedPr => MentionTone::Repo,
        MentionStatus::ClosedPr => MentionTone::Error,
        MentionStatus::Missing => MentionTone::Dim,
    }
}

/// Whether a byte belongs to an owner or repository segment.
fn segment_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

fn valid_segment(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..].iter().all(|&b| segment_byte(b))
}

fn word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether position `at` may start a bare mention.
fn bare_boundary(text: &str, at: usize) -> bool {
    if at == 0 {
        return true;
    }
    let prev = text.as_bytes()[at - 1];
    !word_byte(prev) && !matches!(prev, b'/' | b'&' | b'#')
}

/// Read one run of digits and parse it as a number.
fn take_number(text: &str) -> Option<(u64, usize)> {
    let end = text
        .as_bytes()
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(text.len());
    if end == 0 || end > 18 {
        return None;
    }
    let number: u64 = text[..end].parse().ok()?;
    Some((number, end))
}

/// Match `https://github.com/owner/repo/issues/N` or `.../pull/N`.
fn match_url(text: &str, at: usize) -> Option<(usize, Mention)> {
    let rest = &text[at..];
    let after_host = rest
        .strip_prefix("https://github.com/")
        .or_else(|| rest.strip_prefix("http://github.com/"))?;
    let host_len = rest.len() - after_host.len();
    let slash_owner = after_host.find('/')?;
    let owner = &after_host[..slash_owner];
    if !valid_segment(owner) {
        return None;
    }
    let after_owner = &after_host[slash_owner + 1..];
    let slash_repo = after_owner.find('/')?;
    let repo = &after_owner[..slash_repo];
    if !valid_segment(repo) {
        return None;
    }
    let after_repo = &after_owner[slash_repo + 1..];
    let digits = after_repo
        .strip_prefix("issues/")
        .or_else(|| after_repo.strip_prefix("pull/"))?;
    let (number, digits_len) = take_number(digits)?;
    let consumed =
        host_len + slash_owner + 1 + slash_repo + 1 + after_repo.len() - digits.len() + digits_len;
    let repo = format!("{owner}/{repo}").to_ascii_lowercase();
    Some((
        consumed,
        Mention {
            repo: Some(repo),
            number,
            start: at,
            end: at + consumed,
        },
    ))
}

/// Match `owner/repo#N` at `at`.
fn match_repo_hash(text: &str, at: usize) -> Option<(usize, Mention)> {
    let rest = &text[at..];
    let slash = rest.find('/')?;
    let owner = &rest[..slash];
    if !valid_segment(owner) {
        return None;
    }
    let after_owner = &rest[slash + 1..];
    let hash = after_owner.find('#')?;
    let repo = &after_owner[..hash];
    if !valid_segment(repo) {
        return None;
    }
    let digits = &after_owner[hash + 1..];
    let (number, digits_len) = take_number(digits)?;
    let consumed = slash + 1 + hash + 1 + digits_len;
    let repo = format!("{owner}/{repo}").to_ascii_lowercase();
    Some((
        consumed,
        Mention {
            repo: Some(repo),
            number,
            start: at,
            end: at + consumed,
        },
    ))
}

/// Match a bare `#N` that targets the same repository at `at`.
fn match_bare_hash(text: &str, at: usize) -> Option<(usize, Mention)> {
    if text.as_bytes()[at] != b'#' || !bare_boundary(text, at) {
        return None;
    }
    let (number, digits_len) = take_number(&text[at + 1..])?;
    let consumed = 1 + digits_len;
    Some((
        consumed,
        Mention {
            repo: None,
            number,
            start: at,
            end: at + consumed,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repos(text: &str) -> Vec<(Option<String>, u64)> {
        scan(text)
            .into_iter()
            .map(|mention| (mention.repo, mention.number))
            .collect()
    }

    #[test]
    fn bare_and_explicit_and_url_forms_all_scan() {
        assert_eq!(repos("Depends on #8"), vec![(None, 8)]);
        assert_eq!(
            repos("see octo/repo#12 first"),
            vec![(Some("octo/repo".into()), 12)]
        );
        assert_eq!(
            repos("https://github.com/octo/repo/issues/12"),
            vec![(Some("octo/repo".into()), 12)]
        );
        assert_eq!(
            repos("https://github.com/octo/repo/pull/12"),
            vec![(Some("octo/repo".into()), 12)]
        );
    }

    #[test]
    fn forms_scan_in_document_order_and_mix() {
        assert_eq!(
            repos("#1 then https://github.com/o/r/pull/2 then o/r#3"),
            vec![(None, 1), (Some("o/r".into()), 2), (Some("o/r".into()), 3),]
        );
    }

    #[test]
    fn owner_and_repo_lower_case_in_the_key() {
        assert_eq!(repos("Octo/Repo#7")[0].0.as_deref(), Some("octo/repo"));
    }

    #[test]
    fn word_slash_hash_and_ampersand_block_a_bare_mention() {
        assert!(repos("abc#12").is_empty());
        assert_eq!(
            repos("a/b#12"),
            vec![(Some("a/b".into()), 12)],
            "the owner/repo form owns the match"
        );
        assert!(repos("&#12").is_empty());
        assert!(repos("##12").is_empty());
        assert_eq!(repos("(#12)"), vec![(None, 12)]);
        assert_eq!(repos("start #12 end"), vec![(None, 12)]);
    }

    #[test]
    fn a_url_query_never_yields_a_second_mention() {
        assert_eq!(
            repos("https://github.com/o/r/issues/1?x=y#3"),
            vec![(Some("o/r".into()), 1)]
        );
    }

    #[test]
    fn offsets_split_the_text_exactly() {
        let text = "a #8 b";
        let mentions = scan(text);
        assert_eq!(mentions.len(), 1);
        assert_eq!(&text[mentions[0].start..mentions[0].end], "#8");
    }

    #[test]
    fn multibyte_text_never_breaks_the_scan() {
        assert_eq!(repos("→ ● #12 ◆"), vec![(None, 12)]);
        assert_eq!(repos("超 #12"), vec![(None, 12)]);
    }

    #[test]
    fn classify_maps_every_status_row() {
        assert_eq!(
            classify("open", false, false, false).unwrap(),
            MentionStatus::OpenIssue
        );
        assert_eq!(
            classify("closed", false, false, false).unwrap(),
            MentionStatus::ClosedIssue
        );
        assert_eq!(
            classify("open", false, true, true).unwrap(),
            MentionStatus::DraftPr
        );
        assert_eq!(
            classify("open", false, false, true).unwrap(),
            MentionStatus::OpenPr
        );
        assert_eq!(
            classify("closed", true, false, true).unwrap(),
            MentionStatus::MergedPr
        );
        assert_eq!(
            classify("closed", false, false, true).unwrap(),
            MentionStatus::ClosedPr
        );
        assert!(classify("weird", false, false, true).is_err());
        assert!(classify("weird", false, false, false).is_err());
    }

    #[test]
    fn glyphs_and_tones_follow_the_legend() {
        assert_eq!(glyph(MentionStatus::OpenIssue), "●");
        assert_eq!(tone(MentionStatus::OpenIssue), MentionTone::Ok);
        assert_eq!(glyph(MentionStatus::ClosedIssue), "○");
        assert_eq!(tone(MentionStatus::ClosedIssue), MentionTone::Dim);
        assert_eq!(glyph(MentionStatus::DraftPr), "◇");
        assert_eq!(tone(MentionStatus::DraftPr), MentionTone::Dim);
        assert_eq!(glyph(MentionStatus::OpenPr), "◇");
        assert_eq!(tone(MentionStatus::OpenPr), MentionTone::Ok);
        assert_eq!(glyph(MentionStatus::MergedPr), "◆");
        assert_eq!(tone(MentionStatus::MergedPr), MentionTone::Repo);
        assert_eq!(glyph(MentionStatus::ClosedPr), "◇");
        assert_eq!(tone(MentionStatus::ClosedPr), MentionTone::Error);
        assert_eq!(glyph(MentionStatus::Missing), "");
    }
}
