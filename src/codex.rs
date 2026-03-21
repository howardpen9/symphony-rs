use std::collections::VecDeque;
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Value as JsonValue, json};

use crate::config::{CodexRuntimeSettings, ServiceConfig};
use crate::domain::{AgentUpdate, Issue};
use crate::path_safety;
use crate::ssh;

const INITIALIZE_ID: u64 = 1;
const THREAD_START_ID: u64 = 2;
const TURN_START_ID: u64 = 3;

#[derive(Debug)]
pub struct AppSession {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<StreamEvent>,
    buffered_json: VecDeque<JsonValue>,
    thread_id: String,
    workspace: PathBuf,
    worker_host: Option<String>,
    runtime_settings: CodexRuntimeSettings,
    codex_app_server_pid: Option<String>,
}

#[derive(Debug)]
enum StreamEvent {
    Stdout(String),
    Stderr(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRunResult {
    pub session_id: String,
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug)]
pub enum AppServerError {
    Io(String),
    InvalidWorkspaceCwd(String),
    ResponseTimeout,
    TurnTimeout,
    PortExit(i32),
    ResponseError(String),
    TurnFailed(String),
    TurnCancelled(String),
    ApprovalRequired(String),
    TurnInputRequired(String),
    InvalidPayload(String),
}

impl fmt::Display for AppServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message)
            | Self::InvalidWorkspaceCwd(message)
            | Self::ResponseError(message)
            | Self::TurnFailed(message)
            | Self::TurnCancelled(message)
            | Self::ApprovalRequired(message)
            | Self::TurnInputRequired(message)
            | Self::InvalidPayload(message) => write!(f, "{message}"),
            Self::ResponseTimeout => write!(f, "response_timeout"),
            Self::TurnTimeout => write!(f, "turn_timeout"),
            Self::PortExit(status) => write!(f, "port_exit: {status}"),
        }
    }
}

impl std::error::Error for AppServerError {}

impl AppSession {
    pub fn start(
        workspace: PathBuf,
        worker_host: Option<String>,
        config: &ServiceConfig,
    ) -> Result<Self, AppServerError> {
        validate_workspace_cwd(&workspace, worker_host.as_deref(), config)?;

        let runtime_settings = config
            .codex_runtime_settings(Some(&workspace), worker_host.is_some())
            .map_err(|error| AppServerError::Io(error.to_string()))?;
        let mut child = start_process(&workspace, worker_host.as_deref(), &config.codex.command)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppServerError::Io("missing child stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppServerError::Io("missing child stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppServerError::Io("missing child stderr".to_string()))?;

        let (tx, rx) = mpsc::channel();
        spawn_stream_reader(stdout, tx.clone(), true);
        spawn_stream_reader(stderr, tx, false);

        let codex_app_server_pid = Some(child.id().to_string());
        let mut session = Self {
            child,
            stdin,
            messages: rx,
            buffered_json: VecDeque::new(),
            thread_id: String::new(),
            workspace,
            worker_host,
            runtime_settings,
            codex_app_server_pid,
        };

        session.initialize()?;
        Ok(session)
    }

