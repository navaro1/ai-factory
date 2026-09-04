//! The prompt templates of every execution role.
//!
//! Every built-in template lives here, outside the daemon, so wording
//! changes touch one file. A file `prompts/<name>.md` in the config
//! directory overrides the built-in default; [`file_name`] gives the name
//! of each role. The docs directory `docs/v0.6/prompts/` holds a reference
//! copy of each template, pinned byte for byte by a test.
//!
//! The daemon reads the prompt file of a role each time a task of that role
//! starts. So a saved prompt applies to the next task start, and a running
//! task keeps the prompt it started with. The Settings view edits the files
//! through the daemon, and [`check`] rejects a template with a placeholder
//! the role cannot fill before the file changes.
//!
//! The vocabulary rule: a template names a repository item "ticket" or
//! "PR". A `gh` command inside backticks keeps the GitHub word, because
//! the CLI speaks its own nouns.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::ExecutionRole;

/// The notice that precedes the rendered prompt of a task the daemon
/// interrupted with a stop.
///
/// The daemon prepends it only to a run that resumes the saved session of
/// such a task. The agent reads the worktree to find its place instead of
/// repeating finished work.
pub const RESTART_NOTICE: &str = "Note: the AI Factory daemon stopped and restarted \
while this task ran. This run continues your saved session. Read the worktree \
state first to find where you stopped, then continue the remaining work. Do not \
repeat work that is already done.";

/// The built-in prompt of a refine run.
///
/// It runs in the repository checkout and never creates a worktree.
pub const REFINE_PROMPT: &str = r#"You refine ticket #{number} of {repo}
({owner_repo}). You work in {worktree}, the repository checkout. Never create
a git worktree; stay in this checkout.

Run without the operator. No person reads your text during the run. Do not
ask for approval of a plan, a design, or a change. Do not stop to report a
plan, and do not end a turn with a question. Decide with the facts you have
and act. Stop early only through the escape this prompt names.

Your goal is a complete, testable specification that minimizes delivery time.
Do not implement the change.

Read the ticket, the repository instructions, the relevant code, and its
dependencies. Confirm that the ticket is still valid. Keep the requested scope.
Use parallel tool calls for independent reads. Use subagents only for sizeable,
independent research. Use at most three subagents. Do not use a subagent for
routine reads or for a second review.

The ticket body must contain these sections:

- Problem
- Agreed approach
- Acceptance criteria
- Implementation plan

The implementation plan must use this table:

| Chunk | Goal | Owned files or paths | Depends on | Validation | Wave |
|---|---|---|---|---|---|

Create separate chunks only when the split reduces delivery time. Make each
chunk large enough to justify coordination. Put independent chunks in the same
wave only when they have no dependency and do not edit the same files. Assign
shared files and final integration to one coordinator chunk. Put a shared
interface or data contract before chunks that depend on it. State the final
integration order and final validation. For a small or tightly coupled change,
use one C1 row and state that parallel work would add delay.

Edit the ticket body with `gh`. Write a ticket comment only when it preserves
an important decision that does not belong in the body.

When you need a human decision, add the `needs-human` label to the ticket with
`gh` and state the question in a comment. Stop after the label is on. When the
decision is a choice between named answers, end the comment with one strict
block in this form. Keep the JSON on one line:
<aif-ask-v1>
{"question":"Which workload mode ships first?","options":[{"label":"Fast","description":"deterministic only"},{"label":"Full"}]}
</aif-ask-v1>

When the specification is complete, run
`gh issue edit {number} --remove-label to-refine --add-label refined`.
Run this command only after the ticket body is complete. Then report one line
that says the ticket is refined.

Ticket #{number}: {title}

{body}
"#;

/// The built-in prompt of an implement run.
pub const IMPLEMENT_PROMPT: &str = r#"You implement ticket #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

Run without the operator. No person reads your text during the run. Do not
ask for approval of a plan, a design, or a change. Do not stop to report a
plan, and do not end a turn with a question. Decide with the facts you have
and act. Stop early only through the escape this prompt names.

Your goal is a complete change that meets every acceptance criterion with the
shortest safe delivery time. Follow the repository instructions and keep the
requested scope. Implement the ticket on the current branch.

