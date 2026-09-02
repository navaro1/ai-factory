//! Loads and validates the versioned `factory.toml` configuration.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::exec::{Exec, RealExec};
use crate::model::Stage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRole {
    Refine,
    Implement,
    Review,
    Release,
    TicketCreate,
    TicketChat,
}

impl ExecutionRole {
    pub const ALL: [Self; 6] = [
        Self::Refine,
        Self::Implement,
        Self::Review,
        Self::Release,
        Self::TicketCreate,
        Self::TicketChat,
    ];
    pub const fn table_name(self) -> &'static str {
        match self {
            Self::Refine => "stage.refine",
            Self::Implement => "stage.implement",
            Self::Review => "stage.review",
            Self::Release => "stage.release",
            Self::TicketCreate => "ticket.create",
            Self::TicketChat => "ticket.chat",
        }
    }
    pub const fn stage(self) -> Option<Stage> {
        match self {
            Self::Refine => Some(Stage::Refine),
            Self::Implement => Some(Stage::Implement),
            Self::Review => Some(Stage::Review),
            Self::Release => Some(Stage::Release),
            Self::TicketCreate | Self::TicketChat => None,
        }
    }
}
impl Display for ExecutionRole {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.table_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Opencode,
    Codex,
}
impl Harness {
    pub const fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Codex => "codex",
        }
    }
}

/// Complete settings for a configured execution role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSettings {
    pub harness: Harness,
    pub program: String,
    pub model: String,
    pub effort: Option<String>,
    pub extra_args: Vec<String>,
    pub agent: Option<String>,
    pub profile: Option<String>,
    pub permission_mode: Option<String>,
    pub permission_handler: Option<String>,
    pub tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub strict_mcp: Option<bool>,
    pub auto_approve: Option<bool>,
    pub approval_policy: Option<String>,
    pub sandbox: Option<String>,
}

/// One partial repository override. The global role supplies absent values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleOverride {
    pub harness: Option<Harness>,
    pub program: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub extra_args: Option<Vec<String>>,
    pub agent: Option<String>,
    pub profile: Option<String>,
    pub permission_mode: Option<String>,
    pub permission_handler: Option<String>,
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    pub strict_mcp: Option<bool>,
    pub auto_approve: Option<bool>,
    pub approval_policy: Option<String>,
    pub sandbox: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsSource {
    Global,
    Repository { alias: String },
}

/// One immutable role binding. A task stores this value before execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRoleSettings {
    pub role: ExecutionRole,
    pub source: SettingsSource,
    #[serde(flatten)]
    pub settings: RoleSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "lowercase")]
pub enum ReleasePolicy {
    #[default]
    Manual,
    Interval {
        minutes: u64,
    },
    Threshold {
        count: usize,
    },
}

/// Temporary stage data for callers that still use the old runner interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageConfig {
    pub model: String,
    pub runner: String,
    pub variant: Option<String>,
    pub limit: usize,
    pub yolo: bool,
}
impl StageConfig {
    fn default_limit(stage: Stage) -> usize {
        match stage {
            Stage::Refine | Stage::Implement => 3,
            Stage::Review => 7,
            Stage::Release => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    pub alias: String,
    pub path: PathBuf,
    pub owner_repo: String,
    pub lanes: BTreeMap<Stage, usize>,
    pub release: ReleasePolicy,
    pub role_overrides: BTreeMap<ExecutionRole, RoleOverride>,
}

/// Temporary chat data for callers that still use the old ticket interface.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TicketChatConfig {
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub schema_version: u32,
    pub roles: BTreeMap<ExecutionRole, RoleSettings>,
    pub stages: BTreeMap<Stage, StageConfig>,
    pub repos: BTreeMap<String, RepoConfig>,
    pub ticket_chat: TicketChatConfig,
}

impl Config {
    pub fn stage(&self, stage: Stage) -> &StageConfig {
        &self.stages[&stage]
    }

