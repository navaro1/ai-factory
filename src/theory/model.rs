//! The parser for `theory/model.toml`, the operator's theory of one
//! repository.
//!
//! The file holds one table per entry. Every entry carries an `id`, a
//! `kind`, a `title`, a `statement`, and the relations of its kind. The
//! parser is deliberately two-pass: the first pass converts each entry and
//! reports the first structural error in file order, and the second pass
//! checks every relation reference against the ids of the first pass. Every
//! error names the entry, because the operator edits one entry at a time.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

/// The parsed theory model of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub entries: Vec<Entry>,
}

/// One entry of the model. The `kind` key selects the variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// A claim that must hold in every state it constrains.
    Invariant {
        id: String,
        title: String,
        statement: String,
        /// The states or boundaries this invariant constrains.
        constrains: Vec<String>,
    },
    /// One named region of the system.
    State {
        id: String,
        title: String,
        statement: String,
    },
    /// A move from one state to another.
    Transition {
        id: String,
        title: String,
        statement: String,
        from: String,
        to: String,
    },
    /// A line between two states with the path globs it guards.
    Boundary {
        id: String,
        title: String,
        statement: String,
        /// The two region names this boundary separates. Free text.
        sides: Vec<String>,
        /// The path globs that cross this boundary.
        paths: Vec<String>,
    },
    /// A way the system breaks through one boundary.
    Failure {
        id: String,
        title: String,
        statement: String,
        /// The boundary this failure crosses.
        crosses: String,
    },
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Self::Invariant { id, .. }
            | Self::State { id, .. }
            | Self::Transition { id, .. }
            | Self::Boundary { id, .. }
            | Self::Failure { id, .. } => id,
        }
    }
    pub fn title(&self) -> &str {
        match self {
            Self::Invariant { title, .. }
            | Self::State { title, .. }
            | Self::Transition { title, .. }
            | Self::Boundary { title, .. }
            | Self::Failure { title, .. } => title,
        }
    }
    pub fn statement(&self) -> &str {
        match self {
            Self::Invariant { statement, .. }
            | Self::State { statement, .. }
            | Self::Transition { statement, .. }
            | Self::Boundary { statement, .. }
            | Self::Failure { statement, .. } => statement,
        }
    }
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Invariant { .. } => "invariant",
            Self::State { .. } => "state",
            Self::Transition { .. } => "transition",
            Self::Boundary { .. } => "boundary",
            Self::Failure { .. } => "failure",
        }
    }
}

/// One theory error. `entry` names the offending entry when one exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError {
    pub entry: Option<String>,
    pub message: String,
}

impl Display for ModelError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.entry {
            Some(id) => write!(formatter, "{id}: {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

impl std::error::Error for ModelError {}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    #[serde(default)]
    entry: Vec<RawEntry>,
}

/// The permissive input shape. Every field is optional, because the parser
/// turns each absence into an error that names the entry.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    kind: Option<String>,
    id: Option<String>,
    title: Option<String>,
    statement: Option<String>,
    constrains: Option<Vec<String>>,
    from: Option<String>,
    to: Option<String>,
    sides: Option<Vec<String>>,
    paths: Option<Vec<String>>,
    crosses: Option<String>,
}

fn error(entry: Option<&str>, message: impl Into<String>) -> ModelError {
    ModelError {
        entry: entry.map(str::to_string),
        message: message.into(),
    }
}

fn required_text(value: Option<&str>, id: &str, message: &str) -> Result<String, ModelError> {
    let value = value.unwrap_or_default();
    if value.trim().is_empty() {
        return Err(error(Some(id), message));
    }
    Ok(value.to_string())
}

fn required_list<'a>(
    value: Option<&'a Vec<String>>,
    id: &str,
    kind: &'static str,
    field: &str,
) -> Result<&'a [String], ModelError> {
    match value {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(error(Some(id), format!("{kind} requires {field}"))),
    }
}

