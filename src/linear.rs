use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use reqwest::blocking::Client;
use serde_json::{Value as JsonValue, json};

use crate::config::ServiceConfig;
use crate::domain::{Issue, IssueBlockerRef};
use crate::tracker::{IssueTrackerClient, TrackerError};

const ISSUE_PAGE_SIZE: usize = 50;
const QUERY_BY_STATES: &str = r#"
query SymphonyLinearPoll($projectSlug: String!, $stateNames: [String!]!, $first: Int!, $relationFirst: Int!, $after: String) {
  issues(filter: {project: {slugId: {eq: $projectSlug}}, state: {name: {in: $stateNames}}}, first: $first, after: $after) {
    nodes {
      id
      identifier
      title
      description
      priority
      state { name }
      branchName
      url
      assignee { id }
      labels { nodes { name } }
      inverseRelations(first: $relationFirst) {
        nodes {
          type
          issue { id identifier state { name } }
        }
      }
      createdAt
      updatedAt
    }
    pageInfo { hasNextPage endCursor }
  }
}
"#;

const QUERY_BY_IDS: &str = r#"
query SymphonyLinearIssuesById($ids: [ID!]!, $first: Int!, $relationFirst: Int!) {
  issues(filter: {id: {in: $ids}}, first: $first) {
    nodes {
      id
      identifier
      title
      description
      priority
      state { name }
      branchName
      url
      assignee { id }
      labels { nodes { name } }
      inverseRelations(first: $relationFirst) {
        nodes {
          type
          issue { id identifier state { name } }
        }
      }
      createdAt
      updatedAt
    }
  }
}
"#;

const VIEWER_QUERY: &str = r#"
query SymphonyLinearViewer {
  viewer { id }
}
"#;

const CREATE_COMMENT_MUTATION: &str = r#"
mutation SymphonyCreateComment($issueId: String!, $body: String!) {
  commentCreate(input: {issueId: $issueId, body: $body}) { success }
}
"#;

const UPDATE_STATE_MUTATION: &str = r#"
mutation SymphonyUpdateIssueState($issueId: String!, $stateId: String!) {
  issueUpdate(id: $issueId, input: {stateId: $stateId}) { success }
}
"#;

const STATE_LOOKUP_QUERY: &str = r#"
query SymphonyResolveStateId($issueId: String!, $stateName: String!) {
  issue(id: $issueId) {
    team {
      states(filter: {name: {eq: $stateName}}, first: 1) {
        nodes { id }
      }
    }
  }
}
"#;

#[derive(Debug, Clone)]
pub struct LinearTrackerClient {
    http: Client,
}

impl Default for LinearTrackerClient {
    fn default() -> Self {
        Self {
            http: Client::builder()
                .build()
                .expect("reqwest blocking client should build"),
        }
    }
}

impl LinearTrackerClient {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IssueTrackerClient for LinearTrackerClient {
    fn fetch_candidate_issues(&self, config: &ServiceConfig) -> Result<Vec<Issue>, TrackerError> {
        let project_slug = config
            .tracker
            .project_slug
            .as_ref()
            .ok_or_else(|| TrackerError::new("missing_linear_project_slug"))?;
        let assignee_filter = resolve_assignee_filter(self, config)?;
        self.fetch_by_states(
            config,
            project_slug,
            &config.tracker.active_states,
            assignee_filter,
        )
    }

    fn fetch_issues_by_states(
        &self,
        config: &ServiceConfig,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let project_slug = config
            .tracker
            .project_slug
            .as_ref()
            .ok_or_else(|| TrackerError::new("missing_linear_project_slug"))?;
        self.fetch_by_states(config, project_slug, states, None)
    }

    fn fetch_issue_states_by_ids(
        &self,
        config: &ServiceConfig,
        issue_ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let ids: Vec<String> = issue_ids
            .iter()
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let assignee_filter = resolve_assignee_filter(self, config)?;
        let issue_order_index: BTreeMap<String, usize> = ids
            .iter()
            .enumerate()
            .map(|(index, value)| (value.clone(), index))
            .collect();

        let mut issues = Vec::new();
        for chunk in ids.chunks(ISSUE_PAGE_SIZE) {
            let body = self.graphql(
                config,
                QUERY_BY_IDS,
                json!({
                    "ids": chunk,
                    "first": chunk.len(),
                    "relationFirst": ISSUE_PAGE_SIZE,
                }),
            )?;
            issues.extend(decode_linear_response(&body, assignee_filter.as_ref())?);
        }

        issues.sort_by_key(|issue| {
            issue_order_index
                .get(&issue.id)
                .copied()
                .unwrap_or(usize::MAX)
        });
        Ok(issues)
    }

