//! Loads and validates the versioned `factory.toml` configuration.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::io::Write;
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

/// One role edit that can be saved to `factory.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum SettingsEdit {
    /// Replace one complete global role.
    Global {
        /// The role identity.
        role: ExecutionRole,
        /// The complete global settings.
        settings: RoleSettings,
        /// The stage limit. Ticket roles require `None`.
        limit: Option<usize>,
    },
    /// Replace or remove one repository override.
    Repository {
        /// The repository alias.
        repository: String,
        /// The role identity.
        role: ExecutionRole,
        /// The partial override. `None` removes the override table.
        settings: Option<RoleOverride>,
    },
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
                    let harness = override_settings.harness.unwrap_or(roles[&role].harness);
                    validate_override_for_harness(&override_settings, harness, &key)?;
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
        validate_raw_harness_fields(&self, harness, key)?;
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
    validate_tool_names(&value.tools, &format!("{key}.tools"))?;
    validate_tool_names(&value.disallowed_tools, &format!("{key}.disallowed_tools"))?;
    validate_native_values(value, key)?;
    validate_extra_args(&value.extra_args, value.harness, key)
}

/// Validate one complete role setting from a persisted execution binding.
pub(crate) fn validate_persisted_settings(value: &RoleSettings, key: &str) -> Result<()> {
    nonempty(&value.program, &format!("{key}.program"))?;
    nonempty(&value.model, &format!("{key}.model"))?;
    if let Some(value) = value.effort.as_deref() {
        nonempty(value, &format!("{key}.effort"))?;
    }
    if let Some(value) = value.agent.as_deref() {
        nonempty(value, &format!("{key}.agent"))?;
    }
    if let Some(value) = value.profile.as_deref() {
        nonempty(value, &format!("{key}.profile"))?;
    }
    if let Some(value) = value.permission_handler.as_deref() {
        nonempty(value, &format!("{key}.permission_handler"))?;
    }
    validate_settings(value, key)
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
    if let Some(args) = value.extra_args.as_deref() {
        for arg in args {
            if arg.trim().is_empty() {
                bail!("{key}.extra_args must not contain an empty value");
            }
        }
    }
    Ok(())
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
fn validate_raw_harness_fields(value: &RawRole, harness: Harness, key: &str) -> Result<()> {
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
    if let Some(tools) = &value.tools {
        validate_tool_names(tools, &format!("{key}.tools"))?;
    }
    if let Some(tools) = &value.disallowed_tools {
        validate_tool_names(tools, &format!("{key}.disallowed_tools"))?;
    }
    Ok(())
}
fn validate_override_for_harness(value: &RoleOverride, harness: Harness, key: &str) -> Result<()> {
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
    if let Some(tools) = &value.tools {
        validate_tool_names(tools, &format!("{key}.tools"))?;
    }
    if let Some(tools) = &value.disallowed_tools {
        validate_tool_names(tools, &format!("{key}.disallowed_tools"))?;
    }
    if value.harness.is_some() && value.model.is_none() {
        bail!(
            "{key}.model is required when {key}.harness changes to {}",
            harness.program()
        );
    }
    if let Some(mode) = value.permission_mode.as_deref() {
        validate_choice(
            mode,
            CLAUDE_PERMISSION_MODES,
            &format!("{key}.permission_mode"),
        )?;
    }
    if let Some(policy) = value.approval_policy.as_deref() {
        validate_choice(
            policy,
            CODEX_APPROVAL_POLICIES,
            &format!("{key}.approval_policy"),
        )?;
    }
    if let Some(sandbox) = value.sandbox.as_deref() {
        validate_choice(sandbox, CODEX_SANDBOXES, &format!("{key}.sandbox"))?;
    }
    if let Some(args) = value.extra_args.as_deref() {
        validate_extra_args(args, harness, key)?;
    }
    Ok(())
}
fn validate_tool_names(tools: &[String], key: &str) -> Result<()> {
    for tool in tools {
        nonempty(tool, key)?;
    }
    Ok(())
}
pub const CLAUDE_PERMISSION_MODES: &[&str] = &[
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "manual",
    "dontAsk",
    "plan",
];
pub const CODEX_APPROVAL_POLICIES: &[&str] = &["untrusted", "on-request", "never"];
pub const CODEX_SANDBOXES: &[&str] = &["read-only", "workspace-write", "danger-full-access"];

fn validate_native_values(value: &RoleSettings, key: &str) -> Result<()> {
    if let Some(mode) = value.permission_mode.as_deref() {
        validate_choice(
            mode,
            CLAUDE_PERMISSION_MODES,
            &format!("{key}.permission_mode"),
        )?;
    }
    if let Some(policy) = value.approval_policy.as_deref() {
        validate_choice(
            policy,
            CODEX_APPROVAL_POLICIES,
            &format!("{key}.approval_policy"),
        )?;
    }
    if let Some(sandbox) = value.sandbox.as_deref() {
        validate_choice(sandbox, CODEX_SANDBOXES, &format!("{key}.sandbox"))?;
    }
    Ok(())
}

fn validate_choice(value: &str, choices: &[&str], key: &str) -> Result<()> {
    if !choices.contains(&value) {
        bail!("{key} must be one of {}", choices.join(", "));
    }
    Ok(())
}

/// Reject arguments that replace fields managed by the selected adapter.
pub fn validate_extra_args(args: &[String], harness: Harness, key: &str) -> Result<()> {
    const CLAUDE: &[&str] = &[
        "-p",
        "--print",
        "-c",
        "--continue",
        "-r",
        "--resume",
        "--session-id",
        "--fork-session",
        "--model",
        "--fallback-model",
        "--effort",
        "--agent",
        "--agents",
        "--permission-mode",
        "--permission-prompt-tool",
        "--tools",
        "--allowedTools",
        "--allowed-tools",
        "--disallowedTools",
        "--disallowed-tools",
        "--strict-mcp-config",
        "--output-format",
        "--input-format",
        "--verbose",
        "--json-schema",
        "--add-dir",
    ];
    const OPENCODE: &[&str] = &[
        "-c",
        "--continue",
        "-s",
        "--session",
        "-m",
        "--model",
        "--agent",
        "--format",
        "--dir",
        "--variant",
        "--auto",
        "--share",
    ];
    const CODEX: &[&str] = &[
        "--yolo",
        "--dangerously-bypass-approvals-and-sandbox",
        "--full-auto",
        "--dangerously-bypass-hook-trust",
        "--ignore-rules",
        "--ignore-user-config",
        "--add-dir",
        "-c",
        "--config",
        "-s",
        "--sandbox",
        "-p",
        "--profile",
        "-m",
        "--model",
        "-C",
        "--cd",
        "-o",
        "--output-last-message",
        "--output-schema",
        "--json",
        "--experimental-json",
        "--resume",
        "--session",
        "--session-id",
        "--ask-for-approval",
        "--approval-policy",
        "--format",
        "--auto",
        "--dir",
        "--tools",
        "--strict-mcp-config",
        "-a",
    ];
    let managed = match harness {
        Harness::Claude => CLAUDE,
        Harness::Opencode => OPENCODE,
        Harness::Codex => CODEX,
    };
    for arg in args {
        if arg.trim().is_empty() {
            bail!("{key}.extra_args must not contain an empty value");
        }
        if managed.iter().any(|flag| managed_arg(arg, flag)) {
            bail!("{key}.extra_args contains managed argument {arg:?}");
        }
        if harness == Harness::Opencode && (arg == "--share" || arg.starts_with("--share=")) {
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

fn managed_arg(arg: &str, flag: &str) -> bool {
    arg == flag
        || arg.starts_with(&format!("{flag}="))
        || (flag.starts_with('-')
            && !flag.starts_with("--")
            && flag.len() == 2
            && arg.starts_with(flag)
            && arg.len() > flag.len())
}
fn apply_override(global: &RoleSettings, value: &RoleOverride) -> RoleSettings {
    if let Some(harness) = value.harness {
        return RoleSettings {
            harness,
            program: value
                .program
                .clone()
                .unwrap_or_else(|| harness.program().to_string()),
            model: value
                .model
                .clone()
                .expect("validated harness replacement model"),
            effort: value.effort.clone(),
            extra_args: value.extra_args.clone().unwrap_or_default(),
            agent: value.agent.clone(),
            profile: value.profile.clone(),
            permission_mode: value.permission_mode.clone(),
            permission_handler: value.permission_handler.clone(),
            tools: value.tools.clone().unwrap_or_default(),
            disallowed_tools: value.disallowed_tools.clone().unwrap_or_default(),
            strict_mcp: value.strict_mcp,
            auto_approve: value.auto_approve,
            approval_policy: value.approval_policy.clone(),
            sandbox: value.sandbox.clone(),
        };
    }
    RoleSettings {
        harness: global.harness,
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
        if prefix != "repo" && matches!(key, "runner" | "variant" | "yolo") {
            bail!("{path} is no longer supported; use the typed role settings");
        }
        if let Some(child) = item.as_table() {
            check_legacy_table(child, &path)?;
        }
        if let Some(child) = item.as_inline_table() {
            check_legacy_inline_table(child, &path)?;
        }
    }
    Ok(())
}

fn check_legacy_inline_table(table: &toml_edit::InlineTable, prefix: &str) -> Result<()> {
    for (key, value) in table.iter() {
        let path = format!("{prefix}.{key}");
        if prefix != "repo" && matches!(key, "runner" | "variant" | "yolo") {
            bail!("{path} is no longer supported; use the typed role settings");
        }
        if let Some(child) = value.as_inline_table() {
            check_legacy_inline_table(child, &path)?;
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

/// Return one stable revision from the complete file content.
pub fn file_revision(text: &str) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let hash = text.as_bytes().iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    });
    format!("{:x}-{hash:016x}", text.len())
}

/// Select an absolute, canonical config path when the file exists.
pub fn resolved_config_path(path: Option<&Path>) -> Result<PathBuf> {
    let selected = path.map_or_else(default_config_path, Path::to_path_buf);
    let absolute = if selected.is_absolute() {
        selected
    } else {
        std::env::current_dir()
            .context("cannot read the current directory")?
            .join(selected)
    };
    match fs::canonicalize(&absolute) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(absolute),
        Err(error) => Err(error).with_context(|| format!("cannot resolve {}", absolute.display())),
    }
}

/// Apply one typed role edit while the document retains unrelated content.
pub fn edit_config_text(text: &str, edit: &SettingsEdit) -> Result<String> {
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .context("invalid TOML")?;
    match edit {
        SettingsEdit::Global {
            role,
            settings,
            limit,
        } => {
            if role.stage().is_none() && limit.is_some() {
                bail!("{role}.limit is allowed only on a global stage table");
            }
            let table = global_role_table_mut(&mut document, *role)?;
            write_role_settings(table, settings);
            set_usize(table, "limit", *limit)?;
        }
        SettingsEdit::Repository {
            repository,
            role,
            settings,
        } => {
            if !valid_alias(repository) {
                bail!("repo.\"{repository}\": alias must match [a-z0-9._-]+");
            }
            if !document
                .get("repo")
                .and_then(toml_edit::Item::as_table)
                .is_some_and(|repos| repos.contains_key(repository))
            {
                bail!("repo.{repository}: no configured repository");
            }
            if let Some(settings) = settings {
                let table = repository_role_table_mut(&mut document, repository, *role)?;
                write_role_override(table, settings);
            } else if let Some(table) =
                existing_repository_role_parent_mut(&mut document, repository, *role)?
            {
                table.remove(role_name(*role));
            }
        }
    }
    let candidate = document.to_string();
    Config::parse(&candidate).context("the edited factory configuration is invalid")?;
    Ok(candidate)
}

/// A fully written and synced config file that waits for its atomic rename.
pub(crate) struct PreparedConfig {
    destination: PathBuf,
    temporary: PathBuf,
}

impl Drop for PreparedConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

/// The result of a revision-checked atomic config commit.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AtomicWrite {
    Written,
    Stale { revision: String },
}

/// Write, set permissions, and sync one sibling temporary config file.
pub(crate) fn prepare_config_atomic(path: &Path, text: &str) -> Result<PreparedConfig> {
    let permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
        }
    };
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map_or_else(|| "tmp".to_string(), |value| format!("{value}.tmp"));
    let temporary = path.with_extension(extension);
    let mut file = fs::File::create(&temporary)
        .with_context(|| format!("cannot create {}", temporary.display()))?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)
            .with_context(|| format!("cannot set permissions on {}", temporary.display()))?;
    }
    file.write_all(text.as_bytes())
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("cannot sync {}", temporary.display()))?;
    drop(file);
    Ok(PreparedConfig {
        destination: path.to_path_buf(),
        temporary,
    })
}

