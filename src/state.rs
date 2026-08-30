//! Reads and writes `state.json`, the daemon's only private file.
//!
//! The file holds only what GitHub cannot hold: the operator's stage limit
//! overrides, lane overrides, and release policy overrides, plus the
//! `last_fire_ms` stamp of every release train. GitHub holds all work state,
//! so the daemon rebuilds everything else after a restart. The daemon writes
//! the file through a temporary file and a rename, and only when a value
//! changed. A missing or corrupt file is not an error: the loader logs once
//! and the daemon continues with the config defaults.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::ReleasePolicy;
use crate::model::Stage;

/// One persisted lane reservation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LaneEntry {
    /// The stage the reservation applies to.
    stage: Stage,
    /// The repository alias of the reservation.
    repo: String,
    /// The reserved slots. 0 keeps a former reservation disabled.
    slots: usize,
}

/// The on-disk shape of `state.json`.
///
/// Every field defaults to empty, so a file written by an older daemon and a
/// file with missing sections both load.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
struct StateFile {
    /// The stage limit overrides, by stage.
    #[serde(default)]
    stage_limits: BTreeMap<Stage, usize>,
    /// The lane overrides. A JSON map cannot key on a pair, so they are
    /// entries.
    #[serde(default)]
    lanes: Vec<LaneEntry>,
    /// The release policy overrides, by repository alias.
    #[serde(default)]
    policies: BTreeMap<String, ReleasePolicy>,
    /// The last fire stamp of each train, by repository alias.
    #[serde(default)]
    last_fire_ms: BTreeMap<String, u64>,
}

/// The state the daemon persists across restarts.
///
/// Every field holds overrides only: a value that matches the config file is
/// absent. An empty state is the state that matches the config.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DaemonState {
    /// The stage limit overrides, by stage.
    pub stage_limits: BTreeMap<Stage, usize>,
    /// The lane overrides as `(stage, repo, slots)` entries.
    pub lanes: Vec<(Stage, String, usize)>,
    /// The release policy overrides, by repository alias.
    pub policies: BTreeMap<String, ReleasePolicy>,
    /// The last fire stamp of each train, by repository alias.
    pub last_fire_ms: BTreeMap<String, u64>,
}

impl DaemonState {
    /// Read the state file, or the empty state when it is missing or corrupt.
    ///
    /// A missing or corrupt file is not an error. The call logs it once on
    /// standard error, and the daemon continues with the config defaults.
    pub fn load(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "{}: no state file; the daemon starts with the config defaults",
                    path.display()
                );
                return DaemonState::default();
            }
            Err(e) => {
                eprintln!(
                    "{}: cannot read the state file ({e}); the daemon starts with the \
                     config defaults",
                    path.display()
                );
                return DaemonState::default();
            }
        };
        match serde_json::from_str::<StateFile>(&text) {
            Ok(file) => DaemonState {
                stage_limits: file.stage_limits,
                lanes: file
                    .lanes
                    .into_iter()
                    .map(|entry| (entry.stage, entry.repo, entry.slots))
                    .collect(),
                policies: file.policies,
                last_fire_ms: file.last_fire_ms,
            },
            Err(e) => {
                eprintln!(
                    "{}: corrupt state file ({e}); the daemon starts with the config \
                     defaults",
                    path.display()
                );
                DaemonState::default()
            }
        }
    }

    /// Serialize the state to one JSON line.
    ///
    /// The daemon compares this text against the last written text, so a
    /// drive with no change writes nothing.
    pub fn to_json(&self) -> Result<String> {
        let file = StateFile {
            stage_limits: self.stage_limits.clone(),
            lanes: self
                .lanes
                .iter()
                .map(|(stage, repo, slots)| LaneEntry {
                    stage: *stage,
                    repo: repo.clone(),
                    slots: *slots,
                })
                .collect(),
            policies: self.policies.clone(),
            last_fire_ms: self.last_fire_ms.clone(),
        };
        serde_json::to_string(&file).context("cannot serialize the daemon state")
    }

    /// Write the state through a temporary file and a rename.
    ///
    /// A crash can therefore never leave a half-written `state.json`.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = self.to_json()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("cannot rename {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique directory for one test's files.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aif-state-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_state_survives_a_round_trip() {
        let dir = temp_dir("round-trip");
        let path = dir.join("state.json");
        let state = DaemonState {
            stage_limits: BTreeMap::from([(Stage::Implement, 9)]),
            lanes: vec![(Stage::Review, "borsuk".to_string(), 2)],
            policies: BTreeMap::from([(
                "borsuk".to_string(),
                ReleasePolicy::Interval { minutes: 5 },
            )]),
            last_fire_ms: BTreeMap::from([("borsuk".to_string(), 1_000)]),
        };
        state.save(&path).unwrap();
        let loaded = DaemonState::load(&path);
        assert_eq!(loaded, state);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_state_file_loads_the_defaults() {
        let dir = temp_dir("missing");
        let loaded = DaemonState::load(&dir.join("absent.json"));
        assert_eq!(loaded, DaemonState::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_state_file_loads_the_defaults() {
        let dir = temp_dir("corrupt");
        let path = dir.join("state.json");
        fs::write(&path, "not json at all {").unwrap();
        let loaded = DaemonState::load(&path);
        assert_eq!(loaded, DaemonState::default());
        assert_eq!(loaded.stage_limits.get(&Stage::Refine), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_empty_state_still_round_trips() {
        let dir = temp_dir("empty");
        let path = dir.join("state.json");
        DaemonState::default().save(&path).unwrap();
        let loaded = DaemonState::load(&path);
        assert_eq!(loaded, DaemonState::default());
        let _ = fs::remove_dir_all(dir);
    }
}
