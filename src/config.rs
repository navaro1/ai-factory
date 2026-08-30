//! Loads and validates `factory.toml`, and resolves the state, config, and
//! socket paths from the naming rules in `docs/v0.5/SPEC.md`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::exec::{Exec, RealExec};
use crate::model::Stage;

/// How and when the release train of one repository fires.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "lowercase")]
pub enum ReleasePolicy {
    /// A human fires the train. This is the default.
    #[default]
    Manual,
    /// The train fires when `minutes` passed since the last fire and the
    /// queue is not empty.
    Interval {
        /// Minutes between two fires. At least 1.
        minutes: u64,
    },
    /// The train fires when the queue holds `count` ready pull requests.
    Threshold {
        /// Queue size that fires the train. At least 1.
        count: usize,
    },
}

/// The per-stage agent settings: which runner and model to use, and how much
/// work the stage may hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageConfig {
    /// The model id passed to the runner, for example `claude-opus-5[1m]`.
    pub model: String,
    /// The runner program: `claude` or `opencode`.
    pub runner: String,
    /// The opencode effort variant, for example `xhigh`. None for claude.
    pub variant: Option<String>,
    /// How many tasks this stage may run at once. At least 1.
    pub limit: usize,
    /// Whether the agent auto-approves its own tool calls. True by default.
    pub yolo: bool,
}

impl StageConfig {
    /// Return the default task limit for one stage.
    fn default_limit(stage: Stage) -> usize {
        match stage {
            Stage::Refine | Stage::Implement => 3,
            Stage::Review => 7,
            Stage::Release => 1,
        }
    }
}

/// One configured repository: where it lives, its lanes, and its release
/// policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    /// The alias from the config file, for example `borsuk`.
    pub alias: String,
    /// The absolute path of the repository checkout.
    pub path: PathBuf,
    /// The `owner/name` GitHub slug, filled at load time from the origin
    /// remote. Empty until `Config::load` resolves it.
    pub owner_repo: String,
    /// Stage slots reserved for this repository, keyed by stage.
    pub lanes: BTreeMap<Stage, usize>,
    /// How the release train fires. Manual when absent.
    pub release: ReleasePolicy,
}

/// The whole factory configuration.
///
/// Build it with [`Config::parse`] for the structural part or
/// [`Config::load`] for a file, which also resolves each repository's
/// `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Stage settings. `parse` fills all four stages, so [`Config::stage`]
    /// always finds an entry.
    pub stages: BTreeMap<Stage, StageConfig>,
    /// Repositories keyed by alias.
    pub repos: BTreeMap<String, RepoConfig>,
}

impl Config {
    /// The settings of one stage. All four stages are always present after
    /// `parse`.
    pub fn stage(&self, stage: Stage) -> &StageConfig {
        &self.stages[&stage]
    }

