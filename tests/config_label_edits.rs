use aif::config::{edit_config_text, Config, SettingsEdit};
use aif::labels::{LabelKey, LabelNames};
use std::collections::BTreeMap;

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

fn label_edit(repository: Option<&str>, pairs: &[(LabelKey, Option<&str>)]) -> SettingsEdit {
    SettingsEdit::Labels {
        repository: repository.map(str::to_string),
        names: pairs
            .iter()
            .map(|(key, value)| (*key, value.map(str::to_string)))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[test]
fn a_global_label_edit_creates_the_table_and_parses_back() {
    let text = edit_config_text(
        BASE,
        &label_edit(None, &[(LabelKey::Epic, Some("type:epic"))]),
    )
    .expect("the edit must apply");
    assert!(text.contains("[labels]"), "{text}");
    let config = Config::parse(&text).expect("the result must parse");
    assert_eq!(config.resolved_labels(None).epic, "type:epic");
}

#[test]
fn a_repository_label_edit_writes_under_the_repository() {
    let text = edit_config_text(
        BASE,
        &label_edit(Some("demo"), &[(LabelKey::Chunk, Some("type:chunk"))]),
    )
    .expect("the edit must apply");
    assert!(text.contains("[repo.demo.labels]"), "{text}");
    let config = Config::parse(&text).expect("the result must parse");
    assert_eq!(config.resolved_labels(Some("demo")).chunk, "type:chunk");
    assert_eq!(config.resolved_labels(None).chunk, "chunk");
}

#[test]
fn a_none_value_removes_one_key_and_an_empty_table_goes_away() {
    let with_two = edit_config_text(
        BASE,
        &label_edit(
            None,
            &[
                (LabelKey::Epic, Some("type:epic")),
                (LabelKey::Chunk, Some("type:chunk")),
            ],
        ),
    )
    .unwrap();

    let one_left =
        edit_config_text(&with_two, &label_edit(None, &[(LabelKey::Epic, None)])).unwrap();
    let config = Config::parse(&one_left).expect("the result must parse");
    assert_eq!(config.resolved_labels(None).epic, "epic", "epic went back");
    assert_eq!(config.resolved_labels(None).chunk, "type:chunk");

    let none_left =
        edit_config_text(&one_left, &label_edit(None, &[(LabelKey::Chunk, None)])).unwrap();
    assert!(
        !none_left.contains("[labels]"),
        "the empty table must go away: {none_left}"
    );
    assert_eq!(
        Config::parse(&none_left).unwrap().labels,
        LabelNames::default()
    );
}

#[test]
fn a_repository_label_table_goes_away_when_its_last_key_goes() {
    let set = edit_config_text(
        BASE,
        &label_edit(Some("demo"), &[(LabelKey::Epic, Some("type:epic"))]),
    )
    .unwrap();
    let cleared =
        edit_config_text(&set, &label_edit(Some("demo"), &[(LabelKey::Epic, None)])).unwrap();
    assert!(
        !cleared.contains("[repo.demo.labels]"),
        "the empty table must go away: {cleared}"
    );
    assert_eq!(
        Config::parse(&cleared)
            .unwrap()
            .resolved_labels(Some("demo")),
        LabelNames::default()
    );
}

#[test]
fn a_label_edit_for_an_absent_repository_fails() {
    let error = format!(
        "{:#}",
        edit_config_text(
            BASE,
            &label_edit(Some("absent"), &[(LabelKey::Epic, Some("x"))]),
        )
        .expect_err("an absent repository must fail")
    );
    assert!(error.contains("absent"), "{error}");
}

/// The writer re-parses its own result, so a name collision fails the save
/// instead of reaching the file. The settings panel shows the reason.
#[test]
fn a_label_edit_that_creates_a_collision_is_refused_by_the_writer() {
    let error = format!(
        "{:#}",
        edit_config_text(
            BASE,
            &label_edit(None, &[(LabelKey::Epic, Some("refined"))])
        )
        .expect_err("a collision must fail the edit")
    );
    assert!(error.contains("refined"), "{error}");
    assert!(error.contains("epic"), "{error}");
}

#[test]
fn every_key_survives_a_write_and_a_read() {
    let mut text = BASE.to_string();
    for key in LabelKey::ALL {
        text = edit_config_text(
            &text,
            &label_edit(None, &[(key, Some(&format!("n-{key}")))]),
        )
        .unwrap_or_else(|error| panic!("{key}: {error:#}"));
    }
    let config = Config::parse(&text).expect("every key together must parse");
    for key in LabelKey::ALL {
        assert_eq!(config.resolved_labels(None).get(key), format!("n-{key}"));
    }
}
