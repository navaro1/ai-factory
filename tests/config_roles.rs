use aif::config::{Config, Harness};

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

#[test]
fn a_harness_replacement_resets_program_and_old_harness_fields() {
    let refine = "[stage.refine]\nharness = \"claude\"\nmodel = \"claude-opus-5[1m]\"";
    let old = "[stage.refine]\nharness = \"claude\"\nprogram = \"custom-claude\"\nmodel = \"claude-opus-5[1m]\"\nagent = \"refiner\"\npermission_mode = \"manual\"\ntools = [\"Read\"]";
    let override_old = "[repo.demo.stage.review]\nmodel = \"gpt-5.6-sol-custom\"";
    let override_new = "[repo.demo.stage.refine]\nharness = \"codex\"\nmodel = \"gpt-5.6-sol\"\nprofile = \"reviewer\"\napproval_policy = \"never\"\nsandbox = \"workspace-write\"";
    let text = ROLES
        .replace(refine, old)
        .replace(override_old, override_new);

    let config = Config::parse(&text).expect("the complete replacement must parse");
    let resolved = config
        .resolved_role(Some("demo"), "stage.refine")
        .expect("the replacement must resolve");

    assert_eq!(resolved.settings.program, "codex");
    assert_eq!(resolved.settings.agent, None);
    assert_eq!(resolved.settings.permission_mode, None);
    assert!(resolved.settings.tools.is_empty());
}

#[test]
fn rejects_each_new_managed_argument_form() {
    for argument in [
        "--session-id=value",
        "--format=value",
        "--auto",
        "--dir=value",
        "--tools=value",
        "--strict-mcp-config=value",
    ] {
        let text = ROLES.replacen(
            "extra_args = []",
            &format!("extra_args = [\"{argument}\"]"),
            1,
        );
        let error = Config::parse(&text).expect_err("the managed argument must fail");
        assert!(
            error.to_string().contains("managed argument"),
            "argument {argument:?} gave: {error:#}"
        );
    }
}

#[test]
fn rejects_an_empty_foreign_tool_list() {
    let text = ROLES.replace(
        "[ticket.create]\nharness = \"opencode\"",
        "[ticket.create]\nharness = \"opencode\"\ntools = []",
    );
    let error = Config::parse(&text).expect_err("the foreign list must fail");
    assert!(error.to_string().contains("ticket.create"));
    assert!(error.to_string().contains("unsupported"));
}

#[test]
fn rejects_empty_tool_names_and_inline_legacy_keys() {
    let cases = [
        (
            ROLES.replace("tools = [\"Read\", \"Glob\", \"Grep\"]", "tools = [\"\"]"),
            "ticket.chat.tools",
        ),
        (
            ROLES.replace(
                "[repo.demo]\npath = \"/tmp/demo\"",
                "[repo.demo]\npath = \"/tmp/demo\"\nrelease = { runner = \"old\" }",
            ),
            "repo.demo.release.runner is no longer supported",
        ),
    ];
    for (text, expected) in cases {
        let error = Config::parse(&text).expect_err("the invalid value must fail");
        assert!(
            format!("{error:#}").contains(expected),
            "error was: {error:#}"
        );
    }
}