    /// Apply the global value first and one repository value second.
    pub fn resolved_role(
        &self,
        repository: Option<&str>,
        role: &str,
    ) -> Result<ResolvedRoleSettings> {
        let role = parse_role(role)?;
        let global = self
            .roles
            .get(&role)
            .ok_or_else(|| anyhow!("{role} is required"))?;
        let Some(alias) = repository else {
            return Ok(ResolvedRoleSettings {
                role,
                source: SettingsSource::Global,
                settings: global.clone(),
            });
        };
        let repo = self
            .repos
            .get(alias)
            .ok_or_else(|| anyhow!("repo.{alias}: no configured repository"))?;
        let Some(override_settings) = repo.role_overrides.get(&role) else {
            return Ok(ResolvedRoleSettings {
                role,
                source: SettingsSource::Global,
                settings: global.clone(),
            });
        };
        let settings = apply_override(global, override_settings);
        validate_settings(&settings, &format!("repo.{alias}.{role}"))?;
        Ok(ResolvedRoleSettings {
            role,
            source: SettingsSource::Repository {
                alias: alias.to_string(),
            },
            settings,
        })
    }

    pub fn ticket_chat_model(&self) -> Result<&str, String> {
        self.ticket_chat
            .model
            .as_deref()
            .ok_or_else(|| "ticket.chat.model must not be empty".to_string())
    }

    pub fn parse(text: &str) -> Result<Self> {
        migration_error(text)?;
        let raw: RawConfig = toml::from_str(text).context("invalid TOML")?;
        if raw.schema_version != Some(1) {
            bail!("schema_version must equal 1");
        }
        let stage_limits = [
            raw.stage.refine.as_ref().and_then(|value| value.limit),
            raw.stage.implement.as_ref().and_then(|value| value.limit),
            raw.stage.review.as_ref().and_then(|value| value.limit),
            raw.stage.release.as_ref().and_then(|value| value.limit),
        ];
        let raw_roles = [
            (ExecutionRole::Refine, raw.stage.refine),
            (ExecutionRole::Implement, raw.stage.implement),
            (ExecutionRole::Review, raw.stage.review),
            (ExecutionRole::Release, raw.stage.release),
            (ExecutionRole::TicketCreate, raw.ticket.create),
            (ExecutionRole::TicketChat, raw.ticket.chat),
        ];
        let mut roles = BTreeMap::new();
        for (role, raw_role) in raw_roles {
            let raw_role = raw_role.ok_or_else(|| anyhow!("{role} is required"))?;
            if role.stage().is_none() && raw_role.limit.is_some() {
                bail!("{role}.limit is allowed only on a global stage table");
            }
            let settings = raw_role.into_settings(&role.to_string())?;
            roles.insert(role, settings);
        }
        let mut stages = BTreeMap::new();
        for (index, role) in [
            ExecutionRole::Refine,
            ExecutionRole::Implement,
            ExecutionRole::Review,
            ExecutionRole::Release,
        ]
        .into_iter()
        .enumerate()
        {
            let stage = role.stage().expect("the role has a stage");
            let limit = stage_limits[index].unwrap_or_else(|| StageConfig::default_limit(stage));
            if limit == 0 {
                bail!("{role}.limit must be at least 1");
            }
            let settings = &roles[&role];
            stages.insert(
                stage,
                StageConfig {
                    model: settings.model.clone(),
                    runner: settings.harness.program().to_string(),
                    variant: settings.effort.clone(),
                    limit,
                    yolo: settings.auto_approve.unwrap_or(false),
                },
            );
        }
        let mut repos = BTreeMap::new();
        for (alias, raw_repo) in raw.repo {
            if !valid_alias(&alias) {
                bail!("repo.\"{alias}\": alias must match [a-z0-9._-]+");
            }
            let path = raw_repo
                .path
                .clone()
                .ok_or_else(|| anyhow!("repo.{alias}.path is required"))?;
            if path.trim().is_empty() {
                bail!("repo.{alias}.path must not be empty");
            }
            validate_release(&raw_repo.release, &alias)?;
            let raw_overrides = raw_repo.overrides();
            let mut lanes = BTreeMap::new();
            for (name, count) in raw_repo.lanes {
                let stage = Stage::from_str(&name)
                    .map_err(|error| anyhow!("repo.{alias}.lanes.{name}: {error}"))?;
                lanes.insert(stage, count);
            }
            let mut role_overrides = BTreeMap::new();
            for (role, raw_override) in raw_overrides {
                if let Some(raw_override) = raw_override {
                    let key = format!("repo.{alias}.{role}");
                    let override_settings = raw_override.into_override(&key)?;
                    if let Some(harness) = override_settings.harness {
                        if override_settings.model.is_none() {
                            bail!(
                                "{key}.model is required when {key}.harness changes to {}",
                                harness.program()
                            );
                        }
                    }
                    let effective = apply_override(&roles[&role], &override_settings);
                    validate_settings(&effective, &key)?;
                    role_overrides.insert(role, override_settings);
                }
            }
            repos.insert(
                alias.clone(),
                RepoConfig {
                    alias,
                    path: PathBuf::from(path),
                    owner_repo: String::new(),
                    lanes,
                    release: raw_repo.release,
                    role_overrides,
                },
            );
        }
        validate_lane_sums(&stages, &repos)?;
        Ok(Self {
            schema_version: 1,
            stages,
            repos,
            ticket_chat: TicketChatConfig {
                model: Some(roles[&ExecutionRole::TicketChat].model.clone()),
            },
            roles,
        })
    }