    fn graphql(
        &self,
        config: &ServiceConfig,
        query: &str,
        variables: JsonValue,
    ) -> Result<JsonValue, TrackerError> {
        let api_key = config
            .tracker
            .api_key
            .as_ref()
            .ok_or_else(|| TrackerError::new("missing_linear_api_token"))?;

        let response = self
            .http
            .post(&config.tracker.endpoint)
            .header("Authorization", api_key)
            .header("Content-Type", "application/json")
            .json(&json!({
                "query": query,
                "variables": variables
            }))
            .send()
            .map_err(|error| TrackerError::new(format!("linear_api_request: {error}")))?;

        let status = response.status();
        let body: JsonValue = response
            .json()
            .map_err(|error| TrackerError::new(format!("linear_api_decode_failed: {error}")))?;

        if !status.is_success() {
            return Err(TrackerError::new(format!(
                "linear_api_status: {} body={}",
                status.as_u16(),
                body
            )));
        }

        Ok(body)
    }

    fn create_comment(
        &self,
        config: &ServiceConfig,
        issue_id: &str,
        body: &str,
    ) -> Result<(), TrackerError> {
        let response = self.graphql(
            config,
            CREATE_COMMENT_MUTATION,
            json!({ "issueId": issue_id, "body": body }),
        )?;
        match response["data"]["commentCreate"]["success"].as_bool() {
            Some(true) => Ok(()),
            _ => Err(TrackerError::new("comment_create_failed")),
        }
    }

    fn update_issue_state(
        &self,
        config: &ServiceConfig,
        issue_id: &str,
        state_name: &str,
    ) -> Result<(), TrackerError> {
        let state_id = resolve_state_id(self, config, issue_id, state_name)?;
        let response = self.graphql(
            config,
            UPDATE_STATE_MUTATION,
            json!({ "issueId": issue_id, "stateId": state_id }),
        )?;
        match response["data"]["issueUpdate"]["success"].as_bool() {
            Some(true) => Ok(()),
            _ => Err(TrackerError::new("issue_update_failed")),
        }
    }
}

impl LinearTrackerClient {
    fn fetch_by_states(
        &self,
        config: &ServiceConfig,
        project_slug: &str,
        states: &[String],
        assignee_filter: Option<AssigneeFilter>,
    ) -> Result<Vec<Issue>, TrackerError> {
        let normalized_states: Vec<String> = states
            .iter()
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .collect();
        if normalized_states.is_empty() {
            return Ok(Vec::new());
        }

        let mut cursor = None::<String>;
        let mut issues = Vec::new();

        loop {
            let body = self.graphql(
                config,
                QUERY_BY_STATES,
                json!({
                    "projectSlug": project_slug,
                    "stateNames": normalized_states,
                    "first": ISSUE_PAGE_SIZE,
                    "relationFirst": ISSUE_PAGE_SIZE,
                    "after": cursor,
                }),
            )?;

            let page = decode_linear_page_response(&body, assignee_filter.as_ref())?;
            issues.extend(page.issues);
            if page.has_next_page {
                cursor = page.end_cursor;
            } else {
                break;
            }
        }

        Ok(issues)
    }
}

#[derive(Debug, Clone)]
struct AssigneeFilter {
    match_values: BTreeSet<String>,
}

#[derive(Debug)]
struct LinearPage {
    issues: Vec<Issue>,
    has_next_page: bool,
    end_cursor: Option<String>,
}

fn resolve_assignee_filter(
    client: &LinearTrackerClient,
    config: &ServiceConfig,
) -> Result<Option<AssigneeFilter>, TrackerError> {
    match config.tracker.assignee.as_deref() {
        None => Ok(None),
        Some("me") => {
            let response = client.graphql(config, VIEWER_QUERY, json!({}))?;
            let viewer_id = response["data"]["viewer"]["id"]
                .as_str()
                .ok_or_else(|| TrackerError::new("missing_linear_viewer_identity"))?;
            Ok(Some(AssigneeFilter {
                match_values: BTreeSet::from([viewer_id.to_string()]),
            }))
        }
        Some(assignee) if !assignee.trim().is_empty() => Ok(Some(AssigneeFilter {
            match_values: BTreeSet::from([assignee.trim().to_string()]),
        })),
        _ => Ok(None),
    }
}

fn resolve_state_id(
    client: &LinearTrackerClient,
    config: &ServiceConfig,
    issue_id: &str,
    state_name: &str,
) -> Result<String, TrackerError> {
    let response = client.graphql(
        config,
        STATE_LOOKUP_QUERY,
        json!({ "issueId": issue_id, "stateName": state_name }),
    )?;

    response["data"]["issue"]["team"]["states"]["nodes"][0]["id"]
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| TrackerError::new("state_not_found"))
}