Use the ticket implementation plan as the execution schedule. If routine code
details make the plan stale, update the schedule and continue. If the ticket
has no plan, make the smallest useful schedule before edits.

For each execution wave, start ready chunks concurrently when they are
sizeable, independent, and have separate file ownership. If subagents are
available, start all agents for that wave in one tool turn. Use at most three
subagents at once. Work directly for a small, sequential, single-file, or
tightly coupled change.

Give each subagent the ticket goal, chunk identifier, exact owned paths,
satisfied dependencies, acceptance criteria, and validation command. Tell each
subagent to stay in this worktree, edit only its owned paths, and avoid all git
and `gh` writes. A subagent must not start another subagent. Never
give two concurrent writers the same file. Do not duplicate delegated work.

If subagents are unavailable, execute the chunks directly in dependency order.

After each wave, inspect every owned path and the combined diff. Treat missing
or empty subagent output as a failed chunk. Complete or repair failed work before
the next dependent wave. The coordinator owns shared files, integration, git
operations, and GitHub operations.

Run focused validation after each chunk. Run the required full validation once
after integration. Do not run several full test suites concurrently. Make the
test suite pass. Commit the integrated work in small, complete commits.

Open a draft PR with `gh pr create --draft` when the work is done. Put
`Closes #{number}` in the body. After the command succeeds, run
`gh issue edit {number} --remove-label refined`.

If the specification is incomplete, or you need a human decision, add the
`needs-human` label to ticket #{number} with `gh`, write the question into a
comment on it, and stop. Do not guess. When the decision is a choice between
named answers, end the comment with one strict block in this form. Keep the JSON
on one line:
<aif-ask-v1>
{"question":"Which workload mode ships first?","options":[{"label":"Fast","description":"deterministic only"},{"label":"Full"}]}
</aif-ask-v1>

Report one line at the end: what you did, and the PR number.

Ticket #{number}: {title}

{body}
"#;

/// The built-in prompt of a review run.
pub const REVIEW_PROMPT: &str = r#"You review PR #{number} of {repo}
({owner_repo}). You work in {worktree}, your own git worktree. Never create
another git worktree; work only in this one.

Run without the operator. No person reads your text during the run. Do not
ask for approval of a plan, a design, or a change. Do not stop to report a
plan, and do not end a turn with a question. Decide with the facts you have
and act. Stop early only through the escape this prompt names.

PR #{number}: {title}

{body}

Tickets this PR closes: {tickets}

Read the diff of the PR with `gh pr diff {number}`. Review it for
correctness, tests, and fit with the codebase. Leave your findings as a
review with `gh pr review {number}`. If it is correct, approve it and then run
`gh pr ready {number}`. If it is not correct, request changes with concrete
findings and leave it as a draft.

If the change needs a human decision, add the `needs-human` label to the
PR with `gh`, write the question into a comment, and stop. Do not
guess. When the decision is a choice between named answers, end the comment
with one strict block in this form. Keep the JSON on one line:
<aif-ask-v1>
{"question":"Which workload mode ships first?","options":[{"label":"Fast","description":"deterministic only"},{"label":"Full"}]}
</aif-ask-v1>

Report one line at the end: the review verdict.
"#;

/// The built-in prompt of a release run.
pub const RELEASE_PROMPT: &str = r#"You release the stacked PRs of {repo}
({owner_repo}). You work in {worktree}, the release worktree. Never create
another git worktree; work only in this one.

Run without the operator. No person reads your text during the run. Do not
ask for approval of a plan, a design, or a change. Do not stop to report a
plan, and do not end a turn with a question. Decide with the facts you have
and act. Stop early only through the escape this prompt names.

The batch holds {pr_count} PR(s), in merge order:

{pr_list}

Merge every PR in the listed order with `gh pr merge`, one at a
time. Merge order is {pr_numbers}. After each merge, pull the base branch
into this worktree so the next merge sees the updated state. If a merge
conflicts, stop, and report the PR number that failed.

When all merges are done, report one line: the released PRs.
"#;

/// The built-in prompt of a ticket-creation session.
pub const TICKET_PROMPT: &str = r#"You help the operator create one ticket in the
repository {repo} ({owner_repo}). You work in {worktree}, the repository
checkout. Never create a git worktree; stay in this checkout.