/// Parse the complete model file. Returns the first error, in file order.
pub fn parse(text: &str) -> Result<Model, ModelError> {
    let raw: RawModel = toml::from_str(text).map_err(|e| ModelError {
        entry: None,
        message: format!("invalid TOML: {e}"),
    })?;
    let mut ids = BTreeSet::new();
    let mut entries = Vec::with_capacity(raw.entry.len());
    for (index, raw) in raw.entry.iter().enumerate() {
        let id = match raw.id.as_deref() {
            Some(id) if !id.trim().is_empty() => id.to_string(),
            _ => return Err(error(None, format!("entry {index}: id is required"))),
        };
        if !ids.insert(id.clone()) {
            return Err(error(Some(&id), "duplicate entry id"));
        }
        let title = required_text(raw.title.as_deref(), &id, "title is required")?;
        let statement = required_text(raw.statement.as_deref(), &id, "statement is required")?;
        let kind = raw.kind.as_deref().unwrap_or_default().to_string();
        let entry = match kind.as_str() {
            "invariant" => {
                let constrains =
                    required_list(raw.constrains.as_ref(), &id, "invariant", "constrains")?
                        .to_vec();
                Entry::Invariant {
                    id,
                    title,
                    statement,
                    constrains,
                }
            }
            "state" => Entry::State {
                id,
                title,
                statement,
            },
            "transition" => {
                let from = required_text(raw.from.as_deref(), &id, "transition requires from")?;
                let to = required_text(raw.to.as_deref(), &id, "transition requires to")?;
                Entry::Transition {
                    id,
                    title,
                    statement,
                    from,
                    to,
                }
            }
            "boundary" => {
                let paths = required_list(raw.paths.as_ref(), &id, "boundary", "paths")?;
                for pattern in paths {
                    if let Err(e) = globset::Glob::new(pattern) {
                        return Err(error(
                            Some(&id),
                            format!("invalid path pattern \"{pattern}\": {e}"),
                        ));
                    }
                }
                let sides = two_sides(raw.sides.as_ref(), &id)?;
                Entry::Boundary {
                    id,
                    title,
                    statement,
                    sides,
                    paths: paths.to_vec(),
                }
            }
            "failure" => {
                let crosses =
                    required_text(raw.crosses.as_deref(), &id, "failure requires crosses")?;
                Entry::Failure {
                    id,
                    title,
                    statement,
                    crosses,
                }
            }
            other => return Err(error(Some(&id), format!("unknown kind \"{other}\""))),
        };
        reject_foreign_fields(raw, &entry)?;
        entries.push(entry);
    }
    check_references(&raw.entry, &entries)?;
    Ok(Model { entries })
}

/// Reject the relation fields that belong to another kind.
fn reject_foreign_fields(raw: &RawEntry, entry: &Entry) -> Result<(), ModelError> {
    let id = entry.id();
    let fields = [
        ("constrains", raw.constrains.is_some()),
        ("from", raw.from.is_some()),
        ("to", raw.to.is_some()),
        ("sides", raw.sides.is_some()),
        ("paths", raw.paths.is_some()),
        ("crosses", raw.crosses.is_some()),
    ];
    let allowed = allowed_fields(entry.kind_name());
    for (field, present) in fields {
        if present && !allowed.contains(&field) {
            return Err(error(
                Some(id),
                format!("{field} is not allowed on kind {}", entry.kind_name()),
            ));
        }
    }
    Ok(())
}

fn allowed_fields(kind: &str) -> &'static [&'static str] {
    match kind {
        "invariant" => &["constrains"],
        "state" => &[],
        "transition" => &["from", "to"],
        "boundary" => &["sides", "paths"],
        "failure" => &["crosses"],
        _ => &[],
    }
}

fn two_sides(value: Option<&Vec<String>>, id: &str) -> Result<Vec<String>, ModelError> {
    match value {
        Some(sides) if sides.len() == 2 && sides.iter().all(|side| !side.trim().is_empty()) => {
            Ok(sides.clone())
        }
        _ => Err(error(Some(id), "boundary requires two sides")),
    }
}

