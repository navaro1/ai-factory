//! The fixed candidate values for the Settings value lists and the
//! `opencode models` probe.
//!
//! A value list shows every legal value of one field. The values come from
//! three sources: the tables in this module, the values the pushed settings
//! state already holds for the same field and harness, and the discovered
//! OpenCode models. This module owns the first source and the join of all
//! three. It owns no terminal UI state.

use anyhow::{anyhow, Context};

use crate::config::{Harness, CLAUDE_PERMISSION_MODES, CODEX_APPROVAL_POLICIES, CODEX_SANDBOXES};
use crate::exec::Exec;

/// One Settings field that a value list can edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListField {
    /// The execution harness.
    Harness,
    /// The executable program.
    Program,
    /// The model name.
    Model,
    /// The reasoning effort.
    Effort,
    /// The OpenCode or Claude agent.
    Agent,
    /// The Codex profile.
    Profile,
    /// The Claude permission mode.
    PermissionMode,
    /// The Claude permission handler.
    PermissionHandler,
    /// The Codex approval policy.
    ApprovalPolicy,
    /// The Codex sandbox.
    Sandbox,
}

/// The fixed values that `harness` documents for `field`.
///
/// An empty result means no table exists. The values that the settings
/// state holds and the discovered OpenCode models still apply.
pub fn fixed_values(harness: Harness, field: ListField) -> Vec<String> {
    let none: &[&str] = &[];
    let values: &[&str] = match field {
        ListField::Harness => &["claude", "opencode", "codex"],
        ListField::Program => &[harness.program()],
        ListField::Model => match harness {
            Harness::Claude => &["fable", "opus", "sonnet"],
            Harness::Opencode | Harness::Codex => none,
        },
        ListField::Effort => match harness {
            Harness::Claude => &["low", "medium", "high", "xhigh", "max"],
            Harness::Opencode => &["minimal", "low", "medium", "high", "max"],
            Harness::Codex => &["minimal", "low", "medium", "high", "xhigh"],
        },
        ListField::Agent => match harness {
            Harness::Opencode => &["build", "plan", "general"],
            Harness::Claude | Harness::Codex => none,
        },
        ListField::Profile => none,
        ListField::PermissionMode => CLAUDE_PERMISSION_MODES,
        ListField::PermissionHandler => &["inbox"],
        ListField::ApprovalPolicy => CODEX_APPROVAL_POLICIES,
        ListField::Sandbox => CODEX_SANDBOXES,
    };
    values.iter().map(|value| (*value).to_string()).collect()
}

/// True when the field accepts one free text value.
///
/// The value list of an open field ends with a `custom value...` row that
/// opens the text box. A closed field leaves no value unreachable, so it
/// shows no such row.
pub fn open(field: ListField) -> bool {
    matches!(
        field,
        ListField::Program
            | ListField::Model
            | ListField::Effort
            | ListField::Agent
            | ListField::Profile
            | ListField::PermissionHandler
    )
}

/// True when the field may hold no value.
///
/// The value list of an optional field starts with a `(none)` row that
/// clears the field.
pub fn optional(field: ListField) -> bool {
    matches!(
        field,
        ListField::Effort
            | ListField::Agent
            | ListField::Profile
            | ListField::PermissionMode
            | ListField::PermissionHandler
            | ListField::ApprovalPolicy
            | ListField::Sandbox
    )
}

/// The harness that a value list row stands for.
pub fn harness_value(value: &str) -> Option<Harness> {
    match value {
        "claude" => Some(Harness::Claude),
        "opencode" => Some(Harness::Opencode),
        "codex" => Some(Harness::Codex),
        _ => None,
    }
}

/// Join candidate values into one sorted, deduplicated list.
pub fn join_candidates(sources: impl IntoIterator<Item = Vec<String>>) -> Vec<String> {
    let mut joined = sources
        .into_iter()
        .flat_map(|source| source.into_iter())
        .collect::<Vec<_>>();
    joined.sort();
    joined.dedup();
    joined
}

