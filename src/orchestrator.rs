use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;

use crate::config::ServiceConfig;
use crate::domain::{
    AgentUpdate, AttemptStatus, Issue, OrchestratorRuntimeState, RetryEntry, RunningEntry,
    normalize_state,
};

const CONTINUATION_RETRY_DELAY_MS: u64 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EligibilityBlockReason {
    MissingRequiredFields,
    InactiveState(String),
    TerminalState(String),
    AlreadyRunning,
    AlreadyClaimed,
    NoGlobalSlots,
    NoStateSlots { state: String, limit: usize },
    TodoBlocked,
}

#[derive(Debug, Clone)]
pub struct Orchestrator {
    pub config: ServiceConfig,
    pub state: OrchestratorRuntimeState,
}

impl Orchestrator {
    pub fn new(config: ServiceConfig) -> Self {
        let state = OrchestratorRuntimeState::new(
            config.polling.interval_ms,
            config.agent.max_concurrent_agents,
        );
        Self { config, state }
    }

    pub fn refresh_runtime_config(&mut self, config: ServiceConfig) {
        self.state.poll_interval_ms = config.polling.interval_ms;
        self.state.max_concurrent_agents = config.agent.max_concurrent_agents;
        self.config = config;
    }

    pub fn available_global_slots(&self) -> usize {
        self.state
            .max_concurrent_agents
            .saturating_sub(self.state.running.len())
    }

    pub fn block_reason(&self, issue: &Issue) -> Option<EligibilityBlockReason> {
        if !issue.has_required_fields() {
            return Some(EligibilityBlockReason::MissingRequiredFields);
        }

        let normalized_state = issue.normalized_state();
        let active_states = self.config.normalized_active_states();
        let terminal_states = self.config.normalized_terminal_states();

        if terminal_states.contains(&normalized_state) {
            return Some(EligibilityBlockReason::TerminalState(issue.state.clone()));
        }
        if !active_states.contains(&normalized_state) {
            return Some(EligibilityBlockReason::InactiveState(issue.state.clone()));
        }
        if self.state.running.contains_key(&issue.id) {
            return Some(EligibilityBlockReason::AlreadyRunning);
        }
        if self.state.claimed.contains(&issue.id) {
            return Some(EligibilityBlockReason::AlreadyClaimed);
        }
        if self.available_global_slots() == 0 {
            return Some(EligibilityBlockReason::NoGlobalSlots);
        }

        let state_limit = self.config.max_concurrent_agents_for_state(&issue.state);
        if self.running_issue_count_for_state(&issue.state) >= state_limit {
            return Some(EligibilityBlockReason::NoStateSlots {
                state: issue.state.clone(),
                limit: state_limit,
            });
        }

        if issue.has_non_terminal_blocker(&self.config.tracker.terminal_states) {
            return Some(EligibilityBlockReason::TodoBlocked);
        }

        None
    }

