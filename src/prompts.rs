//! The prompt templates of every execution role.
//!
//! Every built-in template lives here, outside the daemon, so wording
//! changes touch one file. A file `prompts/<name>.md` in the config
//! directory overrides the built-in default; [`file_name`] gives the name
//! of each role and [`ROLES`] lists the roles that have one. The docs
//! directory `docs/v0.6/prompts/` holds a reference copy of each template,
//! pinned byte for byte by a test.
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
use std::io::Write;
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

You are the last agent on this change. You repair every finding yourself. You
never hand a finding back to the author. The PR must leave your run ready for
review, or labelled `needs-human`.

Read the diff of the PR with `gh pr diff {number}`. Review it for
correctness, tests, and fit with the codebase. Read the repository
instructions and the linked tickets.

Before your first edit, check whether the PR comes from a fork:
`gh pr view {number} --json isCrossRepository --jq .isCrossRepository`.
When the command prints `true`, take the human path. Never push a fork repair
to `origin`.

Before your first edit, prove that this worktree holds the PR head. Compare
`gh pr view {number} --json headRefOid --jq .headRefOid` with
`git rev-parse HEAD`. When the two differ, run
`git fetch origin pull/{number}/head` and then `git reset --hard FETCH_HEAD`.

Fix every finding in this worktree. Add the missing tests. Keep the scope of
the linked tickets. Run the full validation of the repository and make it
pass. Commit the repairs in small, complete commits.

Push once, at the end of the run. A push on a draft PR can restart your own
review, so never push a partial fix. Push the commits and open the release
gate in one command line:

`git push origin HEAD:$(gh pr view {number} --json headRefName --jq .headRefName) && gh pr ready {number}`

Never pass `--force`. Never merge the PR.

Record the outcome with `gh pr comment {number}`. Name the findings, the
repairs, and the validation result. GitHub refuses a formal review of your own
PR, so this comment is the record.

When the PR needs no repair, post the record and run `gh pr ready {number}`.

Take the human path when the PR comes from a fork, when a finding needs a human
decision, when the repair leaves the scope of the linked tickets, or when the
push fails. On that path, add the `needs-human` label to the PR with `gh`, write
the question into a comment, leave the draft, and stop. Do not guess. When the
decision is a choice between named answers, end the comment with one strict
block in this form. Keep the JSON on one line:
<aif-ask-v1>
{"question":"Which workload mode ships first?","options":[{"label":"Fast","description":"deterministic only"},{"label":"Full"}]}
</aif-ask-v1>

Report one line at the end: the review verdict, and the number of commits you
pushed.
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

/// Every role the daemon renders a prompt for, in role order.
///
/// The theory roles are absent. `theory.audit` and `theory.chat` carry no
/// template yet, and each will take one template per task purpose, not one
/// per role. A prompt file for them would sit unread, so the daemon
/// publishes no prompt view for them and the Settings view shows no prompt
/// row on them.
pub const ROLES: [ExecutionRole; 6] = [
    ExecutionRole::Refine,
    ExecutionRole::Implement,
    ExecutionRole::Review,
    ExecutionRole::Release,
    ExecutionRole::TicketCreate,
    ExecutionRole::TicketChat,
];

/// The file name of the prompt template of one role, inside the prompts
/// directory, or `None` for a role with no template.
pub const fn file_name(role: ExecutionRole) -> Option<&'static str> {
    match role {
        ExecutionRole::Refine => Some("refine.md"),
        ExecutionRole::Implement => Some("implement.md"),
        ExecutionRole::Review => Some("review.md"),
        ExecutionRole::Release => Some("release.md"),
        ExecutionRole::TicketCreate => Some("ticket.md"),
        ExecutionRole::TicketChat => Some("ticket-chat.md"),
        ExecutionRole::TheoryAudit | ExecutionRole::TheoryChat => None,
    }
}

/// The built-in template of one role, or `None` for a role with no
/// template.
pub const fn builtin(role: ExecutionRole) -> Option<&'static str> {
    match role {
        ExecutionRole::Refine => Some(REFINE_PROMPT),
        ExecutionRole::Implement => Some(IMPLEMENT_PROMPT),
        ExecutionRole::Review => Some(REVIEW_PROMPT),
        ExecutionRole::Release => Some(RELEASE_PROMPT),
        ExecutionRole::TicketCreate => Some(TICKET_PROMPT),
        ExecutionRole::TicketChat => Some(TICKET_CHAT_PROMPT),
        ExecutionRole::TheoryAudit | ExecutionRole::TheoryChat => None,
    }
}

/// The placeholders the daemon fills for one role, or `None` for a role
/// with no template.
///
/// A template may use any subset of them. A placeholder outside the set is
/// an error, both at save time and at dispatch time.
pub const fn placeholders(role: ExecutionRole) -> Option<&'static [&'static str]> {
    match role {
        ExecutionRole::Refine
        | ExecutionRole::Implement
        | ExecutionRole::Review
        | ExecutionRole::Release => Some(STAGE_PLACEHOLDERS),
        ExecutionRole::TicketCreate => Some(TICKET_PLACEHOLDERS),
        ExecutionRole::TicketChat => Some(TICKET_CHAT_PLACEHOLDERS),
        ExecutionRole::TheoryAudit | ExecutionRole::TheoryChat => None,
    }
}