    /// Parse configuration text and run every check that needs no
    /// filesystem or git access. Absent optional keys take their defaults.
    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text).context("invalid TOML")?;

        let mut stages = BTreeMap::new();
        for (name, raw_stage) in raw.stage {
            let stage = Stage::from_str(&name).map_err(|e| anyhow!("stage.{name}: {e}"))?;
            let model = raw_stage
                .model
                .ok_or_else(|| anyhow!("stage.{stage}.model is required"))?;
            let runner = raw_stage
                .runner
                .ok_or_else(|| anyhow!("stage.{stage}.runner is required"))?;
            let limit = raw_stage
                .limit
                .unwrap_or_else(|| StageConfig::default_limit(stage));
            if limit < 1 {
                bail!("stage.{stage}.limit must be at least 1, got {limit}");
            }
            stages.insert(
                stage,
                StageConfig {
                    model,
                    runner,
                    variant: raw_stage.variant,
                    limit,
                    yolo: raw_stage.yolo.unwrap_or(true),
                },
            );
        }
        for stage in Stage::ALL {
            if !stages.contains_key(&stage) {
                bail!("stage.{stage} is required");
            }
        }

        let mut repos = BTreeMap::new();
        for (alias, raw_repo) in raw.repo {
            if !valid_alias(&alias) {
                bail!("repo.\"{alias}\": alias must match [a-z0-9._-]+");
            }
            let path = raw_repo
                .path
                .ok_or_else(|| anyhow!("repo.{alias}.path is required"))?;
            if path.is_empty() {
                bail!("repo.{alias}.path must not be empty");
            }
            let mut lanes = BTreeMap::new();
            for (lane_name, count) in raw_repo.lanes {
                let stage = Stage::from_str(&lane_name)
                    .map_err(|e| anyhow!("repo.{alias}.lanes.{lane_name}: {e}"))?;
                lanes.insert(stage, count);
            }
            let release = raw_repo.release;
            match &release {
                ReleasePolicy::Threshold { count } if *count < 1 => {
                    bail!("repo.{alias}.release.count must be at least 1, got {count}");
                }
                ReleasePolicy::Interval { minutes } if *minutes < 1 => {
                    bail!("repo.{alias}.release.minutes must be at least 1, got {minutes}");
                }
                _ => {}
            }
            repos.insert(
                alias.clone(),
                RepoConfig {
                    alias,
                    path: PathBuf::from(path),
                    owner_repo: String::new(),
                    lanes,
                    release,
                },
            );
        }

        for stage in Stage::ALL {
            let sum = repos
                .values()
                .filter_map(|repo| repo.lanes.get(&stage))
                .try_fold(0usize, |sum, count| {
                    sum.checked_add(*count)
                        .ok_or_else(|| anyhow!("stage.{stage}: lane reservations overflow usize"))
                })?;
            let limit = stages[&stage].limit;
            if sum > limit {
                bail!(
                    "stage.{stage}: lane reservations sum to {sum}, exceeding \
                     stage.{stage}.limit {limit}"
                );
            }
        }

        Ok(Config { stages, repos })
    }

    /// Read and parse the config file. `None` resolves the default path from
    /// the naming rules. A missing file is an error that names where to
    /// create it and the example file to copy. After parsing, every
    /// repository path is checked on disk and its `owner/name` is resolved
    /// from the origin remote.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        Self::load_with_exec(path, &RealExec)
    }

    /// Load a file with the specified command executor.
    fn load_with_exec(path: Option<&Path>, exec: &dyn Exec) -> Result<Self> {
        let path = path.map_or_else(default_config_path, Path::to_path_buf);
        if !path.exists() {
            bail!(
                "no config file at {}; create it there, or copy \
                 docs/v0.5/factory.example.toml as a starting point",
                path.display()
            );
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut config = Config::parse(&text).with_context(|| format!("in {}", path.display()))?;
        config.resolve(exec)?;
        Ok(config)
    }

    /// Check every repository path on disk and fill `owner_repo` from the
    /// origin remote. Needs no network.
    fn resolve(&mut self, exec: &dyn Exec) -> Result<()> {
        for repo in self.repos.values_mut() {
            let alias = repo.alias.clone();
            let path = repo.path.clone();
            if !path.exists() {
                bail!("repo.{alias}.path: {} does not exist", path.display());
            }
            if !path.join(".git").exists() {
                bail!(
                    "repo.{alias}.path: {} holds no .git entry and is not a git \
                     repository",
                    path.display()
                );
            }
            let path_text = path.to_string_lossy().into_owned();
            let out = exec
                .run(
                    "git",
                    &["-C", &path_text, "remote", "get-url", "origin"],
                    None,
                )
                .with_context(|| format!("repo.{alias}: cannot run git"))?;
            if out.status != 0 {
                bail!(
                    "repo.{alias}: git remote get-url origin failed: {}",
                    out.stderr.trim()
                );
            }
            let url = out.stdout.trim();
            let owner_repo = parse_owner_repo(url).ok_or_else(|| {
                anyhow!("repo.{alias}: cannot read owner/repo from origin url {url:?}")
            })?;
            repo.owner_repo = owner_repo;
        }
        Ok(())
    }
}

/// The raw shape of one `[stage.<name>]` table.
#[derive(Debug, Default, Deserialize)]
struct RawStage {
    model: Option<String>,
    runner: Option<String>,
    variant: Option<String>,
    limit: Option<usize>,
    yolo: Option<bool>,
}

