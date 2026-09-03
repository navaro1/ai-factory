use std::path::Path;

use aif::config::{
    edit_config_text, Config, ExecutionRole, Governor, RoleOverride, SettingsEdit, Weekday,
};
use aif::sock::SettingsView;

const BASE: &str = r#"
schema_version = 1

[stage.refine]
harness = "claude"
model = "claude-opus-5[1m]"

[stage.implement]
harness = "opencode"
model = "zai-coding-plan/glm-5.3-flash"

[stage.review]
harness = "claude"
model = "claude-opus-5[1m]"
limit = 7

[stage.release]
harness = "claude"
model = "claude-opus-5[1m]"
limit = 1

[ticket.create]
harness = "claude"
model = "claude-opus-5[1m]"

[ticket.chat]
harness = "claude"
model = "claude-opus-5[1m]"

[repo.borsuk]
path = "/tmp/borsuk"
"#;

fn theory_tables() -> String {
    "\n[theory.audit]\nharness = \"claude\"\nmodel = \"claude-opus-5[1m]\"\n\n\
     [theory.chat]\nharness = \"claude\"\nmodel = \"claude-opus-5[1m]\"\n"
        .to_string()
}

#[test]
fn a_repository_uses_the_governor_and_window_fields_and_defaults_the_rest() {
    let text = BASE.replace(
        "[repo.borsuk]\npath = \"/tmp/borsuk\"",
        "[repo.borsuk]\npath = \"/tmp/borsuk\"\ngovernor = \"off\"\nwindow = 5",
    );

    let config = Config::parse(&text).expect("the theory fields must parse");
    let repo = &config.repos["borsuk"];

    assert_eq!(repo.theory.governor, Governor::Off);
    assert!(!repo.theory.governor.is_on());
    assert_eq!(repo.theory.window, 5);
    assert_eq!(repo.theory.theory, None);
    assert_eq!(
        repo.theory.sweep,
        aif::config::Sweep {
            days: 7,
            after_train: true
        }
    );
    assert_eq!(
        repo.theory.cards,
        aif::config::Cards {
            per_day: 3,
            stale_days: 30
        }
    );
    assert_eq!(
        repo.theory.interview,
        aif::config::Interview {
            weekday: Weekday::Monday,
            minutes: 20
        }
    );
}

#[test]
fn each_zero_value_fails_with_its_full_key_path() {
    let cases = [
        ("governor = \"off\"\nwindow = 0", "repo.borsuk.window"),
        ("governor = \"maybe\"", "repo.borsuk.governor"),
        ("sweep = { days = 0 }", "repo.borsuk.sweep.days"),
        ("cards = { per_day = 0 }", "repo.borsuk.cards.per_day"),
        ("cards = { stale_days = 0 }", "repo.borsuk.cards.stale_days"),
        (
            "interview = { minutes = 0 }",
            "repo.borsuk.interview.minutes",
        ),
    ];
    for (field, expected) in cases {
        let text = BASE.replace(
            "[repo.borsuk]\npath = \"/tmp/borsuk\"",
            &format!("[repo.borsuk]\npath = \"/tmp/borsuk\"\n{field}"),
        );
        let error = Config::parse(&text).expect_err("the zero value must fail");
        assert!(
            format!("{error:#}").contains(expected),
            "{field}: error was {error:#}"
        );
    }
}

#[test]
fn a_partial_sub_table_keeps_the_other_defaults() {
    let text = BASE.replace(
        "[repo.borsuk]\npath = \"/tmp/borsuk\"",
        "[repo.borsuk]\npath = \"/tmp/borsuk\"\nsweep = { days = 5 }\ninterview = { weekday = \"friday\" }",
    );

    let config = Config::parse(&text).expect("the partial tables must parse");
    let theory = &config.repos["borsuk"].theory;

    assert!(theory.sweep.after_train);
    assert_eq!(theory.sweep.days, 5);
    assert_eq!(theory.interview.weekday, Weekday::Friday);
    assert_eq!(theory.interview.minutes, 20);

    let bad = BASE.replace(
        "[repo.borsuk]\npath = \"/tmp/borsuk\"",
        "[repo.borsuk]\npath = \"/tmp/borsuk\"\ninterview = { weekday = \"funday\" }",
    );
    let error = Config::parse(&bad).expect_err("the unknown weekday must fail");
    assert!(format!("{error:#}").contains("repo.borsuk.interview.weekday"));
}

#[test]
fn unknown_keys_stay_rejected_in_the_repository_and_its_sub_tables() {
    let cases = ["unknown = true", "sweep = { days = 5, unknown = true }"];
    for field in cases {
        let text = BASE.replace(
            "[repo.borsuk]\npath = \"/tmp/borsuk\"",
            &format!("[repo.borsuk]\npath = \"/tmp/borsuk\"\n{field}"),
        );
        let error = Config::parse(&text).expect_err("the unknown key must fail");
        assert!(
            format!("{error:#}").contains("unknown field"),
            "{field}: error was {error:#}"
        );
    }
}