/// Rename a prepared file only if the destination still has the expected revision.
///
/// A normal filesystem rename has no content compare-and-swap operation. A writer
/// that changes the destination after this final comparison can still win a race.
pub(crate) fn commit_config_atomic_checked(
    prepared: PreparedConfig,
    expected_revision: &str,
) -> Result<AtomicWrite> {
    let current = fs::read_to_string(&prepared.destination).with_context(|| {
        format!(
            "cannot read {} before rename",
            prepared.destination.display()
        )
    })?;
    let revision = file_revision(&current);
    if revision != expected_revision {
        fs::remove_file(&prepared.temporary)
            .with_context(|| format!("cannot remove stale {}", prepared.temporary.display()))?;
        return Ok(AtomicWrite::Stale { revision });
    }
    finish_config_atomic(prepared)?;
    Ok(AtomicWrite::Written)
}

fn finish_config_atomic(prepared: PreparedConfig) -> Result<()> {
    fs::rename(&prepared.temporary, &prepared.destination).with_context(|| {
        format!(
            "cannot rename {} to {}",
            prepared.temporary.display(),
            prepared.destination.display()
        )
    })?;
    if let Some(parent) = prepared.destination.parent() {
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("cannot sync {}", parent.display()))?;
    }
    Ok(())
}

