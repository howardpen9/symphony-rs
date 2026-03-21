use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::ServiceConfig;
use crate::domain::Issue;
use crate::path_safety;
use crate::ssh;

const REMOTE_WORKSPACE_MARKER: &str = "__SYMPHONY_WORKSPACE__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceAssignment {
    pub path: PathBuf,
    pub workspace_key: String,
    pub created_now: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceManager {
    config: ServiceConfig,
}

impl WorkspaceManager {
    pub fn new(config: ServiceConfig) -> Self {
        Self { config }
    }

    pub fn workspace_path_for_issue(
        &self,
        issue_identifier: &str,
        _worker_host: Option<&str>,
    ) -> Result<PathBuf, WorkspaceError> {
        let safe_id = sanitize_workspace_key(issue_identifier);
        let path = PathBuf::from(&self.config.workspace.root).join(safe_id);
        Ok(path)
    }

    pub fn create_for_issue(
        &self,
        issue: &Issue,
        worker_host: Option<&str>,
    ) -> Result<WorkspaceAssignment, WorkspaceError> {
        let path = self.workspace_path_for_issue(&issue.identifier, worker_host)?;
        self.validate_workspace_path(&path, worker_host)?;
        let assignment = self.ensure_workspace(&path, &issue.identifier, worker_host)?;

        if assignment.created_now {
            if let Some(command) = &self.config.hooks.after_create {
                self.run_hook(
                    command,
                    &assignment.path,
                    issue,
                    "after_create",
                    worker_host,
                    true,
                )?;
            }
        }

        Ok(assignment)
    }

    pub fn run_before_run_hook(
        &self,
        workspace: &Path,
        issue: &Issue,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        if let Some(command) = &self.config.hooks.before_run {
            self.run_hook(command, workspace, issue, "before_run", worker_host, true)
        } else {
            Ok(())
        }
    }

    pub fn run_after_run_hook(&self, workspace: &Path, issue: &Issue, worker_host: Option<&str>) {
        if let Some(command) = &self.config.hooks.after_run {
            let _ = self.run_hook(command, workspace, issue, "after_run", worker_host, false);
        }
    }

    pub fn remove_path(
        &self,
        workspace: &Path,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        if let Some(command) = &self.config.hooks.before_remove {
            let issue = Issue {
                identifier: workspace
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| "issue".to_string()),
                ..Issue::example()
            };
            let _ = self.run_hook(
                command,
                workspace,
                &issue,
                "before_remove",
                worker_host,
                false,
            );
        }