    pub fn select_dispatch_candidates<'a>(&self, issues: &'a [Issue]) -> Vec<&'a Issue> {
        let mut candidates: Vec<&Issue> = issues
            .iter()
            .filter(|issue| self.block_reason(issue).is_none())
            .collect();

        candidates.sort_by(|left, right| compare_issues(left, right));

        let mut selected = Vec::new();
        let mut remaining_slots = self.available_global_slots();
        let mut running_by_state = self.running_counts_by_state();

        for issue in candidates {
            if remaining_slots == 0 {
                break;
            }

            let normalized_state = normalize_state(&issue.state);
            let limit = self.config.max_concurrent_agents_for_state(&issue.state);
            let used = running_by_state
                .get(&normalized_state)
                .copied()
                .unwrap_or_default();
            if used >= limit {
                continue;
            }

            selected.push(issue);
            remaining_slots -= 1;
            running_by_state.insert(normalized_state, used + 1);
        }

        selected
    }

    pub fn mark_running(
        &mut self,
        issue: Issue,
        attempt: Option<u32>,
        worker_host: Option<String>,
        started_at: DateTime<Utc>,
    ) {
        self.state.claimed.insert(issue.id.clone());
        self.state.retry_attempts.remove(&issue.id);
        self.state.running.insert(
            issue.id.clone(),
            RunningEntry {
                issue,
                attempt,
                worker_host,
                workspace_path: None,
                started_at,
                status: AttemptStatus::PreparingWorkspace,
                live_session: Default::default(),
                last_error: None,
            },
        );
    }

    pub fn update_runtime_info(
        &mut self,
        issue_id: &str,
        worker_host: Option<String>,
        workspace_path: PathBuf,
    ) {
        if let Some(entry) = self.state.running.get_mut(issue_id) {
            entry.worker_host = worker_host;
            entry.workspace_path = Some(workspace_path);
            entry.status = AttemptStatus::RunningBeforeRunHook;
        }
    }

    pub fn integrate_update(&mut self, issue_id: &str, update: &AgentUpdate) {
        let Some(entry) = self.state.running.get_mut(issue_id) else {
            return;
        };

        entry.live_session.last_codex_event = Some(update.event.clone());
        entry.live_session.last_codex_timestamp = Some(update.timestamp);
        entry.live_session.last_codex_message = update.last_message.clone();
        if let Some(session_id) = &update.session_id {
            entry.live_session.session_id = Some(session_id.clone());
        }
        if let Some(thread_id) = &update.thread_id {
            entry.live_session.thread_id = Some(thread_id.clone());
        }
        if let Some(turn_id) = &update.turn_id {
            entry.live_session.turn_id = Some(turn_id.clone());
        }
        if let Some(pid) = &update.codex_app_server_pid {
            entry.live_session.codex_app_server_pid = Some(pid.clone());
        }
        if update.event == "session_started" {
            entry.live_session.turn_count += 1;
            entry.status = AttemptStatus::RunningTurn;
        }

        let (delta_input, delta_output, delta_total) =
            token_deltas(&entry.live_session, update.usage.as_ref());
        entry.live_session.codex_input_tokens += delta_input;
        entry.live_session.codex_output_tokens += delta_output;
        entry.live_session.codex_total_tokens += delta_total;
        if let Some(usage) = update.usage.as_ref() {
            if let Some(value) = get_token_usage(usage, TokenKind::Input) {
                entry.live_session.last_reported_input_tokens = value;
            }
            if let Some(value) = get_token_usage(usage, TokenKind::Output) {
                entry.live_session.last_reported_output_tokens = value;
            }
            if let Some(value) = get_token_usage(usage, TokenKind::Total) {
                entry.live_session.last_reported_total_tokens = value;
            }
        }

        self.state.codex_totals.input_tokens += delta_input;
        self.state.codex_totals.output_tokens += delta_output;
        self.state.codex_totals.total_tokens += delta_total;

        if let Some(rate_limits) = extract_rate_limits(update) {
            self.state.codex_rate_limits = Some(rate_limits);
        }

        entry.status = status_from_event(&update.event);
    }

    pub fn refresh_running_issue(&mut self, issue: Issue) {
        if let Some(entry) = self.state.running.get_mut(&issue.id) {
            entry.issue = issue;
        }
    }

    pub fn running_issue_ids(&self) -> Vec<String> {
        self.state.running.keys().cloned().collect()
    }

    pub fn running_entry(&self, issue_id: &str) -> Option<&RunningEntry> {
        self.state.running.get(issue_id)
    }

    pub fn take_running_entry(&mut self, issue_id: &str) -> Option<RunningEntry> {
        self.state.running.remove(issue_id)
    }

    pub fn release_claim(&mut self, issue_id: &str) {
        self.state.claimed.remove(issue_id);
        self.state.retry_attempts.remove(issue_id);
    }

    pub fn record_completion(&mut self, issue_id: &str) {
        self.state.completed.insert(issue_id.to_string());
    }

    pub fn record_runtime_seconds(&mut self, entry: &RunningEntry) {
        let seconds = Utc::now()
            .signed_duration_since(entry.started_at)
            .num_seconds()
            .max(0) as u64;
        self.state.codex_totals.runtime_seconds += seconds;
    }

    pub fn schedule_continuation_retry(
        &mut self,
        issue: &Issue,
        now_ms: u64,
        worker_host: Option<String>,
        workspace_path: Option<PathBuf>,
    ) {
        self.schedule_retry(
            issue,
            1,
            now_ms + CONTINUATION_RETRY_DELAY_MS,
            None,
            worker_host,
            workspace_path,
        );
    }

    pub fn schedule_failure_retry(
        &mut self,
        issue: &Issue,
        attempt: u32,
        now_ms: u64,
        error: Option<String>,
        worker_host: Option<String>,
        workspace_path: Option<PathBuf>,
    ) {
        let delay = failure_backoff_ms(attempt, self.config.agent.max_retry_backoff_ms);
        self.schedule_retry(
            issue,
            attempt,
            now_ms + delay,
            error,
            worker_host,
            workspace_path,
        );
    }

    pub fn take_due_retries(&mut self, now_ms: u64) -> Vec<RetryEntry> {
        let due_ids: Vec<String> = self
            .state
            .retry_attempts
            .iter()
            .filter_map(|(issue_id, entry)| (entry.due_at_ms <= now_ms).then_some(issue_id.clone()))
            .collect();

        let mut entries = Vec::new();
        for issue_id in due_ids {
            if let Some(entry) = self.state.retry_attempts.remove(&issue_id) {
                entries.push(entry);
            }
        }
        entries
    }

    pub fn running_issue_count_for_state(&self, issue_state: &str) -> usize {
        let normalized_state = normalize_state(issue_state);
        self.state
            .running
            .values()
            .filter(|entry| entry.issue.normalized_state() == normalized_state)
            .count()
    }

    fn running_counts_by_state(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for entry in self.state.running.values() {
            *counts.entry(entry.issue.normalized_state()).or_insert(0) += 1;
        }
        counts
    }

    fn schedule_retry(
        &mut self,
        issue: &Issue,
        attempt: u32,
        due_at_ms: u64,
        error: Option<String>,
        worker_host: Option<String>,
        workspace_path: Option<PathBuf>,
    ) {
        self.state.claimed.insert(issue.id.clone());
        self.state.retry_attempts.insert(
            issue.id.clone(),
            RetryEntry {
                issue_id: issue.id.clone(),
                identifier: issue.identifier.clone(),
                attempt,
                due_at_ms,
                error,
                worker_host,
                workspace_path,
            },
        );
    }
}

