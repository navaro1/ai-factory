//! Holds the configurable names of every label the factory owns.
//!
//! The factory drives its pipeline with GitHub labels. Each repository can
//! already carry its own taxonomy, so the name of every gate label and the
//! prefix of every complexity label come from the configuration.
//!
//! [`LabelNames`] holds one complete set. [`LabelNames::default`] returns the
//! historical names, so a configuration without a `[labels]` table keeps the
//! old behaviour. The configuration layers a global `[labels]` table over the
//! default set, then one `[repo.<alias>.labels]` table over that.

use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

/// The default name of the label that asks the factory to shape a raw issue.
pub const DEFAULT_TO_REFINE: &str = "to-refine";

/// The default name of the label that marks a shaped issue as implementable.
pub const DEFAULT_REFINED: &str = "refined";

/// The default name of the label that marks a split parent.
pub const DEFAULT_EPIC: &str = "epic";

/// The default name of the label that marks one sub-ticket of a split.
pub const DEFAULT_CHUNK: &str = "chunk";

/// The default name of the label that asks a human to decide.
pub const DEFAULT_NEEDS_HUMAN: &str = "needs-human";

/// The default name of the label that stacks a pull request into a train.
pub const DEFAULT_RELEASE_STACKED: &str = "release-stacked";

/// The default prefix of the implementation complexity label.
pub const DEFAULT_COMPLEXITY_PREFIX: &str = "complexity:";

/// The default prefix of the review complexity label.
pub const DEFAULT_REVIEW_COMPLEXITY_PREFIX: &str = "review-complexity:";

/// One addressable label name in [`LabelNames`].
///
/// The settings panel and the configuration writer use this key to name one
/// field without a free-form string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelKey {
    ToRefine,
    Refined,
    Epic,
    Chunk,
    NeedsHuman,
    ReleaseStacked,
    ComplexityPrefix,
    ReviewComplexityPrefix,
}

impl LabelKey {
    /// Every key, in the order the settings panel shows them.
    pub const ALL: [Self; 8] = [
        Self::ToRefine,
        Self::Refined,
        Self::Epic,
        Self::Chunk,
        Self::NeedsHuman,
        Self::ReleaseStacked,
        Self::ComplexityPrefix,
        Self::ReviewComplexityPrefix,
    ];

    /// The TOML key of this label inside a `[labels]` table.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToRefine => "to_refine",
            Self::Refined => "refined",
            Self::Epic => "epic",
            Self::Chunk => "chunk",
            Self::NeedsHuman => "needs_human",
            Self::ReleaseStacked => "release_stacked",
            Self::ComplexityPrefix => "complexity_prefix",
            Self::ReviewComplexityPrefix => "review_complexity_prefix",
        }
    }

    /// The default value of this label.
    pub const fn default_value(self) -> &'static str {
        match self {
            Self::ToRefine => DEFAULT_TO_REFINE,
            Self::Refined => DEFAULT_REFINED,
            Self::Epic => DEFAULT_EPIC,
            Self::Chunk => DEFAULT_CHUNK,
            Self::NeedsHuman => DEFAULT_NEEDS_HUMAN,
            Self::ReleaseStacked => DEFAULT_RELEASE_STACKED,
            Self::ComplexityPrefix => DEFAULT_COMPLEXITY_PREFIX,
            Self::ReviewComplexityPrefix => DEFAULT_REVIEW_COMPLEXITY_PREFIX,
        }
    }

    /// True when this key names a prefix instead of a whole label.
    pub const fn is_prefix(self) -> bool {
        matches!(self, Self::ComplexityPrefix | Self::ReviewComplexityPrefix)
    }
}

impl Display for LabelKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One complete set of label names for one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelNames {
    pub to_refine: String,
    pub refined: String,
    pub epic: String,
    pub chunk: String,
    pub needs_human: String,
    pub release_stacked: String,
    pub complexity_prefix: String,
    pub review_complexity_prefix: String,
}