Ask the operator what the ticket should say, in short questions, one topic at
a time. When you know enough, draft the title and body, show them, and on
approval create the ticket with `gh issue create`. Report the new ticket
number.

If the operator asks for something you cannot decide alone, say so plainly
and ask again.
"#;

/// The built-in prompt of a ticket conversation.
pub const TICKET_CHAT_PROMPT: &str = r#"You review ticket #{number} in repository
{repo} ({owner_repo}). The repository checkout is {worktree}.

Ticket title: {title}
Ticket description:
{body}

Labels: {labels}
Author: {author}
Assignees: {assignees}
Updated: {updated_at}
GitHub reference: {github_url}

Start with analysis. Do not propose a title or description change unless the
operator explicitly requests that change.

When the operator explicitly requests a title or description change, finish
the assistant turn with exactly one complete block in this form:

<aif-ticket-proposal-v1>
{"title":"New title","body":"New description"}
</aif-ticket-proposal-v1>

Put valid JSON between the markers. Do not quote the block. Do not put the
block in a code fence. Include no text after the closing marker.
"#;

/// The placeholders the daemon fills in a stage prompt.
const STAGE_PLACEHOLDERS: &[&str] = &[
    "repo",
    "owner_repo",
    "number",
    "title",
    "body",
    "worktree",
    "tickets",
    "pr_list",
    "pr_numbers",
    "pr_count",
];

/// The placeholders the daemon fills in the ticket-creation prompt.
const TICKET_PLACEHOLDERS: &[&str] = &["repo", "owner_repo", "worktree"];

/// The placeholders the daemon fills in the ticket chat prompt.
const TICKET_CHAT_PLACEHOLDERS: &[&str] = &[
    "repo",
    "owner_repo",
    "number",
    "title",
    "body",
    "labels",
    "author",
    "assignees",
    "updated_at",
    "github_url",
    "worktree",
];

/// The file name of the prompt template of one role, inside the prompts
/// directory.
pub const fn file_name(role: ExecutionRole) -> &'static str {
    match role {
        ExecutionRole::Refine => "refine.md",
        ExecutionRole::Implement => "implement.md",
        ExecutionRole::Review => "review.md",
        ExecutionRole::Release => "release.md",
        ExecutionRole::TicketCreate => "ticket.md",
        ExecutionRole::TicketChat => "ticket-chat.md",
    }
}

/// The built-in template of one role.
pub const fn builtin(role: ExecutionRole) -> &'static str {
    match role {
        ExecutionRole::Refine => REFINE_PROMPT,
        ExecutionRole::Implement => IMPLEMENT_PROMPT,
        ExecutionRole::Review => REVIEW_PROMPT,
        ExecutionRole::Release => RELEASE_PROMPT,
        ExecutionRole::TicketCreate => TICKET_PROMPT,
        ExecutionRole::TicketChat => TICKET_CHAT_PROMPT,
    }
}

/// The placeholders the daemon fills for one role.
///
/// A template may use any subset of them. A placeholder outside the set is
/// an error, both at save time and at dispatch time.
pub const fn placeholders(role: ExecutionRole) -> &'static [&'static str] {
    match role {
        ExecutionRole::Refine
        | ExecutionRole::Implement
        | ExecutionRole::Review
        | ExecutionRole::Release => STAGE_PLACEHOLDERS,
        ExecutionRole::TicketCreate => TICKET_PLACEHOLDERS,
        ExecutionRole::TicketChat => TICKET_CHAT_PLACEHOLDERS,
    }
}

/// The path of the prompt file of one role.
pub fn path(prompts_dir: &Path, role: ExecutionRole) -> PathBuf {
    prompts_dir.join(file_name(role))
}

/// One loaded template and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// The template text.
    pub text: String,
    /// True when the text came from the prompt file. False for the built-in.
    pub from_file: bool,
}

/// Read the template of one role.
///
/// The prompt file wins when it exists. An absent file yields the built-in.
/// An unreadable file is an error that names the path.
pub fn load(prompts_dir: &Path, role: ExecutionRole) -> Result<Template> {
    let path = path(prompts_dir, role);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Template {
            text,
            from_file: true,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Template {
            text: builtin(role).to_string(),
            from_file: false,
        }),
        Err(error) => Err(anyhow!("cannot read {}: {error}", path.display())),
    }
}