/// Write one sibling temporary file and replace the config with one rename.
pub fn write_config_atomic(path: &Path, text: &str) -> Result<()> {
    finish_config_atomic(prepare_config_atomic(path, text)?)
}

impl Config {
    /// Parse and resolve a candidate through an injected command runner.
    pub fn parse_resolved(text: &str, exec: &dyn Exec) -> Result<Self> {
        let mut config = Self::parse(text)?;
        config.resolve(exec)?;
        Ok(config)
    }

    /// True when a reload can retain every live repository controller.
    pub fn has_same_topology(&self, other: &Self) -> bool {
        self.repos.len() == other.repos.len()
            && self.repos.iter().all(|(alias, repo)| {
                other.repos.get(alias).is_some_and(|candidate| {
                    repo.alias == candidate.alias
                        && repo.path == candidate.path
                        && repo.owner_repo == candidate.owner_repo
                        && repo.lanes == candidate.lanes
                        && repo.release == candidate.release
                })
            })
    }
}

fn role_parts(role: ExecutionRole) -> (&'static str, &'static str) {
    match role {
        ExecutionRole::Refine => ("stage", "refine"),
        ExecutionRole::Implement => ("stage", "implement"),
        ExecutionRole::Review => ("stage", "review"),
        ExecutionRole::Release => ("stage", "release"),
        ExecutionRole::TicketCreate => ("ticket", "create"),
        ExecutionRole::TicketChat => ("ticket", "chat"),
    }
}