    pub fn load(path: Option<&Path>) -> Result<Self> {
        Self::load_with_exec(path, &RealExec)
    }
    fn load_with_exec(path: Option<&Path>, exec: &dyn Exec) -> Result<Self> {
        let path = path.map_or_else(default_config_path, Path::to_path_buf);
        if !path.exists() {
            bail!("no config file at {}; create it there, or copy docs/v0.5/factory.example.toml as a starting point", path.display());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut config = Self::parse(&text).with_context(|| format!("in {}", path.display()))?;
        config.resolve(exec)?;
        Ok(config)
    }
    fn resolve(&mut self, exec: &dyn Exec) -> Result<()> {
        for repo in self.repos.values_mut() {
            let alias = repo.alias.clone();
            if !repo.path.exists() {
                bail!("repo.{alias}.path: {} does not exist", repo.path.display());
            }
            if !repo.path.join(".git").exists() {
                bail!(
                    "repo.{alias}.path: {} holds no .git entry",
                    repo.path.display()
                );
            }
            let path = repo.path.to_string_lossy().into_owned();
            let output = exec
                .run("git", &["-C", &path, "remote", "get-url", "origin"], None)
                .with_context(|| format!("repo.{alias}: cannot run git"))?;
            if output.status != 0 {
                bail!(
                    "repo.{alias}: git remote get-url origin failed: {}",
                    output.stderr.trim()
                );
            }
            repo.owner_repo = parse_owner_repo(output.stdout.trim()).ok_or_else(|| {
                anyhow!(
                    "repo.{alias}: cannot read owner/repo from origin url {:?}",
                    output.stdout.trim()
                )
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema_version: Option<u32>,
    #[serde(default)]
    stage: RawStages,
    #[serde(default)]
    ticket: RawTickets,
    #[serde(default)]
    repo: BTreeMap<String, RawRepo>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStages {
    refine: Option<RawRole>,
    implement: Option<RawRole>,
    review: Option<RawRole>,
    release: Option<RawRole>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTickets {
    create: Option<RawRole>,
    chat: Option<RawRole>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRole {
    harness: Option<Harness>,
    program: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    extra_args: Option<Vec<String>>,
    agent: Option<String>,
    profile: Option<String>,
    permission_mode: Option<String>,
    permission_handler: Option<String>,
    tools: Option<Vec<String>>,
    disallowed_tools: Option<Vec<String>>,
    strict_mcp: Option<bool>,
    auto_approve: Option<bool>,
    approval_policy: Option<String>,
    sandbox: Option<String>,
    limit: Option<usize>,
}
impl RawRole {
    fn into_settings(self, key: &str) -> Result<RoleSettings> {
        let harness = self
            .harness
            .ok_or_else(|| anyhow!("{key}.harness is required"))?;
        let program = self
            .program
            .unwrap_or_else(|| harness.program().to_string());
        nonempty(&program, &format!("{key}.program"))?;
        let settings = RoleSettings {
            harness,
            program,
            model: required_text(self.model, &format!("{key}.model"))?,
            effort: optional_text(self.effort, &format!("{key}.effort"))?,
            extra_args: self.extra_args.unwrap_or_default(),
            agent: optional_text(self.agent, &format!("{key}.agent"))?,
            profile: optional_text(self.profile, &format!("{key}.profile"))?,
            permission_mode: optional_text(
                self.permission_mode,
                &format!("{key}.permission_mode"),
            )?,
            permission_handler: optional_text(
                self.permission_handler,
                &format!("{key}.permission_handler"),
            )?,
            tools: self.tools.unwrap_or_default(),
            disallowed_tools: self.disallowed_tools.unwrap_or_default(),
            strict_mcp: self.strict_mcp,
            auto_approve: self.auto_approve,
            approval_policy: optional_text(
                self.approval_policy,
                &format!("{key}.approval_policy"),
            )?,
            sandbox: optional_text(self.sandbox, &format!("{key}.sandbox"))?,
        };
        validate_settings(&settings, key)?;
        Ok(settings)
    }
    fn into_override(self, key: &str) -> Result<RoleOverride> {
        if self.limit.is_some() {
            bail!("{key}.limit is allowed only on a global stage table");
        }
        let settings = RoleOverride {
            harness: self.harness,
            program: optional_text(self.program, &format!("{key}.program"))?,
            model: optional_text(self.model, &format!("{key}.model"))?,
            effort: optional_text(self.effort, &format!("{key}.effort"))?,
            extra_args: self.extra_args,
            agent: optional_text(self.agent, &format!("{key}.agent"))?,
            profile: optional_text(self.profile, &format!("{key}.profile"))?,
            permission_mode: optional_text(
                self.permission_mode,
                &format!("{key}.permission_mode"),
            )?,
            permission_handler: optional_text(
                self.permission_handler,
                &format!("{key}.permission_handler"),
            )?,
            tools: self.tools,
            disallowed_tools: self.disallowed_tools,
            strict_mcp: self.strict_mcp,
            auto_approve: self.auto_approve,
            approval_policy: optional_text(
                self.approval_policy,
                &format!("{key}.approval_policy"),
            )?,
            sandbox: optional_text(self.sandbox, &format!("{key}.sandbox"))?,
        };
        validate_override(&settings, key)?;
        Ok(settings)
    }
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepo {
    path: Option<String>,
    #[serde(default)]
    lanes: BTreeMap<String, usize>,
    #[serde(default)]
    release: ReleasePolicy,
    #[serde(default)]
    stage: RawStageOverrides,
    #[serde(default)]
    ticket: RawTicketOverrides,
}
impl RawRepo {
    fn overrides(&self) -> [(ExecutionRole, Option<RawRole>); 6] {
        [
            (ExecutionRole::Refine, self.stage.refine.clone()),
            (ExecutionRole::Implement, self.stage.implement.clone()),
            (ExecutionRole::Review, self.stage.review.clone()),
            (ExecutionRole::Release, self.stage.release.clone()),
            (ExecutionRole::TicketCreate, self.ticket.create.clone()),
            (ExecutionRole::TicketChat, self.ticket.chat.clone()),
        ]
    }
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStageOverrides {
    refine: Option<RawRole>,
    implement: Option<RawRole>,
    review: Option<RawRole>,
    release: Option<RawRole>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTicketOverrides {
    create: Option<RawRole>,
    chat: Option<RawRole>,
}

fn required_text(value: Option<String>, key: &str) -> Result<String> {
    let value = value.ok_or_else(|| anyhow!("{key} is required"))?;
    nonempty(&value, key)?;
    Ok(value)
}
fn optional_text(value: Option<String>, key: &str) -> Result<Option<String>> {
    if let Some(value) = value.as_deref() {
        nonempty(value, key)?;
    }
    Ok(value)
}
fn nonempty(value: &str, key: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{key} must not be empty");
    }
    Ok(())
}
fn validate_settings(value: &RoleSettings, key: &str) -> Result<()> {
    validate_fields(
        value.harness,
        value.agent.is_some(),
        value.profile.is_some(),
        value.permission_mode.is_some()
            || value.permission_handler.is_some()
            || !value.tools.is_empty()
            || !value.disallowed_tools.is_empty()
            || value.strict_mcp.is_some(),
        value.auto_approve.is_some(),
        value.approval_policy.is_some() || value.sandbox.is_some(),
        key,
    )?;
    validate_args(&value.extra_args, key)
}
fn validate_override(value: &RoleOverride, key: &str) -> Result<()> {
    if let Some(harness) = value.harness {
        validate_fields(
            harness,
            value.agent.is_some(),
            value.profile.is_some(),
            value.permission_mode.is_some()
                || value.permission_handler.is_some()
                || value.tools.is_some()
                || value.disallowed_tools.is_some()
                || value.strict_mcp.is_some(),
            value.auto_approve.is_some(),
            value.approval_policy.is_some() || value.sandbox.is_some(),
            key,
        )?;
    }
    value
        .extra_args
        .as_deref()
        .map_or(Ok(()), |args| validate_args(args, key))
}
fn validate_fields(
    harness: Harness,
    agent: bool,
    profile: bool,
    claude: bool,
    opencode: bool,
    codex: bool,
    key: &str,
) -> Result<()> {
    let invalid = match harness {
        Harness::Claude => profile || opencode || codex,
        Harness::Opencode => profile || claude || codex,
        Harness::Codex => agent || claude || opencode,
    };
    if invalid {
        bail!(
            "{key}: contains fields unsupported by {}",
            harness.program()
        );
    }
    Ok(())
}
fn validate_args(args: &[String], key: &str) -> Result<()> {
    const MANAGED: &[&str] = &[
        "--model",
        "-m",
        "--cwd",
        "-C",
        "--resume",
        "--session",
        "--permission-mode",
        "--permission-prompt-tool",
        "--output-format",
        "--json",
        "--jsonl",
        "--profile",
        "--agent",
        "--effort",
        "--variant",
        "--approval-policy",
        "--sandbox",
    ];
    for arg in args {
        if arg.trim().is_empty() {
            bail!("{key}.extra_args must not contain an empty value");
        }
        if MANAGED
            .iter()
            .any(|flag| arg == *flag || arg.starts_with(&format!("{flag}=")))
        {
            bail!("{key}.extra_args contains managed argument {arg:?}");
        }
        if arg == "--share" || arg.starts_with("--share=") {
            bail!("{key}.extra_args must not enable OpenCode sharing");
        }
        if arg.contains("dangerously-bypass-approvals-and-sandbox")
            || arg.contains("dangerously-skip-permissions")
        {
            bail!("{key}.extra_args must not contain a combined dangerous bypass argument");
        }
    }
    Ok(())
}
fn apply_override(global: &RoleSettings, value: &RoleOverride) -> RoleSettings {
    RoleSettings {
        harness: value.harness.unwrap_or(global.harness),
        program: value
            .program
            .clone()
            .unwrap_or_else(|| global.program.clone()),
        model: value.model.clone().unwrap_or_else(|| global.model.clone()),
        effort: value.effort.clone().or_else(|| global.effort.clone()),
        extra_args: value
            .extra_args
            .clone()
            .unwrap_or_else(|| global.extra_args.clone()),
        agent: value.agent.clone().or_else(|| global.agent.clone()),
        profile: value.profile.clone().or_else(|| global.profile.clone()),
        permission_mode: value
            .permission_mode
            .clone()
            .or_else(|| global.permission_mode.clone()),
        permission_handler: value
            .permission_handler
            .clone()
            .or_else(|| global.permission_handler.clone()),
        tools: value.tools.clone().unwrap_or_else(|| global.tools.clone()),
        disallowed_tools: value
            .disallowed_tools
            .clone()
            .unwrap_or_else(|| global.disallowed_tools.clone()),
        strict_mcp: value.strict_mcp.or(global.strict_mcp),
        auto_approve: value.auto_approve.or(global.auto_approve),
        approval_policy: value
            .approval_policy
            .clone()
            .or_else(|| global.approval_policy.clone()),
        sandbox: value.sandbox.clone().or_else(|| global.sandbox.clone()),
    }
}
fn parse_role(value: &str) -> Result<ExecutionRole> {
    match value {
        "stage.refine" => Ok(ExecutionRole::Refine),
        "stage.implement" => Ok(ExecutionRole::Implement),
        "stage.review" => Ok(ExecutionRole::Review),
        "stage.release" => Ok(ExecutionRole::Release),
        "ticket.create" => Ok(ExecutionRole::TicketCreate),
        "ticket.chat" => Ok(ExecutionRole::TicketChat),
        _ => bail!("{value}: unknown execution role"),
    }
}
fn validate_release(value: &ReleasePolicy, alias: &str) -> Result<()> {
    match value {
        ReleasePolicy::Threshold { count: 0 } => {
            bail!("repo.{alias}.release.count must be at least 1")
        }
        ReleasePolicy::Interval { minutes: 0 } => {
            bail!("repo.{alias}.release.minutes must be at least 1")
        }
        _ => Ok(()),
    }
}
fn validate_lane_sums(
    stages: &BTreeMap<Stage, StageConfig>,
    repos: &BTreeMap<String, RepoConfig>,
) -> Result<()> {
    for stage in Stage::ALL {
        let sum = repos
            .values()
            .filter_map(|repo| repo.lanes.get(&stage))
            .try_fold(0usize, |sum, value| {
                sum.checked_add(*value)
                    .ok_or_else(|| anyhow!("stage.{stage}: lane reservations overflow usize"))
            })?;
        if sum > stages[&stage].limit {
            bail!(
                "stage.{stage}: lane reservations sum to {sum}, exceeding stage.{stage}.limit {}",
                stages[&stage].limit
            );
        }
    }
    Ok(())
}
fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.chars().all(|value| {
            value.is_ascii_lowercase() || value.is_ascii_digit() || matches!(value, '.' | '_' | '-')
        })
}

/// Reject removed configuration names before serde reports a generic key error.
fn migration_error(text: &str) -> Result<()> {
    let document = text
        .parse::<toml_edit::DocumentMut>()
        .context("invalid TOML")?;
    if document.as_table().contains_key("ticket_chat") {
        bail!("ticket_chat is no longer supported; use [ticket.chat]");
    }
    check_legacy_table(document.as_table(), "")
}

fn check_legacy_table(table: &toml_edit::Table, prefix: &str) -> Result<()> {
    for (key, item) in table.iter() {
        let path = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };
        if matches!(key, "runner" | "variant" | "yolo") {
            bail!("{path} is no longer supported; use the typed role settings");
        }
        if let Some(child) = item.as_table() {
            check_legacy_table(child, &path)?;
        }
    }
    Ok(())
}

pub fn parse_owner_repo(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let path = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
    {
        rest.split_once('/')?.1
    } else {
        url.split_once(':')?.1
    };
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    (!owner.is_empty() && !name.is_empty() && parts.next().is_none())
        .then(|| format!("{owner}/{name}"))
}
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join("aif")
}
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("aif")
}
pub fn default_config_path() -> PathBuf {
    config_dir().join("factory.toml")
}
pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(|dir| PathBuf::from(dir).join("aif").join("daemon.sock"))
        .unwrap_or_else(|| state_dir().join("daemon.sock"))
}
fn xdg_dir(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(fallback))
        })
        .unwrap_or_else(|| PathBuf::from(fallback))
}
