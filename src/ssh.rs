use std::env;
use std::fmt;
use std::process::{Child, Command, Output, Stdio};

#[derive(Debug)]
pub struct SshError {
    message: String,
}

impl SshError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SshError {}

pub fn run(host: &str, command: &str) -> Result<Output, SshError> {
    let ssh = ssh_executable()?;
    Command::new(ssh)
        .args(ssh_args(host, command))
        .output()
        .map_err(|error| SshError::new(format!("ssh_run_failed: {error}")))
}

pub fn start_process(host: &str, command: &str) -> Result<Child, SshError> {
    let ssh = ssh_executable()?;
    Command::new(ssh)
        .args(ssh_args(host, command))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| SshError::new(format!("ssh_spawn_failed: {error}")))
}

pub fn remote_shell_command(command: &str) -> String {
    format!("bash -lc {}", shell_escape(command))
}

fn ssh_executable() -> Result<String, SshError> {
    Ok("ssh".to_string())
}

fn ssh_args(host: &str, command: &str) -> Vec<String> {
    let target = parse_target(host);
    let mut args = Vec::new();

    if let Ok(config_path) = env::var("SYMPHONY_SSH_CONFIG") {
        if !config_path.trim().is_empty() {
            args.push("-F".to_string());
            args.push(config_path);
        }
    }

    args.push("-T".to_string());
    if let Some(port) = target.port {
        args.push("-p".to_string());
        args.push(port);
    }
    args.push(target.destination);
    args.push(remote_shell_command(command));
    args
}

fn parse_target(target: &str) -> ParsedTarget {
    let trimmed = target.trim();
    if let Some((destination, port)) = trimmed.rsplit_once(':') {
        if !destination.is_empty()
            && port.chars().all(|ch| ch.is_ascii_digit())
            && (!destination.contains(':')
                || (destination.starts_with('[') && destination.ends_with(']')))
        {
            return ParsedTarget {
                destination: destination.to_string(),
                port: Some(port.to_string()),
            };
        }
    }

    ParsedTarget {
        destination: trimmed.to_string(),
        port: None,
    }
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

struct ParsedTarget {
    destination: String,
    port: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::remote_shell_command;

    #[test]
    fn wraps_remote_command_in_bash() {
        assert!(remote_shell_command("echo hi").starts_with("bash -lc "));
    }
}
