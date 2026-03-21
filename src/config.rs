use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use serde_yaml::{Mapping, Value as YamlValue};

use crate::domain::{WorkflowDefinition, normalize_state};
use crate::path_safety;

pub const DEFAULT_LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 30_000;
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_MAX_CONCURRENT_AGENTS: usize = 10;
pub const DEFAULT_MAX_TURNS: u32 = 20;
pub const DEFAULT_MAX_RETRY_BACKOFF_MS: u64 = 300_000;
pub const DEFAULT_CODEX_COMMAND: &str = "codex app-server";
pub const DEFAULT_TURN_TIMEOUT_MS: u64 = 3_600_000;
pub const DEFAULT_READ_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_STALL_TIMEOUT_MS: i64 = 300_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceConfig {
    pub tracker: TrackerConfig,
    pub polling: PollingConfig,
    pub workspace: WorkspaceConfig,
    pub worker: WorkerConfig,
    pub agent: AgentConfig,
    pub codex: CodexConfig,
    pub hooks: HooksConfig,
    pub observability: ObservabilityConfig,
    pub server: ServerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerConfig {
    pub kind: Option<String>,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub project_slug: Option<String>,
    pub assignee: Option<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollingConfig {
    pub interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkerConfig {
    pub ssh_hosts: Vec<String>,
    pub max_concurrent_agents_per_host: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub max_concurrent_agents: usize,
    pub max_turns: u32,
    pub max_retry_backoff_ms: u64,
    pub max_concurrent_agents_by_state: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexConfig {
    pub command: String,
    pub approval_policy: JsonValue,
    pub thread_sandbox: String,
    pub turn_sandbox_policy: Option<JsonValue>,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub stall_timeout_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HooksConfig {
    pub after_create: Option<String>,
    pub before_run: Option<String>,
    pub after_run: Option<String>,
    pub before_remove: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservabilityConfig {
    pub dashboard_enabled: bool,
    pub refresh_ms: u64,
    pub render_interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub port: Option<u16>,
    pub host: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexRuntimeSettings {
    pub approval_policy: JsonValue,
    pub thread_sandbox: String,
    pub turn_sandbox_policy: JsonValue,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            tracker: TrackerConfig::default(),
            polling: PollingConfig::default(),
            workspace: WorkspaceConfig::default(),
            worker: WorkerConfig::default(),
            agent: AgentConfig::default(),
            codex: CodexConfig::default(),
            hooks: HooksConfig::default(),
            observability: ObservabilityConfig::default(),
            server: ServerConfig::default(),
        }
    }
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            kind: Some("linear".to_string()),
            endpoint: DEFAULT_LINEAR_ENDPOINT.to_string(),
            api_key: env::var("LINEAR_API_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            project_slug: None,
            assignee: env::var("LINEAR_ASSIGNEE")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            active_states: vec!["Todo".to_string(), "In Progress".to_string()],
            terminal_states: vec![
                "Closed".to_string(),
                "Cancelled".to_string(),
                "Canceled".to_string(),
                "Duplicate".to_string(),
                "Done".to_string(),
            ],
        }
    }
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            interval_ms: DEFAULT_POLL_INTERVAL_MS,
        }
    }
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            root: default_workspace_root().to_string_lossy().to_string(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_concurrent_agents: DEFAULT_MAX_CONCURRENT_AGENTS,
            max_turns: DEFAULT_MAX_TURNS,
            max_retry_backoff_ms: DEFAULT_MAX_RETRY_BACKOFF_MS,
            max_concurrent_agents_by_state: BTreeMap::new(),
        }
    }
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            command: DEFAULT_CODEX_COMMAND.to_string(),
            approval_policy: json!({
                "reject": {
                    "sandbox_approval": true,
                    "rules": true,
                    "mcp_elicitations": true
                }
            }),
            thread_sandbox: "workspace-write".to_string(),
            turn_sandbox_policy: None,
            turn_timeout_ms: DEFAULT_TURN_TIMEOUT_MS,
            read_timeout_ms: DEFAULT_READ_TIMEOUT_MS,
            stall_timeout_ms: DEFAULT_STALL_TIMEOUT_MS,
        }
    }
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            after_create: None,
            before_run: None,
            after_run: None,
            before_remove: None,
            timeout_ms: DEFAULT_HOOK_TIMEOUT_MS,
        }
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            dashboard_enabled: true,
            refresh_ms: 1_000,
            render_interval_ms: 16,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: None,
            host: "127.0.0.1".to_string(),
        }
    }
}

