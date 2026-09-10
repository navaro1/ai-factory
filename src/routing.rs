//! Selects one typed execution route from GitHub complexity labels.

use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::labels::{LabelKey, LabelNames};
use crate::model::ItemKind;

/// The two pipeline stages that use tag routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TagRouteStage {
    Implement,
    Review,
}

impl TagRouteStage {
    pub const ALL: [Self; 2] = [Self::Implement, Self::Review];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Implement => "implement",
            Self::Review => "review",
        }
    }

    /// The configurable label whose value is this stage's prefix.
    pub const fn prefix_key(self) -> LabelKey {
        match self {
            Self::Implement => LabelKey::ComplexityPrefix,
            Self::Review => LabelKey::ReviewComplexityPrefix,
        }
    }

    /// This stage's prefix under one label set.
    pub fn label_prefix(self, names: &LabelNames) -> &str {
        names.get(self.prefix_key())
    }
}

impl Display for TagRouteStage {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A closed complexity scale in increasing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComplexityLevel {
    Low,
    Medium,
    High,
    VeryHigh,
}

impl ComplexityLevel {
    pub const ALL: [Self; 4] = [Self::Low, Self::Medium, Self::High, Self::VeryHigh];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::VeryHigh => "very-high",
        }
    }
}

impl Display for ComplexityLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One cell in the fixed stage and complexity matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TagRouteKey {
    pub stage: TagRouteStage,
    pub level: ComplexityLevel,
}

impl TagRouteKey {
    pub const fn new(stage: TagRouteStage, level: ComplexityLevel) -> Self {
        Self { stage, level }
    }

    pub fn table_name(self) -> String {
        format!("tag_routes.{}.{}", self.stage, self.level)
    }

    /// The full label that selects this route under one label set.
    pub fn label(self, names: &LabelNames) -> String {
        format!("{}{}", self.stage.label_prefix(names), self.level)
    }
}

impl Display for TagRouteKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.stage, self.level)
    }
}

/// The selected level and every valid label that contributed to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSelection {
    pub level: ComplexityLevel,
    pub matched_labels: Vec<String>,
}

/// One item and label that contributed to a stored route selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRouteMatch {
    pub kind: ItemKind,
    pub number: u64,
    pub label: String,
}

/// The tag evidence stored with one task role binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRouteBinding {
    pub key: TagRouteKey,
    pub matches: Vec<TagRouteMatch>,
}

/// Select the highest exact label, or medium when no valid label exists.
pub fn select_level(
    stage: TagRouteStage,
    labels: &[String],
    names: &LabelNames,
) -> TagSelection {
    let mut level = None;
    let mut matched_labels = Vec::new();
    for label in labels {
        let candidate = level_from_label(stage, label, names);
        if let Some(candidate) = candidate {
            level =
                Some(level.map_or(candidate, |current: ComplexityLevel| current.max(candidate)));
            matched_labels.push(label.clone());
        }
    }
    TagSelection {
        level: level.unwrap_or(ComplexityLevel::Medium),
        matched_labels,
    }
}

/// Parse one exact label for the selected stage.
pub fn level_from_label(
    stage: TagRouteStage,
    label: &str,
    names: &LabelNames,
) -> Option<ComplexityLevel> {
    let prefix = stage.label_prefix(names);
    ComplexityLevel::ALL
        .into_iter()
        .find(|candidate| label == format!("{prefix}{}", candidate.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn missing_and_unknown_labels_select_medium() {
        assert_eq!(
            select_level(TagRouteStage::Implement, &[], &LabelNames::default()).level,
            ComplexityLevel::Medium
        );
        assert_eq!(
            select_level(
                TagRouteStage::Review,
                &labels(&["review-complexity:urgent", "complexity:high"]),
                &LabelNames::default(),
            ),
            TagSelection {
                level: ComplexityLevel::Medium,
                matched_labels: vec![],
            }
        );
    }

    #[test]
    fn exact_stage_labels_select_the_highest_level() {
        let selection = select_level(
            TagRouteStage::Implement,
            &labels(&[
                "complexity:low",
                "Complexity:very-high",
                "complexity:very-high",
                "review-complexity:high",
            ]),
            &LabelNames::default(),
        );
        assert_eq!(selection.level, ComplexityLevel::VeryHigh);
        assert_eq!(
            selection.matched_labels,
            labels(&["complexity:low", "complexity:very-high"])
        );
    }

    /// A repository can rename the prefix. The selector then matches the
    /// configured prefix and ignores the historical one.
    #[test]
    fn a_renamed_prefix_replaces_the_default_prefix() {
        let mut names = LabelNames::default();
        names.set(LabelKey::ComplexityPrefix, "size/".to_string());

        let selection = select_level(
            TagRouteStage::Implement,
            &labels(&["size/high", "complexity:very-high"]),
            &names,
        );
        assert_eq!(selection.level, ComplexityLevel::High);
        assert_eq!(selection.matched_labels, labels(&["size/high"]));

        assert_eq!(
            TagRouteKey::new(TagRouteStage::Implement, ComplexityLevel::High).label(&names),
            "size/high"
        );
    }

    /// Renaming one stage prefix leaves the other stage alone.
    #[test]
    fn a_renamed_implement_prefix_leaves_review_alone() {
        let mut names = LabelNames::default();
        names.set(LabelKey::ComplexityPrefix, "size/".to_string());
        let found = select_level(
            TagRouteStage::Review,
            &labels(&["review-complexity:very-high"]),
            &names,
        );
        assert_eq!(found.level, ComplexityLevel::VeryHigh);
    }

    #[test]
    fn every_exact_label_selects_its_typed_level() {
        for stage in TagRouteStage::ALL {
            for level in ComplexityLevel::ALL {
                let names = LabelNames::default();
                let label = format!("{}{}", stage.label_prefix(&names), level);
                assert_eq!(
                    select_level(stage, std::slice::from_ref(&label), &names),
                    TagSelection {
                        level,
                        matched_labels: vec![label],
                    }
                );
            }
        }
    }
}
