use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use chrono::Utc;
use serde::Serialize;

use crate::config::ServiceConfig;
use crate::domain::{AgentUpdate, Issue, OrchestratorRuntimeState};
use crate::orchestrator::Orchestrator;
use crate::runner::{DefaultAgentRunner, RunRequest, RunnerExitStatus, WorkerRuntimeInfo};
use crate::tracker::IssueTrackerClient;
use crate::workflow::WorkflowLoader;
use crate::workspace::WorkspaceManager;

pub struct SymphonyService {
    workflow_path: PathBuf,
    workflow: crate::domain::WorkflowDefinition,
    config: ServiceConfig,
    orchestrator: Orchestrator,
    tracker: Arc<dyn IssueTrackerClient>,
    runner: DefaultAgentRunner,
    workspace_manager: WorkspaceManager,
    event_tx: Sender<ServiceEvent>,
    event_rx: Receiver<ServiceEvent>,
    running_controls: HashMap<String, RunningControl>,
    externally_cancelled: HashSet<String>,
    last_workflow_mtime: Option<SystemTime>,
    startup_cleanup_done: bool,
}

struct RunningControl {
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

#[derive(Debug)]
enum ServiceEvent {
    RuntimeInfo {
        issue_id: String,
        info: WorkerRuntimeInfo,
    },
    Update {
        issue_id: String,
        update: AgentUpdate,
    },
    Exit {
        issue_id: String,
        status: RunnerExitStatus,
    },
}

#[derive(Debug, Serialize)]
pub struct ServiceSnapshot {
    pub workflow_path: String,
    pub dispatch_ready: bool,
    pub validation_errors: Vec<String>,
    pub state: OrchestratorRuntimeState,
}

impl SymphonyService {
    pub fn from_workflow_path(
        workflow_path: impl AsRef<Path>,
        tracker: Arc<dyn IssueTrackerClient>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let workflow_path = workflow_path.as_ref().to_path_buf();
        let workflow = WorkflowLoader::from_path(&workflow_path)?;
        let config = ServiceConfig::from_workflow_definition(&workflow)?;
        let orchestrator = Orchestrator::new(config.clone());
        let workspace_manager = WorkspaceManager::new(config.clone());
        let runner = DefaultAgentRunner::new(tracker.clone());
        let (event_tx, event_rx) = mpsc::channel();
        let last_workflow_mtime = std::fs::metadata(&workflow_path)
            .ok()
            .and_then(|meta| meta.modified().ok());

        Ok(Self {
            workflow_path,
            workflow,
            config,
            orchestrator,
            tracker,
            runner,
            workspace_manager,
            event_tx,
            event_rx,
            running_controls: HashMap::new(),
            externally_cancelled: HashSet::new(),
            last_workflow_mtime,
            startup_cleanup_done: false,
        })
    }

    pub fn run_single_cycle(&mut self) {
        self.drain_worker_events();
        self.reload_if_changed();

        if !self.startup_cleanup_done {
            self.run_startup_terminal_cleanup();
            self.startup_cleanup_done = true;
        }

        let now_ms = current_time_ms();
        self.cancel_stalled_runs();
        self.reconcile_running_issues();
        self.process_due_retries(now_ms);
        self.dispatch_new_work();
    }

    pub fn run_forever(&mut self) -> ! {
        loop {
            self.run_single_cycle();
            thread::sleep(Duration::from_millis(self.config.polling.interval_ms));
        }
    }

    pub fn snapshot(&self) -> ServiceSnapshot {
        ServiceSnapshot {
            workflow_path: self.workflow_path.display().to_string(),
            dispatch_ready: self.config.dispatch_ready(),
            validation_errors: self
                .config
                .validate_for_dispatch()
                .into_iter()
                .map(|error| error.to_string())
                .collect(),
            state: self.orchestrator.state.clone(),
        }
    }

