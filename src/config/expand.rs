use std::collections::BTreeMap;
use std::fmt;

use super::{
    AssignmentTarget, Config, Destination, MAX_ASSIGNMENT_VALUE_LEN, MAX_PATH_EXPRESSION_LEN,
    Statement, VariablePolicy, variable_policy,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpansionError {
    pub line: usize,
    pub message: String,
}

impl ExpansionError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for ExpansionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ExpansionError {}

pub(super) fn expand(mut config: Config) -> Result<Config, ExpansionError> {
    let mut variables = BTreeMap::<String, String>::new();

    for statement in &mut config.statements {
        match statement {
            Statement::Assignment(assignment) => {
                let limit = match assignment.target {
                    AssignmentTarget::Maildir => MAX_PATH_EXPRESSION_LEN,
                    AssignmentTarget::MessageLimit(_) | AssignmentTarget::User => {
                        MAX_ASSIGNMENT_VALUE_LEN
                    }
                };
                assignment.value =
                    expand_text(&assignment.value, assignment.line, limit, &variables)?;
                variables.insert(assignment.name.clone(), assignment.value.clone());
            }
            Statement::Recipe(recipe) => {
                if let Some(lock) = &mut recipe.lock {
                    *lock = expand_text(lock, recipe.line, MAX_PATH_EXPRESSION_LEN, &variables)?;
                }
                let path = match &mut recipe.destination {
                    Destination::Mbox(path)
                    | Destination::Maildir(path)
                    | Destination::Auto(path) => path,
                };
                *path = expand_text(
                    path,
                    recipe.action_line,
                    MAX_PATH_EXPRESSION_LEN,
                    &variables,
                )?;
            }
        }
    }

    Ok(config)
}

fn expand_text(
    input: &str,
    line: usize,
    limit: usize,
    variables: &BTreeMap<String, String>,
) -> Result<String, ExpansionError> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(input.len().min(limit));
    let mut index = 0;

    while index < bytes.len() {
        let Some(relative_dollar) = bytes[index..].iter().position(|byte| *byte == b'$') else {
            push_bounded(&mut output, &bytes[index..], limit, line)?;
            break;
        };
        let dollar = index + relative_dollar;
        push_bounded(&mut output, &bytes[index..dollar], limit, line)?;

        let (name, next) = parse_reference(input, dollar, line)?;
        let value = variables
            .get(name)
            .ok_or_else(|| match variable_policy(name) {
                VariablePolicy::RuntimeOnly => ExpansionError::new(
                    line,
                    format!("runtime variable {name} is not available in this context"),
                ),
                _ => ExpansionError::new(line, format!("variable {name} is not defined")),
            })?;
        push_bounded(&mut output, value.as_bytes(), limit, line)?;
        index = next;
    }

    String::from_utf8(output)
        .map_err(|_| ExpansionError::new(line, "expanded value is not valid UTF-8"))
}

fn parse_reference(
    input: &str,
    dollar: usize,
    line: usize,
) -> Result<(&str, usize), ExpansionError> {
    let bytes = input.as_bytes();
    let start = dollar
        .checked_add(1)
        .ok_or_else(|| ExpansionError::new(line, "variable reference offset overflows"))?;
    let Some(first) = bytes.get(start).copied() else {
        return Err(ExpansionError::new(
            line,
            "'$' must be followed by NAME or {NAME}",
        ));
    };

    if first == b'{' {
        let name_start = start + 1;
        let close = bytes[name_start..]
            .iter()
            .position(|byte| *byte == b'}')
            .map(|offset| name_start + offset)
            .ok_or_else(|| ExpansionError::new(line, "variable reference is missing '}'"))?;
        let name = &input[name_start..close];
        validate_reference_name(name, line)?;
        return Ok((name, close + 1));
    }

    if !is_name_start(first) {
        return Err(ExpansionError::new(
            line,
            "unsupported '$' expansion; use $NAME or ${NAME}",
        ));
    }
    let mut end = start + 1;
    while end < bytes.len() && is_name_continue(bytes[end]) {
        end += 1;
    }
    Ok((&input[start..end], end))
}

fn validate_reference_name(name: &str, line: usize) -> Result<(), ExpansionError> {
    let mut bytes = name.bytes();
    let valid = bytes.next().is_some_and(is_name_start) && bytes.all(is_name_continue);
    if !valid {
        return Err(ExpansionError::new(
            line,
            "variable reference contains an invalid name",
        ));
    }
    Ok(())
}

fn is_name_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_name_continue(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn push_bounded(
    output: &mut Vec<u8>,
    value: &[u8],
    limit: usize,
    line: usize,
) -> Result<(), ExpansionError> {
    let new_len = output
        .len()
        .checked_add(value.len())
        .ok_or_else(|| ExpansionError::new(line, "expanded value length overflows"))?;
    if new_len > limit {
        return Err(ExpansionError::new(
            line,
            format!("expanded value exceeds the hard limit of {limit} bytes"),
        ));
    }
    output.extend_from_slice(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConditionKind, parse};

    #[test]
    fn expands_both_variable_reference_forms_sequentially() {
        let config = parse(
            "ROOT=mail\nBOX=${ROOT}/inbox\nMAILDIR=/srv/$ROOT\n:0 :lock-$BOX\nmaildir:$BOX\n",
        )
        .unwrap()
        .expand()
        .unwrap();

        let Statement::Assignment(box_assignment) = &config.statements[1] else {
            panic!("expected assignment");
        };
        assert_eq!(box_assignment.value, "mail/inbox");
        assert_eq!(config.maildir(), Some("/srv/mail"));
        let Statement::Recipe(recipe) = &config.statements[3] else {
            panic!("expected recipe");
        };
        assert_eq!(recipe.lock.as_deref(), Some("lock-mail/inbox"));
        assert_eq!(
            recipe.destination,
            Destination::Maildir("mail/inbox".into())
        );
    }

    #[test]
    fn rejects_undefined_forward_and_runtime_references() {
        for (source, expected) in [
            ("A=$B\nB=value\n", "variable B is not defined"),
            (
                ":0\nmaildir:$LASTFOLDER\n",
                "runtime variable LASTFOLDER is not available in this context",
            ),
        ] {
            let error = parse(source).unwrap().expand().unwrap_err();
            assert_eq!(error.message, expected);
        }
    }

    #[test]
    fn rejects_unsupported_and_malformed_references() {
        for source in ["A=$$\n", "A=${NAME:-value}\n", "A=${NAME\n", "A=$\n"] {
            assert!(parse(source).unwrap().expand().is_err(), "{source:?}");
        }
    }

    #[test]
    fn bounds_expanded_paths_before_allocation_growth() {
        let source = format!(
            "A={}\nB={}\n:0\nmaildir:$A$B\n",
            "a".repeat(MAX_PATH_EXPRESSION_LEN),
            "b"
        );
        let error = parse(&source).unwrap().expand().unwrap_err();

        assert_eq!(error.line, 4);
        assert_eq!(
            error.message,
            format!("expanded value exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes")
        );
    }

    #[test]
    fn leaves_regex_patterns_unchanged() {
        let config = parse("NAME=value\n:0\n* ^Subject: $NAME$\ninbox/\n")
            .unwrap()
            .expand()
            .unwrap();
        let Statement::Recipe(recipe) = &config.statements[1] else {
            panic!("expected recipe");
        };
        let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
            panic!("expected regex");
        };

        assert_eq!(regex.pattern(), "^Subject: $NAME$");
    }
}
