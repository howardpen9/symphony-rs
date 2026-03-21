use std::fmt;
use std::fs;
use std::path::Path;

use serde_yaml::{Mapping, Value as YamlValue};

use crate::domain::WorkflowDefinition;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowError {
    MissingWorkflowFile(String),
    WorkflowParseError(String),
    WorkflowFrontMatterNotMap,
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingWorkflowFile(path) => write!(f, "missing_workflow_file: {path}"),
            Self::WorkflowParseError(reason) => write!(f, "workflow_parse_error: {reason}"),
            Self::WorkflowFrontMatterNotMap => {
                write!(f, "workflow_front_matter_not_a_map")
            }
        }
    }
}

impl std::error::Error for WorkflowError {}

pub struct WorkflowLoader;

impl WorkflowLoader {
    pub fn from_path(path: impl AsRef<Path>) -> Result<WorkflowDefinition, WorkflowError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .map_err(|_| WorkflowError::MissingWorkflowFile(path.display().to_string()))?;
        Self::parse(&contents)
    }

    pub fn parse(contents: &str) -> Result<WorkflowDefinition, WorkflowError> {
        let (front_matter_lines, prompt_lines) = split_front_matter(contents);
        let front_matter_yaml = front_matter_lines.join("\n");
        let config = parse_front_matter(&front_matter_yaml)?;
        let prompt_template = prompt_lines.join("\n").trim().to_string();

        Ok(WorkflowDefinition {
            config,
            prompt_template,
        })
    }
}

fn split_front_matter(contents: &str) -> (Vec<&str>, Vec<&str>) {
    let lines: Vec<&str> = contents.lines().collect();
    match lines.first().copied() {
        Some("---") => {
            let mut front_matter = Vec::new();
            let mut prompt_start = None;

            for (index, line) in lines.iter().enumerate().skip(1) {
                if *line == "---" {
                    prompt_start = Some(index + 1);
                    break;
                }
                front_matter.push(*line);
            }

            let prompt_lines = prompt_start
                .map(|index| lines[index..].to_vec())
                .unwrap_or_default();

            (front_matter, prompt_lines)
        }
        _ => (Vec::new(), lines),
    }
}

fn parse_front_matter(contents: &str) -> Result<Mapping, WorkflowError> {
    if contents.trim().is_empty() {
        return Ok(Mapping::new());
    }

    match serde_yaml::from_str::<YamlValue>(contents) {
        Ok(YamlValue::Mapping(mapping)) => Ok(mapping),
        Ok(_) => Err(WorkflowError::WorkflowFrontMatterNotMap),
        Err(error) => Err(WorkflowError::WorkflowParseError(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::WorkflowLoader;

    #[test]
    fn parses_front_matter_and_prompt_body() {
        let workflow = WorkflowLoader::parse(
            "---\ntracker:\n  kind: linear\n---\n\nHello {{ issue.identifier }}\n",
        )
        .expect("workflow should parse");

        assert!(workflow.config.contains_key("tracker"));
        assert_eq!(workflow.prompt_template, "Hello {{ issue.identifier }}");
    }

    #[test]
    fn supports_prompt_only_workflow_files() {
        let workflow =
            WorkflowLoader::parse("Investigate {{ issue.title }}").expect("valid prompt");
        assert!(workflow.config.is_empty());
        assert_eq!(workflow.prompt_template, "Investigate {{ issue.title }}");
    }
}
