//! The parser for the run skills, one directory per surface under
//! `.claude/skills/run-<surface>/`.
//!
//! A surface has one `SKILL.md` that names the driver and the tier it
//! reaches, and one file per feature that needs a drive recipe. Both
//! carry flat `key: value` front matter between two `---` lines.
//!
//! Nothing here fails a poll. A file that does not parse becomes one lint
//! finding that names the file, and every file that did parse stays in the
//! set. That is why every entry point returns a [`Finding`] instead of an
//! error type the caller must handle.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};

use super::verify::{Area, VerifyMap};

pub use super::verify::Tier;

/// The six sections of a skill file, in the order design section 4.2
/// lists them. The set is closed: a heading outside it is prose.
pub const SECTIONS: [&str; 6] = ["Run", "Fast", "Auth or seed", "Drive", "Logs", "Gotchas"];

/// The directory prefix that marks a run skill.
const RUN_PREFIX: &str = "run-";

/// One surface skill, parsed from `run-<surface>/SKILL.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSkill {
    /// The skill name of the front matter, as Claude Code reads it.
    pub name: String,
    pub description: String,
    /// The surface this skill drives.
    pub surface: String,
    /// The program that drives the surface.
    pub driver: String,
    /// How far the driver reaches.
    pub tier: Tier,
    /// What the driver cannot see, in the author's words.
    pub blind: String,
    /// Everything below the front matter.
    pub body: String,
    /// The sections of [`SECTIONS`] the body carries, by heading.
    pub sections: BTreeMap<String, String>,
    /// The `features/README.md` index of the surface, when it has one.
    /// Every slice of design section 4.6 inlines it.
    pub index: Option<String>,
}

/// One feature file, parsed from `run-<surface>/features/<id>.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Feature {
    /// The surface the feature belongs to.
    pub surface: String,
    /// The feature id: the file stem.
    pub id: String,
    /// The area of `theory/verify.toml` the feature binds to.
    pub area: String,
    /// The command that checks the feature in seconds, when it has one.
    pub fast: Option<String>,
    /// Everything below the front matter.
    pub body: String,
}

/// One lint finding. The doctor prints it and the AREAS panel marks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The surface the file belongs to. Empty when the path names none.
    pub surface: String,
    /// The file, relative to the skill directory.
    pub file: String,
    /// What is wrong, in the operator's words.
    pub reason: String,
}

impl Display for Finding {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.file, self.reason)
    }
}

/// Every run skill of one commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillSet {
    /// The surface skills, by surface name.
    pub surfaces: BTreeMap<String, RunSkill>,
    /// The feature files, in path order.
    pub features: Vec<Feature>,
    /// One finding per file that did not parse.
    pub lint: Vec<Finding>,
}

/// What one file of a skills tree is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// The `SKILL.md` of one surface.
    Skill,
    /// The `features/README.md` index of one surface.
    Index,
    /// One `features/<id>.md` file.
    Feature,
}

/// Which files of a tree listing the skills cache reads.
///
/// The listing carries everything under `.claude/skills/`. Only a
/// `run-<surface>` directory holds a run skill, and inside it only the
/// skill file, the feature index, and the feature files are markdown. A
/// helper script is not.
pub fn classify(path: &str) -> Option<FileKind> {
    let inside = inside(path)?;
    if inside == "SKILL.md" {
        return Some(FileKind::Skill);
    }
    let rest = inside.strip_prefix("features/")?;
    if rest.contains('/') || !rest.ends_with(".md") {
        return None;
    }
    if rest == "README.md" {
        return Some(FileKind::Index);
    }
    Some(FileKind::Feature)
}

