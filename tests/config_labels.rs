use aif::config::Config;
use aif::labels::{LabelKey, LabelNames};

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

fn with(extra: &str) -> String {
    format!("{BASE}{extra}")
}

#[test]
fn a_config_without_a_labels_table_keeps_the_historical_names() {
    let config = Config::parse(BASE).expect("the base config must parse");
    assert_eq!(config.labels, LabelNames::default());
    assert_eq!(config.resolved_labels(Some("demo")), LabelNames::default());
    assert_eq!(config.resolved_labels(None), LabelNames::default());
}

#[test]
fn a_global_table_replaces_only_the_named_labels() {
    let config = Config::parse(&with(
        r#"
[labels]
epic = "type:epic"
chunk = "type:chunk"
"#,
    ))
    .expect("the global table must parse");
    let names = config.resolved_labels(Some("demo"));
    assert_eq!(names.epic, "type:epic");
    assert_eq!(names.chunk, "type:chunk");
    assert_eq!(names.refined, "refined");
    assert_eq!(names.to_refine, "to-refine");
    assert_eq!(names.release_stacked, "release-stacked");
}

#[test]
fn a_repository_table_overrides_the_global_table() {
    let config = Config::parse(&with(
        r#"
[labels]
epic = "type:epic"
refined = "ready"

[repo.demo.labels]
epic = "programme"
"#,
    ))
    .expect("both tables must parse");
    let global = config.resolved_labels(None);
    assert_eq!(global.epic, "type:epic");
    assert_eq!(global.refined, "ready");

    let demo = config.resolved_labels(Some("demo"));
    assert_eq!(demo.epic, "programme", "the repository wins");
    assert_eq!(demo.refined, "ready", "the global value stays");
}

#[test]
fn an_unknown_repository_falls_back_to_the_global_set() {
    let config = Config::parse(&with(
        r#"
[labels]
epic = "type:epic"
"#,
    ))
    .expect("the global table must parse");
    assert_eq!(config.resolved_labels(Some("absent")), config.labels);
}

#[test]
fn every_label_key_is_configurable() {
    for key in LabelKey::ALL {
        let text = with(&format!("\n[labels]\n{key} = \"renamed-{key}\"\n"));
        let config = Config::parse(&text).unwrap_or_else(|error| panic!("{key}: {error:#}"));
        let names = config.resolved_labels(Some("demo"));
        assert_eq!(names.get(key), format!("renamed-{key}"));
        for other in LabelKey::ALL {
            if other != key {
                assert_eq!(names.get(other), other.default_value(), "{other} moved");
            }
        }
    }
}

#[test]
fn every_label_key_is_configurable_per_repository() {
    for key in LabelKey::ALL {
        let text = with(&format!("\n[repo.demo.labels]\n{key} = \"repo-{key}\"\n"));
        let config = Config::parse(&text).unwrap_or_else(|error| panic!("{key}: {error:#}"));
        assert_eq!(
            config.resolved_labels(Some("demo")).get(key),
            format!("repo-{key}")
        );
        assert_eq!(config.resolved_labels(None).get(key), key.default_value());
    }
}

#[test]
fn an_empty_label_name_fails_with_its_full_key_path() {
    let error = Config::parse(&with("\n[labels]\nepic = \"\"\n"))
        .expect_err("an empty name must fail")
        .to_string();
    assert!(error.contains("labels.epic"), "{error}");
}

#[test]
fn a_padded_label_name_fails() {
    let error = Config::parse(&with("\n[repo.demo.labels]\nepic = \" epic \"\n"))
        .expect_err("a padded name must fail")
        .to_string();
    assert!(error.contains("repo.demo.labels.epic"), "{error}");
}

#[test]
fn two_keys_with_one_name_fail_in_the_global_table() {
    let error = Config::parse(&with("\n[labels]\nepic = \"refined\"\n"))
        .expect_err("a collision must fail")
        .to_string();
    assert!(error.contains("refined"), "{error}");
    assert!(error.contains("epic"), "{error}");
}

#[test]
fn a_repository_table_cannot_collide_with_a_global_name() {
    let error = Config::parse(&with(
        r#"
[labels]
refined = "ready"

[repo.demo.labels]
epic = "ready"
"#,
    ))
    .expect_err("a cross-layer collision must fail")
    .to_string();
    assert!(error.contains("repo.demo.labels"), "{error}");
    assert!(error.contains("ready"), "{error}");
}

#[test]
fn an_unknown_label_key_is_rejected() {
    let error = format!(
        "{:#}",
        Config::parse(&with("\n[labels]\nblocked = \"blocked\"\n"))
            .expect_err("an unknown key must fail")
    );
    assert!(error.contains("blocked"), "{error}");
}

#[test]
fn a_prefix_keeps_its_trailing_colon() {
    let config = Config::parse(&with(
        r#"
[repo.demo.labels]
complexity_prefix = "size/"
review_complexity_prefix = "review-size/"
"#,
    ))
    .expect("a prefix must parse");
    let names = config.resolved_labels(Some("demo"));
    assert_eq!(names.complexity_prefix, "size/");
    assert_eq!(names.review_complexity_prefix, "review-size/");
}
