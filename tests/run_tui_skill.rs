//! The `run-tui` skill of this repository parses and lints clean.
//!
//! The daemon reads these files from the skills checkout at the polled
//! commit, so the files must hold the contract of `src/theory/skills.rs`.

use std::fs;
use std::path::Path;

use aif::theory::skills::{self, SkillSet};
use aif::theory::verify::{Area, Tier, VerifyMap};

/// One file of the `run-tui` skill, relative to the repository root.
fn read(rel: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|error| panic!("cannot read {rel}: {error}"))
}

/// The parsed skill, with the failure reason in the panic.
fn skill() -> skills::RunSkill {
    let rel = ".claude/skills/run-tui/SKILL.md";
    let text = read(rel);
    skills::parse_skill(rel, &text)
        .unwrap_or_else(|finding| panic!("{}: {}", finding.file, finding.reason))
}

/// The parsed `theory-view` feature, with the failure reason in the panic.
fn feature() -> skills::Feature {
    let rel = ".claude/skills/run-tui/features/theory-view.md";
    let text = read(rel);
    skills::parse_feature(rel, &text)
        .unwrap_or_else(|finding| panic!("{}: {}", finding.file, finding.reason))
}

#[test]
fn the_run_tui_skill_declares_the_terminal_tier_and_no_placeholders() {
    let one = skill();
    assert_eq!(one.name, "run-tui");
    assert_eq!(one.surface, "tui");
    assert_eq!(one.driver, "tmux");
    assert_eq!(one.tier, Tier::Terminal);
    assert!(
        skills::placeholder_findings(&one).is_empty(),
        "the skill sections hold placeholders"
    );
}

#[test]
fn the_theory_view_feature_lints_clean_against_the_tui_area() {
    let mut one = skill();
    one.index = Some(read(".claude/skills/run-tui/features/README.md"));
    let mut set = SkillSet::default();
    set.surfaces.insert(one.surface.clone(), one.clone());
    set.features.push(feature());
    let verify = VerifyMap {
        areas: vec![Area {
            id: "tui".to_string(),
            boundary: String::new(),
            statement: String::new(),
            skills: vec!["run-tui".to_string()],
            min_tier: Tier::None,
        }],
        measurers: Vec::new(),
    };

    let findings = skills::lint(&set, &verify);

    assert!(findings.is_empty(), "lint findings: {findings:?}");
}