fn decode_linear_page_response(
    response: &JsonValue,
    assignee_filter: Option<&AssigneeFilter>,
) -> Result<LinearPage, TrackerError> {
    let issues = decode_linear_response(response, assignee_filter)?;
    let page_info = &response["data"]["issues"]["pageInfo"];
    Ok(LinearPage {
        issues,
        has_next_page: page_info["hasNextPage"].as_bool().unwrap_or(false),
        end_cursor: page_info["endCursor"].as_str().map(ToString::to_string),
    })
}

fn decode_linear_response(
    response: &JsonValue,
    assignee_filter: Option<&AssigneeFilter>,
) -> Result<Vec<Issue>, TrackerError> {
    if let Some(errors) = response.get("errors") {
        return Err(TrackerError::new(format!(
            "linear_graphql_errors: {errors}"
        )));
    }

    let nodes = response["data"]["issues"]["nodes"]
        .as_array()
        .ok_or_else(|| TrackerError::new("linear_unknown_payload"))?;

    Ok(nodes
        .iter()
        .filter_map(|node| normalize_issue(node, assignee_filter))
        .collect())
}

fn normalize_issue(node: &JsonValue, assignee_filter: Option<&AssigneeFilter>) -> Option<Issue> {
    let assignee = node.get("assignee");
    let assignee_id = assignee
        .and_then(|value| value.get("id"))
        .and_then(JsonValue::as_str)
        .map(ToString::to_string);

    let assigned_to_worker = match assignee_filter {
        Some(filter) => assignee_id
            .as_ref()
            .map(|value| filter.match_values.contains(value))
            .unwrap_or(false),
        None => true,
    };

    Some(Issue {
        id: node.get("id")?.as_str()?.to_string(),
        identifier: node.get("identifier")?.as_str()?.to_string(),
        title: node.get("title")?.as_str()?.to_string(),
        description: node
            .get("description")
            .and_then(JsonValue::as_str)
            .map(ToString::to_string),
        priority: node.get("priority").and_then(JsonValue::as_i64),
        state: node
            .get("state")
            .and_then(|value| value.get("name"))
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string(),
        branch_name: node
            .get("branchName")
            .and_then(JsonValue::as_str)
            .map(ToString::to_string),
        url: node
            .get("url")
            .and_then(JsonValue::as_str)
            .map(ToString::to_string),
        assignee_id,
        labels: extract_labels(node),
        blocked_by: extract_blockers(node),
        assigned_to_worker,
        created_at: parse_datetime(node.get("createdAt")),
        updated_at: parse_datetime(node.get("updatedAt")),
    })
}

fn extract_labels(node: &JsonValue) -> Vec<String> {
    node["labels"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.get("name").and_then(JsonValue::as_str))
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

fn extract_blockers(node: &JsonValue) -> Vec<IssueBlockerRef> {
    node["inverseRelations"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|relation| {
            let relation_type = relation.get("type")?.as_str()?.trim().to_ascii_lowercase();
            if relation_type != "blocks" {
                return None;
            }
            let issue = relation.get("issue")?;
            Some(IssueBlockerRef {
                id: issue
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .map(ToString::to_string),
                identifier: issue
                    .get("identifier")
                    .and_then(JsonValue::as_str)
                    .map(ToString::to_string),
                state: issue
                    .get("state")
                    .and_then(|value| value.get("name"))
                    .and_then(JsonValue::as_str)
                    .map(ToString::to_string),
            })
        })
        .collect()
}

fn parse_datetime(value: Option<&JsonValue>) -> Option<DateTime<Utc>> {
    value
        .and_then(JsonValue::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::normalize_issue;
    use crate::domain::Issue;
    use serde_json::json;

    #[test]
    fn normalizes_linear_issue_payload() {
        let issue = normalize_issue(
            &json!({
                "id": "1",
                "identifier": "ABC-1",
                "title": "Example",
                "description": "Body",
                "priority": 2,
                "state": { "name": "Todo" },
                "branchName": "abc-1",
                "url": "https://linear.app",
                "assignee": { "id": "user_1" },
                "labels": { "nodes": [{ "name": "Bug" }] },
                "inverseRelations": {
                    "nodes": [{
                        "type": "blocks",
                        "issue": { "id": "2", "identifier": "ABC-0", "state": { "name": "In Progress" } }
                    }]
                },
                "createdAt": "2026-03-21T00:00:00Z",
                "updatedAt": "2026-03-21T00:00:00Z"
            }),
            None,
        )
        .expect("issue should normalize");

        assert_eq!(issue.identifier, "ABC-1");
        assert_eq!(issue.labels, vec!["bug"]);
    }

    #[test]
    fn example_issue_still_compiles() {
        let issue = Issue::example();
        assert_eq!(issue.identifier, "ABC-123");
    }
}