impl SkillSet {
    /// Build the set from the skill files of one commit.
    ///
    /// Each pair is the path of the tree listing and the file text. A file
    /// that does not parse becomes one finding in [`SkillSet::lint`] and
    /// blocks nothing.
    pub fn from_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut set = SkillSet::default();
        let mut indexes: BTreeMap<String, String> = BTreeMap::new();
        for (path, text) in files {
            match classify(path) {
                Some(FileKind::Skill) => match parse_skill(path, text) {
                    Ok(skill) => {
                        set.surfaces.insert(skill.surface.clone(), skill);
                    }
                    Err(finding) => set.lint.push(finding),
                },
                Some(FileKind::Index) => {
                    indexes.insert(surface_of(path), text.to_string());
                }
                Some(FileKind::Feature) => match parse_feature(path, text) {
                    Ok(feature) => set.features.push(feature),
                    Err(finding) => set.lint.push(finding),
                },
                None => {}
            }
        }
        // A skill file can follow its index in tree order, so the
        // attachment waits for the whole listing.
        for (surface, text) in indexes {
            if let Some(skill) = set.surfaces.get_mut(&surface) {
                skill.index = Some(text);
            }
        }
        set
    }

    /// The feature ids of one surface, in path order.
    pub fn feature_ids(&self, surface: &str) -> Vec<String> {
        self.features
            .iter()
            .filter(|feature| feature.surface == surface)
            .map(|feature| feature.id.clone())
            .collect()
    }
}

/// Parse one `SKILL.md`.
///
/// `path` names the file for the finding. The surface comes from the front
/// matter, not the directory, because the daemon groups by what the skill
/// declares.
pub fn parse_skill(path: &str, text: &str) -> Result<RunSkill, Finding> {
    let file = inside(path).unwrap_or_else(|| path.to_string());
    let surface = surface_of(path);
    let fail = |reason: String| Finding {
        surface: surface.clone(),
        file: file.clone(),
        reason,
    };
    let (keys, body) = front_matter(text).map_err(&fail)?;
    let required = |key: &str| -> Result<String, Finding> {
        match keys.get(key) {
            Some(value) if !value.is_empty() => Ok(value.clone()),
            _ => Err(fail(format!("{key} is required"))),
        }
    };
    let name = required("name")?;
    let description = required("description")?;
    let declared = required("surface")?;
    let driver = required("driver")?;
    let tier_name = required("tier")?;
    let tier =
        Tier::parse(&tier_name).ok_or_else(|| fail(format!("unknown tier \"{tier_name}\"")))?;
    Ok(RunSkill {
        name,
        description,
        surface: declared,
        driver,
        tier,
        blind: keys.get("blind").cloned().unwrap_or_default(),
        sections: sections(body),
        body: body.to_string(),
        index: None,
    })
}

/// Parse one `features/<id>.md`.
///
/// The id is the file stem and the surface is the `run-<surface>` directory
/// of the path, because a feature file names neither.
pub fn parse_feature(path: &str, text: &str) -> Result<Feature, Finding> {
    let file = inside(path).unwrap_or_else(|| path.to_string());
    let surface = surface_of(path);
    let id = file
        .rsplit('/')
        .next()
        .unwrap_or(&file)
        .trim_end_matches(".md")
        .to_string();
    let fail = |reason: String| Finding {
        surface: surface.clone(),
        file: file.clone(),
        reason,
    };
    let (keys, body) = front_matter(text).map_err(&fail)?;
    let area = match keys.get("area") {
        Some(value) if !value.is_empty() => value.clone(),
        _ => return Err(fail("area is required".to_string())),
    };
    Ok(Feature {
        surface,
        id,
        area,
        fast: keys.get("fast").filter(|value| !value.is_empty()).cloned(),
        body: body.to_string(),
    })
}

/// Every finding of one skill set: the files that did not parse, then the
/// features that name an area the map does not hold.
pub fn lint(set: &SkillSet, verify: &VerifyMap) -> Vec<Finding> {
    let known: BTreeSet<&str> = verify.areas.iter().map(|area| area.id.as_str()).collect();
    let mut findings = set.lint.clone();
    for feature in &set.features {
        if !known.contains(feature.area.as_str()) {
            findings.push(Finding {
                surface: feature.surface.clone(),
                file: format!("features/{}.md", feature.id),
                reason: format!("area {} unknown", feature.area),
            });
        }
    }
    findings
}

