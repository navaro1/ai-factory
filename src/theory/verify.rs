//! The parser for `theory/verify.toml`, the operator's map from the model
//! to the checks that hold it.
//!
//! An area names one boundary of the model and carries the run skills and
//! the measurers that guard it. The parser reports the first error in file
//! order and names the offending area, because the operator edits one area
//! at a time. A missing file is an empty map, not an error.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

use globset::{Glob, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use super::model::{Entry, Model};

/// The driver ladder: how far into the system a check reaches.
///
/// The order is the ladder order, so `Ord` answers the floor question:
/// a line holds when its tier is at or above the area's `min_tier`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// No driver reaches the feature.
    #[default]
    None,
    /// A command line program or a terminal UI, end to end.
    Terminal,
    /// Status, headers, body, and the side effects a log shows.
    Http,
    /// Rendered markup and component behaviour.
    Dom,
    /// A rendered user interface, headless.
    Browser,
}

impl Tier {
    /// The five tiers, in ladder order.
    pub const ALL: [Tier; 5] = [
        Tier::None,
        Tier::Terminal,
        Tier::Http,
        Tier::Dom,
        Tier::Browser,
    ];

    /// The lowercase name of the tier, as the front matter writes it.
    pub fn name(self) -> &'static str {
        match self {
            Tier::None => "none",
            Tier::Terminal => "terminal",
            Tier::Http => "http",
            Tier::Dom => "dom",
            Tier::Browser => "browser",
        }
    }

    /// Parse one of the five lowercase tier names.
    pub fn parse(text: &str) -> Option<Self> {
        Tier::ALL.into_iter().find(|tier| tier.name() == text)
    }
}

impl Display for Tier {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// When a measurer runs. The order is the stage order: implement runs
/// `fast`, review runs `pr`, and the release train runs `full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Fast,
    Pr,
    Full,
}

impl Mode {
    /// The lowercase name of the mode, as the file writes it.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Fast => "fast",
            Mode::Pr => "pr",
            Mode::Full => "full",
        }
    }

    /// Parse one of the three lowercase mode names.
    pub fn parse(text: &str) -> Option<Self> {
        [Mode::Fast, Mode::Pr, Mode::Full]
            .into_iter()
            .find(|mode| mode.name() == text)
    }
}

impl Display for Mode {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One area of the map: a boundary of the model and what guards it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Area {
    pub id: String,
    /// The boundary entry of the model this area guards.
    pub boundary: String,
    pub statement: String,
    /// The run skills the operator bound to the area, by surface. The
    /// name may carry the `run-` directory prefix.
    pub skills: Vec<String>,
    /// The lowest tier a check of this area may reach. Default `none`.
    pub min_tier: Tier,
}

/// One command that measures an area.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measurer {
    pub id: String,
    /// The id of the area this measurer belongs to.
    pub area: String,
    pub command: String,
    pub mode: Mode,
    pub timeout_s: u64,
}

/// The parsed verification map of one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyMap {
    pub areas: Vec<Area>,
    /// Every measurer of every area, flattened, in file order.
    pub measurers: Vec<Measurer>,
}

/// One verification map error. `area` names the offending area when one
/// exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyError {
    pub area: Option<String>,
    pub message: String,
}

