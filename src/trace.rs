// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Typed, value-free events used to explain filtering decisions.

use std::fmt;

use crate::config::MAX_ASSIGNMENT_NAME_LEN;

/// One filtering event in execution order.
///
/// Events intentionally contain no message bytes, variable values, regular
/// expression text, command arguments, or destination paths. A later renderer
/// can therefore format the default trace without first trying to redact
/// hostile or sensitive values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    VariableAssigned {
        line: Option<usize>,
        name: TraceName,
        source: VariableSource,
    },
    LastFolderUpdated,
    ConditionEvaluated {
        recipe_line: usize,
        condition_index: usize,
        kind: ConditionKind,
        negated: bool,
        matched: bool,
    },
    RecipeEvaluated {
        line: usize,
        decision: RecipeDecision,
    },
    Delivery {
        recipe_line: usize,
        destination: DestinationKind,
        stage: DeliveryStage,
    },
    ExternalCommand {
        recipe_line: usize,
        stage: ExternalCommandStage,
    },
}

/// A bounded variable name taken from already validated configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceName(String);

impl TraceName {
    pub fn new(name: &str) -> Result<Self, TraceNameError> {
        if name.len() > MAX_ASSIGNMENT_NAME_LEN {
            return Err(TraceNameError);
        }
        let mut bytes = name.bytes();
        let valid = bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
            && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
        if !valid {
            return Err(TraceNameError);
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceNameError;

impl fmt::Display for TraceNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("trace variable name is invalid or exceeds its size limit")
    }
}

impl std::error::Error for TraceNameError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableSource {
    RcFile,
    CommandLine,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionKind {
    HeaderRegex,
    BodyRegex,
    MessageRegex,
    SmallerThan,
    LargerThan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeDecision {
    Selected,
    Deferred,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationKind {
    Maildir,
    Mbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStage {
    Preparing,
    Published,
    Failed(FailureClass),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalCommandStage {
    Starting,
    Succeeded,
    Failed(FailureClass),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    InputLimit,
    Transient,
    Permanent,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_event_contains_a_name_but_no_value_slot() {
        let event = TraceEvent::VariableAssigned {
            line: Some(7),
            name: TraceName::new("MAILBOX").unwrap(),
            source: VariableSource::RcFile,
        };

        let rendered = format!("{event:?}");
        assert!(rendered.contains("MAILBOX"));
        assert!(!rendered.contains("secret-value"));
    }

    #[test]
    fn bounds_and_validates_variable_names() {
        assert!(TraceName::new("NAME_1").is_ok());
        assert!(TraceName::new("1NAME").is_err());
        assert!(TraceName::new(&"N".repeat(MAX_ASSIGNMENT_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn models_each_required_execution_decision() {
        let events = [
            TraceEvent::LastFolderUpdated,
            TraceEvent::ConditionEvaluated {
                recipe_line: 2,
                condition_index: 0,
                kind: ConditionKind::HeaderRegex,
                negated: false,
                matched: false,
            },
            TraceEvent::RecipeEvaluated {
                line: 2,
                decision: RecipeDecision::Skipped,
            },
            TraceEvent::Delivery {
                recipe_line: 5,
                destination: DestinationKind::Maildir,
                stage: DeliveryStage::Failed(FailureClass::Transient),
            },
            TraceEvent::ExternalCommand {
                recipe_line: 9,
                stage: ExternalCommandStage::Starting,
            },
        ];

        assert_eq!(events.len(), 5);
    }
}