#[test]
fn preserves_every_supported_field_and_default_program() {
    let text = ROLES
        .replace(
            "[stage.refine]\nharness = \"claude\"\nmodel = \"claude-opus-5[1m]\"",
            "[stage.refine]\nharness = \"claude\"\nprogram = \"claude-refine\"\nmodel = \"claude-refine-model\"\neffort = \"high\"\nextra_args = [\"--notice\", \"refine\"]\nagent = \"refiner\"\npermission_mode = \"manual\"\npermission_handler = \"inbox\"\ntools = [\"Read\", \"Glob\"]\ndisallowed_tools = [\"Bash\"]\nstrict_mcp = true",
        )
        .replace(
            "[stage.implement]\nharness = \"opencode\"\nmodel = \"zai-coding-plan/glm-5.3-flash\"",
            "[stage.implement]\nharness = \"opencode\"\nprogram = \"opencode-build\"\nmodel = \"opencode-build-model\"\neffort = \"max\"\nextra_args = [\"--notice\", \"build\"]\nagent = \"builder\"\nauto_approve = false",
        )
        .replace(
            "[stage.review]\nharness = \"codex\"\nmodel = \"gpt-5.6-sol\"\nprofile = \"reviewer\"\neffort = \"xhigh\"\napproval_policy = \"never\"\nsandbox = \"workspace-write\"\nextra_args = []",
            "[stage.review]\nharness = \"codex\"\nprogram = \"codex-review\"\nmodel = \"codex-review-model\"\neffort = \"xhigh\"\nextra_args = [\"--notice\", \"review\"]\nprofile = \"reviewer\"\napproval_policy = \"never\"\nsandbox = \"workspace-write\"",
        );
    let config = Config::parse(&text).expect("every supported field must parse");

    let refine = &config.resolved_role(None, "stage.refine").unwrap().settings;
    assert_eq!(refine.harness, Harness::Claude);
    assert_eq!(refine.program, "claude-refine");
    assert_eq!(refine.model, "claude-refine-model");
    assert_eq!(refine.effort.as_deref(), Some("high"));
    assert_eq!(refine.extra_args, ["--notice", "refine"]);
    assert_eq!(refine.agent.as_deref(), Some("refiner"));
    assert_eq!(refine.permission_mode.as_deref(), Some("manual"));
    assert_eq!(refine.permission_handler.as_deref(), Some("inbox"));
    assert_eq!(refine.tools, ["Read", "Glob"]);
    assert_eq!(refine.disallowed_tools, ["Bash"]);
    assert_eq!(refine.strict_mcp, Some(true));

    let implement = &config
        .resolved_role(None, "stage.implement")
        .unwrap()
        .settings;
    assert_eq!(implement.harness, Harness::Opencode);
    assert_eq!(implement.program, "opencode-build");
    assert_eq!(implement.model, "opencode-build-model");
    assert_eq!(implement.effort.as_deref(), Some("max"));
    assert_eq!(implement.extra_args, ["--notice", "build"]);
    assert_eq!(implement.agent.as_deref(), Some("builder"));
    assert_eq!(implement.auto_approve, Some(false));

    let review = &config.resolved_role(None, "stage.review").unwrap().settings;
    assert_eq!(review.harness, Harness::Codex);
    assert_eq!(review.program, "codex-review");
    assert_eq!(review.model, "codex-review-model");
    assert_eq!(review.effort.as_deref(), Some("xhigh"));
    assert_eq!(review.extra_args, ["--notice", "review"]);
    assert_eq!(review.profile.as_deref(), Some("reviewer"));
    assert_eq!(review.approval_policy.as_deref(), Some("never"));
    assert_eq!(review.sandbox.as_deref(), Some("workspace-write"));

    for (role, program) in [
        ("stage.release", "claude"),
        ("ticket.create", "opencode"),
        ("ticket.chat", "claude"),
    ] {
        assert_eq!(
            config.resolved_role(None, role).unwrap().settings.program,
            program
        );
    }
}

#[test]
fn every_removed_name_has_a_direct_migration_error() {
    for (old, expected) in [
        ("runner = \"claude\"", "runner is no longer supported"),
        ("variant = \"xhigh\"", "variant is no longer supported"),
        ("yolo = true", "yolo is no longer supported"),
        (
            "[ticket_chat]\nmodel = \"old\"",
            "ticket_chat is no longer supported",
        ),
    ] {
        let text = if old.starts_with("[ticket_chat]") {
            format!("{ROLES}\n{old}\n")
        } else {
            ROLES.replacen("harness = \"claude\"", old, 1)
        };
        let error = Config::parse(&text).expect_err("the old configuration must fail");
        assert!(
            format!("{error:#}").contains(expected),
            "error was: {error:#}"
        );
    }
}
