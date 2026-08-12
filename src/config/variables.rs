// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use super::{MAX_ASSIGNMENT_NAME_LEN, MAX_ASSIGNMENT_VALUE_LEN};

pub const MAX_COMMAND_LINE_VARIABLES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLimitVariable {
    MessageSize,
    HeadersSize,
    BodySize,
    HeaderLineSize,
    HeaderFieldSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentTarget {
    Maildir,
    LogFile,
    Verbose,
    MessageLimit(MessageLimitVariable),
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableSource {
    RcFile,
    CommandLine,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariablePolicy {
    RcOnly(AssignmentTarget),
    RcOrCommandLine(AssignmentTarget),
    RuntimeOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedVariable {
    name: String,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedVariableError {
    message: String,
}

impl SuppliedVariable {
    pub fn parse(input: String) -> Result<Self, SuppliedVariableError> {
        let (name, value) = input.split_once('=').ok_or_else(|| {
            SuppliedVariableError::new("--set value must have the form NAME=VALUE")
        })?;
        if name.len() > MAX_ASSIGNMENT_NAME_LEN {
            return Err(SuppliedVariableError::new(format!(
                "--set name exceeds the hard limit of {MAX_ASSIGNMENT_NAME_LEN} bytes"
            )));
        }
        if value.len() > MAX_ASSIGNMENT_VALUE_LEN {
            return Err(SuppliedVariableError::new(format!(
                "--set value exceeds the hard limit of {MAX_ASSIGNMENT_VALUE_LEN} bytes"
            )));
        }
        if !valid_name(name) {
            return Err(SuppliedVariableError::new(
                "--set name must start with an ASCII letter or '_' and contain only ASCII letters, digits, or '_'",
            ));
        }
        if !variable_policy(name).allows(VariableSource::CommandLine) {
            return Err(SuppliedVariableError::new(format!(
                "variable {name} cannot be supplied with --set"
            )));
        }
        Ok(Self {
            name: name.to_owned(),
            value: value.to_owned(),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }
}

impl SuppliedVariableError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SuppliedVariableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SuppliedVariableError {}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

pub fn variable_policy(name: &str) -> VariablePolicy {
    match name {
        "MAILDIR" => VariablePolicy::RcOnly(AssignmentTarget::Maildir),
        "LOGFILE" => VariablePolicy::RcOnly(AssignmentTarget::LogFile),
        "VERBOSE" => VariablePolicy::RcOnly(AssignmentTarget::Verbose),
        "LASTFOLDER" => VariablePolicy::RuntimeOnly,
        "LIMIT_MSG_SIZE" => VariablePolicy::RcOnly(AssignmentTarget::MessageLimit(
            MessageLimitVariable::MessageSize,
        )),
        "LIMIT_MSG_HEADERS" => VariablePolicy::RcOnly(AssignmentTarget::MessageLimit(
            MessageLimitVariable::HeadersSize,
        )),
        "LIMIT_MSG_BODY" => VariablePolicy::RcOnly(AssignmentTarget::MessageLimit(
            MessageLimitVariable::BodySize,
        )),
        "LIMIT_HEADER_LINE" => VariablePolicy::RcOnly(AssignmentTarget::MessageLimit(
            MessageLimitVariable::HeaderLineSize,
        )),
        "LIMIT_HEADER_FIELD" => VariablePolicy::RcOnly(AssignmentTarget::MessageLimit(
            MessageLimitVariable::HeaderFieldSize,
        )),
        _ => VariablePolicy::RcOrCommandLine(AssignmentTarget::User),
    }
}

impl VariablePolicy {
    pub fn allows(self, source: VariableSource) -> bool {
        matches!(
            (self, source),
            (Self::RcOnly(_), VariableSource::RcFile)
                | (
                    Self::RcOrCommandLine(_),
                    VariableSource::RcFile | VariableSource::CommandLine
                )
                | (Self::RuntimeOnly, VariableSource::Runtime)
        )
    }

    pub fn assignment_target(self, source: VariableSource) -> Option<AssignmentTarget> {
        match (self, source) {
            (Self::RcOnly(target), VariableSource::RcFile)
            | (
                Self::RcOrCommandLine(target),
                VariableSource::RcFile | VariableSource::CommandLine,
            ) => Some(target),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_explicit_sources_to_variable_classes() {
        assert_eq!(
            variable_policy("MAILDIR").assignment_target(VariableSource::RcFile),
            Some(AssignmentTarget::Maildir)
        );
        assert!(!variable_policy("MAILDIR").allows(VariableSource::CommandLine));
        assert!(variable_policy("LASTFOLDER").allows(VariableSource::Runtime));
        assert!(!variable_policy("LASTFOLDER").allows(VariableSource::RcFile));
        assert_eq!(
            variable_policy("VERBOSE").assignment_target(VariableSource::RcFile),
            Some(AssignmentTarget::Verbose)
        );
        assert_eq!(
            variable_policy("LOGFILE").assignment_target(VariableSource::RcFile),
            Some(AssignmentTarget::LogFile)
        );
        assert!(!variable_policy("VERBOSE").allows(VariableSource::CommandLine));
        assert!(!variable_policy("LOGFILE").allows(VariableSource::CommandLine));
        assert_eq!(
            variable_policy("USER_VALUE").assignment_target(VariableSource::CommandLine),
            Some(AssignmentTarget::User)
        );
    }

    #[test]
    fn parses_bounded_command_line_variables() {
        let variable = SuppliedVariable::parse("BOX=one=two".into()).unwrap();
        assert_eq!(variable.name(), "BOX");
        assert_eq!(variable.value(), "one=two");

        for input in ["BOX", "=value", "9BOX=value", "BOX-NAME=value"] {
            assert!(SuppliedVariable::parse(input.into()).is_err(), "{input:?}");
        }
    }

    #[test]
    fn rejects_command_line_sources_not_allowed_by_policy() {
        for name in ["MAILDIR", "LASTFOLDER", "LIMIT_MSG_BODY"] {
            let error = SuppliedVariable::parse(format!("{name}=value")).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("variable {name} cannot be supplied with --set")
            );
        }
    }

    #[test]
    fn bounds_command_line_name_and_value() {
        let name_at_limit = "A".repeat(MAX_ASSIGNMENT_NAME_LEN);
        assert!(SuppliedVariable::parse(format!("{name_at_limit}=value")).is_ok());
        assert!(
            SuppliedVariable::parse(format!("{}=value", "A".repeat(MAX_ASSIGNMENT_NAME_LEN + 1)))
                .is_err()
        );

        let value_at_limit = "v".repeat(MAX_ASSIGNMENT_VALUE_LEN);
        assert!(SuppliedVariable::parse(format!("A={value_at_limit}")).is_ok());
        assert!(
            SuppliedVariable::parse(format!("A={}", "v".repeat(MAX_ASSIGNMENT_VALUE_LEN + 1)))
                .is_err()
        );
    }
}