    pub fn snapshot_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.snapshot())
    }

    fn reload_if_changed(&mut self) {
        let Ok(metadata) = std::fs::metadata(&self.workflow_path) else {
            return;
        };
        let Ok(modified) = metadata.modified() else {
            return;
        };
        if self
            .last_workflow_mtime
            .is_some_and(|mtime| mtime >= modified)
        {
            return;
        }

        match WorkflowLoader::from_path(&self.workflow_path) {
            Ok(workflow) => match ServiceConfig::from_workflow_definition(&workflow) {
                Ok(config) => {
                    self.workflow = workflow;
                    self.workspace_manager = WorkspaceManager::new(config.clone());
                    self.orchestrator.refresh_runtime_config(config.clone());
                    self.config = config;
                    self.last_workflow_mtime = Some(modified);
                }
                Err(error) => {
                    eprintln!("workflow reload failed; keeping last known good config: {error}");
                }
            },
            Err(error) => {
                eprintln!("workflow reload failed; keeping last known good config: {error}");
            }
        }
    }

    fn run_startup_terminal_cleanup(&self) {
        if let Ok(issues) = self
            .tracker
            .fetch_issues_by_states(&self.config, &self.config.tracker.terminal_states)
        {
            for issue in issues {
                let _ = self
                    .workspace_manager
                    .remove_issue_workspaces(&issue.identifier, None);
            }
        }
    }

    fn drain_worker_events(&mut self) {
        loop {
            match self.event_rx.try_recv() {
                Ok(ServiceEvent::RuntimeInfo { issue_id, info }) => {
                    self.orchestrator.update_runtime_info(
                        &issue_id,
                        info.worker_host,
                        info.workspace_path,
                    );
                }
                Ok(ServiceEvent::Update { issue_id, update }) => {
                    self.orchestrator.integrate_update(&issue_id, &update);
                }
                Ok(ServiceEvent::Exit { issue_id, status }) => {
                    if let Some(control) = self.running_controls.remove(&issue_id) {
                        let _ = control.handle.join();
                    }

                    if self.externally_cancelled.remove(&issue_id) {
                        continue;
                    }

                    let Some(entry) = self.orchestrator.take_running_entry(&issue_id) else {
                        continue;
                    };
                    self.orchestrator.record_runtime_seconds(&entry);

                    match status {
                        RunnerExitStatus::Completed => {
                            self.orchestrator.record_completion(&issue_id);
                            self.orchestrator.schedule_continuation_retry(
                                &entry.issue,
                                current_time_ms(),
                                entry.worker_host.clone(),
                                entry.workspace_path.clone(),
                            );
                        }
                        RunnerExitStatus::Cancelled(_) => {
                            self.orchestrator.release_claim(&issue_id);
                        }
                        RunnerExitStatus::Failed(error) => {
                            let next_attempt = entry.attempt.unwrap_or(0) + 1;
                            self.orchestrator.schedule_failure_retry(
                                &entry.issue,
                                next_attempt,
                                current_time_ms(),
                                Some(error),
                                entry.worker_host.clone(),
                                entry.workspace_path.clone(),
                            );
                        }
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn cancel_stalled_runs(&mut self) {
        if self.config.codex.stall_timeout_ms <= 0 {
            return;
        }

        let timeout_ms = self.config.codex.stall_timeout_ms;
        let running_ids: Vec<String> = self.orchestrator.running_issue_ids();
        for issue_id in running_ids {
            let Some(entry) = self.orchestrator.running_entry(&issue_id).cloned() else {
                continue;
            };
            let last_activity = entry
                .live_session
                .last_codex_timestamp
                .unwrap_or(entry.started_at);
            let elapsed_ms = Utc::now()
                .signed_duration_since(last_activity)
                .num_milliseconds();
            if elapsed_ms > timeout_ms {
                self.cancel_running_issue(
                    &issue_id,
                    false,
                    Some(format!("stalled for {elapsed_ms}ms without codex activity")),
                );
            }
        }
    }

    fn reconcile_running_issues(&mut self) {
        let running_ids = self.orchestrator.running_issue_ids();
        if running_ids.is_empty() {
            return;
        }

        match self
            .tracker
            .fetch_issue_states_by_ids(&self.config, &running_ids)
        {
            Ok(issues) => {
                let visible_ids: HashSet<String> =
                    issues.iter().map(|issue| issue.id.clone()).collect();
                for issue in issues {
                    let is_terminal = self
                        .config
                        .normalized_terminal_states()
                        .contains(&issue.normalized_state());
                    let is_active = self
                        .config
                        .normalized_active_states()
                        .contains(&issue.normalized_state());

                    if is_terminal {
                        self.cancel_running_issue(&issue.id, true, None);
                    } else if !issue.assigned_to_worker || !is_active {
                        self.cancel_running_issue(&issue.id, false, None);
                    } else {
                        self.orchestrator.refresh_running_issue(issue);
                    }
                }

                for issue_id in running_ids {
                    if !visible_ids.contains(&issue_id) {
                        self.cancel_running_issue(&issue_id, false, None);
                    }
                }
            }
            Err(error) => {
                eprintln!("running issue reconciliation failed: {error}");
            }
        }
    }

    fn process_due_retries(&mut self, now_ms: u64) {
        let retry_entries = self.orchestrator.take_due_retries(now_ms);
        if retry_entries.is_empty() {
            return;
        }

        let candidates = self
            .tracker
            .fetch_candidate_issues(&self.config)
            .unwrap_or_default();
        let candidate_map: HashMap<String, Issue> = candidates
            .into_iter()
            .map(|issue| (issue.id.clone(), issue))
            .collect();

        for retry in retry_entries {
            self.orchestrator.release_claim(&retry.issue_id);

            let issue = candidate_map.get(&retry.issue_id).cloned().or_else(|| {
                self.tracker
                    .fetch_issue_states_by_ids(&self.config, std::slice::from_ref(&retry.issue_id))
                    .ok()
                    .and_then(|mut issues| issues.pop())
            });

            match issue {
                Some(issue)
                    if self
                        .config
                        .normalized_terminal_states()
                        .contains(&issue.normalized_state()) =>
                {
                    let _ = if let Some(path) = retry.workspace_path.as_ref() {
                        self.workspace_manager
                            .remove_path(path, retry.worker_host.as_deref())
                    } else {
                        self.workspace_manager.remove_issue_workspaces(
                            &issue.identifier,
                            retry.worker_host.as_deref(),
                        )
                    };
                    self.orchestrator.release_claim(&retry.issue_id);
                }
                Some(issue) if self.orchestrator.block_reason(&issue).is_none() => {
                    self.dispatch_issue(issue, Some(retry.attempt), retry.worker_host.clone());
                }
                Some(issue) if retryable_issue(&self.config, &issue) => {
                    self.orchestrator.schedule_failure_retry(
                        &issue,
                        retry.attempt + 1,
                        current_time_ms(),
                        Some("no available orchestrator slots".to_string()),
                        retry.worker_host.clone(),
                        retry.workspace_path.clone(),
                    );
                }
                Some(_) | None => {
                    self.orchestrator.release_claim(&retry.issue_id);
                }
            }
        }
    }

    fn dispatch_new_work(&mut self) {
        if !self.config.dispatch_ready() {
            return;
        }

        let issues = match self.tracker.fetch_candidate_issues(&self.config) {
            Ok(issues) => issues,
            Err(error) => {
                eprintln!("candidate issue fetch failed: {error}");
                return;
            }
        };

        for issue in self
            .orchestrator
            .select_dispatch_candidates(&issues)
            .into_iter()
            .cloned()
            .collect::<Vec<Issue>>()
        {
            self.dispatch_issue(issue, None, None);
        }
    }

    fn dispatch_issue(
        &mut self,
        issue: Issue,
        attempt: Option<u32>,
        preferred_worker_host: Option<String>,
    ) {
        let worker_host = match self.select_worker_host(preferred_worker_host.as_deref()) {
            Some(host) => host,
            None => {
                if let Some(attempt) = attempt {
                    self.orchestrator.schedule_failure_retry(
                        &issue,
                        attempt + 1,
                        current_time_ms(),
                        Some("no available worker capacity".to_string()),
                        preferred_worker_host,
                        None,
                    );
                }
                return;
            }
        };

        let issue_id = issue.id.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let request = RunRequest {
            issue: issue.clone(),
            attempt,
            worker_host: worker_host.clone(),
            config: self.config.clone(),
            workflow: self.workflow.clone(),
        };
        let runner = self.runner.clone();
        let sender = self.event_tx.clone();
        let cancel_for_thread = cancel.clone();
        let issue_id_for_thread = issue_id.clone();

        self.orchestrator
            .mark_running(issue, attempt, worker_host.clone(), Utc::now());

        let handle = thread::spawn(move || {
            let issue_id_runtime = issue_id_for_thread.clone();
            let mut runtime_callback = |info: WorkerRuntimeInfo| {
                let _ = sender.send(ServiceEvent::RuntimeInfo {
                    issue_id: issue_id_runtime.clone(),
                    info,
                });
            };

            let issue_id_update = issue_id_for_thread.clone();
            let mut update_callback = |update: AgentUpdate| {
                let _ = sender.send(ServiceEvent::Update {
                    issue_id: issue_id_update.clone(),
                    update,
                });
            };

            let status = runner.run_attempt(
                request,
                cancel_for_thread,
                &mut runtime_callback,
                &mut update_callback,
            );
            let _ = sender.send(ServiceEvent::Exit {
                issue_id: issue_id_for_thread,
                status,
            });
        });

        self.running_controls
            .insert(issue_id, RunningControl { cancel, handle });
    }

    fn cancel_running_issue(
        &mut self,
        issue_id: &str,
        cleanup_workspace: bool,
        retry_error: Option<String>,
    ) {
        if let Some(control) = self.running_controls.get(issue_id) {
            control.cancel.store(true, Ordering::Relaxed);
        }
        self.externally_cancelled.insert(issue_id.to_string());

        let Some(entry) = self.orchestrator.take_running_entry(issue_id) else {
            return;
        };
        self.orchestrator.record_runtime_seconds(&entry);

        if cleanup_workspace {
            let _ = if let Some(path) = entry.workspace_path.as_ref() {
                self.workspace_manager
                    .remove_path(path, entry.worker_host.as_deref())
            } else {
                self.workspace_manager
                    .remove_issue_workspaces(&entry.issue.identifier, entry.worker_host.as_deref())
            };
        }

        match retry_error {
            Some(error) => {
                let next_attempt = entry.attempt.unwrap_or(0) + 1;
                self.orchestrator.schedule_failure_retry(
                    &entry.issue,
                    next_attempt,
                    current_time_ms(),
                    Some(error),
                    entry.worker_host.clone(),
                    entry.workspace_path.clone(),
                );
            }
            None => self.orchestrator.release_claim(issue_id),
        }
    }

    fn select_worker_host(&self, preferred: Option<&str>) -> Option<Option<String>> {
        if self.config.worker.ssh_hosts.is_empty() {
            return Some(None);
        }

        let available_hosts: Vec<&String> = self
            .config
            .worker
            .ssh_hosts
            .iter()
            .filter(|host| self.worker_host_slots_available(host))
            .collect();
        if available_hosts.is_empty() {
            return None;
        }

        if let Some(preferred) = preferred {
            if available_hosts
                .iter()
                .any(|host| host.as_str() == preferred)
            {
                return Some(Some(preferred.to_string()));
            }
        }

        let host = available_hosts
            .into_iter()
            .min_by_key(|host| self.running_worker_host_count(host))
            .map(|host| Some(host.clone()));
        Some(host.flatten())
    }

    fn worker_host_slots_available(&self, worker_host: &str) -> bool {
        match self.config.worker.max_concurrent_agents_per_host {
            Some(limit) => self.running_worker_host_count(worker_host) < limit,
            None => true,
        }
    }

    fn running_worker_host_count(&self, worker_host: &str) -> usize {
        self.orchestrator
            .state
            .running
            .values()
            .filter(|entry| entry.worker_host.as_deref() == Some(worker_host))
            .count()
    }
}

fn retryable_issue(config: &ServiceConfig, issue: &Issue) -> bool {
    config
        .normalized_active_states()
        .contains(&issue.normalized_state())
        && !config
            .normalized_terminal_states()
            .contains(&issue.normalized_state())
        && issue.assigned_to_worker
        && !issue.has_non_terminal_blocker(&config.tracker.terminal_states)
}

fn current_time_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}