/// The features of one area, in the order of requirement R3.
///
/// The `skills` list of the area comes first, so the operator's binding
/// wins over a feature's own front matter, and every feature that names
/// the area follows. A feature appears once.
pub fn resolve<'a>(area: &str, verify: &VerifyMap, set: &'a SkillSet) -> Vec<&'a Feature> {
    let Some(entry) = area_of(area, verify) else {
        return Vec::new();
    };
    let mut out: Vec<&Feature> = Vec::new();
    for name in &entry.skills {
        let surface = surface_name(name);
        for feature in &set.features {
            if feature.surface == surface {
                push_once(&mut out, feature);
            }
        }
    }
    for feature in &set.features {
        if feature.area == area {
            push_once(&mut out, feature);
        }
    }
    out
}

/// Append `feature` unless the same surface and id are already there.
fn push_once<'a>(out: &mut Vec<&'a Feature>, feature: &'a Feature) {
    if !out
        .iter()
        .any(|seen| seen.surface == feature.surface && seen.id == feature.id)
    {
        out.push(feature);
    }
}

/// The surfaces that map to one area, in the order of requirement R3.
///
/// The `skills` list of the area names surfaces directly, so a surface
/// with no feature file still covers the area. The surfaces of the bound
/// features follow. A name here need not have a skill file that parsed,
/// which is how a broken `SKILL.md` still marks the areas it covers.
pub fn surface_names(area: &str, verify: &VerifyMap, set: &SkillSet) -> Vec<String> {
    let Some(entry) = area_of(area, verify) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for name in &entry.skills {
        let surface = surface_name(name);
        if !names.iter().any(|seen| seen == surface) {
            names.push(surface.to_string());
        }
    }
    for feature in &set.features {
        if feature.area == area && !names.iter().any(|seen| seen == &feature.surface) {
            names.push(feature.surface.clone());
        }
    }
    names
}

/// The surface skills that map to one area, in the order of requirement
/// R3. A surface whose skill file did not parse is absent.
pub fn surfaces<'a>(area: &str, verify: &VerifyMap, set: &'a SkillSet) -> Vec<&'a RunSkill> {
    surface_names(area, verify, set)
        .iter()
        .filter_map(|name| set.surfaces.get(name))
        .collect()
}

/// The highest tier of the surfaces that map to one area, `none` when no
/// surface maps to it.
pub fn area_tier(area: &str, verify: &VerifyMap, set: &SkillSet) -> Tier {
    surfaces(area, verify, set)
        .iter()
        .map(|skill| skill.tier)
        .max()
        .unwrap_or_default()
}

fn area_of<'a>(area: &str, verify: &'a VerifyMap) -> Option<&'a Area> {
    verify.areas.iter().find(|entry| entry.id == area)
}

/// The surface a `skills` entry names, with the directory prefix removed.
fn surface_name(name: &str) -> &str {
    name.strip_prefix(RUN_PREFIX).unwrap_or(name)
}

/// The path inside the skill directory: `SKILL.md` or `features/<id>.md`.
///
/// `None` when no path component starts with `run-`, which is how a file
/// outside a run skill is rejected.
fn inside(path: &str) -> Option<String> {
    let mut parts = path.split('/');
    let index = parts.position(|part| part.starts_with(RUN_PREFIX))?;
    let rest: Vec<&str> = path.split('/').skip(index + 1).collect();
    if rest.is_empty() {
        return None;
    }
    Some(rest.join("/"))
}

/// The surface of a path under `run-<surface>/`, empty when it names none.
fn surface_of(path: &str) -> String {
    path.split('/')
        .find_map(|part| part.strip_prefix(RUN_PREFIX))
        .unwrap_or_default()
        .to_string()
}

