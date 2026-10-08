// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Statement-ordered configuration of trace detail and logging controls.

use super::TraceDetail;
use crate::config::{AssignmentTarget, Config, Statement};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFailurePolicy {
    Advisory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceConfig {
    verbose: bool,
    logfile: Option<String>,
    detail: TraceDetail,
    may_log: bool,
    failure_policy: LogFailurePolicy,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            verbose: false,
            logfile: None,
            detail: TraceDetail::Metadata,
            may_log: false,
            failure_policy: LogFailurePolicy::Advisory,
        }
    }
}

impl TraceConfig {
    pub fn from_config(config: &Config) -> Result<Self, TraceConfigError> {
        let mut settings = Self {
            may_log: statements_may_log(&config.statements),
            ..Self::default()
        };
        for statement in &config.statements {
            let Statement::Assignment(assignment) = statement else {
                if let Statement::Unset(unset) = statement {
                    match unset.target {
                        AssignmentTarget::Verbose => settings.verbose = false,
                        AssignmentTarget::LogFile => settings.logfile = None,
                        AssignmentTarget::LogDetail => settings.detail = TraceDetail::Metadata,
                        _ => {}
                    }
                }
                continue;
            };
            match assignment.target {
                AssignmentTarget::Verbose => {
                    settings.verbose =
                        parse_procmail_boolean(&assignment.value).ok_or_else(|| {
                            TraceConfigError {
                                line: assignment.line,
                                name: assignment.name.clone(),
                                reason: "expected a procmail boolean value".to_owned(),
                            }
                        })?;
                }
                AssignmentTarget::LogFile => {
                    settings.logfile =
                        (!assignment.value.is_empty()).then(|| assignment.value.clone());
                }
                AssignmentTarget::LogDetail => {
                    settings.detail = match assignment.value.as_str() {
                        "metadata" => TraceDetail::Metadata,
                        "values" => TraceDetail::Values,
                        _ => {
                            return Err(TraceConfigError {
                                line: assignment.line,
                                name: assignment.name.clone(),
                                reason: "expected 'metadata' or 'values'".to_owned(),
                            });
                        }
                    };
                }
                _ => {}
            }
        }
        Ok(settings)
    }

    pub fn verbose(&self) -> bool {
        self.verbose
    }

    pub fn logfile(&self) -> Option<&str> {
        self.logfile.as_deref()
    }

    pub fn enabled(&self) -> bool {
        self.verbose || self.may_log
    }

    pub fn failure_policy(&self) -> LogFailurePolicy {
        self.failure_policy
    }

    pub fn detail(&self) -> TraceDetail {
        self.detail
    }
}

fn statements_may_log(statements: &[Statement]) -> bool {
    statements.iter().any(|statement| match statement {
        Statement::Assignment(assignment) => {
            assignment.target == AssignmentTarget::Log
                || assignment.target == AssignmentTarget::LogAbstract && assignment.value != "no"
        }
        Statement::Include(_) | Statement::Switch(_) => true,
        Statement::Recipe(recipe) => match &recipe.action {
            crate::config::RecipeAction::Block(children) => statements_may_log(children),
            _ => false,
        },
        Statement::Unset(_) | Statement::CommandAssignment(_) => false,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceConfigError {
    pub line: usize,
    pub name: String,
    pub reason: String,
}

impl fmt::Display for TraceConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "line {}: invalid {}: {}",
            self.line, self.name, self.reason
        )
    }
}

impl std::error::Error for TraceConfigError {}

pub(crate) fn parse_procmail_boolean(value: &str) -> Option<bool> {
    let value = value.to_ascii_lowercase();
    if value.starts_with(|character: char| character.is_ascii_digit() && character != '0')
        || ["on", "y", "t", "e"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
    {
        Some(true)
    } else if value.starts_with('0')
        || ["off", "n", "f", "d"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
    {
        Some(false)
    } else {
        None
    }
}
