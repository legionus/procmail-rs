// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use super::{MAX_ASSIGNMENT_NAME_LEN, MAX_ASSIGNMENT_VALUE_LEN, MAX_SHELL_SETTING_LEN};

pub const MAX_COMMAND_LINE_VARIABLES: usize = 256;
pub const MAX_LOCK_TIMEOUT_SECONDS: u64 = 86_400;
pub const MAX_PROCESS_TIMEOUT_SECONDS: u64 = 86_400;

pub fn validate_lock_method(value: &str) -> Result<(), String> {
    match value {
        "flock" | "dotlock" => Ok(()),
        _ => Err("LOCKMETHOD must be 'flock' or 'dotlock'".to_owned()),
    }
}

pub fn parse_lock_timeout_seconds(value: &str) -> Result<u64, String> {
    let seconds = value.parse::<u64>().map_err(|_| {
        format!("LOCKTIMEOUT must be an integer from 1 to {MAX_LOCK_TIMEOUT_SECONDS}")
    })?;
    if !(1..=MAX_LOCK_TIMEOUT_SECONDS).contains(&seconds) {
        return Err(format!(
            "LOCKTIMEOUT must be an integer from 1 to {MAX_LOCK_TIMEOUT_SECONDS}"
        ));
    }
    Ok(seconds)
}