/// The raw shape of one `[repo.<alias>]` table.
#[derive(Debug, Default, Deserialize)]
struct RawRepo {
    path: Option<String>,
    #[serde(default)]
    lanes: BTreeMap<String, usize>,
    #[serde(default)]
    release: ReleasePolicy,
}

/// The raw shape of the whole file.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    stage: BTreeMap<String, RawStage>,
    #[serde(default)]
    repo: BTreeMap<String, RawRepo>,
}

/// Whether `alias` matches `[a-z0-9._-]+`.
fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Parse a git remote url into `owner/name`.
///
/// Accepts the scp form `git@github.com:owner/repo.git` and the
/// `https://github.com/owner/repo` form, with or without the `.git` suffix.
/// Returns None for anything else.
pub fn parse_owner_repo(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"));
    let path = match after_scheme {
        Some(rest) => {
            // `rest` is `host/owner/repo`; the host may carry a `git@` user.
            let (host, path) = rest.split_once('/')?;
            if host.is_empty() {
                return None;
            }
            path
        }
        None => match url.split_once(':') {
            // scp-like form: `git@github.com:owner/repo`.
            Some((_user_host, path)) => path,
            None => return None,
        },
    };
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// The state directory: `$XDG_STATE_HOME/aif` or `~/.local/state/aif`.
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join("aif")
}

/// The config directory: `$XDG_CONFIG_HOME/aif` or `~/.config/aif`.
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("aif")
}

/// The default config file path: `config_dir()/factory.toml`.
pub fn default_config_path() -> PathBuf {
    config_dir().join("factory.toml")
}

/// The control socket path: `$XDG_RUNTIME_DIR/aif/daemon.sock`, else
/// `state_dir()/daemon.sock`.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("aif").join("daemon.sock"),
        None => state_dir().join("daemon.sock"),
    }
}