impl Display for VerifyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.area {
            Some(id) => write!(formatter, "{id}: {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

impl std::error::Error for VerifyError {}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMap {
    #[serde(default)]
    area: Vec<RawArea>,
}

/// The permissive input shape. Every field is optional, because the parser
/// turns each absence into an error that names the area.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArea {
    id: Option<String>,
    boundary: Option<String>,
    statement: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    min_tier: Option<String>,
    #[serde(default)]
    measurer: Vec<RawMeasurer>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMeasurer {
    id: Option<String>,
    command: Option<String>,
    mode: Option<String>,
    timeout_s: Option<u64>,
}

fn error(area: Option<&str>, message: impl Into<String>) -> VerifyError {
    VerifyError {
        area: area.map(str::to_string),
        message: message.into(),
    }
}

fn required_text(value: Option<&str>, id: &str, message: &str) -> Result<String, VerifyError> {
    let value = value.unwrap_or_default();
    if value.trim().is_empty() {
        return Err(error(Some(id), message));
    }
    Ok(value.to_string())
}

impl VerifyMap {
    /// Parse the complete map file against `model`.
    ///
    /// Every area must name a boundary entry of the model, because the
    /// boundary carries the path globs that map a diff onto the area.
    /// Returns the first error, in file order.
    pub fn parse(text: &str, model: &Model) -> Result<Self, VerifyError> {
        let raw: RawMap = toml::from_str(text).map_err(|e| VerifyError {
            area: None,
            message: format!("invalid TOML: {e}"),
        })?;
        let boundaries: BTreeSet<&str> = model
            .entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Boundary { .. }))
            .map(Entry::id)
            .collect();
        let mut ids = BTreeSet::new();
        let mut areas = Vec::with_capacity(raw.area.len());
        let mut measurers = Vec::new();
        for (index, raw) in raw.area.into_iter().enumerate() {
            let id = match raw.id.as_deref() {
                Some(id) if !id.trim().is_empty() => id.to_string(),
                _ => return Err(error(None, format!("area {index}: id is required"))),
            };
            if !ids.insert(id.clone()) {
                return Err(error(Some(&id), "duplicate area id"));
            }
            let boundary = required_text(raw.boundary.as_deref(), &id, "boundary is required")?;
            if !boundaries.contains(boundary.as_str()) {
                return Err(error(
                    Some(&id),
                    format!("boundary names unknown boundary {boundary}"),
                ));
            }
            let statement = required_text(raw.statement.as_deref(), &id, "statement is required")?;
            let min_tier = match raw.min_tier.as_deref() {
                None => Tier::None,
                Some(text) => Tier::parse(text)
                    .ok_or_else(|| error(Some(&id), format!("unknown min_tier \"{text}\"")))?,
            };
            for entry in raw.measurer {
                measurers.push(measurer(entry, &id)?);
            }
            areas.push(Area {
                id,
                boundary,
                statement,
                skills: raw.skills,
                min_tier,
            });
        }
        Ok(VerifyMap { areas, measurers })
    }

    /// The ids of the areas whose boundary globs match one of `paths`.
    ///
    /// The boundary of the model owns the globs, so the map needs no path
    /// of its own. The result keeps file order and names each area once.
    pub fn areas_for_paths<'a>(&'a self, model: &Model, paths: &[&str]) -> Vec<&'a str> {
        self.areas
            .iter()
            .filter(|area| {
                let globs = boundary_globs(model, &area.boundary);
                globs.is_some_and(|set| paths.iter().any(|path| set.is_match(path)))
            })
            .map(|area| area.id.as_str())
            .collect()
    }
}

fn measurer(raw: RawMeasurer, area: &str) -> Result<Measurer, VerifyError> {
    let id = required_text(raw.id.as_deref(), area, "a measurer requires id")?;
    let command = required_text(
        raw.command.as_deref(),
        area,
        &format!("measurer {id} requires command"),
    )?;
    let mode = match raw.mode.as_deref() {
        None => return Err(error(Some(area), format!("measurer {id} requires mode"))),
        Some(text) => Mode::parse(text).ok_or_else(|| {
            error(
                Some(area),
                format!("measurer {id} has unknown mode \"{text}\""),
            )
        })?,
    };
    let timeout_s = raw.timeout_s.unwrap_or_default();
    if timeout_s == 0 {
        return Err(error(
            Some(area),
            format!("measurer {id} requires a timeout_s above 0"),
        ));
    }
    Ok(Measurer {
        id,
        area: area.to_string(),
        command,
        mode,
        timeout_s,
    })
}