        match worker_host {
            Some(host) => {
                let script = format!(
                    "{}\nrm -rf \"$workspace\"",
                    remote_shell_assign("workspace", &workspace.to_string_lossy())
                );
                let output = ssh::run(host, &script).map_err(|error| {
                    WorkspaceError::new(format!("workspace_remove_failed: {error}"))
                })?;
                if !output.status.success() {
                    return Err(WorkspaceError::new(format!(
                        "workspace_remove_failed: status={} output={}",
                        output.status.code().unwrap_or(-1),
                        String::from_utf8_lossy(&output.stdout)
                    )));
                }
                Ok(())
            }
            None => {
                if workspace.exists() {
                    fs::remove_dir_all(workspace).map_err(|error| {
                        WorkspaceError::new(format!("workspace_remove_failed: {error}"))
                    })?;
                }
                Ok(())
            }
        }
    }

    pub fn remove_issue_workspaces(
        &self,
        identifier: &str,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        let workspace = self.workspace_path_for_issue(identifier, worker_host)?;
        self.remove_path(&workspace, worker_host)
    }

    fn ensure_workspace(
        &self,
        workspace: &Path,
        issue_identifier: &str,
        worker_host: Option<&str>,
    ) -> Result<WorkspaceAssignment, WorkspaceError> {
        match worker_host {
            Some(host) => self.ensure_remote_workspace(workspace, issue_identifier, host),
            None => self.ensure_local_workspace(workspace, issue_identifier),
        }
    }

    fn ensure_local_workspace(
        &self,
        workspace: &Path,
        issue_identifier: &str,
    ) -> Result<WorkspaceAssignment, WorkspaceError> {
        let created_now = if workspace.is_dir() {
            false
        } else {
            if workspace.exists() {
                fs::remove_file(workspace)
                    .or_else(|_| fs::remove_dir_all(workspace))
                    .ok();
            }
            fs::create_dir_all(workspace).map_err(|error| {
                WorkspaceError::new(format!("workspace_prepare_failed: {error}"))
            })?;
            true
        };

        Ok(WorkspaceAssignment {
            path: workspace.to_path_buf(),
            workspace_key: sanitize_workspace_key(issue_identifier),
            created_now,
        })
    }

    fn ensure_remote_workspace(
        &self,
        workspace: &Path,
        issue_identifier: &str,
        worker_host: &str,
    ) -> Result<WorkspaceAssignment, WorkspaceError> {
        let script = [
            "set -eu".to_string(),
            remote_shell_assign("workspace", &workspace.to_string_lossy()),
            "if [ -d \"$workspace\" ]; then".to_string(),
            "  created=0".to_string(),
            "elif [ -e \"$workspace\" ]; then".to_string(),
            "  rm -rf \"$workspace\"".to_string(),
            "  mkdir -p \"$workspace\"".to_string(),
            "  created=1".to_string(),
            "else".to_string(),
            "  mkdir -p \"$workspace\"".to_string(),
            "  created=1".to_string(),
            "fi".to_string(),
            "cd \"$workspace\"".to_string(),
            format!(
                "printf '%s\\t%s\\t%s\\n' '{}' \"$created\" \"$(pwd -P)\"",
                REMOTE_WORKSPACE_MARKER
            ),
        ]
        .join("\n");

        let output = ssh::run(worker_host, &script)
            .map_err(|error| WorkspaceError::new(format!("workspace_prepare_failed: {error}")))?;

        if !output.status.success() {
            return Err(WorkspaceError::new(format!(
                "workspace_prepare_failed: status={} output={}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stdout)
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let (created_now, path) = parse_remote_workspace_output(&stdout)?;
        Ok(WorkspaceAssignment {
            path,
            workspace_key: sanitize_workspace_key(issue_identifier),
            created_now,
        })
    }

    fn validate_workspace_path(
        &self,
        workspace: &Path,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        match worker_host {
            Some(_host) => {
                let value = workspace.to_string_lossy();
                if value.trim().is_empty() {
                    return Err(WorkspaceError::new("workspace_path_unreadable: empty"));
                }
                if value.contains('\n') || value.contains('\r') || value.contains('\0') {
                    return Err(WorkspaceError::new(
                        "workspace_path_unreadable: invalid characters",
                    ));
                }
                Ok(())
            }
            None => {
                let expanded_workspace = path_safety::canonicalize(workspace).map_err(|error| {
                    WorkspaceError::new(format!("workspace_path_unreadable: {error}"))
                })?;
                let expanded_root =
                    path_safety::canonicalize(Path::new(&self.config.workspace.root)).map_err(
                        |error| WorkspaceError::new(format!("workspace_path_unreadable: {error}")),
                    )?;

                if expanded_workspace == expanded_root {
                    return Err(WorkspaceError::new("workspace_equals_root"));
                }

                let root_prefix = format!("{}/", expanded_root.to_string_lossy());
                let workspace_prefix = format!("{}/", expanded_workspace.to_string_lossy());

                if workspace_prefix.starts_with(&root_prefix) {
                    Ok(())
                } else {
                    Err(WorkspaceError::new("workspace_outside_root"))
                }
            }
        }
    }

    fn run_hook(
        &self,
        command: &str,
        workspace: &Path,
        issue: &Issue,
        hook_name: &str,
        worker_host: Option<&str>,
        fatal: bool,
    ) -> Result<(), WorkspaceError> {
        match worker_host {
            Some(host) => self.run_remote_hook(command, workspace, issue, hook_name, host, fatal),
            None => self.run_local_hook(command, workspace, issue, hook_name, fatal),
        }
    }

    fn run_local_hook(
        &self,
        command: &str,
        workspace: &Path,
        issue: &Issue,
        hook_name: &str,
        fatal: bool,
    ) -> Result<(), WorkspaceError> {
        let output = Command::new("sh")
            .arg("-lc")
            .arg(command)
            .current_dir(workspace)
            .env("SYMPHONY_ISSUE_ID", &issue.id)
            .env("SYMPHONY_ISSUE_IDENTIFIER", &issue.identifier)
            .output()
            .map_err(|error| WorkspaceError::new(format!("workspace_hook_failed: {error}")))?;

        self.handle_hook_result(output, workspace, hook_name, fatal)
    }

    fn run_remote_hook(
        &self,
        command: &str,
        workspace: &Path,
        issue: &Issue,
        hook_name: &str,
        worker_host: &str,
        fatal: bool,
    ) -> Result<(), WorkspaceError> {
        let script = format!(
            "{}\ncd \"$workspace\"\nexport SYMPHONY_ISSUE_ID={}\nexport SYMPHONY_ISSUE_IDENTIFIER={}\n{}",
            remote_shell_assign("workspace", &workspace.to_string_lossy()),
            shell_escape(&issue.id),
            shell_escape(&issue.identifier),
            command
        );

        let output = ssh::run(worker_host, &script)
            .map_err(|error| WorkspaceError::new(format!("workspace_hook_failed: {error}")))?;

        self.handle_hook_result(output, workspace, hook_name, fatal)
    }

    fn handle_hook_result(
        &self,
        output: std::process::Output,
        workspace: &Path,
        hook_name: &str,
        fatal: bool,
    ) -> Result<(), WorkspaceError> {
        if output.status.success() {
            return Ok(());
        }

        let error = WorkspaceError::new(format!(
            "workspace_hook_failed: hook={} workspace={} status={} output={}",
            hook_name,
            workspace.display(),
            output.status.code().unwrap_or(-1),
            sanitize_output(&output.stdout)
        ));

        if fatal { Err(error) } else { Ok(()) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceError {
    message: String,
}

impl WorkspaceError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WorkspaceError {}

impl From<io::Error> for WorkspaceError {
    fn from(error: io::Error) -> Self {
        WorkspaceError::new(error.to_string())
    }
}

pub fn sanitize_workspace_key(identifier: &str) -> String {
    let sanitized: String = identifier
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}

fn remote_shell_assign(variable_name: &str, raw_path: &str) -> String {
    [
        format!("{variable_name}={}", shell_escape(raw_path)),
        format!("case \"$${variable_name}\" in"),
        format!("  '~') {variable_name}=\"$HOME\" ;;"),
        format!("  '~/'*) {variable_name}=\"$HOME/${{{variable_name}#~/}}\" ;;"),
        "esac".to_string(),
    ]
    .join("\n")
}

fn parse_remote_workspace_output(output: &str) -> Result<(bool, PathBuf), WorkspaceError> {
    for line in output.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() == 3 && parts[0] == REMOTE_WORKSPACE_MARKER {
            return Ok((parts[1] == "1", PathBuf::from(parts[2])));
        }
    }

    Err(WorkspaceError::new(format!(
        "workspace_prepare_failed: invalid output {output}"
    )))
}

fn sanitize_output(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    if text.len() > 2_048 {
        format!("{}... (truncated)", &text[..2_048])
    } else {
        text.to_string()
    }
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::sanitize_workspace_key;

    #[test]
    fn sanitizes_workspace_key_to_spec_charset() {
        assert_eq!(sanitize_workspace_key("ABC-123/fix bug"), "ABC-123_fix_bug");
    }
}