/// One XDG directory with its home-relative fallback.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var).filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => match std::env::var_os("HOME").filter(|value| !value.is_empty()) {
            Some(home) => PathBuf::from(home).join(fallback),
            None => PathBuf::from(fallback),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CmdOut, ScriptExec};

    const EXAMPLE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/v0.5/factory.example.toml"
    );

    fn parse_err(text: &str) -> String {
        Config::parse(text).unwrap_err().to_string()
    }

    fn config_text(stage_overrides: &[(Stage, &str)], suffix: &str) -> String {
        let mut text = String::new();
        for stage in Stage::ALL {
            text.push_str(&format!(
                "[stage.{stage}]\nmodel = \"model\"\nrunner = \"runner\"\n"
            ));
            if let Some((_, value)) = stage_overrides
                .iter()
                .find(|(candidate, _)| *candidate == stage)
            {
                text.push_str(value);
                text.push('\n');
            }
        }
        text.push_str(suffix);
        text
    }

    #[test]
    fn example_file_parses_with_every_override() {
        let text = fs::read_to_string(EXAMPLE).expect("the example file must exist");
        let config = Config::parse(&text).expect("the example file must parse");

        let refine = config.stage(Stage::Refine);
        assert_eq!(refine.model, "claude-opus-5[1m]");
        assert_eq!(refine.runner, "claude");
        assert_eq!(refine.limit, 3);
        assert!(refine.yolo);
        assert_eq!(refine.variant, None);

        let implement = config.stage(Stage::Implement);
        assert_eq!(implement.model, "zai-coding-plan/glm-5.3-flash");
        assert_eq!(implement.runner, "opencode");
        assert_eq!(implement.limit, 3);
        assert!(implement.yolo, "yolo defaults to true when absent");
        assert_eq!(implement.variant, None);

        let review = config.stage(Stage::Review);
        assert_eq!(review.model, "openai/gpt-5.6-sol");
        assert_eq!(review.runner, "opencode");
        assert_eq!(review.variant, Some("xhigh".to_string()));
        assert_eq!(review.limit, 7);
        assert!(review.yolo);

        let release = config.stage(Stage::Release);
        assert_eq!(release.model, "claude-opus-5[1m]");
        assert_eq!(release.runner, "claude");
        assert_eq!(release.variant, None);
        assert_eq!(release.limit, 1);
        assert!(release.yolo);

        let borsuk = &config.repos["borsuk"];
        assert_eq!(borsuk.alias, "borsuk");
        assert_eq!(borsuk.path, PathBuf::from("/home/navaro/Workplace/borsuk"));
        assert_eq!(
            borsuk.lanes.get(&Stage::Implement),
            Some(&1),
            "the override is kept"
        );
        assert_eq!(borsuk.lanes.len(), 1);
        assert_eq!(borsuk.release, ReleasePolicy::Manual);
        assert_eq!(borsuk.owner_repo, "", "parse does not resolve remotes");

        let qubitsok = &config.repos["qubitsok"];
        assert_eq!(qubitsok.alias, "qubitsok");
        assert_eq!(
            qubitsok.path,
            PathBuf::from("/home/navaro/Workplace/qubitsok")
        );
        assert_eq!(qubitsok.owner_repo, "");
        assert!(qubitsok.lanes.is_empty(), "lanes default to empty");
        assert_eq!(
            qubitsok.release,
            ReleasePolicy::Threshold { count: 3 },
            "the threshold override is kept"
        );
    }

    #[test]
    fn absent_optional_keys_take_the_specified_defaults() {
        let text = config_text(&[], "[repo.x]\npath = \"/tmp/x\"\n");
        let config = Config::parse(&text).unwrap();

        assert_eq!(config.stage(Stage::Refine).limit, 3);
        assert_eq!(config.stage(Stage::Implement).limit, 3);
        assert_eq!(config.stage(Stage::Review).limit, 7);
        assert_eq!(config.stage(Stage::Release).limit, 1);
        assert!(config.stage(Stage::Review).yolo);
        assert_eq!(config.stage(Stage::Release).variant, None);
        assert_eq!(config.stage(Stage::Refine).model, "model");
        assert_eq!(config.stage(Stage::Review).runner, "runner");

        let repo = &config.repos["x"];
        assert!(repo.lanes.is_empty());
        assert_eq!(repo.release, ReleasePolicy::Manual);
        assert_eq!(repo.owner_repo, "");
    }

    #[test]
    fn a_missing_model_names_the_stage_key() {
        let err = parse_err("[stage.refine]\nrunner = \"claude\"\n");
        assert!(err.contains("stage.refine.model"), "message was: {err}");
    }

    #[test]
    fn a_missing_runner_names_the_stage_key() {
        let err = parse_err("[stage.refine]\nmodel = \"model\"\n");
        assert!(err.contains("stage.refine.runner"), "message was: {err}");
    }

    #[test]
    fn a_missing_stage_table_names_the_stage_key() {
        let text = concat!(
            "[stage.refine]\nmodel = \"m\"\nrunner = \"r\"\n",
            "[stage.implement]\nmodel = \"m\"\nrunner = \"r\"\n",
            "[stage.review]\nmodel = \"m\"\nrunner = \"r\"\n",
        );
        let err = parse_err(text);
        assert!(err.contains("stage.release"), "message was: {err}");
    }

    #[test]
    fn parse_owner_repo_covers_the_git_forms() {
        assert_eq!(
            parse_owner_repo("git@github.com:o/r.git"),
            Some("o/r".to_string())
        );
        assert_eq!(
            parse_owner_repo("https://github.com/o/r.git"),
            Some("o/r".to_string())
        );
        assert_eq!(
            parse_owner_repo("https://github.com/o/r"),
            Some("o/r".to_string())
        );
        assert_eq!(
            parse_owner_repo("ssh://git@github.com/o/r.git"),
            Some("o/r".to_string())
        );
        assert_eq!(parse_owner_repo("https://github.com/o/r/issues"), None);
        assert_eq!(parse_owner_repo("/home/navaro/Workplace/borsuk"), None);
        assert_eq!(parse_owner_repo(""), None);
    }

    #[test]
    fn a_bad_alias_names_the_repo_key() {
        let text = config_text(&[], "[repo.\"Borsuk\"]\npath = \"/tmp/x\"\n");
        let err = parse_err(&text);
        assert!(err.contains("repo.\"Borsuk\""), "message was: {err}");
        assert!(err.contains("alias must match"), "message was: {err}");
    }

    #[test]
    fn a_zero_limit_names_the_stage_key() {
        let text = config_text(&[(Stage::Review, "limit = 0")], "");
        let err = parse_err(&text);
        assert!(err.contains("stage.review.limit"), "message was: {err}");
    }

    #[test]
    fn an_unknown_stage_section_names_the_key() {
        let text = config_text(&[], "[stage.refin]\nmodel = \"x\"\nrunner = \"r\"\n");
        let err = parse_err(&text);
        assert!(err.contains("stage.refin"), "message was: {err}");
    }

    #[test]
    fn an_unknown_lane_stage_names_the_lane_key() {
        let text = config_text(
            &[],
            "[repo.borsuk]\npath = \"/tmp/b\"\nlanes = { refin = 1 }\n",
        );
        let err = parse_err(&text);
        assert!(
            err.contains("repo.borsuk.lanes.refin"),
            "message was: {err}"
        );
    }

    #[test]
    fn a_lane_sum_over_the_stage_limit_is_rejected() {
        let text = config_text(
            &[(Stage::Implement, "limit = 2")],
            concat!(
                "[repo.borsuk]\n",
                "path = \"/tmp/b\"\n",
                "lanes = { implement = 2 }\n",
                "[repo.qubitsok]\n",
                "path = \"/tmp/q\"\n",
                "lanes = { implement = 1 }\n"
            ),
        );
        let err = parse_err(&text);
        assert!(err.contains("stage.implement"), "message was: {err}");
        assert!(err.contains("sum to 3"), "message was: {err}");
    }

    #[test]
    fn a_lane_sum_equal_to_the_limit_is_accepted() {
        let text = config_text(
            &[(Stage::Implement, "limit = 3")],
            concat!(
                "[repo.borsuk]\n",
                "path = \"/tmp/b\"\n",
                "lanes = { implement = 1 }\n",
                "[repo.qubitsok]\n",
                "path = \"/tmp/q\"\n",
                "lanes = { implement = 2 }\n"
            ),
        );
        assert!(Config::parse(&text).is_ok());
    }

    #[test]
    fn a_lane_sum_overflow_names_the_stage_key() {
        let text = config_text(
            &[],
            concat!(
                "[repo.a]\npath = \"/tmp/a\"\nlanes = { implement = 9223372036854775807 }\n",
                "[repo.b]\npath = \"/tmp/b\"\nlanes = { implement = 9223372036854775807 }\n",
                "[repo.c]\npath = \"/tmp/c\"\nlanes = { implement = 9223372036854775807 }\n",
            ),
        );

        let err = parse_err(&text);

        assert!(err.contains("stage.implement"), "message was: {err}");
        assert!(err.contains("overflow"), "message was: {err}");
    }

    #[test]
    fn a_threshold_count_below_one_names_the_key() {
        let text = config_text(
            &[],
            "[repo.x]\npath = \"/tmp/x\"\nrelease = { policy = \"threshold\", count = 0 }\n",
        );
        let err = parse_err(&text);
        assert!(err.contains("repo.x.release.count"), "message was: {err}");
    }

    #[test]
    fn an_interval_below_one_minute_names_the_key() {
        let text = config_text(
            &[],
            "[repo.x]\npath = \"/tmp/x\"\nrelease = { policy = \"interval\", minutes = 0 }\n",
        );
        let err = parse_err(&text);
        assert!(err.contains("repo.x.release.minutes"), "message was: {err}");
    }

    #[test]
    fn a_missing_path_names_the_repo_key() {
        let text = config_text(&[], "[repo.x]\n");
        let err = parse_err(&text);
        assert!(err.contains("repo.x.path"), "message was: {err}");
    }

    #[test]
    fn release_policy_survives_a_json_round_trip() {
        let policy = ReleasePolicy::Threshold { count: 3 };
        let text = serde_json::to_string(&policy).unwrap();
        assert!(text.contains("\"policy\":\"threshold\""), "got: {text}");
        assert_eq!(
            serde_json::from_str::<ReleasePolicy>(&text).unwrap(),
            policy
        );
        assert_eq!(
            serde_json::from_str::<ReleasePolicy>("{\"policy\":\"manual\"}").unwrap(),
            ReleasePolicy::Manual
        );
    }

    #[test]
    fn path_helpers_follow_the_naming_rules() {
        assert!(state_dir().ends_with("aif"));
        assert!(default_config_path().ends_with("factory.toml"));
        assert!(socket_path().ends_with("daemon.sock"));
    }

    // --- Filesystem and scripted command resolution. ---

    use std::sync::atomic::{AtomicU32, Ordering};

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique temporary directory for one test.
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aif-task2-{}-{}-{}",
            label,
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("the old temp dir must be removable");
        }
        fs::create_dir_all(&dir).expect("the temp dir must be creatable");
        dir
    }

    #[test]
    fn a_missing_repository_path_names_the_key() {
        let text = config_text(
            &[],
            concat!(
                "[repo.ghost]\n",
                "path = \"",
                "/nonexistent/aif-task2-path-check",
                "\"\n"
            ),
        );
        let mut config = Config::parse(&text).unwrap();
        let err = config.resolve(&ScriptExec::new()).unwrap_err().to_string();
        assert!(err.contains("repo.ghost.path"), "message was: {err}");
        assert!(err.contains("does not exist"), "message was: {err}");
    }

    #[test]
    fn a_directory_without_git_names_the_key() {
        let dir = temp_dir("no-git");
        let text = config_text(
            &[],
            &format!("[repo.plain]\npath = \"{}\"\n", dir.display()),
        );
        let mut config = Config::parse(&text).unwrap();
        let err = config.resolve(&ScriptExec::new()).unwrap_err().to_string();
        assert!(err.contains("repo.plain.path"), "message was: {err}");
        assert!(err.contains(".git"), "message was: {err}");
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn a_git_failure_names_the_repo() {
        let dir = temp_dir("fake-git");
        fs::create_dir(dir.join(".git")).expect("the .git dir must be creatable");
        let text = config_text(
            &[],
            &format!("[repo.broken]\npath = \"{}\"\n", dir.display()),
        );
        let mut config = Config::parse(&text).unwrap();
        let repo_path = dir.to_string_lossy().into_owned();
        let exec = ScriptExec::new().expect(
            move |call| {
                call.program == "git"
                    && call.argv() == ["-C", &repo_path, "remote", "get-url", "origin"]
            },
            CmdOut {
                status: 2,
                stdout: String::new(),
                stderr: "no origin\n".to_string(),
            },
        );
        let err = config.resolve(&exec).unwrap_err().to_string();
        assert!(err.contains("repo.broken"), "message was: {err}");
        assert!(err.contains("no origin"), "message was: {err}");
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn load_resolves_owner_repo_through_the_executor() {
        let dir = temp_dir("scripted-repo");
        fs::create_dir(dir.join(".git")).expect("the .git dir must be creatable");

        let config_path = dir.join("factory.toml");
        fs::write(
            &config_path,
            config_text(
                &[],
                &format!("[repo.local]\npath = \"{}\"\n", dir.display()),
            ),
        )
        .expect("the config write must succeed");
        let repo_path = dir.to_string_lossy().into_owned();
        let exec = ScriptExec::new().expect(
            move |call| {
                call.program == "git"
                    && call.argv() == ["-C", &repo_path, "remote", "get-url", "origin"]
            },
            CmdOut::ok("git@github.com:o/r.git\n"),
        );

        let config = Config::load_with_exec(Some(&config_path), &exec)
            .expect("the scripted load must succeed");
        assert_eq!(config.repos["local"].owner_repo, "o/r");
        assert_eq!(exec.calls().len(), 1);
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }

    #[test]
    fn a_missing_config_file_names_where_to_create_it_and_the_example() {
        let dir = temp_dir("missing");
        let missing = dir.join("factory.toml");
        let err = Config::load(Some(&missing)).unwrap_err().to_string();
        assert!(
            err.contains(&missing.display().to_string()),
            "message was: {err}"
        );
        assert!(err.contains("factory.example.toml"), "message was: {err}");
        fs::remove_dir_all(&dir).expect("the temp dir must be removable");
    }
}