#[test]
fn a_theory_table_requires_a_path_and_checkout_follows_it() {
    let missing = BASE.replace(
        "[repo.borsuk]\npath = \"/tmp/borsuk\"",
        "[repo.borsuk]\npath = \"/tmp/borsuk\"\ntheory = { repo = \"o/r\" }",
    );
    let error = Config::parse(&missing).expect_err("the missing theory path must fail");
    assert!(format!("{error:#}").contains("repo.borsuk.theory.path"));

    let text = BASE.replace(
        "[repo.borsuk]\npath = \"/tmp/borsuk\"",
        "[repo.borsuk]\npath = \"/tmp/borsuk\"\ntheory = { repo = \"o/r\", path = \"/tmp/theory\" }",
    );
    let config = Config::parse(&text).expect("the complete theory table must parse");
    let repo = &config.repos["borsuk"];
    assert_eq!(
        repo.theory.theory.as_ref().unwrap().repo.as_deref(),
        Some("o/r")
    );
    assert_eq!(
        repo.theory.checkout(Path::new("/tmp/code")),
        Path::new("/tmp/theory")
    );

    let plain = Config::parse(BASE).expect("the plain repository must parse");
    assert_eq!(
        plain.repos["borsuk"]
            .theory
            .checkout(Path::new("/tmp/code")),
        Path::new("/tmp/code")
    );
}

#[test]
fn a_theory_repo_value_must_be_owner_slash_name() {
    for repo_value in ["", "justname", "o/r/x", "o/", "/r"] {
        let text = BASE.replace(
            "[repo.borsuk]\npath = \"/tmp/borsuk\"",
            &format!(
                "[repo.borsuk]\npath = \"/tmp/borsuk\"\ntheory = {{ repo = \"{repo_value}\", path = \"/tmp/theory\" }}"
            ),
        );
        let error = Config::parse(&text).expect_err("the bad theory repo must fail");
        assert!(
            format!("{error:#}").contains("repo.borsuk.theory.repo"),
            "{repo_value:?}: error was {error:#}"
        );
    }
}

#[test]
fn the_theory_roles_are_optional_global_tables() {
    let config =
        Config::parse(&format!("{BASE}{}", theory_tables())).expect("the theory tables must parse");
    let view = SettingsView::from_config(&config, "revision").expect("the view must build");

    assert_eq!(config.roles.len(), 8);
    assert_eq!(view.global.len(), 8);
    assert_eq!(view.repositories.len(), 6);
    let resolved = config
        .resolved_role(None, "theory.audit")
        .expect("the global theory audit role must resolve");
    assert_eq!(resolved.source, aif::config::SettingsSource::Global);
    assert_eq!(resolved.role, ExecutionRole::TheoryAudit);
    assert_eq!(resolved.settings.model, "claude-opus-5[1m]");
}

#[test]
fn a_theory_role_rejects_a_stage_limit() {
    for role in ["theory.chat", "theory.audit"] {
        let text = format!(
            "{BASE}\n[{role}]\nharness = \"claude\"\nmodel = \"claude-opus-5[1m]\"\nlimit = 2\n"
        );
        let error = Config::parse(&text).expect_err("the theory limit must fail");
        assert_eq!(
            format!("{error:#}"),
            format!("{role}.limit is allowed only on a global stage table")
        );
    }
}

#[test]
fn the_theory_role_metadata_holds() {
    assert_eq!(ExecutionRole::ALL.len(), 8);
    assert_eq!(ExecutionRole::TheoryAudit.stage(), None);
    assert_eq!(ExecutionRole::TheoryChat.stage(), None);
    assert_eq!(ExecutionRole::TheoryAudit.table_name(), "theory.audit");
    assert_eq!(ExecutionRole::TheoryChat.table_name(), "theory.chat");
    assert!(!ExecutionRole::TheoryAudit.overridable());
    assert!(!ExecutionRole::TheoryChat.overridable());
    for role in ExecutionRole::ALL.iter().take(6) {
        assert!(role.overridable());
    }
}

#[test]
fn an_edit_cannot_add_a_repository_override_for_a_theory_role() {
    for role in [ExecutionRole::TheoryAudit, ExecutionRole::TheoryChat] {
        let error = edit_config_text(
            BASE,
            &SettingsEdit::Repository {
                repository: "borsuk".to_string(),
                role,
                settings: Some(RoleOverride::default()),
            },
        )
        .expect_err("the theory override must fail");
        assert!(
            format!("{error:#}").contains(role.table_name()),
            "error was {error:#}"
        );
        assert!(format!("{error:#}").contains("a theory role takes no repository override"));
    }
}