/// The path of the prompt file of one role, or an error naming a role with
/// no template.
pub fn path(prompts_dir: &Path, role: ExecutionRole) -> Result<PathBuf> {
    Ok(prompts_dir.join(name_of(role)?))
}

/// The file name of one role, or an error that names the role.
fn name_of(role: ExecutionRole) -> Result<&'static str> {
    file_name(role).ok_or_else(|| anyhow!("the {role} role has no prompt template"))
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
/// An unreadable file is an error that names the path. A role with no
/// template is an error that names the role.
pub fn load(prompts_dir: &Path, role: ExecutionRole) -> Result<Template> {
    let path = path(prompts_dir, role)?;
    let builtin = builtin(role).ok_or_else(|| anyhow!("the {role} role has no prompt template"))?;
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Template {
            text,
            from_file: true,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Template {
            text: builtin.to_string(),
            from_file: false,
        }),
        Err(error) => Err(anyhow!("cannot read {}: {error}", path.display())),
    }
}

/// Check one template against the placeholder set of its role.
///
/// The error names the first unknown placeholder and lists the known ones.
/// A blank template is an error too: the agent would start with no
/// instructions. A role with no template is an error that names the role.
pub fn check(role: ExecutionRole, text: &str) -> Result<()> {
    let allowed =
        placeholders(role).ok_or_else(|| anyhow!("the {role} role has no prompt template"))?;
    if text.trim().is_empty() {
        bail!("the prompt is empty");
    }
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
/// the destination, so a reader never sees a half-written prompt. A role
/// with no template is an error that names the role.
pub fn save(prompts_dir: &Path, role: ExecutionRole, text: &str) -> Result<()> {
    let name = name_of(role)?;
    fs::create_dir_all(prompts_dir)
        .with_context(|| format!("cannot create {}", prompts_dir.display()))?;
    let destination = prompts_dir.join(name);
    let temporary = prompts_dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = write_all_synced(&temporary, text)
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

/// Write one file and flush it to the storage device.
///
/// The flush matters because a rename follows: without it a crash can
/// leave the renamed destination empty, and an empty prompt would start an
/// agent with no instructions.
fn write_all_synced(path: &Path, text: &str) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// Remove the prompt file of one role, so the built-in template applies.
///
/// An absent file is not an error. A role with no template is an error that
/// names the role.
pub fn reset(prompts_dir: &Path, role: ExecutionRole) -> Result<()> {
    let path = path(prompts_dir, role)?;
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
    while let Some((start, end, token)) = next_span(rest) {
        out.push_str(&rest[..start]);
        match values.iter().find(|(name, _)| *name == token) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[start..end]),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// List the `{placeholder}` tokens of a template, in first-seen order.
///
/// A token is placeholder-shaped when it holds only ASCII letters, digits,
/// underscores, and hyphens. Other brace content stays untouched.
///
/// This walk and the one in [`fill_template`] share [`next_span`], so a
/// template that passes [`check`] always fills.
pub fn scan_placeholders(template: &str) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    let mut rest = template;
    while let Some((_, end, token)) = next_span(rest) {
        if placeholder_shaped(token) && !found.contains(&token) {
            found.push(token);
        }
        rest = &rest[end..];
    }
    found
}

/// The next `{...}` span of `text`: the start byte, the byte after the
/// closing brace, and the text between the braces.
///
/// A `{` inside a span means the true opener is the later one, so
/// `{ and {number}` holds the span `{number}`. One stray brace therefore
/// never hides the placeholder behind it. Every index sits on an ASCII
/// brace, so every slice keeps a character boundary.
fn next_span(text: &str) -> Option<(usize, usize, &str)> {
    let mut from = 0;
    loop {
        let open = from + text[from..].find('{')?;
        let after = &text[open + 1..];
        let close = after.find('}')?;
        match after[..close].rfind('{') {
            Some(inner) => from = open + 1 + inner,
            None => return Some((open, open + close + 2, &after[..close])),
        }
    }
}

/// True when a span between braces may name a placeholder.
fn placeholder_shaped(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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
        let names = ROLES.map(|role| file_name(role).expect("a listed role names a file"));
        for (index, name) in names.iter().enumerate() {
            assert!(name.ends_with(".md"), "{name} is not a markdown file");
            assert!(
                !names[..index].contains(name),
                "{name} is the file name of two roles"
            );
        }
        assert_eq!(file_name(ExecutionRole::TicketCreate), Some("ticket.md"));
        assert_eq!(file_name(ExecutionRole::TicketChat), Some("ticket-chat.md"));
        assert_eq!(builtin(ExecutionRole::Refine), Some(REFINE_PROMPT));
        assert_eq!(builtin(ExecutionRole::TicketChat), Some(TICKET_CHAT_PROMPT));
    }

    #[test]
    fn every_builtin_template_passes_the_check_of_its_role() {
        for role in ROLES {
            let text = builtin(role).expect("a listed role has a built-in template");
            check(role, text).unwrap_or_else(|error| {
                panic!("the built-in {role} prompt fails its own check: {error:#}")
            });
            let allowed = placeholders(role).expect("a listed role has a placeholder set");
            for token in scan_placeholders(text) {
                assert!(
                    allowed.contains(&token),
                    "the built-in {role} prompt uses {{{token}}} outside its placeholder set"
                );
            }
            // Every literal `{word}` of the text must reach the scan. A
            // stray brace that hid one would leave it unfilled at dispatch.
            for literal in text.split('{').skip(1).filter_map(|rest| {
                rest.find('}')
                    .map(|end| &rest[..end])
                    .filter(|token| placeholder_shaped(token))
            }) {
                assert!(
                    scan_placeholders(text).contains(&literal),
                    "the built-in {role} prompt hides {{{literal}}} from the scan"
                );
            }
        }
    }

    /// The theory roles carry no template yet. Every prompt entry point
    /// refuses them by name, so nothing writes a file that no task reads.
    #[test]
    fn the_theory_roles_have_no_prompt_and_every_entry_point_refuses_them() {
        assert_eq!(
            ROLES.len() + 2,
            ExecutionRole::ALL.len(),
            "only the two theory roles stay outside ROLES"
        );
        let dir = temp_prompts_dir("theory");
        for role in [ExecutionRole::TheoryAudit, ExecutionRole::TheoryChat] {
            assert!(!ROLES.contains(&role), "{role}");
            assert_eq!(file_name(role), None, "{role}");
            assert_eq!(builtin(role), None, "{role}");
            assert!(placeholders(role).is_none(), "{role}");
            let expected = format!("the {role} role has no prompt template");
            for result in [
                path(&dir, role).map(|_| ()),
                load(&dir, role).map(|_| ()),
                check(role, "anything"),
                save(&dir, role, "anything"),
                reset(&dir, role),
            ] {
                assert_eq!(result.unwrap_err().to_string(), expected);
            }
        }
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "no file was written"
        );
        fs::remove_dir_all(dir).unwrap();
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
        assert!(scan_placeholders("{not a} {} {unclosed").is_empty());
        assert_eq!(scan_placeholders(r#"{"question":"x"} {a}"#), vec!["a"]);
    }

    /// One stray `{` must not hide the placeholder behind it. The scan and
    /// the fill share one span rule, so a template that passes the check
    /// always fills: every placeholder-shaped token the fill would leave
    /// literal is a token the scan reports.
    #[test]
    fn a_stray_brace_never_hides_the_placeholder_behind_it() {
        for template in [
            "Use { as a brace. Ticket {number}.",
            "Ticket {number}. See { and {frobnicate}.",
            "{ {number} }",
            "{unclosed {oops}",
        ] {
            let scanned = scan_placeholders(template);
            let filled = fill_template(
                template,
                &scanned
                    .iter()
                    .map(|name| (*name, format!("<{name}>")))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            assert!(
                scan_placeholders(&filled).is_empty(),
                "{template:?} left a placeholder unfilled in {filled:?}"
            );
        }

        assert_eq!(
            scan_placeholders("Use { as a brace. Ticket {number}."),
            vec!["number"]
        );
        let error = check(ExecutionRole::Implement, "See { and {frobnicate}.").unwrap_err();
        assert!(error.to_string().contains("{frobnicate}"), "{error:#}");
        assert_eq!(
            fill_template(
                "Use { as a brace. #{number}",
                &[("number", "142".to_string())]
            )
            .unwrap(),
            "Use { as a brace. #142"
        );
    }

    /// The parsers index by byte but cut only on ASCII braces, so a
    /// multi-byte template neither panics nor loses a character.
    #[test]
    fn the_parsers_keep_multibyte_text_whole() {
        let template = "zażółć {number} — gęślą jaźń {title}\n";
        assert_eq!(scan_placeholders(template), vec!["number", "title"]);
        assert_eq!(
            fill_template(
                template,
                &[("number", "142".to_string()), ("title", "łódź".to_string()),],
            )
            .unwrap(),
            "zażółć 142 — gęślą jaźń łódź\n"
        );
        assert!(scan_placeholders("żółw {ą} ok").is_empty());
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
    fn the_review_prompt_mandates_a_repair_a_push_and_the_ready_flip() {
        let prompt = REVIEW_PROMPT
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for required in [
            "You repair every finding yourself",
            "ready for review, or labelled `needs-human`",
            "gh pr view {number} --json isCrossRepository --jq .isCrossRepository",
            "Never push a fork repair to `origin`",
            "Take the human path when the PR comes from a fork",
            "prove that this worktree holds the PR head",
            "Push once, at the end of the run",
            "git push origin HEAD:$(gh pr view {number} --json headRefName --jq .headRefName) && gh pr ready {number}",
            "Never pass `--force`. Never merge the PR.",
            "GitHub refuses a formal review of your own PR",
            "add the `needs-human` label to the PR",
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