impl Default for LabelNames {
    fn default() -> Self {
        Self {
            to_refine: DEFAULT_TO_REFINE.to_string(),
            refined: DEFAULT_REFINED.to_string(),
            epic: DEFAULT_EPIC.to_string(),
            chunk: DEFAULT_CHUNK.to_string(),
            needs_human: DEFAULT_NEEDS_HUMAN.to_string(),
            release_stacked: DEFAULT_RELEASE_STACKED.to_string(),
            complexity_prefix: DEFAULT_COMPLEXITY_PREFIX.to_string(),
            review_complexity_prefix: DEFAULT_REVIEW_COMPLEXITY_PREFIX.to_string(),
        }
    }
}

impl LabelNames {
    /// Read one name by key.
    pub fn get(&self, key: LabelKey) -> &str {
        match key {
            LabelKey::ToRefine => &self.to_refine,
            LabelKey::Refined => &self.refined,
            LabelKey::Epic => &self.epic,
            LabelKey::Chunk => &self.chunk,
            LabelKey::NeedsHuman => &self.needs_human,
            LabelKey::ReleaseStacked => &self.release_stacked,
            LabelKey::ComplexityPrefix => &self.complexity_prefix,
            LabelKey::ReviewComplexityPrefix => &self.review_complexity_prefix,
        }
    }

    /// Write one name by key.
    pub fn set(&mut self, key: LabelKey, value: String) {
        let slot = match key {
            LabelKey::ToRefine => &mut self.to_refine,
            LabelKey::Refined => &mut self.refined,
            LabelKey::Epic => &mut self.epic,
            LabelKey::Chunk => &mut self.chunk,
            LabelKey::NeedsHuman => &mut self.needs_human,
            LabelKey::ReleaseStacked => &mut self.release_stacked,
            LabelKey::ComplexityPrefix => &mut self.complexity_prefix,
            LabelKey::ReviewComplexityPrefix => &mut self.review_complexity_prefix,
        };
        *slot = value;
    }

    /// True when the list holds the exact name of this key.
    pub fn has(&self, key: LabelKey, labels: &[String]) -> bool {
        let wanted = self.get(key);
        labels.iter().any(|label| label == wanted)
    }

    /// Every whole label name, without the two prefixes.
    pub fn whole_names(&self) -> Vec<&str> {
        LabelKey::ALL
            .into_iter()
            .filter(|key| !key.is_prefix())
            .map(|key| self.get(key))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_set_keeps_the_historical_names() {
        let names = LabelNames::default();
        assert_eq!(names.to_refine, "to-refine");
        assert_eq!(names.refined, "refined");
        assert_eq!(names.epic, "epic");
        assert_eq!(names.chunk, "chunk");
        assert_eq!(names.needs_human, "needs-human");
        assert_eq!(names.release_stacked, "release-stacked");
        assert_eq!(names.complexity_prefix, "complexity:");
        assert_eq!(names.review_complexity_prefix, "review-complexity:");
    }

    #[test]
    fn every_key_reads_and_writes_its_own_field() {
        for key in LabelKey::ALL {
            let mut names = LabelNames::default();
            assert_eq!(names.get(key), key.default_value());
            names.set(key, "changed".to_string());
            assert_eq!(names.get(key), "changed");
            for other in LabelKey::ALL {
                if other != key {
                    assert_eq!(names.get(other), other.default_value(), "{other} moved");
                }
            }
        }
    }

    #[test]
    fn has_matches_one_exact_name() {
        let mut names = LabelNames::default();
        names.set(LabelKey::Epic, "type:epic".to_string());
        let labels = vec!["type:epic".to_string(), "priority:0".to_string()];
        assert!(names.has(LabelKey::Epic, &labels));
        assert!(!names.has(LabelKey::Refined, &labels));
        assert!(!names.has(LabelKey::Epic, &["epic".to_string()]));
    }

    #[test]
    fn whole_names_drops_the_two_prefixes() {
        let names = LabelNames::default();
        assert_eq!(
            names.whole_names(),
            vec![
                "to-refine",
                "refined",
                "epic",
                "chunk",
                "needs-human",
                "release-stacked"
            ]
        );
    }

    #[test]
    fn every_toml_key_is_unique_and_snake_case() {
        let mut seen = std::collections::BTreeSet::new();
        for key in LabelKey::ALL {
            assert!(seen.insert(key.as_str()), "{key} repeats");
            assert!(
                key.as_str()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "{key} is not snake case"
            );
        }
    }
}
