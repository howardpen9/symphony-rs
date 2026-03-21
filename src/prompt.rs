use std::fmt;

use crate::domain::Issue;

pub trait PromptRenderer {
    fn render(
        &self,
        template: &str,
        issue: &Issue,
        attempt: Option<u32>,
    ) -> Result<String, PromptError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LiquidLikePromptRenderer;

impl PromptRenderer for LiquidLikePromptRenderer {
    fn render(
        &self,
        template: &str,
        issue: &Issue,
        attempt: Option<u32>,
    ) -> Result<String, PromptError> {
        let tokens = tokenize(template)?;
        let mut index = 0;
        let nodes = parse_nodes(&tokens, &mut index)?;
        if index != tokens.len() {
            return Err(PromptError::UnexpectedControlTag(
                "unexpected trailing control tag".to_string(),
            ));
        }
        render_nodes(&nodes, issue, attempt)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Text(String),
    Variable(String),
    If(String),
    Else,
    EndIf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Text(String),
    Variable(String),
    If {
        expression: String,
        when_true: Vec<Node>,
        when_false: Vec<Node>,
    },
}

fn tokenize(template: &str) -> Result<Vec<Token>, PromptError> {
    let mut tokens = Vec::new();
    let mut cursor = 0usize;

    while cursor < template.len() {
        let next_var = template[cursor..].find("{{").map(|index| cursor + index);
        let next_ctrl = template[cursor..].find("{%").map(|index| cursor + index);
        let next = match (next_var, next_ctrl) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        };

        if let Some(start) = next {
            if start > cursor {
                tokens.push(Token::Text(template[cursor..start].to_string()));
            }

            if template[start..].starts_with("{{") {
                let rest = &template[start + 2..];
                let end = rest.find("}}").ok_or(PromptError::UnterminatedExpression)?;
                tokens.push(Token::Variable(rest[..end].trim().to_string()));
                cursor = start + 2 + end + 2;
            } else {
                let rest = &template[start + 2..];
                let end = rest.find("%}").ok_or(PromptError::UnterminatedControlTag)?;
                let control = rest[..end].trim();
                match control {
                    "else" => tokens.push(Token::Else),
                    "endif" => tokens.push(Token::EndIf),
                    _ if control.starts_with("if ") => {
                        tokens.push(Token::If(control[3..].trim().to_string()))
                    }
                    _ => {
                        return Err(PromptError::UnexpectedControlTag(control.to_string()));
                    }
                }
                cursor = start + 2 + end + 2;
            }
        } else {
            tokens.push(Token::Text(template[cursor..].to_string()));
            break;
        }
    }

    Ok(tokens)
}

fn parse_nodes(tokens: &[Token], index: &mut usize) -> Result<Vec<Node>, PromptError> {
    let mut nodes = Vec::new();

    while *index < tokens.len() {
        match &tokens[*index] {
            Token::Text(value) => {
                nodes.push(Node::Text(value.clone()));
                *index += 1;
            }
            Token::Variable(value) => {
                nodes.push(Node::Variable(value.clone()));
                *index += 1;
            }
            Token::If(expression) => {
                *index += 1;
                let when_true = parse_nodes(tokens, index)?;
                let when_false = if matches!(tokens.get(*index), Some(Token::Else)) {
                    *index += 1;
                    parse_nodes(tokens, index)?
                } else {
                    Vec::new()
                };

                match tokens.get(*index) {
                    Some(Token::EndIf) => {
                        *index += 1;
                        nodes.push(Node::If {
                            expression: expression.clone(),
                            when_true,
                            when_false,
                        });
                    }
                    _ => {
                        return Err(PromptError::UnexpectedControlTag(
                            "missing endif".to_string(),
                        ));
                    }
                }
            }
            Token::Else | Token::EndIf => break,
        }
    }

    Ok(nodes)
}

fn render_nodes(
    nodes: &[Node],
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    let mut output = String::new();

    for node in nodes {
        match node {
            Node::Text(value) => output.push_str(value),
            Node::Variable(expression) => {
                output.push_str(&resolve_expression(expression, issue, attempt)?.render());
            }
            Node::If {
                expression,
                when_true,
                when_false,
            } => {
                if resolve_expression(expression, issue, attempt)?.truthy() {
                    output.push_str(&render_nodes(when_true, issue, attempt)?);
                } else {
                    output.push_str(&render_nodes(when_false, issue, attempt)?);
                }
            }
        }
    }

    Ok(output.trim().to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Null,
    Number(String),
    String(String),
    List(Vec<String>),
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Number(value) => value != "0",
            Self::String(value) => !value.is_empty(),
            Self::List(values) => !values.is_empty(),
        }
    }