    pub fn run_turn(
        &mut self,
        prompt: &str,
        issue: &Issue,
        cancel_flag: &AtomicBool,
        on_update: &mut dyn FnMut(AgentUpdate),
        tool_executor: &dyn Fn(Option<&str>, JsonValue) -> JsonValue,
        config: &ServiceConfig,
    ) -> Result<TurnRunResult, AppServerError> {
        let turn_id = self.start_turn(prompt, issue)?;
        let session_id = format!("{}-{}", self.thread_id, turn_id);

        emit_update(
            on_update,
            "session_started",
            &session_id,
            Some(&self.thread_id),
            Some(&turn_id),
            self.codex_app_server_pid.as_deref(),
            self.worker_host.as_deref(),
            Some(json!({
                "threadId": self.thread_id,
                "turnId": turn_id,
                "workspace": self.workspace,
            })),
        );

        let deadline = Instant::now() + Duration::from_millis(config.codex.turn_timeout_ms);
        loop {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = self.stop();
                return Err(AppServerError::TurnCancelled(
                    "turn_cancelled_by_reconciliation".to_string(),
                ));
            }

            let Some(message) = self.next_json_message(deadline)? else {
                return Err(AppServerError::TurnTimeout);
            };

            if let Some(method) = message.get("method").and_then(JsonValue::as_str) {
                match method {
                    "turn/completed" => {
                        emit_update(
                            on_update,
                            "turn_completed",
                            &session_id,
                            Some(&self.thread_id),
                            Some(&turn_id),
                            self.codex_app_server_pid.as_deref(),
                            self.worker_host.as_deref(),
                            Some(message.clone()),
                        );
                        return Ok(TurnRunResult {
                            session_id,
                            thread_id: self.thread_id.clone(),
                            turn_id,
                        });
                    }
                    "turn/failed" => {
                        emit_update(
                            on_update,
                            "turn_failed",
                            &session_id,
                            Some(&self.thread_id),
                            Some(&turn_id),
                            self.codex_app_server_pid.as_deref(),
                            self.worker_host.as_deref(),
                            Some(message.clone()),
                        );
                        return Err(AppServerError::TurnFailed(message.to_string()));
                    }
                    "turn/cancelled" => {
                        emit_update(
                            on_update,
                            "turn_cancelled",
                            &session_id,
                            Some(&self.thread_id),
                            Some(&turn_id),
                            self.codex_app_server_pid.as_deref(),
                            self.worker_host.as_deref(),
                            Some(message.clone()),
                        );
                        return Err(AppServerError::TurnCancelled(message.to_string()));
                    }
                    "item/tool/requestUserInput" => {
                        emit_update(
                            on_update,
                            "turn_input_required",
                            &session_id,
                            Some(&self.thread_id),
                            Some(&turn_id),
                            self.codex_app_server_pid.as_deref(),
                            self.worker_host.as_deref(),
                            Some(message.clone()),
                        );
                        return Err(AppServerError::TurnInputRequired(
                            "turn_input_required".to_string(),
                        ));
                    }
                    "item/tool/call" => {
                        let id = message
                            .get("id")
                            .and_then(JsonValue::as_u64)
                            .ok_or_else(|| AppServerError::InvalidPayload(message.to_string()))?;
                        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
                        let tool_name = tool_call_name(&params);
                        let arguments = tool_call_arguments(&params);
                        let result = tool_executor(tool_name.as_deref(), arguments);
                        self.send_message(json!({ "id": id, "result": result }))?;

                        let event = if result["success"].as_bool() == Some(true) {
                            "tool_call_completed"
                        } else {
                            "tool_call_failed"
                        };
                        emit_update(
                            on_update,
                            event,
                            &session_id,
                            Some(&self.thread_id),
                            Some(&turn_id),
                            self.codex_app_server_pid.as_deref(),
                            self.worker_host.as_deref(),
                            Some(message.clone()),
                        );
                    }
                    "item/commandExecution/requestApproval"
                    | "item/fileChange/requestApproval"
                    | "execCommandApproval"
                    | "applyPatchApproval" => {
                        self.handle_approval_request(
                            &message,
                            method,
                            &session_id,
                            &turn_id,
                            on_update,
                        )?;
                    }
                    _ if needs_input(method, &message) => {
                        emit_update(
                            on_update,
                            "turn_input_required",
                            &session_id,
                            Some(&self.thread_id),
                            Some(&turn_id),
                            self.codex_app_server_pid.as_deref(),
                            self.worker_host.as_deref(),
                            Some(message.clone()),
                        );
                        return Err(AppServerError::TurnInputRequired(
                            "turn_input_required".to_string(),
                        ));
                    }
                    _ => {
                        emit_update(
                            on_update,
                            "notification",
                            &session_id,
                            Some(&self.thread_id),
                            Some(&turn_id),
                            self.codex_app_server_pid.as_deref(),
                            self.worker_host.as_deref(),
                            Some(message.clone()),
                        );
                    }
                }
            }
        }
    }

    pub fn stop(&mut self) -> Result<(), AppServerError> {
        match self.child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => {
                self.child
                    .kill()
                    .map_err(|error| AppServerError::Io(format!("codex_stop_failed: {error}")))?;
                let _ = self.child.wait();
                Ok(())
            }
            Err(error) => Err(AppServerError::Io(format!("codex_stop_failed: {error}"))),
        }
    }

    fn initialize(&mut self) -> Result<(), AppServerError> {
        self.send_message(json!({
            "method": "initialize",
            "id": INITIALIZE_ID,
            "params": {
                "capabilities": { "experimentalApi": true },
                "clientInfo": {
                    "name": "symphony-rs",
                    "title": "Symphony Rust",
                    "version": "0.1.0"
                }
            }
        }))?;
        self.await_response(INITIALIZE_ID, Duration::from_millis(5_000))?;
        self.send_message(json!({ "method": "initialized", "params": {} }))?;

        self.send_message(json!({
            "method": "thread/start",
            "id": THREAD_START_ID,
            "params": {
                "approvalPolicy": self.runtime_settings.approval_policy,
                "sandbox": self.runtime_settings.thread_sandbox,
                "cwd": self.workspace,
                "dynamicTools": [{
                    "name": "linear_graphql",
                    "description": "Execute a raw GraphQL query or mutation against Linear using Symphony's configured auth.",
                    "inputSchema": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["query"],
                        "properties": {
                            "query": { "type": "string" },
                            "variables": {
                                "type": ["object", "null"],
                                "additionalProperties": true
                            }
                        }
                    }
                }]
            }
        }))?;

        let response = self.await_response(THREAD_START_ID, Duration::from_millis(5_000))?;
        self.thread_id = response["thread"]["id"]
            .as_str()
            .ok_or_else(|| AppServerError::InvalidPayload(response.to_string()))?
            .to_string();
        Ok(())
    }

    fn start_turn(&mut self, prompt: &str, issue: &Issue) -> Result<String, AppServerError> {
        self.send_message(json!({
            "method": "turn/start",
            "id": TURN_START_ID,
            "params": {
                "threadId": self.thread_id,
                "input": [{
                    "type": "text",
                    "text": prompt
                }],
                "cwd": self.workspace,
                "title": format!("{}: {}", issue.identifier, issue.title),
                "approvalPolicy": self.runtime_settings.approval_policy,
                "sandboxPolicy": self.runtime_settings.turn_sandbox_policy,
            }
        }))?;

        let response = self.await_response(TURN_START_ID, Duration::from_millis(5_000))?;
        response["turn"]["id"]
            .as_str()
            .map(ToString::to_string)
            .ok_or_else(|| AppServerError::InvalidPayload(response.to_string()))
    }

    fn handle_approval_request(
        &mut self,
        message: &JsonValue,
        method: &str,
        session_id: &str,
        turn_id: &str,
        on_update: &mut dyn FnMut(AgentUpdate),
    ) -> Result<(), AppServerError> {
        let id = message
            .get("id")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| AppServerError::InvalidPayload(message.to_string()))?;
        let auto_approve =
            self.runtime_settings.approval_policy == JsonValue::String("never".to_string());
        if !auto_approve {
            emit_update(
                on_update,
                "approval_required",
                session_id,
                Some(&self.thread_id),
                Some(turn_id),
                self.codex_app_server_pid.as_deref(),
                self.worker_host.as_deref(),
                Some(message.clone()),
            );
            return Err(AppServerError::ApprovalRequired(message.to_string()));
        }

        let decision = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                json!({ "decision": "acceptForSession" })
            }
            _ => json!({ "decision": "approved_for_session" }),
        };
        self.send_message(json!({ "id": id, "result": decision }))?;

        emit_update(
            on_update,
            "approval_auto_approved",
            session_id,
            Some(&self.thread_id),
            Some(turn_id),
            self.codex_app_server_pid.as_deref(),
            self.worker_host.as_deref(),
            Some(message.clone()),
        );
        Ok(())
    }

    fn send_message(&mut self, message: JsonValue) -> Result<(), AppServerError> {
        let mut line = serde_json::to_vec(&message)
            .map_err(|error| AppServerError::Io(format!("json_encode_failed: {error}")))?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .map_err(|error| AppServerError::Io(format!("codex_write_failed: {error}")))
    }

    fn await_response(
        &mut self,
        request_id: u64,
        timeout: Duration,
    ) -> Result<JsonValue, AppServerError> {
        let deadline = Instant::now() + timeout;
        loop {
            let Some(message) = self.next_json_message(deadline)? else {
                return Err(AppServerError::ResponseTimeout);
            };

            match message.get("id").and_then(JsonValue::as_u64) {
                Some(id) if id == request_id => {
                    if let Some(error) = message.get("error") {
                        return Err(AppServerError::ResponseError(error.to_string()));
                    }
                    if let Some(result) = message.get("result") {
                        return Ok(result.clone());
                    }
                    return Err(AppServerError::InvalidPayload(message.to_string()));
                }
                _ => self.buffered_json.push_back(message),
            }
        }
    }

    fn next_json_message(
        &mut self,
        deadline: Instant,
    ) -> Result<Option<JsonValue>, AppServerError> {
        if let Some(message) = self.buffered_json.pop_front() {
            return Ok(Some(message));
        }

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }

            let wait = (deadline - now).min(Duration::from_millis(200));
            match self.messages.recv_timeout(wait) {
                Ok(StreamEvent::Stdout(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(payload) = serde_json::from_str::<JsonValue>(trimmed) {
                        return Ok(Some(payload));
                    }
                }
                Ok(StreamEvent::Stderr(_line)) => {}
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(status) = self.child.try_wait().map_err(|error| {
                        AppServerError::Io(format!("codex_wait_failed: {error}"))
                    })? {
                        return Err(AppServerError::PortExit(status.code().unwrap_or(-1)));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AppServerError::Io("codex_stream_disconnected".to_string()));
                }
            }
        }
    }
}