/// Check one template against the placeholder set of its role.
///
/// The error names the first unknown placeholder and lists the known ones.
/// A blank template is an error too: the agent would start with no
/// instructions.
pub fn check(role: ExecutionRole, text: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("the prompt is empty");
    }
    let allowed = placeholders(role);
    if let Some(token) = scan_placeholders(text)
        .into_iter()
        .find(|token| !allowed.contains(token))
    {
        let known = allowed
            .iter()
            .map(|name| format!("{{{name}}}"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "the prompt uses the unknown placeholder {{{token}}}; the {role} prompt knows {known}"
        );
    }
    Ok(())
}

/// Write the prompt file of one role.
///
/// The text goes to a sibling temporary file first, and one rename replaces
/// the destination, so a reader never sees a half-written prompt.
pub fn save(prompts_dir: &Path, role: ExecutionRole, text: &str) -> Result<()> {
    fs::create_dir_all(prompts_dir)
        .with_context(|| format!("cannot create {}", prompts_dir.display()))?;
    let destination = path(prompts_dir, role);
    let temporary = prompts_dir.join(format!(".{}.{}.tmp", file_name(role), std::process::id()));
    let written = fs::write(&temporary, text)
        .with_context(|| format!("cannot write {}", temporary.display()))
        .and_then(|()| {
            fs::rename(&temporary, &destination).with_context(|| {
                format!(
                    "cannot rename {} to {}",
                    temporary.display(),
                    destination.display()
                )
            })
        });
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Remove the prompt file of one role, so the built-in template applies.
///
/// An absent file is not an error.
pub fn reset(prompts_dir: &Path, role: ExecutionRole) -> Result<()> {
    let path = path(prompts_dir, role);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow!("cannot remove {}: {error}", path.display())),
    }
}