impl ServiceConfig {
    pub fn from_workflow_definition(definition: &WorkflowDefinition) -> Result<Self, ConfigError> {
        Self::from_workflow_config(&definition.config)
    }

    pub fn from_workflow_config(config: &Mapping) -> Result<Self, ConfigError> {
        let raw: RawServiceConfig = serde_yaml::from_value(YamlValue::Mapping(config.clone()))
            .map_err(|error| ConfigError::invalid_workflow(error.to_string()))?;

        let tracker = raw
            .tracker
            .map(TrackerConfig::from_raw)
            .transpose()?
            .unwrap_or_default();
        let polling = raw
            .polling
            .map(PollingConfig::from_raw)
            .transpose()?
            .unwrap_or_default();
        let workspace = raw
            .workspace
            .map(WorkspaceConfig::from_raw)
            .transpose()?
            .unwrap_or_default();
        let worker = raw
            .worker
            .map(WorkerConfig::from_raw)
            .transpose()?
            .unwrap_or_default();
        let agent = raw
            .agent
            .map(AgentConfig::from_raw)
            .transpose()?
            .unwrap_or_default();
        let codex = raw
            .codex
            .map(CodexConfig::from_raw)
            .transpose()?
            .unwrap_or_default();
        let hooks = raw
            .hooks
            .map(HooksConfig::from_raw)
            .transpose()?
            .unwrap_or_default();
        let observability = raw
            .observability
            .map(ObservabilityConfig::from_raw)
            .transpose()?
            .unwrap_or_default();
        let server = raw
            .server
            .map(ServerConfig::from_raw)
            .transpose()?
            .unwrap_or_default();

        Ok(Self {
            tracker,
            polling,
            workspace,
            worker,
            agent,
            codex,
            hooks,
            observability,
            server,
        })
    }

    pub fn validate_for_dispatch(&self) -> Vec<ConfigValidationError> {
        let mut errors = Vec::new();

        match self.tracker.kind.as_deref() {
            Some("linear") | Some("memory") => {}
            Some(kind) => errors.push(ConfigValidationError::UnsupportedTrackerKind(
                kind.to_string(),
            )),
            None => errors.push(ConfigValidationError::MissingTrackerKind),
        }

        if self.tracker.kind.as_deref() == Some("linear") && self.tracker.api_key.is_none() {
            errors.push(ConfigValidationError::MissingTrackerApiKey);
        }

        if self.tracker.kind.as_deref() == Some("linear")
            && self
                .tracker
                .project_slug
                .as_deref()
                .map(str::trim)
                .is_none_or(str::is_empty)
        {
            errors.push(ConfigValidationError::MissingTrackerProjectSlug);
        }

        if self.codex.command.trim().is_empty() {
            errors.push(ConfigValidationError::MissingCodexCommand);
        }

        errors
    }

    pub fn dispatch_ready(&self) -> bool {
        self.validate_for_dispatch().is_empty()
    }

    pub fn normalized_active_states(&self) -> BTreeSet<String> {
        self.tracker
            .active_states
            .iter()
            .map(|state| normalize_state(state))
            .collect()
    }

    pub fn normalized_terminal_states(&self) -> BTreeSet<String> {
        self.tracker
            .terminal_states
            .iter()
            .map(|state| normalize_state(state))
            .collect()
    }

    pub fn max_concurrent_agents_for_state(&self, state: &str) -> usize {
        self.agent
            .max_concurrent_agents_by_state
            .get(&normalize_state(state))
            .copied()
            .unwrap_or(self.agent.max_concurrent_agents)
    }

    pub fn workspace_root_path(&self) -> PathBuf {
        expand_local_path(&self.workspace.root)
    }

    pub fn codex_runtime_settings(
        &self,
        workspace: Option<&Path>,
        remote: bool,
    ) -> Result<CodexRuntimeSettings, ConfigError> {
        Ok(CodexRuntimeSettings {
            approval_policy: self.codex.approval_policy.clone(),
            thread_sandbox: self.codex.thread_sandbox.clone(),
            turn_sandbox_policy: self.resolve_runtime_turn_sandbox_policy(workspace, remote)?,
        })
    }

    pub fn resolve_runtime_turn_sandbox_policy(
        &self,
        workspace: Option<&Path>,
        remote: bool,
    ) -> Result<JsonValue, ConfigError> {
        if let Some(policy) = &self.codex.turn_sandbox_policy {
            return Ok(policy.clone());
        }

        let workspace_root = workspace
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.workspace_root_path());

        if remote {
            return Ok(default_turn_sandbox_policy(&workspace_root));
        }

        let canonical = path_safety::canonicalize(&workspace_root).map_err(|error| {
            ConfigError::invalid_workflow(format!("unsafe turn sandbox policy: {error}"))
        })?;

