use aif::config::Config;

const ROLES: &str = r#"
schema_version = 1

[stage.refine]
harness = "claude"
model = "claude-opus-5[1m]"

[stage.implement]
harness = "opencode"
model = "zai-coding-plan/glm-5.3-flash"

[stage.review]
harness = "codex"
model = "gpt-5.6-sol"
profile = "reviewer"
effort = "xhigh"
approval_policy = "never"
sandbox = "workspace-write"
extra_args = []
limit = 7

[stage.release]
harness = "claude"
model = "claude-opus-5[1m]"

[ticket.create]
harness = "opencode"
model = "zai-coding-plan/glm-5.3-flash"

[ticket.chat]
harness = "claude"
model = "claude-opus-5[1m]"
permission_mode = "manual"
permission_handler = "inbox"
tools = ["Read", "Glob", "Grep"]
extra_args = []

[repo.demo]
path = "/tmp/demo"

[repo.demo.stage.review]
model = "gpt-5.6-sol-custom"
"#;

#[test]
fn parses_six_global_roles_and_applies_one_repository_override() {
    let config = Config::parse(ROLES).expect("the versioned role configuration must parse");

    let global = config
        .resolved_role(None, "stage.review")
        .expect("the global review role must resolve");
    assert_eq!(global.settings.model, "gpt-5.6-sol");

    let repository = config
        .resolved_role(Some("demo"), "stage.review")
        .expect("the repository review role must resolve");
    assert_eq!(repository.settings.model, "gpt-5.6-sol-custom");
}

#[test]
fn a_repository_override_can_clear_a_global_argument_list() {
    let text = ROLES
        .replace(
            "sandbox = \"workspace-write\"\nextra_args = []",
            "sandbox = \"workspace-write\"\nextra_args = [\"--notice\"]",
        )
        .replace(
            "[repo.demo.stage.review]\nmodel",
            "[repo.demo.stage.review]\nextra_args = []\nmodel",
        );

    let config = Config::parse(&text).expect("the override must parse");
    let resolved = config
        .resolved_role(Some("demo"), "stage.review")
        .expect("the review role must resolve");

    assert!(resolved.settings.extra_args.is_empty());
}

#[test]
fn a_legacy_runner_key_has_a_direct_migration_error() {
    let text = ROLES.replacen("harness = \"claude\"", "runner = \"claude\"", 1);

    let error = Config::parse(&text).expect_err("legacy configuration must fail");

    assert!(
        error.to_string().contains("runner is no longer supported"),
        "error was: {error:#}"
    );
}

#[test]
fn a_repository_override_cannot_add_a_field_from_another_harness() {
    let text = ROLES.replace(
        "[repo.demo.stage.review]",
        "[repo.demo.stage.refine]\nprofile = \"not-a-claude-field\"\n\n[repo.demo.stage.review]",
    );

    let error = Config::parse(&text).expect_err("the invalid override must fail during parsing");

    assert!(error.to_string().contains("repo.demo.stage.refine"));
    assert!(error.to_string().contains("unsupported"));
}

#[test]
fn a_ticket_role_cannot_set_a_stage_limit() {
    let text = ROLES.replace(
        "[ticket.create]\nharness",
        "[ticket.create]\nlimit = 1\nharness",
    );

    let error = Config::parse(&text).expect_err("ticket limits must fail");

    assert!(error.to_string().contains("ticket.create.limit"));
}

#[test]
fn rejects_empty_unknown_and_managed_values() {
    let cases = [
        (
            ROLES.replacen("model = \"claude-opus-5[1m]\"", "model = \"  \"", 1),
            "stage.refine.model",
        ),
        (
            ROLES.replacen(
                "harness = \"claude\"",
                "harness = \"claude\"\nunknown = true",
                1,
            ),
            "unknown field `unknown`",
        ),
        (
            ROLES.replacen("extra_args = []", "extra_args = [\"--model=other\"]", 1),
            "managed argument",
        ),
    ];

    for (text, expected) in cases {
        let error = Config::parse(&text).expect_err("the invalid configuration must fail");
        assert!(
            format!("{error:#}").contains(expected),
            "error was: {error:#}"
        );
    }
}

#[test]
fn requires_a_model_when_a_repository_changes_harness() {
    let text = ROLES.replace(
        "[repo.demo.stage.review]\nmodel = \"gpt-5.6-sol-custom\"",
        "[repo.demo.stage.review]\nharness = \"opencode\"",
    );

    let error = Config::parse(&text).expect_err("a partial harness replacement must fail");

    assert!(error.to_string().contains("repo.demo.stage.review.model"));
}
