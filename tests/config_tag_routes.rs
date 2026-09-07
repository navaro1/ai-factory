use aif::config::{edit_config_text, Config, Harness, RoleOverride, SettingsEdit, SettingsSource};
use aif::routing::{ComplexityLevel, TagRouteKey, TagRouteStage};
use aif::sock::SettingsView;

const BASE: &str = r#"
schema_version = 1

[stage.refine]
harness = "claude"
model = "refine"

[stage.implement]
harness = "opencode"
model = "implement"
auto_approve = true

[stage.review]
harness = "opencode"
model = "review"
auto_approve = true

[stage.release]
harness = "claude"
model = "release"

[ticket.create]
harness = "claude"
model = "create"

[ticket.chat]
harness = "claude"
model = "chat"

[repo.demo]
path = "/tmp/demo"
"#;

#[test]
fn built_in_routes_form_the_balanced_matrix() {
    let config = Config::parse(BASE).expect("the base config must parse");
    let cases = [
        (
            TagRouteStage::Implement,
            ComplexityLevel::Low,
            "gpt-5.6-luna",
            "high",
        ),
        (
            TagRouteStage::Review,
            ComplexityLevel::Low,
            "gpt-5.6-luna",
            "xhigh",
        ),
        (
            TagRouteStage::Implement,
            ComplexityLevel::Medium,
            "gpt-5.6-terra",
            "high",
        ),
        (
            TagRouteStage::Review,
            ComplexityLevel::Medium,
            "gpt-5.6-terra",
            "xhigh",
        ),
        (
            TagRouteStage::Implement,
            ComplexityLevel::High,
            "gpt-5.6-sol",
            "xhigh",
        ),
        (
            TagRouteStage::Review,
            ComplexityLevel::High,
            "gpt-5.6-sol",
            "max",
        ),
        (
            TagRouteStage::Implement,
            ComplexityLevel::VeryHigh,
            "gpt-6-astra",
            "xhigh",
        ),
        (
            TagRouteStage::Review,
            ComplexityLevel::VeryHigh,
            "gpt-6-astra",
            "max",
        ),
    ];

    for (stage, level, model, effort) in cases {
        let route = config
            .resolved_tag_route(None, TagRouteKey::new(stage, level))
            .expect("the built-in route must resolve");
        assert_eq!(route.source, SettingsSource::BuiltIn);
        assert_eq!(route.settings.harness, Harness::Codex);
        assert_eq!(route.settings.program, "codex");
        assert_eq!(route.settings.model, model);
        assert_eq!(route.settings.effort.as_deref(), Some(effort));
        assert_eq!(route.settings.auto_approve, Some(false));
        assert_eq!(route.settings.approval_policy.as_deref(), Some("never"));
        assert_eq!(route.settings.sandbox.as_deref(), Some("workspace-write"));
    }
}

#[test]
fn repository_route_fields_override_the_effective_global_route() {
    let text = format!(
        "{BASE}\n\
         [tag_routes.implement.high]\n\
         effort = \"max\"\n\n\
         [repo.demo.tag_routes.implement.high]\n\
         model = \"repo-high\"\n"
    );
    let config = Config::parse(&text).expect("the route overrides must parse");
    let key = TagRouteKey::new(TagRouteStage::Implement, ComplexityLevel::High);

    let global = config
        .resolved_tag_route(None, key)
        .expect("the global route must resolve");
    assert_eq!(global.source, SettingsSource::Global);
    assert_eq!(global.settings.model, "gpt-5.6-sol");
    assert_eq!(global.settings.effort.as_deref(), Some("max"));

    let repository = config
        .resolved_tag_route(Some("demo"), key)
        .expect("the repository route must resolve");
    assert_eq!(
        repository.source,
        SettingsSource::Repository {
            alias: "demo".to_string()
        }
    );
    assert_eq!(repository.settings.model, "repo-high");
    assert_eq!(repository.settings.effort.as_deref(), Some("max"));
}