    fn render(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Number(value) | Self::String(value) => value.clone(),
            Self::List(values) => values.join(", "),
        }
    }
}

fn resolve_expression(
    expression: &str,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<Value, PromptError> {
    if expression.contains('|') {
        return Err(PromptError::UnknownFilter(expression.to_string()));
    }

    let value = match expression {
        "attempt" => attempt
            .map(|value| Value::Number(value.to_string()))
            .unwrap_or(Value::Null),
        "issue.id" => Value::String(issue.id.clone()),
        "issue.identifier" => Value::String(issue.identifier.clone()),
        "issue.title" => Value::String(issue.title.clone()),
        "issue.description" => issue
            .description
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
        "issue.priority" => issue
            .priority
            .map(|value| Value::Number(value.to_string()))
            .unwrap_or(Value::Null),
        "issue.state" => Value::String(issue.state.clone()),
        "issue.branch_name" => issue
            .branch_name
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
        "issue.url" => issue.url.clone().map(Value::String).unwrap_or(Value::Null),
        "issue.assignee_id" => issue
            .assignee_id
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
        "issue.labels" => Value::List(issue.labels.clone()),
        "issue.blocked_by" => Value::List(
            issue
                .blocked_by
                .iter()
                .map(|blocker| {
                    blocker
                        .identifier
                        .clone()
                        .or_else(|| blocker.id.clone())
                        .or_else(|| blocker.state.clone())
                        .unwrap_or_else(|| "unknown-blocker".to_string())
                })
                .collect(),
        ),
        "issue.created_at" => issue
            .created_at
            .map(|value| Value::String(value.to_rfc3339()))
            .unwrap_or(Value::Null),
        "issue.updated_at" => issue
            .updated_at
            .map(|value| Value::String(value.to_rfc3339()))
            .unwrap_or(Value::Null),
        other => return Err(PromptError::UnknownVariable(other.to_string())),
    };

    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    UnterminatedExpression,
    UnterminatedControlTag,
    UnknownVariable(String),
    UnknownFilter(String),
    UnexpectedControlTag(String),
}

impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedExpression => {
                write!(
                    f,
                    "template_parse_error: unterminated `{{ ... }}` expression"
                )
            }
            Self::UnterminatedControlTag => {
                write!(
                    f,
                    "template_parse_error: unterminated `{{% ... %}}` control tag"
                )
            }
            Self::UnknownVariable(value) => {
                write!(f, "template_render_error: unknown variable `{value}`")
            }
            Self::UnknownFilter(value) => {
                write!(
                    f,
                    "template_render_error: filters are not supported `{value}`"
                )
            }
            Self::UnexpectedControlTag(value) => {
                write!(f, "template_parse_error: unsupported control tag `{value}`")
            }
        }
    }
}

impl std::error::Error for PromptError {}

#[cfg(test)]
mod tests {
    use super::{LiquidLikePromptRenderer, PromptRenderer};
    use crate::domain::Issue;

    #[test]
    fn renders_variables_and_if_blocks() {
        let renderer = LiquidLikePromptRenderer;
        let issue = Issue::example();
        let output = renderer
            .render(
                "{% if issue.description %}{{ issue.identifier }}: {{ issue.description }}{% else %}missing{% endif %}",
                &issue,
                Some(1),
            )
            .expect("template should render");

        assert!(output.contains("ABC-123"));
    }

    #[test]
    fn fails_on_unknown_variables() {
        let renderer = LiquidLikePromptRenderer;
        let issue = Issue::example();
        let error = renderer
            .render("{{ issue.nope }}", &issue, None)
            .expect_err("unknown variables should fail");

        assert_eq!(
            error.to_string(),
            "template_render_error: unknown variable `issue.nope`"
        );
    }
}