/// Fill a prompt template.
///
/// Every placeholder must be known; an unknown one is an error that names
/// it, never a silent literal. A filled value stays literal: a `{body}`
/// inside a ticket title is not filled again.
pub fn fill_template(template: &str, values: &[(&str, String)]) -> Result<String> {
    for token in scan_placeholders(template) {
        if !values.iter().any(|(name, _)| *name == token) {
            bail!("the prompt template uses the unknown placeholder {{{token}}}");
        }
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let token = &after[..end];
        if let Some((_, value)) = values.iter().find(|(name, _)| *name == token) {
            out.push_str(value);
        } else {
            out.push_str(&rest[start..start + end + 2]);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// List the `{placeholder}` tokens of a template, in first-seen order.
///
/// A token is placeholder-shaped when it holds only ASCII letters, digits,
/// underscores, and hyphens. Other brace content stays untouched.
pub fn scan_placeholders(template: &str) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            break;
        };
        let token = &after[..end];
        if !token.is_empty()
            && !token.contains('{')
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && !found.contains(&token)
        {
            found.push(token);
        }
        rest = &after[end + 1..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty prompts directory under the system temporary root.
    fn temp_prompts_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aif-prompts-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_role_has_one_file_name_and_its_builtin_template() {
        let names = ExecutionRole::ALL.map(file_name);
        for (index, name) in names.iter().enumerate() {
            assert!(name.ends_with(".md"), "{name} is not a markdown file");
            assert!(
                !names[..index].contains(name),
                "{name} is the file name of two roles"
            );
        }
        assert_eq!(file_name(ExecutionRole::TicketCreate), "ticket.md");
        assert_eq!(file_name(ExecutionRole::TicketChat), "ticket-chat.md");
        assert_eq!(builtin(ExecutionRole::Refine), REFINE_PROMPT);
        assert_eq!(builtin(ExecutionRole::TicketChat), TICKET_CHAT_PROMPT);
    }

    #[test]
    fn every_builtin_template_passes_the_check_of_its_role() {
        for role in ExecutionRole::ALL {
            check(role, builtin(role)).unwrap_or_else(|error| {
                panic!("the built-in {role} prompt fails its own check: {error:#}")
            });
            for token in scan_placeholders(builtin(role)) {
                assert!(
                    placeholders(role).contains(&token),
                    "the built-in {role} prompt uses {{{token}}} outside its placeholder set"
                );
            }
        }
    }

    #[test]
    fn the_check_rejects_an_unknown_placeholder_and_an_empty_prompt() {
        let error = check(ExecutionRole::Implement, "hello {frobnicate}").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("{frobnicate}"), "{message}");
        assert!(message.contains("stage.implement"), "{message}");
        assert!(message.contains("{number}"), "{message}");

        let error = check(ExecutionRole::TicketCreate, "ticket #{number}").unwrap_err();
        assert!(error.to_string().contains("{number}"));

        let error = check(ExecutionRole::Refine, " \n\t").unwrap_err();
        assert_eq!(error.to_string(), "the prompt is empty");

        check(ExecutionRole::Release, "release {pr_list} now").unwrap();
        check(
            ExecutionRole::Review,
            r#"literal json {"question":"x"} keeps {number}"#,
        )
        .unwrap();
    }

    #[test]
    fn load_prefers_the_file_and_falls_back_to_the_builtin() {
        let dir = temp_prompts_dir("load");
        let missing = dir.join("absent");
        let loaded = load(&missing, ExecutionRole::Review).unwrap();
        assert_eq!(
            loaded,
            Template {
                text: REVIEW_PROMPT.to_string(),
                from_file: false
            }
        );

        save(&missing, ExecutionRole::Review, "custom {number}\n").unwrap();
        assert_eq!(
            fs::read_to_string(missing.join("review.md")).unwrap(),
            "custom {number}\n"
        );
        assert!(
            fs::read_dir(&missing).unwrap().count() == 1,
            "the save leaves no temporary file behind"
        );
        let loaded = load(&missing, ExecutionRole::Review).unwrap();
        assert_eq!(
            loaded,
            Template {
                text: "custom {number}\n".to_string(),
                from_file: true
            }
        );

        reset(&missing, ExecutionRole::Review).unwrap();
        reset(&missing, ExecutionRole::Review).unwrap();
        assert!(!missing.join("review.md").exists());
        assert!(!load(&missing, ExecutionRole::Review).unwrap().from_file);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_placeholders_finds_placeholder_shaped_tokens() {
        assert_eq!(scan_placeholders("{a} x {b_1} {a}"), vec!["a", "b_1"]);
        assert!(scan_placeholders("{not a} {} {unclosed {oops}").is_empty());
    }

    #[test]
    fn fill_template_rejects_an_unknown_placeholder_and_fills_known_ones() {
        let error = fill_template("hi {name} {other}", &[("name", "x".to_string())]).unwrap_err();
        assert!(error.to_string().contains("other"));
        let filled = fill_template("hi {name}", &[("name", "x".to_string())]).unwrap();
        assert_eq!(filled, "hi x");

        let error = fill_template("hi {not-known}", &[("name", "x".to_string())]).unwrap_err();
        assert!(error.to_string().contains("not-known"));

        let filled = fill_template(
            "title={title}; body={body}",
            &[
                ("title", "keep {body} literal".to_string()),
                ("body", "body text".to_string()),
            ],
        )
        .unwrap();
        assert_eq!(filled, "title=keep {body} literal; body=body text");
    }

    #[test]
    fn fill_template_fills_the_tickets_placeholder() {
        let filled = fill_template(
            "Tickets this PR closes: {tickets}",
            &[("tickets", "#4, #9".to_string())],
        )
        .unwrap();
        assert_eq!(filled, "Tickets this PR closes: #4, #9");

        let error =
            fill_template("hi {tickets} {nope}", &[("tickets", "none".to_string())]).unwrap_err();
        assert!(error.to_string().contains("nope"));
    }

    /// Drop every backtick span from one line.
    ///
    /// A `gh` command inside backticks keeps the GitHub nouns, so the ban test
    /// reads the line without the command text.
    fn strip_backticks(line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut inside = false;
        for character in line.chars() {
            match character {
                '`' => inside = !inside,
                _ if !inside => out.push(character),
                _ => {}
            }
        }
        out
    }
    /// Every template must carry the vocabulary.
    const VOCABULARY_PROMPTS: [&str; 6] = [
        REFINE_PROMPT,
        IMPLEMENT_PROMPT,
        REVIEW_PROMPT,
        RELEASE_PROMPT,
        TICKET_PROMPT,
        TICKET_CHAT_PROMPT,
    ];

    #[test]
    fn the_vocabulary_prompts_use_only_ticket_and_pr() {
        for prompt in VOCABULARY_PROMPTS {
            for line in prompt.lines() {
                let bare = strip_backticks(line).to_lowercase();
                assert!(
                    !bare.contains("issue"),
                    "a line breaks the vocabulary with \"issue\": {line}"
                );
                assert!(
                    !bare.contains("pull request"),
                    "a line breaks the vocabulary with \"pull request\": {line}"
                );
            }
        }
    }

    #[test]
    fn backtick_stripping_removes_only_command_text() {
        assert_eq!(strip_backticks("run `gh issue edit 7` now"), "run  now");
        assert_eq!(strip_backticks("no commands here"), "no commands here");
    }

    #[test]
    fn the_docs_copies_match_the_consts_byte_for_byte() {
        assert_eq!(
            REFINE_PROMPT,
            include_str!("../docs/v0.6/prompts/refine.md")
        );
        assert_eq!(
            IMPLEMENT_PROMPT,
            include_str!("../docs/v0.6/prompts/implement.md")
        );
        assert_eq!(
            RELEASE_PROMPT,
            include_str!("../docs/v0.6/prompts/release.md")
        );
        assert_eq!(
            TICKET_PROMPT,
            include_str!("../docs/v0.6/prompts/ticket.md")
        );
        assert_eq!(
            TICKET_CHAT_PROMPT,
            include_str!("../docs/v0.6/prompts/ticket-chat.md")
        );
        assert_eq!(
            REVIEW_PROMPT,
            include_str!("../docs/v0.6/prompts/review.md")
        );
    }

    #[test]
    fn the_choice_prompts_show_a_block_that_the_ask_parser_accepts() {
        for prompt in [REFINE_PROMPT, IMPLEMENT_PROMPT, REVIEW_PROMPT] {
            let ask = crate::ask::parse_ask_block(prompt)
                .expect("the choice prompt must contain one valid ask block");
            assert_eq!(ask.question, "Which workload mode ships first?");
            assert_eq!(ask.options.len(), 2);
        }
    }

    #[test]
    fn the_refine_prompt_defines_a_parallel_execution_plan() {
        let prompt = REFINE_PROMPT
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for required in [
            "| Chunk | Goal | Owned files or paths | Depends on | Validation | Wave |",
            "Put independent chunks in the same",
            "do not edit the same files",
            "Assign shared files and final integration to one coordinator chunk",
            "use one C1 row",
            "Use at most three subagents",
        ] {
            assert!(prompt.contains(required), "missing: {required}");
        }
    }

    #[test]
    fn the_implement_prompt_consumes_parallel_waves_safely() {
        let prompt = IMPLEMENT_PROMPT
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for required in [
            "Use the ticket implementation plan as the execution schedule",
            "start all agents for that wave in one tool turn",
            "Use at most three subagents at once",
            "avoid all git and `gh` writes",
            "Never give two concurrent writers the same file",
            "If subagents are unavailable, execute the chunks directly",
            "The coordinator owns shared files, integration, git operations, and GitHub operations",
        ] {
            assert!(prompt.contains(required), "missing: {required}");
        }
    }

    #[test]
    fn the_stage_prompts_run_without_the_operator() {
        let paragraph = "Run without the operator. No person reads your text during \
the run. Do not ask for approval of a plan, a design, or a change. Do not \
stop to report a plan, and do not end a turn with a question. Decide with \
the facts you have and act. Stop early only through the escape this prompt \
names.";
        for (prompt, opening_end) in [
            (REFINE_PROMPT, "stay in this checkout."),
            (IMPLEMENT_PROMPT, "work only in this one."),
            (REVIEW_PROMPT, "work only in this one."),
            (RELEASE_PROMPT, "work only in this one."),
        ] {
            let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
            let position = normalized
                .find(paragraph)
                .expect("the stage prompt holds the autonomy paragraph");
            let before = normalized[..position].trim_end();
            assert!(
                before.ends_with(opening_end),
                "the autonomy paragraph does not follow the opening paragraph: {before}"
            );
        }
        for prompt in [TICKET_PROMPT, TICKET_CHAT_PROMPT] {
            let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                !normalized.contains(paragraph),
                "a ticket prompt must not hold the autonomy paragraph"
            );
        }
    }
}