#[test]
fn the_settings_view_reports_all_routes_and_each_field_source() {
    let text = format!(
        "{BASE}\n\
         [tag_routes.implement.high]\n\
         effort = \"max\"\n\n\
         [repo.demo.tag_routes.implement.high]\n\
         model = \"repo-high\"\n"
    );
    let config = Config::parse(&text).expect("the route overrides must parse");
    let view =
        SettingsView::from_config(&config, "revision", &[]).expect("the settings view must build");
    let key = TagRouteKey::new(TagRouteStage::Implement, ComplexityLevel::High);

    assert_eq!(view.global_tag_routes.len(), 8);
    assert_eq!(view.repository_tag_routes.len(), 8);
    let global = view
        .global_tag_routes
        .iter()
        .find(|route| route.key == key)
        .unwrap();
    assert!(global.overridden);
    assert_eq!(global.sources.harness, SettingsSource::BuiltIn);
    assert_eq!(global.sources.model, SettingsSource::BuiltIn);
    assert_eq!(global.sources.effort, SettingsSource::Global);
    let repository = view
        .repository_tag_routes
        .iter()
        .find(|route| route.repository == "demo" && route.key == key)
        .unwrap();
    assert!(repository.overridden);
    assert_eq!(repository.sources.harness, SettingsSource::BuiltIn);
    assert_eq!(repository.sources.effort, SettingsSource::Global);
    assert_eq!(
        repository.sources.model,
        SettingsSource::Repository {
            alias: "demo".to_string()
        }
    );
}

#[test]
fn a_route_harness_change_requires_a_model_and_rejects_foreign_fields() {
    let missing_model =
        format!("{BASE}\n[tag_routes.review.low]\nharness = \"opencode\"\nauto_approve = true\n");
    let error = Config::parse(&missing_model).expect_err("a harness change needs a model");
    assert!(error.to_string().contains("tag_routes.review.low.model"));

    let foreign = format!(
        "{BASE}\n[tag_routes.review.low]\nharness = \"opencode\"\nmodel = \"m\"\nsandbox = \"workspace-write\"\n"
    );
    let error = Config::parse(&foreign).expect_err("opencode has no sandbox field");
    assert!(error
        .to_string()
        .contains("tag_routes.review.low: contains fields unsupported by opencode"));
}

#[test]
fn route_edits_preserve_comments_and_remove_only_the_selected_override() {
    let text = format!("# keep this comment\n{BASE}");
    let key = TagRouteKey::new(TagRouteStage::Review, ComplexityLevel::High);
    let global = edit_config_text(
        &text,
        &SettingsEdit::GlobalTagRoute {
            key,
            settings: Some(RoleOverride {
                model: Some("global-review".to_string()),
                ..RoleOverride::default()
            }),
        },
    )
    .expect("the global route edit must work");
    assert!(global.starts_with("# keep this comment\n"));
    assert!(global.contains("[tag_routes.review.high]"));
    assert_eq!(
        Config::parse(&global)
            .unwrap()
            .resolved_tag_route(None, key)
            .unwrap()
            .settings
            .model,
        "global-review"
    );

    let repository = edit_config_text(
        &global,
        &SettingsEdit::RepositoryTagRoute {
            repository: "demo".to_string(),
            key,
            settings: Some(RoleOverride {
                effort: Some("high".to_string()),
                ..RoleOverride::default()
            }),
        },
    )
    .expect("the repository route edit must work");
    assert!(repository.contains("[repo.demo.tag_routes.review.high]"));
    let resolved = Config::parse(&repository)
        .unwrap()
        .resolved_tag_route(Some("demo"), key)
        .unwrap();
    assert_eq!(resolved.settings.model, "global-review");
    assert_eq!(resolved.settings.effort.as_deref(), Some("high"));

    let removed = edit_config_text(
        &repository,
        &SettingsEdit::RepositoryTagRoute {
            repository: "demo".to_string(),
            key,
            settings: None,
        },
    )
    .expect("the repository route removal must work");
    let resolved = Config::parse(&removed)
        .unwrap()
        .resolved_tag_route(Some("demo"), key)
        .unwrap();
    assert_eq!(resolved.source, SettingsSource::Global);
    assert_eq!(resolved.settings.model, "global-review");
    assert_eq!(resolved.settings.effort.as_deref(), Some("max"));
}

#[test]
fn unknown_route_tables_are_rejected() {
    let unknown_level = format!("{BASE}\n[tag_routes.implement.urgent]\nmodel = \"m\"\n");
    let error = Config::parse(&unknown_level).expect_err("the route level is closed");
    let message = format!("{error:#}");
    assert!(message.contains("unknown field `urgent`"), "{message}");

    let unknown_stage = format!("{BASE}\n[tag_routes.refine.low]\nmodel = \"m\"\n");
    let error = Config::parse(&unknown_stage).expect_err("the route stage is closed");
    let message = format!("{error:#}");
    assert!(message.contains("unknown field `refine`"), "{message}");
}