        Ok(default_turn_sandbox_policy(&canonical))
    }
}

impl TrackerConfig {
    fn from_raw(raw: RawTrackerConfig) -> Result<Self, ConfigError> {
        Ok(Self {
            kind: raw.kind.or_else(|| Some("linear".to_string())),
            endpoint: raw
                .endpoint
                .unwrap_or_else(|| DEFAULT_LINEAR_ENDPOINT.to_string()),
            api_key: raw
                .api_key
                .as_deref()
                .and_then(resolve_secret_setting)
                .or_else(|| {
                    env::var("LINEAR_API_KEY")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                }),
            project_slug: normalize_optional_string(raw.project_slug),
            assignee: raw
                .assignee
                .as_deref()
                .and_then(resolve_secret_setting)
                .or_else(|| {
                    env::var("LINEAR_ASSIGNEE")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                }),
            active_states: non_empty_string_list(raw.active_states)
                .unwrap_or_else(|| TrackerConfig::default().active_states),
            terminal_states: non_empty_string_list(raw.terminal_states)
                .unwrap_or_else(|| TrackerConfig::default().terminal_states),
        })
    }
}

impl PollingConfig {
    fn from_raw(raw: RawPollingConfig) -> Result<Self, ConfigError> {
        let interval_ms = raw.interval_ms.unwrap_or(DEFAULT_POLL_INTERVAL_MS);
        if interval_ms == 0 {
            return Err(ConfigError::invalid_workflow(
                "polling.interval_ms must be greater than 0",
            ));
        }
        Ok(Self { interval_ms })
    }
}

impl WorkspaceConfig {
    fn from_raw(raw: RawWorkspaceConfig) -> Result<Self, ConfigError> {
        Ok(Self {
            root: resolve_path_value(raw.root.as_deref(), &default_workspace_root()),
        })
    }
}

impl WorkerConfig {
    fn from_raw(raw: RawWorkerConfig) -> Result<Self, ConfigError> {
        if let Some(limit) = raw.max_concurrent_agents_per_host {
            if limit == 0 {
                return Err(ConfigError::invalid_workflow(
                    "worker.max_concurrent_agents_per_host must be greater than 0",
                ));
            }
        }

        Ok(Self {
            ssh_hosts: raw
                .ssh_hosts
                .unwrap_or_default()
                .into_iter()
                .filter_map(|value| normalize_optional_string(Some(value)))
                .collect(),
            max_concurrent_agents_per_host: raw.max_concurrent_agents_per_host,
        })
    }
}

impl AgentConfig {
    fn from_raw(raw: RawAgentConfig) -> Result<Self, ConfigError> {
        let max_concurrent_agents = raw
            .max_concurrent_agents
            .unwrap_or(DEFAULT_MAX_CONCURRENT_AGENTS);
        let max_turns = raw.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
        let max_retry_backoff_ms = raw
            .max_retry_backoff_ms
            .unwrap_or(DEFAULT_MAX_RETRY_BACKOFF_MS);

        if max_concurrent_agents == 0 {
            return Err(ConfigError::invalid_workflow(
                "agent.max_concurrent_agents must be greater than 0",
            ));
        }
        if max_turns == 0 {
            return Err(ConfigError::invalid_workflow(
                "agent.max_turns must be greater than 0",
            ));
        }
        if max_retry_backoff_ms == 0 {
            return Err(ConfigError::invalid_workflow(
                "agent.max_retry_backoff_ms must be greater than 0",
            ));
        }

        let mut max_concurrent_agents_by_state = BTreeMap::new();
        for (state, limit) in raw.max_concurrent_agents_by_state.unwrap_or_default() {
            let normalized_state = normalize_state(&state);
            if normalized_state.is_empty() {
                return Err(ConfigError::invalid_workflow(
                    "agent.max_concurrent_agents_by_state keys must not be blank",
                ));
            }
            if limit == 0 {
                return Err(ConfigError::invalid_workflow(
                    "agent.max_concurrent_agents_by_state values must be positive integers",
                ));
            }
            max_concurrent_agents_by_state.insert(normalized_state, limit);
        }

        Ok(Self {
            max_concurrent_agents,
            max_turns,
            max_retry_backoff_ms,
            max_concurrent_agents_by_state,
        })
    }
}