/// Split the flat `key: value` front matter from the body.
///
/// The file must open with a `---` line and close the block with another.
/// A key repeats at most once; the last line wins. The value keeps every
/// character after the first colon, so a command with a colon survives.
fn front_matter(text: &str) -> Result<(BTreeMap<String, String>, &str), String> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(|| "the file does not open with front matter".to_string())?;
    let offset = text.len() - rest.len();
    let mut keys = BTreeMap::new();
    let mut consumed = 0usize;
    let mut closed = false;
    for line in rest.split_inclusive('\n') {
        consumed += line.len();
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.trim_end() == "---" {
            closed = true;
            break;
        }
        if trimmed.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("the front matter line \"{trimmed}\" has no colon"));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("the front matter line \"{trimmed}\" has no key"));
        }
        keys.insert(key.to_string(), value.trim().to_string());
    }
    if !closed {
        return Err("the front matter has no closing \"---\" line".to_string());
    }
    Ok((keys, &text[offset + consumed..]))
}

/// The sections of [`SECTIONS`] the body carries, by heading.
fn sections(body: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut buffer = String::new();
    for line in body.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            if let Some(name) = current.take() {
                out.insert(name, buffer.trim_end().to_string());
            }
            buffer.clear();
            let title = title.trim();
            current = SECTIONS
                .iter()
                .find(|name| **name == title)
                .map(|name| (*name).to_string());
            continue;
        }
        if line.starts_with("# ") {
            if let Some(name) = current.take() {
                out.insert(name, buffer.trim_end().to_string());
            }
            buffer.clear();
            continue;
        }
        if current.is_some() {
            buffer.push_str(line);
            buffer.push('\n');
        }
    }
    if let Some(name) = current {
        out.insert(name, buffer.trim_end().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theory::model;

    const SKILL: &str = "---\nname: run-web\ndescription: Launch and drive the web app.\n\
surface: web\ndriver: playwright-cli\ntier: browser\n\
blind: pixels below 320 px width, native file dialogs\n---\n\
# Run web\n\n## Run\nnpm run dev on port 4000\n\n## Fast\nnpx playwright test\n\n\
## Notes\nnot a section\n";

    const FEATURE: &str = "---\narea: web-checkout\n\
fast: npx playwright test checkout --reporter=line\n---\n\
# Checkout\n\nThe cart pays.\n";

    fn fixture_model() -> model::Model {
        let text = concat!(
            "[[entry]]\nkind = \"boundary\"\nid = \"B-checkout\"\ntitle = \"t\"\n",
            "statement = \"s\"\nsides = [\"in\", \"out\"]\npaths = [\"web/**\"]\n",
        );
        model::parse(text).expect("the fixture model must parse")
    }

    fn map(extra: &str) -> VerifyMap {
        let text = format!(
            "[[area]]\nid = \"web-checkout\"\nboundary = \"B-checkout\"\nstatement = \"s\"\n{extra}\n"
        );
        VerifyMap::parse(&text, &fixture_model()).expect("the fixture map must parse")
    }

    #[test]
    fn a_skill_and_a_feature_parse_every_front_matter_key() {
        let skill = parse_skill(".claude/skills/run-web/SKILL.md", SKILL)
            .expect("the complete skill must parse");

        assert_eq!(skill.name, "run-web");
        assert_eq!(skill.description, "Launch and drive the web app.");
        assert_eq!(skill.surface, "web");
        assert_eq!(skill.driver, "playwright-cli");
        assert_eq!(skill.tier, Tier::Browser);
        assert_eq!(
            skill.blind,
            "pixels below 320 px width, native file dialogs"
        );
        assert_eq!(
            skill.sections.get("Run").map(String::as_str),
            Some("npm run dev on port 4000")
        );
        assert_eq!(
            skill.sections.get("Fast").map(String::as_str),
            Some("npx playwright test")
        );
        assert_eq!(skill.sections.get("Notes"), None);
        assert!(skill.body.starts_with("# Run web"), "body: {}", skill.body);

        let feature = parse_feature(".claude/skills/run-web/features/checkout.md", FEATURE)
            .expect("the complete feature must parse");

        assert_eq!(feature.surface, "web");
        assert_eq!(feature.id, "checkout");
        assert_eq!(feature.area, "web-checkout");
        assert_eq!(
            feature.fast.as_deref(),
            Some("npx playwright test checkout --reporter=line")
        );

        let bare = parse_skill(
            ".claude/skills/run-api/SKILL.md",
            "---\nname: run-api\ndescription: d\nsurface: api\ndriver: curl\ntier: http\n---\n",
        )
        .expect("a skill without blind must parse");
        assert_eq!(bare.blind, "");
        assert_eq!(bare.tier, Tier::Http);
    }

    #[test]
    fn a_skill_with_an_unknown_tier_fails_with_a_finding_that_names_the_file() {
        let text = SKILL.replace("tier: browser", "tier: pixels");

        let finding = parse_skill(".claude/skills/run-web/SKILL.md", &text)
            .expect_err("an unknown tier must fail");

        assert_eq!(finding.surface, "web");
        assert_eq!(finding.file, "SKILL.md");
        assert_eq!(finding.to_string(), "SKILL.md: unknown tier \"pixels\"");

        let missing = parse_skill(
            ".claude/skills/run-web/SKILL.md",
            "---\nname: n\ndescription: d\nsurface: web\ntier: http\n---\n",
        )
        .expect_err("a missing driver must fail");
        assert_eq!(missing.to_string(), "SKILL.md: driver is required");

        let raw = parse_skill(".claude/skills/run-web/SKILL.md", "# no front matter\n")
            .expect_err("a file without front matter must fail");
        assert_eq!(
            raw.to_string(),
            "SKILL.md: the file does not open with front matter"
        );

        let unbounded = parse_feature(
            ".claude/skills/run-web/features/x.md",
            "---\narea: web-checkout\n",
        )
        .expect_err("an unclosed front matter must fail");
        assert_eq!(
            unbounded.to_string(),
            "features/x.md: the front matter has no closing \"---\" line"
        );

        let no_area = parse_feature(
            ".claude/skills/run-web/features/x.md",
            "---\nfast: c\n---\n",
        )
        .expect_err("a feature without an area must fail");
        assert_eq!(no_area.to_string(), "features/x.md: area is required");
    }

    #[test]
    fn lint_names_a_feature_whose_area_is_not_in_the_map_and_passes_a_bound_one() {
        let bound = SkillSet::from_files([
            (".claude/skills/run-web/SKILL.md", SKILL),
            (".claude/skills/run-web/features/checkout.md", FEATURE),
        ]);
        assert!(lint(&bound, &map("")).is_empty());

        let text = FEATURE.replace("area: web-checkout", "area: nope");
        let loose = SkillSet::from_files([
            (".claude/skills/run-web/SKILL.md", SKILL),
            (".claude/skills/run-web/features/x.md", text.as_str()),
        ]);

        let findings = lint(&loose, &map(""));

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].surface, "web");
        assert_eq!(findings[0].to_string(), "features/x.md: area nope unknown");
    }

    #[test]
    fn a_parse_error_stays_a_finding_and_the_other_files_survive() {
        let broken = SKILL.replace("tier: browser", "tier: pixels");
        let api =
            "---\nname: run-api\ndescription: d\nsurface: api\ndriver: curl\ntier: http\n---\n";
        let set = SkillSet::from_files([
            (".claude/skills/run-web/SKILL.md", broken.as_str()),
            (".claude/skills/run-web/features/checkout.md", FEATURE),
            (".claude/skills/run-api/SKILL.md", api),
        ]);

        assert_eq!(set.surfaces.keys().collect::<Vec<_>>(), vec!["api"]);
        assert_eq!(set.feature_ids("web"), vec!["checkout".to_string()]);
        assert_eq!(set.lint.len(), 1);
        assert_eq!(set.lint[0].surface, "web");
        assert_eq!(set.lint[0].to_string(), "SKILL.md: unknown tier \"pixels\"");
        assert_eq!(lint(&set, &map("")).len(), 1);

        // The broken surface still maps to the area of its feature, so
        // the AREAS row can carry the mark.
        assert_eq!(
            surface_names("web-checkout", &map(""), &set),
            vec!["web".to_string()]
        );
        assert!(surfaces("web-checkout", &map(""), &set).is_empty());
        assert_eq!(area_tier("web-checkout", &map(""), &set), Tier::None);
    }

    #[test]
    fn the_tier_names_one_type_through_both_module_paths() {
        let tier: crate::theory::skills::Tier = crate::theory::verify::Tier::Browser;

        assert_eq!(tier.name(), "browser");
    }

    #[test]
    fn classify_reads_the_skill_the_index_and_the_feature_files_only() {
        assert_eq!(
            classify(".claude/skills/run-web/SKILL.md"),
            Some(FileKind::Skill)
        );
        assert_eq!(
            classify(".claude/skills/run-web/features/README.md"),
            Some(FileKind::Index)
        );
        assert_eq!(
            classify(".claude/skills/run-web/features/checkout.md"),
            Some(FileKind::Feature)
        );
        for path in [
            ".claude/skills/run-web/wait_for.sh",
            ".claude/skills/run-web/features/deep/x.md",
            ".claude/skills/verify/SKILL.md",
            ".claude/skills/run-web",
        ] {
            assert_eq!(classify(path), None, "path {path} must be skipped");
        }
    }

    #[test]
    fn the_feature_index_joins_its_surface_whatever_the_tree_order() {
        let index = "# Features of web\n\n- checkout: the cart pays\n";
        for order in [0, 1] {
            let mut files = vec![
                (".claude/skills/run-web/SKILL.md", SKILL),
                (".claude/skills/run-web/features/README.md", index),
            ];
            if order == 1 {
                files.reverse();
            }
            files.push((".claude/skills/run-web/features/checkout.md", FEATURE));

            let set = SkillSet::from_files(files);

            assert_eq!(set.surfaces["web"].index.as_deref(), Some(index));
            assert_eq!(set.feature_ids("web"), vec!["checkout".to_string()]);
            assert!(set.lint.is_empty(), "the index carries no front matter");
        }

        let bare = SkillSet::from_files([(".claude/skills/run-api/SKILL.md", SKILL)]);
        assert_eq!(bare.surfaces["web"].index, None);
    }

    #[test]
    fn resolve_puts_the_operator_list_before_the_feature_front_matter() {
        let api =
            "---\nname: run-api\ndescription: d\nsurface: api\ndriver: curl\ntier: http\n---\n";
        let orders = "---\narea: other\n---\n";
        let set = SkillSet::from_files([
            (".claude/skills/run-web/SKILL.md", SKILL),
            (".claude/skills/run-web/features/checkout.md", FEATURE),
            (".claude/skills/run-api/SKILL.md", api),
            (".claude/skills/run-api/features/orders.md", orders),
        ]);
        let bound = map("skills = [\"run-api\"]");

        let features = resolve("web-checkout", &bound, &set);

        assert_eq!(
            features
                .iter()
                .map(|feature| feature.id.as_str())
                .collect::<Vec<_>>(),
            vec!["orders", "checkout"]
        );
        assert_eq!(
            surfaces("web-checkout", &bound, &set)
                .iter()
                .map(|skill| skill.surface.as_str())
                .collect::<Vec<_>>(),
            vec!["api", "web"]
        );
        assert_eq!(area_tier("web-checkout", &bound, &set), Tier::Browser);
        assert_eq!(
            surface_names("web-checkout", &bound, &set),
            vec!["api".to_string(), "web".to_string()]
        );
        assert!(resolve("nope", &bound, &set).is_empty());
        assert_eq!(area_tier("nope", &bound, &set), Tier::None);
        assert_eq!(
            area_tier("web-checkout", &map(""), &SkillSet::default()),
            Tier::None
        );
    }
}
