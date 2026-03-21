use std::fmt;

use serde_json::Value as JsonValue;

use crate::config::ServiceConfig;
use crate::domain::Issue;

pub trait IssueTrackerClient: Send + Sync {
    fn fetch_candidate_issues(&self, config: &ServiceConfig) -> Result<Vec<Issue>, TrackerError>;
    fn fetch_issues_by_states(
        &self,
        config: &ServiceConfig,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError>;
    fn fetch_issue_states_by_ids(
        &self,
        config: &ServiceConfig,
        issue_ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError>;
    fn graphql(
        &self,
        config: &ServiceConfig,
        query: &str,
        variables: JsonValue,
    ) -> Result<JsonValue, TrackerError>;
    fn create_comment(
        &self,
        config: &ServiceConfig,
        issue_id: &str,
        body: &str,
    ) -> Result<(), TrackerError>;
    fn update_issue_state(
        &self,
        config: &ServiceConfig,
        issue_id: &str,
        state_name: &str,
    ) -> Result<(), TrackerError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerError {
    message: String,
}

impl TrackerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TrackerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TrackerError {}