fn validate_workspace_cwd(
    workspace: &Path,
    worker_host: Option<&str>,
    config: &ServiceConfig,
) -> Result<(), AppServerError> {
    match worker_host {
        Some(host) => {
            let value = workspace.to_string_lossy();
            if value.trim().is_empty() {
                return Err(AppServerError::InvalidWorkspaceCwd(format!(
                    "invalid_workspace_cwd: empty_remote_workspace {host}"
                )));
            }
            if value.contains('\n') || value.contains('\r') || value.contains('\0') {
                return Err(AppServerError::InvalidWorkspaceCwd(format!(
                    "invalid_workspace_cwd: invalid_remote_workspace {host} {value}"
                )));
            }
            Ok(())
        }
        None => {
            let canonical_workspace = path_safety::canonicalize(workspace)
                .map_err(|error| AppServerError::InvalidWorkspaceCwd(error.to_string()))?;
            let canonical_root = path_safety::canonicalize(&config.workspace_root_path())
                .map_err(|error| AppServerError::InvalidWorkspaceCwd(error.to_string()))?;

            if canonical_workspace == canonical_root {
                return Err(AppServerError::InvalidWorkspaceCwd(format!(
                    "invalid_workspace_cwd: workspace_root {}",
                    canonical_workspace.display()
                )));
            }

            let root_prefix = format!("{}/", canonical_root.to_string_lossy());
            let workspace_prefix = format!("{}/", canonical_workspace.to_string_lossy());
            if workspace_prefix.starts_with(&root_prefix) {
                Ok(())
            } else {
                Err(AppServerError::InvalidWorkspaceCwd(format!(
                    "invalid_workspace_cwd: outside_workspace_root {} {}",
                    canonical_workspace.display(),
                    canonical_root.display()
                )))
            }
        }
    }
}