/// Check every reference against the ids of the first pass.
fn check_references(_raw: &[RawEntry], entries: &[Entry]) -> Result<(), ModelError> {
    let kinds: BTreeSet<(&str, &str)> = entries
        .iter()
        .map(|entry| (entry.id(), entry.kind_name()))
        .collect();
    let is_kind = |id: &str, kind: &str| kinds.contains(&(id, kind));
    for entry in entries {
        let id = entry.id();
        match entry {
            Entry::Transition { from, to, .. } => {
                if !is_kind(from, "state") {
                    return Err(error(Some(id), format!("from names unknown state {from}")));
                }
                if !is_kind(to, "state") {
                    return Err(error(Some(id), format!("to names unknown state {to}")));
                }
            }
            Entry::Invariant { constrains, .. } => {
                for reference in constrains {
                    if !is_kind(reference, "state") && !is_kind(reference, "boundary") {
                        return Err(error(
                            Some(id),
                            format!("constrains names unknown entry {reference}"),
                        ));
                    }
                }
            }
            Entry::Failure { crosses, .. } => {
                if !is_kind(crosses, "boundary") {
                    return Err(error(
                        Some(id),
                        format!("crosses names unknown boundary {crosses}"),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, id: &str, extra: &str) -> String {
        format!(
            "[[entry]]\nkind = \"{kind}\"\nid = \"{id}\"\ntitle = \"{id} title\"\nstatement = \"{id} statement\"\n{extra}\n"
        )
    }

    fn err(text: &str) -> String {
        parse(text)
            .expect_err("the broken model must fail")
            .to_string()
    }

    #[test]
    fn a_four_entry_file_parses_into_the_four_enum_arms() {
        let text = format!(
            "{}{}{}{}",
            entry("state", "S-1", ""),
            entry(
                "boundary",
                "B-1",
                "sides = [\"inside\", \"outside\"]\npaths = [\"src/api/**\"]"
            ),
            entry("transition", "T-2", "from = \"S-1\"\nto = \"S-1\""),
            entry("invariant", "I-4", "constrains = [\"S-1\", \"B-1\"]"),
        );

        let model = parse(&text).expect("the four-entry model must parse");

        assert_eq!(model.entries.len(), 4);
        assert_eq!(
            model.entries[0],
            Entry::State {
                id: "S-1".to_string(),
                title: "S-1 title".to_string(),
                statement: "S-1 statement".to_string(),
            }
        );
        assert_eq!(
            model.entries[1],
            Entry::Boundary {
                id: "B-1".to_string(),
                title: "B-1 title".to_string(),
                statement: "B-1 statement".to_string(),
                sides: vec!["inside".to_string(), "outside".to_string()],
                paths: vec!["src/api/**".to_string()],
            }
        );
        assert_eq!(
            model.entries[2],
            Entry::Transition {
                id: "T-2".to_string(),
                title: "T-2 title".to_string(),
                statement: "T-2 statement".to_string(),
                from: "S-1".to_string(),
                to: "S-1".to_string(),
            }
        );
        assert_eq!(
            model.entries[3],
            Entry::Invariant {
                id: "I-4".to_string(),
                title: "I-4 title".to_string(),
                statement: "I-4 statement".to_string(),
                constrains: vec!["S-1".to_string(), "B-1".to_string()],
            }
        );
        let names: Vec<&str> = model.entries.iter().map(Entry::kind_name).collect();
        assert_eq!(names, ["state", "boundary", "transition", "invariant"]);
        assert_eq!(model.entries[3].id(), "I-4");
        assert_eq!(model.entries[3].title(), "I-4 title");
        assert_eq!(model.entries[3].statement(), "I-4 statement");
    }

    #[test]
    fn each_broken_entry_names_its_id() {
        let base = entry("state", "S-1", "");
        let duplicate = format!("{base}{}", entry("state", "S-1", ""));
        assert_eq!(err(&duplicate), "S-1: duplicate entry id");

        let transition = format!(
            "{}{}",
            entry("state", "S-1", ""),
            entry("transition", "T-3", "from = \"S-1\"\nto = \"S-9\"")
        );
        assert_eq!(err(&transition), "T-3: to names unknown state S-9");

        let no_paths = entry("boundary", "B-1", "sides = [\"a\", \"b\"]\npaths = []");
        assert_eq!(err(&no_paths), "B-1: boundary requires paths");

        let bad_pattern = entry("boundary", "B-1", "sides = [\"a\", \"b\"]\npaths = [\"[\"]");
        assert!(
            err(&bad_pattern).starts_with("B-1: invalid path pattern \"[\""),
            "error was: {}",
            err(&bad_pattern)
        );

        let widget = entry("widget", "X-1", "");
        assert_eq!(err(&widget), "X-1: unknown kind \"widget\"");
    }

    #[test]
    fn failure_and_state_reference_errors_name_the_entry() {
        let failure = entry("failure", "F-1", "crosses = \"B-9\"");
        assert_eq!(err(&failure), "F-1: crosses names unknown boundary B-9");

        let state = entry("state", "S-1", "from = \"S-2\"");
        assert_eq!(err(&state), "S-1: from is not allowed on kind state");
    }

    #[test]
    fn the_remaining_required_relations_and_references_fail_in_file_order() {
        assert_eq!(
            err(&entry("invariant", "I-1", "constrains = []")),
            "I-1: invariant requires constrains"
        );
        assert_eq!(
            err(&entry("transition", "T-1", "to = \"S-1\"")),
            "T-1: transition requires from"
        );
        assert_eq!(
            err(&entry("transition", "T-1", "from = \"S-1\"")),
            "T-1: transition requires to"
        );
        assert_eq!(
            err(&entry(
                "boundary",
                "B-1",
                "paths = [\"src/**\"]\nsides = [\"a\"]"
            )),
            "B-1: boundary requires two sides"
        );
        assert_eq!(
            err(&entry("failure", "F-1", "")),
            "F-1: failure requires crosses"
        );
        let invariant = format!(
            "{}{}",
            entry("state", "S-1", ""),
            entry("invariant", "I-2", "constrains = [\"S-9\"]")
        );
        assert_eq!(err(&invariant), "I-2: constrains names unknown entry S-9");
    }

    #[test]
    fn the_first_error_in_file_order_wins() {
        let text = format!(
            "{}{}",
            entry("state", "S-1", "from = \"S-2\""),
            entry(
                "boundary",
                "B-2",
                "sides = [\"inside\", \"outside\"]\npaths = []"
            )
        );
        assert_eq!(err(&text), "S-1: from is not allowed on kind state");
    }

    #[test]
    fn an_entry_without_an_id_names_its_index() {
        let text = "[[entry]]\nkind = \"state\"\ntitle = \"t\"\nstatement = \"s\"\n";
        assert_eq!(err(text), "entry 0: id is required");
    }

    #[test]
    fn a_toml_syntax_error_names_no_entry() {
        let parsed = parse("not toml [").expect_err("the broken file must fail");
        assert_eq!(parsed.entry, None);
        assert!(parsed.message.starts_with("invalid TOML: "));
    }

    #[test]
    fn an_empty_model_parses() {
        let model = parse("").expect("an empty model must parse");
        assert!(model.entries.is_empty());
    }
}