fn role_name(role: ExecutionRole) -> &'static str {
    role_parts(role).1
}

fn global_role_table_mut(
    document: &mut toml_edit::DocumentMut,
    role: ExecutionRole,
) -> Result<&mut toml_edit::Table> {
    let (section, name) = role_parts(role);
    document
        .get_mut(section)
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|table| table.get_mut(name))
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| anyhow!("{role} is required"))
}

fn existing_repository_role_parent_mut<'a>(
    document: &'a mut toml_edit::DocumentMut,
    repository: &str,
    role: ExecutionRole,
) -> Result<Option<&'a mut toml_edit::Table>> {
    let (section, _) = role_parts(role);
    let repo = document
        .get_mut("repo")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|repos| repos.get_mut(repository))
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| anyhow!("repo.{repository}: no configured repository"))?;
    Ok(repo
        .get_mut(section)
        .and_then(toml_edit::Item::as_table_mut))
}

fn repository_role_table_mut<'a>(
    document: &'a mut toml_edit::DocumentMut,
    repository: &str,
    role: ExecutionRole,
) -> Result<&'a mut toml_edit::Table> {
    let (section, name) = role_parts(role);
    let repo = document
        .get_mut("repo")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|repos| repos.get_mut(repository))
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| anyhow!("repo.{repository}: no configured repository"))?;
    if !repo.contains_key(section) {
        repo.insert(section, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let parent = repo
        .get_mut(section)
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| anyhow!("repo.{repository}.{section} must be a table"))?;
    if !parent.contains_key(name) {
        parent.insert(name, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    parent
        .get_mut(name)
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| anyhow!("repo.{repository}.{section}.{name} must be a table"))
}

fn write_role_settings(table: &mut toml_edit::Table, settings: &RoleSettings) {
    set_string(table, "harness", Some(harness_name(settings.harness)));
    set_string(table, "program", Some(&settings.program));
    set_string(table, "model", Some(&settings.model));
    set_string(table, "effort", settings.effort.as_deref());
    set_strings(table, "extra_args", Some(&settings.extra_args));
    set_string(table, "agent", settings.agent.as_deref());
    set_string(table, "profile", settings.profile.as_deref());
    set_string(
        table,
        "permission_mode",
        settings.permission_mode.as_deref(),
    );
    set_string(
        table,
        "permission_handler",
        settings.permission_handler.as_deref(),
    );
    set_strings(table, "tools", Some(&settings.tools));
    set_strings(table, "disallowed_tools", Some(&settings.disallowed_tools));
    set_bool(table, "strict_mcp", settings.strict_mcp);
    set_bool(table, "auto_approve", settings.auto_approve);
    set_string(
        table,
        "approval_policy",
        settings.approval_policy.as_deref(),
    );
    set_string(table, "sandbox", settings.sandbox.as_deref());
}

fn write_role_override(table: &mut toml_edit::Table, settings: &RoleOverride) {
    set_string(table, "harness", settings.harness.map(harness_name));
    set_string(table, "program", settings.program.as_deref());
    set_string(table, "model", settings.model.as_deref());
    set_string(table, "effort", settings.effort.as_deref());
    set_strings(table, "extra_args", settings.extra_args.as_deref());
    set_string(table, "agent", settings.agent.as_deref());
    set_string(table, "profile", settings.profile.as_deref());
    set_string(
        table,
        "permission_mode",
        settings.permission_mode.as_deref(),
    );
    set_string(
        table,
        "permission_handler",
        settings.permission_handler.as_deref(),
    );
    set_strings(table, "tools", settings.tools.as_deref());
    set_strings(
        table,
        "disallowed_tools",
        settings.disallowed_tools.as_deref(),
    );
    set_bool(table, "strict_mcp", settings.strict_mcp);
    set_bool(table, "auto_approve", settings.auto_approve);
    set_string(
        table,
        "approval_policy",
        settings.approval_policy.as_deref(),
    );
    set_string(table, "sandbox", settings.sandbox.as_deref());
}

fn harness_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "claude",
        Harness::Opencode => "opencode",
        Harness::Codex => "codex",
    }
}