/// The compiled path globs of one boundary entry of the model.
fn boundary_globs(model: &Model, boundary: &str) -> Option<globset::GlobSet> {
    let paths = model.entries.iter().find_map(|entry| match entry {
        Entry::Boundary { id, paths, .. } if id == boundary => Some(paths),
        _ => None,
    })?;
    let mut builder = GlobSetBuilder::new();
    for pattern in paths {
        builder.add(Glob::new(pattern).ok()?);
    }
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        let text = concat!(
            "[[entry]]\nkind = \"boundary\"\nid = \"B-checkout\"\ntitle = \"t\"\n",
            "statement = \"s\"\nsides = [\"in\", \"out\"]\npaths = [\"web/**\"]\n",
            "[[entry]]\nkind = \"boundary\"\nid = \"B-orders\"\ntitle = \"t\"\n",
            "statement = \"s\"\nsides = [\"in\", \"out\"]\npaths = [\"api/orders/**\"]\n",
            "[[entry]]\nkind = \"state\"\nid = \"S-1\"\ntitle = \"t\"\nstatement = \"s\"\n",
        );
        super::super::model::parse(text).expect("the fixture model must parse")
    }

    fn area(id: &str, extra: &str) -> String {
        format!("[[area]]\nid = \"{id}\"\nboundary = \"B-checkout\"\nstatement = \"s\"\n{extra}\n")
    }

    fn err(text: &str) -> String {
        VerifyMap::parse(text, &model())
            .expect_err("the broken map must fail")
            .to_string()
    }

    #[test]
    fn a_map_parses_areas_with_a_floor_and_flattens_the_measurers() {
        let text = format!(
            "{}{}",
            area(
                "web-checkout",
                "min_tier = \"browser\"\nskills = [\"run-web\"]\n\
                 [[area.measurer]]\nid = \"poll_p95\"\ncommand = \"aif bench\"\n\
                 mode = \"pr\"\ntimeout_s = 30"
            ),
            area("api-orders", ""),
        );

        let map = VerifyMap::parse(&text, &model()).expect("the map must parse");

        assert_eq!(map.areas.len(), 2);
        assert_eq!(
            map.areas[0],
            Area {
                id: "web-checkout".to_string(),
                boundary: "B-checkout".to_string(),
                statement: "s".to_string(),
                skills: vec!["run-web".to_string()],
                min_tier: Tier::Browser,
            }
        );
        assert_eq!(map.areas[1].min_tier, Tier::None);
        assert_eq!(map.areas[1].skills, Vec::<String>::new());
        assert_eq!(
            map.measurers,
            vec![Measurer {
                id: "poll_p95".to_string(),
                area: "web-checkout".to_string(),
                command: "aif bench".to_string(),
                mode: Mode::Pr,
                timeout_s: 30,
            }]
        );
    }

    #[test]
    fn a_map_rejects_an_unknown_boundary_an_unknown_floor_and_a_zero_timeout() {
        let unknown = "[[area]]\nid = \"web\"\nboundary = \"B-nope\"\nstatement = \"s\"\n";
        assert_eq!(err(unknown), "web: boundary names unknown boundary B-nope");

        let state = "[[area]]\nid = \"web\"\nboundary = \"S-1\"\nstatement = \"s\"\n";
        assert_eq!(err(state), "web: boundary names unknown boundary S-1");

        assert_eq!(
            err(&area("web", "min_tier = \"pixels\"")),
            "web: unknown min_tier \"pixels\""
        );

        let zero = area(
            "web",
            "[[area.measurer]]\nid = \"m1\"\ncommand = \"c\"\nmode = \"pr\"\ntimeout_s = 0",
        );
        assert_eq!(err(&zero), "web: measurer m1 requires a timeout_s above 0");

        let mode = area(
            "web",
            "[[area.measurer]]\nid = \"m1\"\ncommand = \"c\"\nmode = \"slow\"\ntimeout_s = 5",
        );
        assert_eq!(err(&mode), "web: measurer m1 has unknown mode \"slow\"");

        let extra = area("web", "colour = \"red\"");
        assert!(err(&extra).contains("unknown field"), "{}", err(&extra));

        let duplicate = format!("{}{}", area("web", ""), area("web", ""));
        assert_eq!(err(&duplicate), "web: duplicate area id");
    }

    #[test]
    fn a_missing_map_file_is_an_empty_map() {
        let map = VerifyMap::parse("", &model()).expect("an empty file must parse");
        assert_eq!(map, VerifyMap::default());
    }

    #[test]
    fn areas_for_paths_maps_a_diff_through_the_boundary_globs() {
        let text = format!(
            "{}[[area]]\nid = \"api-orders\"\nboundary = \"B-orders\"\nstatement = \"s\"\n",
            area("web-checkout", ""),
        );
        let model = model();
        let map = VerifyMap::parse(&text, &model).expect("the map must parse");

        assert_eq!(
            map.areas_for_paths(&model, &["web/pay.ts", "docs/x.md"]),
            vec!["web-checkout"]
        );
        assert_eq!(
            map.areas_for_paths(&model, &["api/orders/new.rs", "web/pay.ts"]),
            vec!["web-checkout", "api-orders"]
        );
        assert!(map.areas_for_paths(&model, &["docs/x.md"]).is_empty());
    }

    #[test]
    fn the_tier_ladder_orders_and_round_trips_its_five_names() {
        assert!(Tier::None < Tier::Terminal);
        assert!(Tier::Terminal < Tier::Http);
        assert!(Tier::Http < Tier::Dom);
        assert!(Tier::Dom < Tier::Browser);
        for tier in Tier::ALL {
            assert_eq!(Tier::parse(tier.name()), Some(tier));
            assert_eq!(tier.to_string(), tier.name());
        }
        assert_eq!(Tier::parse("pixels"), None);
        assert_eq!(Tier::default(), Tier::None);
    }
}
