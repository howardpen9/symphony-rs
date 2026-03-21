use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::{Value as JsonValue, json};

use crate::codex::{AppServerError, AppSession};
use crate::config::ServiceConfig;
use crate::domain::{AgentUpdate, Issue, WorkflowDefinition};
use crate::prompt::{LiquidLikePromptRenderer, PromptRenderer};
use crate::tracker::IssueTrackerClient;
use crate::workspace::WorkspaceManager;

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub issue: Issue,
    pub attempt: Option<u32>,
    pub worker_host: Option<String>,
    pub config: ServiceConfig,
    pub workflow: WorkflowDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerExitStatus {
    Completed,
    Cancelled(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct WorkerRuntimeInfo {
    pub worker_host: Option<String>,
    pub workspace_path: PathBuf,
}

#[derive(Clone)]
pub struct DefaultAgentRunner {
    tracker: Arc<dyn IssueTrackerClient>,
    prompt_renderer: LiquidLikePromptRenderer,
}

impl DefaultAgentRunner {
    pub fn new(tracker: Arc<dyn IssueTrackerClient>) -> Self {
        Self {
            tracker,
            prompt_renderer: LiquidLikePromptRenderer,
        }
    }

    pub fn run_attempt(
        &self,
        request: RunRequest,
        cancel_flag: Arc<AtomicBool>,
        on_runtime_info: &mut dyn FnMut(WorkerRuntimeInfo),
        on_update: &mut dyn FnMut(AgentUpdate),
    ) -> RunnerExitStatus {
        let workspace_manager = WorkspaceManager::new(request.config.clone());
        let assignment = match workspace_manager
            .create_for_issue(&request.issue, request.worker_host.as_deref())
        {
            Ok(assignment) => assignment,
            Err(error) => return RunnerExitStatus::Failed(error.to_string()),
        };

        on_runtime_info(WorkerRuntimeInfo {
            worker_host: request.worker_host.clone(),
            workspace_path: assignment.path.clone(),
        });

        if let Err(error) = workspace_manager.run_before_run_hook(
            &assignment.path,
            &request.issue,
            request.worker_host.as_deref(),
        ) {
            workspace_manager.run_after_run_hook(
                &assignment.path,
                &request.issue,
                request.worker_host.as_deref(),
            );
            return RunnerExitStatus::Failed(error.to_string());
        }

        let result = self.run_turns(&request, &assignment.path, cancel_flag.as_ref(), on_update);

        workspace_manager.run_after_run_hook(
            &assignment.path,
            &request.issue,
            request.worker_host.as_deref(),
        );

        result
    }

    fn run_turns(
        &self,
        request: &RunRequest,
        workspace: &PathBuf,
        cancel_flag: &AtomicBool,
        on_update: &mut dyn FnMut(AgentUpdate),
    ) -> RunnerExitStatus {
        let mut session = match AppSession::start(
            workspace.clone(),
            request.worker_host.clone(),
            &request.config,
        ) {
            Ok(session) => session,
            Err(error) => return RunnerExitStatus::Failed(error.to_string()),
        };

        let mut current_issue = request.issue.clone();
        let max_turns = request.config.agent.max_turns;
        let tracker = self.tracker.clone();
        let config = request.config.clone();

        let result = (1..=max_turns).find_map(|turn_number| {
            let prompt = if turn_number == 1 {
                match self.prompt_renderer.render(
                    &request.workflow.prompt_template,
                    &current_issue,
                    request.attempt,
                ) {
                    Ok(prompt) => prompt,
                    Err(error) => return Some(RunnerExitStatus::Failed(error.to_string())),
                }
            } else {
                continuation_prompt(turn_number, max_turns)
            };

            let tool_executor = |tool_name: Option<&str>, arguments: JsonValue| {
                execute_dynamic_tool(tracker.as_ref(), &config, tool_name, arguments)
            };

            match session.run_turn(
                &prompt,
                &current_issue,
                cancel_flag,
                on_update,
                &tool_executor,
                &request.config,
            ) {
                Ok(_) => {
                    match continue_with_issue(tracker.as_ref(), &request.config, &current_issue) {
                        Ok(Some(refreshed_issue)) if turn_number < max_turns => {
                            current_issue = refreshed_issue;
                            None
                        }
                        Ok(Some(_)) => Some(RunnerExitStatus::Completed),
                        Ok(None) => Some(RunnerExitStatus::Completed),
                        Err(error) => Some(RunnerExitStatus::Failed(error.to_string())),
                    }
                }
                Err(AppServerError::TurnCancelled(message)) => {
                    Some(RunnerExitStatus::Cancelled(message))
                }
                Err(AppServerError::TurnInputRequired(message)) => {
                    Some(RunnerExitStatus::Failed(message))
                }
                Err(error) => Some(RunnerExitStatus::Failed(error.to_string())),
            }
        });

        let _ = session.stop();
        result.unwrap_or(RunnerExitStatus::Completed)
    }
}

fn continue_with_issue(
    tracker: &dyn IssueTrackerClient,
    config: &ServiceConfig,
    issue: &Issue,
) -> Result<Option<Issue>, RunnerError> {
    let refreshed = tracker
        .fetch_issue_states_by_ids(config, std::slice::from_ref(&issue.id))
        .map_err(|error| RunnerError::new(format!("issue_state_refresh_failed: {error}")))?;

    match refreshed.into_iter().next() {
        Some(issue) if active_issue_state(config, &issue.state) => Ok(Some(issue)),
        Some(_) | None => Ok(None),
    }
}

fn active_issue_state(config: &ServiceConfig, state_name: &str) -> bool {
    config
        .normalized_active_states()
        .contains(&crate::domain::normalize_state(state_name))
}

fn continuation_prompt(turn_number: u32, max_turns: u32) -> String {
    format!(
        "Continuation guidance:\n\n- The previous Codex turn completed normally, but the issue is still active.\n- This is continuation turn #{turn_number} of {max_turns} for the current agent run.\n- Resume from the current workspace state instead of restarting.\n- The original task instructions and prior turn context are already in the thread.\n- Focus on the remaining ticket work and do not stop while the issue remains active unless blocked."
    )
}

fn execute_dynamic_tool(
    tracker: &dyn IssueTrackerClient,
    config: &ServiceConfig,
    tool_name: Option<&str>,
    arguments: JsonValue,
) -> JsonValue {
    match tool_name {
        Some("linear_graphql") => execute_linear_graphql(tracker, config, arguments),
        other => dynamic_tool_response(
            false,
            json!({
                "error": {
                    "message": format!("Unsupported dynamic tool: {:?}", other),
                    "supportedTools": ["linear_graphql"]
                }
            }),
        ),
    }
}

fn execute_linear_graphql(
    tracker: &dyn IssueTrackerClient,
    config: &ServiceConfig,
    arguments: JsonValue,
) -> JsonValue {
    let (query, variables) = match normalize_linear_graphql_arguments(arguments) {
        Ok(values) => values,
        Err(message) => {
            return dynamic_tool_response(false, json!({ "error": { "message": message } }));
        }
    };

    match tracker.graphql(config, &query, variables) {
        Ok(response) => {
            let success = response
                .get("errors")
                .and_then(JsonValue::as_array)
                .is_none_or(|errors| errors.is_empty());
            dynamic_tool_response(success, response)
        }
        Err(error) => dynamic_tool_response(
            false,
            json!({
                "error": {
                    "message": "Linear GraphQL tool execution failed.",
                    "reason": error.to_string()
                }
            }),
        ),
    }
}

fn normalize_linear_graphql_arguments(arguments: JsonValue) -> Result<(String, JsonValue), String> {
    match arguments {
        JsonValue::String(query) => {
            let trimmed = query.trim().to_string();
            if trimmed.is_empty() {
                Err("`linear_graphql` requires a non-empty `query` string.".to_string())
            } else {
                Ok((trimmed, json!({})))
            }
        }
        JsonValue::Object(map) => {
            let query = map
                .get("query")
                .and_then(JsonValue::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "`linear_graphql` requires a non-empty `query` string.".to_string())?;
            let variables = map.get("variables").cloned().unwrap_or_else(|| json!({}));
            if !variables.is_object() && !variables.is_null() {
                return Err("`linear_graphql.variables` must be a JSON object.".to_string());
            }
            Ok((
                query.to_string(),
                if variables.is_null() {
                    json!({})
                } else {
                    variables
                },
            ))
        }
        _ => Err(
            "`linear_graphql` expects a query string or an object with `query` and optional `variables`."
                .to_string(),
        ),
    }
}

fn dynamic_tool_response(success: bool, payload: JsonValue) -> JsonValue {
    let output = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    json!({
        "success": success,
        "output": output,
        "contentItems": [{
            "type": "inputText",
            "text": output
        }]
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerError {
    message: String,
}

impl RunnerError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RunnerError {}