pub fn parse_process_timeout_seconds(value: &str) -> Result<u64, String> {
    let seconds = value.parse::<u64>().map_err(|_| {
        format!("TIMEOUT must be an integer from 1 to {MAX_PROCESS_TIMEOUT_SECONDS}")
    })?;
    if !(1..=MAX_PROCESS_TIMEOUT_SECONDS).contains(&seconds) {
        return Err(format!(
            "TIMEOUT must be an integer from 1 to {MAX_PROCESS_TIMEOUT_SECONDS}"
        ));
    }
    Ok(seconds)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLimitVariable {
    MessageSize,
    HeadersSize,
    BodySize,
    HeaderLineSize,
    HeaderFieldSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcLimitVariable {
    Assignments,
    Statements,
    Recipes,
    Conditions,
    Regexes,
    ConditionsPerRecipe,
    NestingDepth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentTarget {
    Maildir,
    LogFile,
    LogDetail,
    Verbose,
    Durability,
    LockMethod,
    LockFile,
    LockTimeout,
    LineBuf,
    ProcessTimeout,
    Shell,
    ShellFlags,
    Path,
    ExitCode,
    Host,
    MessageLimit(MessageLimitVariable),
    RcLimit(RcLimitVariable),
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableSource {
    RcFile,
    CommandLine,
    Environment,
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
    source: VariableSource,
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
        let policy = variable_policy(name);
        if !policy.allows(VariableSource::CommandLine) {
            return Err(SuppliedVariableError::new(format!(
                "variable {name} cannot be supplied with --set"
            )));
        }
        let target = policy
            .assignment_target(VariableSource::CommandLine)
            .expect("an allowed command-line variable has an assignment target");
        let limit = assignment_value_limit(target);
        if value.len() > limit {
            return Err(SuppliedVariableError::new(format!(
                "--set {name} value exceeds the hard limit of {limit} bytes"
            )));
        }
        Ok(Self {
            name: name.to_owned(),
            value: value.to_owned(),
            source: VariableSource::CommandLine,
        })
    }

    pub fn from_environment(
        name: &'static str,
        value: String,
    ) -> Result<Self, SuppliedVariableError> {
        if !matches!(name, "HOME" | "LOGNAME") {
            return Err(SuppliedVariableError::new(format!(
                "environment variable {name} is not admitted"
            )));
        }
        if value.len() > MAX_ASSIGNMENT_VALUE_LEN {
            return Err(SuppliedVariableError::new(format!(
                "environment variable {name} exceeds the hard limit of {MAX_ASSIGNMENT_VALUE_LEN} bytes"
            )));
        }
        Ok(Self {
            name: name.to_owned(),
            value,
            source: VariableSource::Environment,
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }

    pub(crate) fn source(&self) -> VariableSource {
        self.source
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
        "LOGDETAIL" => VariablePolicy::RcOnly(AssignmentTarget::LogDetail),
        "VERBOSE" => VariablePolicy::RcOnly(AssignmentTarget::Verbose),
        "DURABILITY" => VariablePolicy::RcOnly(AssignmentTarget::Durability),
        "LOCKMETHOD" => VariablePolicy::RcOnly(AssignmentTarget::LockMethod),
        "LOCKFILE" => VariablePolicy::RcOnly(AssignmentTarget::LockFile),
        "LOCKTIMEOUT" => VariablePolicy::RcOnly(AssignmentTarget::LockTimeout),
        "LINEBUF" => VariablePolicy::RcOnly(AssignmentTarget::LineBuf),
        "TIMEOUT" => VariablePolicy::RcOnly(AssignmentTarget::ProcessTimeout),
        "SHELL" => VariablePolicy::RcOrCommandLine(AssignmentTarget::Shell),
        "SHELLFLAGS" => VariablePolicy::RcOrCommandLine(AssignmentTarget::ShellFlags),
        "PATH" => VariablePolicy::RcOrCommandLine(AssignmentTarget::Path),
        "EXITCODE" => VariablePolicy::RcOnly(AssignmentTarget::ExitCode),
        "HOST" => VariablePolicy::RcOnly(AssignmentTarget::Host),
        "LASTFOLDER" | "MATCH" => VariablePolicy::RuntimeOnly,
        name if name.strip_prefix("MATCH").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        }) =>
        {
            VariablePolicy::RuntimeOnly
        }
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
        "LIMIT_MAX_ASSIGNMENTS" => {
            VariablePolicy::RcOnly(AssignmentTarget::RcLimit(RcLimitVariable::Assignments))
        }
        "LIMIT_RC_STATEMENTS" => {
            VariablePolicy::RcOnly(AssignmentTarget::RcLimit(RcLimitVariable::Statements))
        }
        "LIMIT_RC_RECIPES" => {
            VariablePolicy::RcOnly(AssignmentTarget::RcLimit(RcLimitVariable::Recipes))
        }
        "LIMIT_RC_CONDITIONS" => {
            VariablePolicy::RcOnly(AssignmentTarget::RcLimit(RcLimitVariable::Conditions))
        }
        "LIMIT_RC_REGEXES" => {
            VariablePolicy::RcOnly(AssignmentTarget::RcLimit(RcLimitVariable::Regexes))
        }
        "LIMIT_RECIPE_CONDITIONS" => VariablePolicy::RcOnly(AssignmentTarget::RcLimit(
            RcLimitVariable::ConditionsPerRecipe,
        )),
        "LIMIT_RECIPE_NESTING" => {
            VariablePolicy::RcOnly(AssignmentTarget::RcLimit(RcLimitVariable::NestingDepth))
        }
        _ => VariablePolicy::RcOrCommandLine(AssignmentTarget::User),
    }
}

pub fn assignment_value_limit(target: AssignmentTarget) -> usize {
    match target {
        AssignmentTarget::Maildir | AssignmentTarget::LogFile | AssignmentTarget::LockFile => {
            super::MAX_PATH_EXPRESSION_LEN
        }
        AssignmentTarget::Shell | AssignmentTarget::ShellFlags | AssignmentTarget::Path => {
            MAX_SHELL_SETTING_LEN
        }
        AssignmentTarget::LogDetail
        | AssignmentTarget::Verbose
        | AssignmentTarget::Durability
        | AssignmentTarget::LockMethod
        | AssignmentTarget::LockTimeout
        | AssignmentTarget::LineBuf
        | AssignmentTarget::ProcessTimeout
        | AssignmentTarget::ExitCode
        | AssignmentTarget::Host
        | AssignmentTarget::MessageLimit(_)
        | AssignmentTarget::RcLimit(_)
        | AssignmentTarget::User => MAX_ASSIGNMENT_VALUE_LEN,
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
        assert!(variable_policy("MATCH").allows(VariableSource::Runtime));
        assert!(variable_policy("MATCH1").allows(VariableSource::Runtime));
        assert!(!variable_policy("MATCH1").allows(VariableSource::RcFile));
        assert_eq!(
            variable_policy("VERBOSE").assignment_target(VariableSource::RcFile),
            Some(AssignmentTarget::Verbose)
        );
        assert_eq!(
            variable_policy("LOGFILE").assignment_target(VariableSource::RcFile),
            Some(AssignmentTarget::LogFile)
        );
        assert_eq!(
            variable_policy("LOGDETAIL").assignment_target(VariableSource::RcFile),
            Some(AssignmentTarget::LogDetail)
        );
        assert!(!variable_policy("VERBOSE").allows(VariableSource::CommandLine));
        assert!(!variable_policy("LOGFILE").allows(VariableSource::CommandLine));
        assert!(!variable_policy("LOGDETAIL").allows(VariableSource::CommandLine));
        assert_eq!(
            variable_policy("USER_VALUE").assignment_target(VariableSource::CommandLine),
            Some(AssignmentTarget::User)
        );
        assert_eq!(
            variable_policy("SHELL").assignment_target(VariableSource::RcFile),
            Some(AssignmentTarget::Shell)
        );
        assert_eq!(
            variable_policy("SHELLFLAGS").assignment_target(VariableSource::CommandLine),
            Some(AssignmentTarget::ShellFlags)
        );
        assert_eq!(
            variable_policy("PATH").assignment_target(VariableSource::RcFile),
            Some(AssignmentTarget::Path)
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
    fn admits_only_passwd_backed_initial_names() {
        for name in ["HOME", "LOGNAME"] {
            let variable = SuppliedVariable::from_environment(name, "value".into()).unwrap();
            assert_eq!(variable.name(), name);
            assert_eq!(variable.source(), VariableSource::Environment);
        }
        assert!(SuppliedVariable::from_environment("PATH", "value".into()).is_err());
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

    #[test]
    fn applies_shell_setting_limit_to_command_line_values() {
        for name in ["SHELL", "SHELLFLAGS", "PATH"] {
            assert!(
                SuppliedVariable::parse(format!("{name}={}", "x".repeat(MAX_SHELL_SETTING_LEN)))
                    .is_ok()
            );
            let error = SuppliedVariable::parse(format!(
                "{name}={}",
                "x".repeat(MAX_SHELL_SETTING_LEN + 1)
            ))
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "--set {name} value exceeds the hard limit of {MAX_SHELL_SETTING_LEN} bytes"
                )
            );
        }
    }
}