fn start_process(
    workspace: &Path,
    worker_host: Option<&str>,
    command: &str,
) -> Result<Child, AppServerError> {
    match worker_host {
        Some(host) => {
            let remote_command = format!(
                "cd {} && exec {}",
                shell_escape(&workspace.to_string_lossy()),
                command
            );
            ssh::start_process(host, &remote_command)
                .map_err(|error| AppServerError::Io(error.to_string()))
        }
        None => std::process::Command::new("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| AppServerError::Io(format!("codex_spawn_failed: {error}"))),
    }
}

fn spawn_stream_reader<R: std::io::Read + Send + 'static>(
    stream: R,
    sender: Sender<StreamEvent>,
    stdout: bool,
) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            let _ = if stdout {
                sender.send(StreamEvent::Stdout(line))
            } else {
                sender.send(StreamEvent::Stderr(line))
            };
        }
    });
}

fn emit_update(
    on_update: &mut dyn FnMut(AgentUpdate),
    event: &str,
    session_id: &str,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
    codex_app_server_pid: Option<&str>,
    worker_host: Option<&str>,
    payload: Option<JsonValue>,
) {
    let usage = payload.as_ref().and_then(extract_usage).or_else(|| {
        payload
            .as_ref()
            .and_then(|value| value.get("usage").cloned())
    });
    let rate_limits = payload.as_ref().and_then(extract_rate_limits);
    let last_message = payload.as_ref().map(summarize_payload);

    on_update(AgentUpdate {
        event: event.to_string(),
        timestamp: Utc::now(),
        session_id: Some(session_id.to_string()),
        thread_id: thread_id.map(ToString::to_string),
        turn_id: turn_id.map(ToString::to_string),
        codex_app_server_pid: codex_app_server_pid.map(ToString::to_string),
        worker_host: worker_host.map(ToString::to_string),
        last_message,
        usage,
        rate_limits,
        payload,
    });
}