/// Parse the stdout of `opencode models` into model names.
///
/// The command prints one model per line. The parse trims each line, drops
/// blank lines and ANSI color codes, and keeps the file order.
pub fn parse_opencode_models(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|line| strip_ansi(line).trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Run `opencode models` once and parse its model list.
pub fn fetch_opencode_models(exec: &dyn Exec) -> anyhow::Result<Vec<String>> {
    let output = exec
        .run("opencode", &["models"], None)
        .context("cannot run opencode models")?;
    if output.status != 0 {
        return Err(anyhow!(
            "opencode models exited with status {}: {}",
            output.status,
            output.stderr.trim()
        ));
    }
    Ok(parse_opencode_models(&output.stdout))
}

/// Remove the ANSI escape sequences of one line.
fn strip_ansi(line: &str) -> String {
    let mut clean = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(character) = chars.next() {
        if character != '\x1b' {
            clean.push(character);
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        for follower in chars.by_ref() {
            if follower.is_ascii_alphabetic() {
                break;
            }
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CmdOut, ScriptExec};

    #[test]
    fn fixed_values_follow_the_harness_tables() {
        assert_eq!(
            fixed_values(Harness::Claude, ListField::Effort),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(
            fixed_values(Harness::Opencode, ListField::Effort),
            ["minimal", "low", "medium", "high", "max"]
        );
        assert_eq!(
            fixed_values(Harness::Codex, ListField::Effort),
            ["minimal", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(
            fixed_values(Harness::Opencode, ListField::Agent),
            ["build", "plan", "general"]
        );
        assert!(fixed_values(Harness::Claude, ListField::Agent).is_empty());
        assert!(fixed_values(Harness::Codex, ListField::Profile).is_empty());
        assert_eq!(fixed_values(Harness::Codex, ListField::Program), ["codex"]);
        assert_eq!(
            fixed_values(Harness::Claude, ListField::PermissionMode),
            CLAUDE_PERMISSION_MODES
        );
        assert_eq!(
            fixed_values(Harness::Claude, ListField::ApprovalPolicy),
            CODEX_APPROVAL_POLICIES
        );
        assert_eq!(
            fixed_values(Harness::Claude, ListField::Sandbox),
            CODEX_SANDBOXES
        );
        assert_eq!(
            fixed_values(Harness::Claude, ListField::PermissionHandler),
            ["inbox"]
        );
    }

    #[test]
    fn the_harness_table_lists_the_three_harnesses() {
        assert_eq!(
            fixed_values(Harness::Claude, ListField::Harness),
            ["claude", "opencode", "codex"]
        );
        assert_eq!(harness_value("opencode"), Some(Harness::Opencode));
        assert_eq!(harness_value("codex"), Some(Harness::Codex));
        assert_eq!(harness_value("claude"), Some(Harness::Claude));
        assert_eq!(harness_value("other"), None);
    }

    #[test]
    fn the_candidate_join_sorts_and_deduplicates_every_source() {
        let joined = join_candidates([
            vec!["sonnet".to_string(), "opus".to_string()],
            vec!["opus".to_string(), "zai/glm".to_string()],
            vec!["zai/glm".to_string()],
        ]);
        assert_eq!(joined, ["opus", "sonnet", "zai/glm"]);
        assert!(join_candidates(Vec::<Vec<String>>::new()).is_empty());
    }

    #[test]
    fn open_and_optional_cover_the_documented_fields() {
        for field in [
            ListField::Program,
            ListField::Model,
            ListField::Effort,
            ListField::Agent,
            ListField::Profile,
            ListField::PermissionHandler,
        ] {
            assert!(open(field), "{field:?} must stay open");
        }
        for field in [
            ListField::Harness,
            ListField::PermissionMode,
            ListField::ApprovalPolicy,
            ListField::Sandbox,
        ] {
            assert!(!open(field), "{field:?} must stay closed");
        }
        for field in [
            ListField::Effort,
            ListField::Agent,
            ListField::Profile,
            ListField::PermissionMode,
            ListField::PermissionHandler,
            ListField::ApprovalPolicy,
            ListField::Sandbox,
        ] {
            assert!(optional(field), "{field:?} must stay optional");
        }
        for field in [ListField::Harness, ListField::Program, ListField::Model] {
            assert!(!optional(field), "{field:?} must stay required");
        }
    }

    #[test]
    fn the_model_parse_reads_one_model_per_line_through_script_exec() {
        let exec = ScriptExec::new().expect(
            |call| call.program == "opencode" && call.argv() == ["models"],
            CmdOut::ok(
                "\x1b[1mzai-coding-plan/glm-5.3-flash\x1b[0m\n\
                 zai-coding-plan/glm-5.3\n\
                 \n\
                 anthropic/claude-sonnet-4\n",
            ),
        );
        let models = fetch_opencode_models(&exec).expect("the probe succeeds");
        assert_eq!(
            models,
            [
                "zai-coding-plan/glm-5.3-flash",
                "zai-coding-plan/glm-5.3",
                "anthropic/claude-sonnet-4"
            ]
        );
        let calls = exec.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "opencode");
        assert_eq!(calls[0].argv(), ["models"]);
    }

    #[test]
    fn a_failed_model_probe_reports_the_exit_status() {
        let exec = ScriptExec::new().expect(
            |call| call.program == "opencode",
            CmdOut {
                status: 1,
                stdout: String::new(),
                stderr: "no provider plan".to_string(),
            },
        );
        let error = fetch_opencode_models(&exec).expect_err("the probe fails");
        assert!(error.to_string().contains("status 1"), "{error}");
        assert!(error.to_string().contains("no provider plan"), "{error}");
    }

    #[test]
    fn an_empty_model_list_parses_to_no_rows() {
        assert!(parse_opencode_models("").is_empty());
        assert!(parse_opencode_models("\n  \n").is_empty());
    }
}
