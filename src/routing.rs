//! Selects one typed execution route from GitHub complexity labels.

use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

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

    pub const fn label_prefix(self) -> &'static str {
        match self {
            Self::Implement => "complexity:",
            Self::Review => "review-complexity:",
        }
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

    pub fn label(self) -> String {
        format!("{}{}", self.stage.label_prefix(), self.level)
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
pub fn select_level(stage: TagRouteStage, labels: &[String]) -> TagSelection {
    let mut level = ComplexityLevel::Medium;
    let mut found = false;
    let mut matched_labels = Vec::new();
    for label in labels {
        let candidate = level_from_label(stage, label);
        if let Some(candidate) = candidate {
            found = true;
            level = level.max(candidate);
            matched_labels.push(label.clone());
        }
    }
    if !found {
        level = ComplexityLevel::Medium;
    }
    TagSelection {
        level,
        matched_labels,
    }
}

/// Parse one exact label for the selected stage.
pub fn level_from_label(stage: TagRouteStage, label: &str) -> Option<ComplexityLevel> {
    ComplexityLevel::ALL
        .into_iter()
        .find(|candidate| label == format!("{}{}", stage.label_prefix(), candidate.as_str()))
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
            select_level(TagRouteStage::Implement, &[]).level,
            ComplexityLevel::Medium
        );
        assert_eq!(
            select_level(
                TagRouteStage::Review,
                &labels(&["review-complexity:urgent", "complexity:high"]),
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
        );
        assert_eq!(selection.level, ComplexityLevel::VeryHigh);
        assert_eq!(
            selection.matched_labels,
            labels(&["complexity:low", "complexity:very-high"])
        );
    }
}