fn set_string(table: &mut toml_edit::Table, key: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            insert_value_preserving_decor(table, key, toml_edit::Value::from(value));
        }
        None => {
            table.remove(key);
        }
    }
}

fn set_strings(table: &mut toml_edit::Table, key: &str, values: Option<&[String]>) {
    match values {
        Some(values) => {
            let mut array = toml_edit::Array::new();
            for value in values {
                array.push(value.as_str());
            }
            insert_value_preserving_decor(table, key, toml_edit::Value::Array(array));
        }
        None => {
            table.remove(key);
        }
    }
}

fn set_bool(table: &mut toml_edit::Table, key: &str, value: Option<bool>) {
    match value {
        Some(value) => {
            insert_value_preserving_decor(table, key, toml_edit::Value::from(value));
        }
        None => {
            table.remove(key);
        }
    }
}

fn set_usize(table: &mut toml_edit::Table, key: &str, value: Option<usize>) -> Result<()> {
    match value {
        Some(value) => {
            let value = i64::try_from(value).context("the stage limit is too large")?;
            insert_value_preserving_decor(table, key, toml_edit::Value::from(value));
        }
        None => {
            table.remove(key);
        }
    }
    Ok(())
}

fn insert_value_preserving_decor(
    table: &mut toml_edit::Table,
    key: &str,
    mut value: toml_edit::Value,
) {
    let existing = table.get_key_value(key).map(|(key, item)| {
        (
            key.clone(),
            item.as_value().map(|value| value.decor().clone()),
        )
    });
    if let Some(decor) = existing.as_ref().and_then(|(_, decor)| decor.clone()) {
        *value.decor_mut() = decor;
    }
    let item = toml_edit::Item::Value(value);
    if let Some((key, _)) = existing {
        table.insert_formatted(&key, item);
    } else {
        table.insert(key, item);
    }
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    fn config_text() -> String {
        let mut text = "# keep this factory comment\nschema_version = 1\n".to_string();
        for stage in Stage::ALL {
            text.push_str(&format!(
                "\n# keep the {stage} comment\n[stage.{stage}]\nharness = \"claude\"\nmodel = \"old-{stage}\"\nlimit = 3\n"
            ));
        }
        text.push_str("\n[ticket.create]\nharness = \"opencode\"\nmodel = \"create\"\n");
        text.push_str("\n[ticket.chat]\nharness = \"claude\"\nmodel = \"chat\"\n");
        text.push_str("\n[repo.demo]\npath = \"/tmp/demo\"\n");
        text.push_str("\n[repo.demo.stage.review]\nmodel = \"repo-review\"\n");
        text
    }

    #[test]
    fn a_global_role_edit_preserves_comments_and_unrelated_structure() {
        let text = config_text().replace(
            "model = \"old-review\"\nlimit = 3",
            "# keep the model field comment\nmodel = \"old-review\" # keep the model inline comment\nlimit = 3 # keep the limit inline comment",
        );
        let config = Config::parse(&text).unwrap();
        let mut settings = config.roles[&ExecutionRole::Review].clone();
        settings.model = "new-review".to_string();
        settings.effort = Some("high".to_string());

        let edited = edit_config_text(
            &text,
            &SettingsEdit::Global {
                role: ExecutionRole::Review,
                settings,
                limit: Some(8),
            },
        )
        .unwrap();

        assert!(edited.contains("# keep this factory comment"));
        assert!(edited.contains("# keep the refine comment"));
        assert!(edited.contains("# keep the model field comment"));
        assert!(edited.contains("model = \"new-review\" # keep the model inline comment"));
        assert!(edited.contains("limit = 8 # keep the limit inline comment"));
        assert!(edited.contains("[repo.demo.stage.review]"));
        let parsed = Config::parse(&edited).unwrap();
        assert_eq!(parsed.roles[&ExecutionRole::Review].model, "new-review");
        assert_eq!(
            parsed.roles[&ExecutionRole::Review].effort.as_deref(),
            Some("high")
        );
        assert_eq!(parsed.stage(Stage::Review).limit, 8);
    }

    #[test]
    fn an_atomic_config_write_replaces_the_file_and_removes_its_temporary_file() {
        let dir = std::env::temp_dir().join(format!(
            "aif-config-atomic-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("factory.toml");
        fs::write(&path, "old").unwrap();

        write_config_atomic(&path, "new").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn an_atomic_config_write_preserves_the_original_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "aif-config-permissions-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("factory.toml");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        write_config_atomic(&path, "new").unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_checked_commit_detects_a_change_after_temporary_file_preparation() {
        let dir = std::env::temp_dir().join(format!(
            "aif-config-checked-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("factory.toml");
        fs::write(&path, "original").unwrap();
        let prepared = prepare_config_atomic(&path, "candidate").unwrap();
        fs::write(&path, "external").unwrap();

        let outcome = commit_config_atomic_checked(prepared, &file_revision("original")).unwrap();

        assert_eq!(
            outcome,
            AtomicWrite::Stale {
                revision: file_revision("external")
            }
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "external");
        assert!(!path.with_extension("toml.tmp").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_file_revision_depends_only_on_complete_content() {
        assert_eq!(file_revision("same"), file_revision("same"));
        assert_ne!(file_revision("same"), file_revision("same\n"));
        assert_ne!(file_revision("same"), file_revision("other"));
    }
}
