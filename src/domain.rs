use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_yaml::Mapping as YamlMapping;

pub fn normalize_state(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueBlockerRef {
    pub id: Option<String>,
    pub identifier: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub state: String,
    pub branch_name: Option<String>,
    pub url: Option<String>,
    pub assignee_id: Option<String>,
    pub labels: Vec<String>,
    pub blocked_by: Vec<IssueBlockerRef>,
    pub assigned_to_worker: bool,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Issue {
    pub fn has_required_fields(&self) -> bool {
        !self.id.trim().is_empty()
            && !self.identifier.trim().is_empty()
            && !self.title.trim().is_empty()
            && !self.state.trim().is_empty()
    }

    pub fn normalized_state(&self) -> String {
        normalize_state(&self.state)
    }

    pub fn has_non_terminal_blocker(&self, terminal_states: &[String]) -> bool {
        if self.normalized_state() != "todo" {
            return false;
        }

        let terminal: BTreeSet<String> = terminal_states
            .iter()
            .map(|value| normalize_state(value))
            .collect();

        self.blocked_by
            .iter()
            .any(|blocker| match blocker.state.as_deref() {
                Some(state) => !terminal.contains(&normalize_state(state)),
                None => true,
            })
    }

    pub fn example() -> Self {
        Self {
            id: "issue_001".to_string(),
            identifier: "ABC-123".to_string(),
            title: "Port Symphony orchestrator to Rust".to_string(),
            description: Some("Create a Rust implementation of Symphony.".to_string()),
            priority: Some(1),
            state: "Todo".to_string(),
            branch_name: Some("abc-123-port-symphony-to-rust".to_string()),
            url: Some("https://linear.app/example/issue/ABC-123".to_string()),
            assignee_id: None,
            labels: vec!["rust".to_string(), "orchestration".to_string()],
            blocked_by: Vec::new(),
            assigned_to_worker: true,
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowDefinition {
    pub config: YamlMapping,
    pub prompt_template: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    PreparingWorkspace,
    RunningBeforeRunHook,
    StartingSession,
    RunningTurn,
    Succeeded,
    Failed,
    TimedOut,
    Stalled,
    Canceled,
    TurnInputRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LiveSession {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub codex_app_server_pid: Option<String>,
    pub last_codex_event: Option<String>,
    pub last_codex_timestamp: Option<DateTime<Utc>>,
    pub last_codex_message: Option<String>,
    pub codex_input_tokens: u64,
    pub codex_output_tokens: u64,
    pub codex_total_tokens: u64,
    pub last_reported_input_tokens: u64,
    pub last_reported_output_tokens: u64,
    pub last_reported_total_tokens: u64,
    pub turn_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningEntry {
    pub issue: Issue,
    pub attempt: Option<u32>,
    pub worker_host: Option<String>,
    pub workspace_path: Option<PathBuf>,
    pub started_at: DateTime<Utc>,
    pub status: AttemptStatus,
    pub live_session: LiveSession,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryEntry {
    pub issue_id: String,
    pub identifier: String,
    pub attempt: u32,
    pub due_at_ms: u64,
    pub error: Option<String>,
    pub worker_host: Option<String>,
    pub workspace_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CodexTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub runtime_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentUpdate {
    pub event: String,
    pub timestamp: DateTime<Utc>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub codex_app_server_pid: Option<String>,
    pub worker_host: Option<String>,
    pub last_message: Option<String>,
    pub usage: Option<JsonValue>,
    pub rate_limits: Option<JsonValue>,
    pub payload: Option<JsonValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OrchestratorRuntimeState {
    pub poll_interval_ms: u64,
    pub max_concurrent_agents: usize,
    pub next_poll_due_at_ms: Option<u64>,
    pub poll_check_in_progress: bool,
    pub running: BTreeMap<String, RunningEntry>,
    pub completed: BTreeSet<String>,
    pub claimed: BTreeSet<String>,
    pub retry_attempts: BTreeMap<String, RetryEntry>,
    pub codex_totals: CodexTotals,
    pub codex_rate_limits: Option<JsonValue>,
}

impl OrchestratorRuntimeState {
    pub fn new(poll_interval_ms: u64, max_concurrent_agents: usize) -> Self {
        Self {
            poll_interval_ms,
            max_concurrent_agents,
            next_poll_due_at_ms: None,
            poll_check_in_progress: false,
            running: BTreeMap::new(),
            completed: BTreeSet::new(),
            claimed: BTreeSet::new(),
            retry_attempts: BTreeMap::new(),
            codex_totals: CodexTotals::default(),
            codex_rate_limits: None,
        }
    }
}