impl CodexConfig {
    fn from_raw(raw: RawCodexConfig) -> Result<Self, ConfigError> {
        let command = normalize_optional_string(raw.command)
            .unwrap_or_else(|| DEFAULT_CODEX_COMMAND.to_string());
        let turn_timeout_ms = raw.turn_timeout_ms.unwrap_or(DEFAULT_TURN_TIMEOUT_MS);
        let read_timeout_ms = raw.read_timeout_ms.unwrap_or(DEFAULT_READ_TIMEOUT_MS);
        let stall_timeout_ms = raw.stall_timeout_ms.unwrap_or(DEFAULT_STALL_TIMEOUT_MS);

        if command.trim().is_empty() {
            return Err(ConfigError::invalid_workflow(
                "codex.command must be a non-empty string",
            ));
        }
        if turn_timeout_ms == 0 {
            return Err(ConfigError::invalid_workflow(
                "codex.turn_timeout_ms must be greater than 0",
            ));
        }
        if read_timeout_ms == 0 {
            return Err(ConfigError::invalid_workflow(
                "codex.read_timeout_ms must be greater than 0",
            ));
        }

        Ok(Self {
            command,
            approval_policy: raw.approval_policy.unwrap_or_else(default_approval_policy),
            thread_sandbox: normalize_optional_string(raw.thread_sandbox)
                .unwrap_or_else(|| "workspace-write".to_string()),
            turn_sandbox_policy: raw.turn_sandbox_policy.map(normalize_json_value),
            turn_timeout_ms,
            read_timeout_ms,
            stall_timeout_ms,
        })
    }
}

impl HooksConfig {
    fn from_raw(raw: RawHooksConfig) -> Result<Self, ConfigError> {
        let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_HOOK_TIMEOUT_MS);
        if timeout_ms == 0 {
            return Err(ConfigError::invalid_workflow(
                "hooks.timeout_ms must be greater than 0",
            ));
        }
        Ok(Self {
            after_create: normalize_optional_string(raw.after_create),
            before_run: normalize_optional_string(raw.before_run),
            after_run: normalize_optional_string(raw.after_run),
            before_remove: normalize_optional_string(raw.before_remove),
            timeout_ms,
        })
    }
}

impl ObservabilityConfig {
    fn from_raw(raw: RawObservabilityConfig) -> Result<Self, ConfigError> {
        let refresh_ms = raw.refresh_ms.unwrap_or(1_000);
        let render_interval_ms = raw.render_interval_ms.unwrap_or(16);
        if refresh_ms == 0 {
            return Err(ConfigError::invalid_workflow(
                "observability.refresh_ms must be greater than 0",
            ));
        }
        if render_interval_ms == 0 {
            return Err(ConfigError::invalid_workflow(
                "observability.render_interval_ms must be greater than 0",
            ));
        }
        Ok(Self {
            dashboard_enabled: raw.dashboard_enabled.unwrap_or(true),
            refresh_ms,
            render_interval_ms,
        })
    }
}