fn tool_call_name(params: &JsonValue) -> Option<String> {
    [
        params.get("tool"),
        params.get("name"),
        params.get("toolName"),
        params.pointer("/tool/name"),
    ]
    .into_iter()
    .flatten()
    .find_map(JsonValue::as_str)
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(ToString::to_string)
}

fn tool_call_arguments(params: &JsonValue) -> JsonValue {
    params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn needs_input(method: &str, payload: &JsonValue) -> bool {
    matches!(
        method,
        "turn/input_required"
            | "turn/needs_input"
            | "turn/request_input"
            | "turn/provide_input"
            | "turn/approval_required"
    ) || payload
        .pointer("/params/requiresInput")
        .and_then(JsonValue::as_bool)
        == Some(true)
        || payload
            .pointer("/params/inputRequired")
            .and_then(JsonValue::as_bool)
            == Some(true)
}

fn extract_usage(payload: &JsonValue) -> Option<JsonValue> {
    payload
        .pointer("/params/msg/payload/info/total_token_usage")
        .cloned()
        .or_else(|| {
            payload
                .pointer("/params/msg/info/total_token_usage")
                .cloned()
        })
        .or_else(|| payload.pointer("/params/usage").cloned())
        .or_else(|| payload.get("usage").cloned())
        .or_else(|| payload.pointer("/tokenUsage/total").cloned())
}

fn extract_rate_limits(payload: &JsonValue) -> Option<JsonValue> {
    if payload.get("limit_id").is_some() {
        return Some(payload.clone());
    }

    match payload {
        JsonValue::Object(map) => map.values().find_map(extract_rate_limits),
        JsonValue::Array(values) => values.iter().find_map(extract_rate_limits),
        _ => None,
    }
}

fn summarize_payload(payload: &JsonValue) -> String {
    payload
        .get("method")
        .and_then(JsonValue::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| payload.to_string())
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