pub fn failure_backoff_ms(attempt: u32, max_backoff_ms: u64) -> u64 {
    let exponent = attempt.saturating_sub(1).min(10);
    let factor = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
    10_000u64.saturating_mul(factor).min(max_backoff_ms)
}

fn compare_issues(left: &Issue, right: &Issue) -> Ordering {
    left.priority
        .unwrap_or(i64::MAX)
        .cmp(&right.priority.unwrap_or(i64::MAX))
        .then_with(|| left.created_at.cmp(&right.created_at))
        .then_with(|| left.identifier.cmp(&right.identifier))
}

fn status_from_event(event: &str) -> AttemptStatus {
    match event {
        "session_started" | "notification" | "tool_call_completed" | "approval_auto_approved" => {
            AttemptStatus::RunningTurn
        }
        "turn_completed" => AttemptStatus::Succeeded,
        "turn_failed" | "tool_call_failed" | "approval_required" => AttemptStatus::Failed,
        "turn_cancelled" => AttemptStatus::Canceled,
        "turn_input_required" => AttemptStatus::TurnInputRequired,
        _ => AttemptStatus::RunningTurn,
    }
}

fn token_deltas(
    session: &crate::domain::LiveSession,
    usage: Option<&JsonValue>,
) -> (u64, u64, u64) {
    let Some(usage) = usage else {
        return (0, 0, 0);
    };

    let input = get_token_usage(usage, TokenKind::Input)
        .and_then(|value| value.checked_sub(session.last_reported_input_tokens))
        .unwrap_or(0);
    let output = get_token_usage(usage, TokenKind::Output)
        .and_then(|value| value.checked_sub(session.last_reported_output_tokens))
        .unwrap_or(0);
    let total = get_token_usage(usage, TokenKind::Total)
        .and_then(|value| value.checked_sub(session.last_reported_total_tokens))
        .unwrap_or(0);
    (input, output, total)
}

fn extract_rate_limits(update: &AgentUpdate) -> Option<JsonValue> {
    update
        .rate_limits
        .clone()
        .or_else(|| update.payload.as_ref().and_then(search_rate_limits))
}

fn search_rate_limits(value: &JsonValue) -> Option<JsonValue> {
    if value.get("limit_id").is_some() {
        return Some(value.clone());
    }

    match value {
        JsonValue::Object(map) => map.values().find_map(search_rate_limits),
        JsonValue::Array(values) => values.iter().find_map(search_rate_limits),
        _ => None,
    }
}

enum TokenKind {
    Input,
    Output,
    Total,
}

fn get_token_usage(usage: &JsonValue, kind: TokenKind) -> Option<u64> {
    let keys: &[&str] = match kind {
        TokenKind::Input => &[
            "input_tokens",
            "prompt_tokens",
            "inputTokens",
            "promptTokens",
        ],
        TokenKind::Output => &[
            "output_tokens",
            "completion_tokens",
            "outputTokens",
            "completionTokens",
        ],
        TokenKind::Total => &["total_tokens", "total", "totalTokens"],
    };

    keys.iter()
        .find_map(|key| usage.get(*key))
        .and_then(integer_like)
}

fn integer_like(value: &JsonValue) -> Option<u64> {
    match value {
        JsonValue::Number(number) => number.as_u64(),
        JsonValue::String(value) => value.trim().parse::<u64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Orchestrator, failure_backoff_ms};
    use crate::config::ServiceConfig;
    use crate::domain::{Issue, IssueBlockerRef};

    #[test]
    fn retries_use_exponential_backoff_with_cap() {
        assert_eq!(failure_backoff_ms(1, 300_000), 10_000);
        assert_eq!(failure_backoff_ms(2, 300_000), 20_000);
        assert_eq!(failure_backoff_ms(6, 300_000), 300_000);
    }

    #[test]
    fn blocked_todo_issue_is_not_dispatchable() {
        let mut config = ServiceConfig::default();
        config.tracker.project_slug = Some("demo".to_string());
        let orchestrator = Orchestrator::new(config);
        let mut issue = Issue::example();
        issue.blocked_by = vec![IssueBlockerRef {
            id: Some("blocker".to_string()),
            identifier: Some("ABC-1".to_string()),
            state: Some("In Progress".to_string()),
        }];

        assert!(orchestrator.block_reason(&issue).is_some());
    }
}