impl ServerConfig {
    fn from_raw(raw: RawServerConfig) -> Result<Self, ConfigError> {
        Ok(Self {
            port: raw.port,
            host: normalize_optional_string(raw.host).unwrap_or_else(|| "127.0.0.1".to_string()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValidationError {
    MissingTrackerKind,
    UnsupportedTrackerKind(String),
    MissingTrackerApiKey,
    MissingTrackerProjectSlug,
    MissingCodexCommand,
}

impl fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTrackerKind => write!(f, "tracker.kind is required"),
            Self::UnsupportedTrackerKind(kind) => {
                write!(f, "tracker.kind `{kind}` is not supported")
            }
            Self::MissingTrackerApiKey => {
                write!(f, "tracker.api_key is missing after environment resolution")
            }
            Self::MissingTrackerProjectSlug => {
                write!(f, "tracker.project_slug is required for Linear dispatch")
            }
            Self::MissingCodexCommand => write!(f, "codex.command must be non-empty"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    pub fn invalid_workflow(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ConfigError {}

impl std::error::Error for ConfigValidationError {}

pub fn default_workspace_root() -> PathBuf {
    env::temp_dir().join("symphony_workspaces")
}

pub fn default_approval_policy() -> JsonValue {
    json!({
        "reject": {
            "sandbox_approval": true,
            "rules": true,
            "mcp_elicitations": true
        }
    })
}

pub fn resolve_env_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(name) = trimmed.strip_prefix('$') {
        if valid_env_name(name) {
            return env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
        }
    }

    Some(trimmed.to_string())
}

pub fn resolve_secret_setting(raw: &str) -> Option<String> {
    resolve_env_token(raw)
}

pub fn expand_local_path(raw: &str) -> PathBuf {
    let resolved = resolve_env_token(raw).unwrap_or_else(|| raw.to_string());
    if let Some(stripped) = resolved.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return Path::new(&home).join(stripped);
        }
    }
    if resolved == "~" {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(resolved)
}

pub fn resolve_path_value(raw: Option<&str>, default: &Path) -> String {
    match raw {
        Some(value) => match normalize_optional_string(Some(value.to_string())) {
            Some(trimmed) => expand_local_path(&trimmed).to_string_lossy().to_string(),
            None => default.to_string_lossy().to_string(),
        },
        None => default.to_string_lossy().to_string(),
    }
}

fn default_turn_sandbox_policy(workspace_root: &Path) -> JsonValue {
    json!({
        "type": "workspaceWrite",
        "writableRoots": [workspace_root.to_string_lossy().to_string()],
        "readOnlyAccess": { "type": "fullAccess" },
        "networkAccess": false,
        "excludeTmpdirEnvVar": false,
        "excludeSlashTmp": false
    })
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn non_empty_string_list(values: Option<Vec<String>>) -> Option<Vec<String>> {
    let values: Vec<String> = values
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| normalize_optional_string(Some(value)))
        .collect();

    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn normalize_json_value(value: JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(map) => JsonValue::Object(
            map.into_iter()
                .map(|(key, nested)| (key, normalize_json_value(nested)))
                .collect::<JsonMap<String, JsonValue>>(),
        ),
        JsonValue::Array(values) => {
            JsonValue::Array(values.into_iter().map(normalize_json_value).collect())
        }
        other => other,
    }
}

fn valid_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[derive(Debug, Deserialize, Default)]
struct RawServiceConfig {
    tracker: Option<RawTrackerConfig>,
    polling: Option<RawPollingConfig>,
    workspace: Option<RawWorkspaceConfig>,
    worker: Option<RawWorkerConfig>,
    agent: Option<RawAgentConfig>,
    codex: Option<RawCodexConfig>,
    hooks: Option<RawHooksConfig>,
    observability: Option<RawObservabilityConfig>,
    server: Option<RawServerConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct RawTrackerConfig {
    kind: Option<String>,
    endpoint: Option<String>,
    api_key: Option<String>,
    project_slug: Option<String>,
    assignee: Option<String>,
    active_states: Option<Vec<String>>,
    terminal_states: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
struct RawPollingConfig {
    interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct RawWorkspaceConfig {
    root: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawWorkerConfig {
    ssh_hosts: Option<Vec<String>>,
    max_concurrent_agents_per_host: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct RawAgentConfig {
    max_concurrent_agents: Option<usize>,
    max_turns: Option<u32>,
    max_retry_backoff_ms: Option<u64>,
    max_concurrent_agents_by_state: Option<BTreeMap<String, usize>>,
}

#[derive(Debug, Deserialize, Default)]
struct RawCodexConfig {
    command: Option<String>,
    approval_policy: Option<JsonValue>,
    thread_sandbox: Option<String>,
    turn_sandbox_policy: Option<JsonValue>,
    turn_timeout_ms: Option<u64>,
    read_timeout_ms: Option<u64>,
    stall_timeout_ms: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
struct RawHooksConfig {
    after_create: Option<String>,
    before_run: Option<String>,
    after_run: Option<String>,
    before_remove: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct RawObservabilityConfig {
    dashboard_enabled: Option<bool>,
    refresh_ms: Option<u64>,
    render_interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct RawServerConfig {
    port: Option<u16>,
    host: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value as YamlValue;

    fn workflow_config_from_yaml(yaml: &str) -> Mapping {
        match serde_yaml::from_str::<YamlValue>(yaml).expect("yaml should parse") {
            YamlValue::Mapping(mapping) => mapping,
            other => panic!("expected mapping, got {other:?}"),
        }
    }

    #[test]
    fn parses_and_normalizes_state_limits() {
        let config = ServiceConfig::from_workflow_config(&workflow_config_from_yaml(
            r#"
agent:
  max_concurrent_agents_by_state:
    In Progress: 2
    Todo: 1
"#,
        ))
        .expect("config should parse");

        assert_eq!(config.max_concurrent_agents_for_state("In Progress"), 2);
        assert_eq!(config.max_concurrent_agents_for_state("todo"), 1);
    }

    #[test]
    fn expands_workspace_root_env_tokens() {
        let value = resolve_path_value(Some("~/demo"), &default_workspace_root());
        assert!(value.ends_with("/demo"));
    }

    #[test]
    fn validation_reports_missing_linear_settings() {
        let mut config = ServiceConfig::default();
        config.tracker.api_key = None;
        config.tracker.project_slug = None;
        let errors = config.validate_for_dispatch();

        assert_eq!(errors.len(), 2);
    }
}
