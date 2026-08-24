// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use super::{MAX_ASSIGNMENT_NAME_LEN, MAX_ASSIGNMENT_VALUE_LEN, MAX_SHELL_SETTING_LEN};

pub const MAX_COMMAND_LINE_VARIABLES: usize = 256;
pub const MAX_LOCK_TIMEOUT_SECONDS: u64 = 86_400;
pub const MAX_PROCESS_TIMEOUT_SECONDS: u64 = 86_400;
pub const DEFAULT_LOCK_EXT: &str = ".lock";
pub const UNSUPPORTED_PROCMAIL_VARIABLES: &[&str] = &[
    "DEFAULT",
    "ORGMAIL",
    "COMSAT",
    "DELIVERED",
    "LOG",
    "MSGPREFIX",
    "NORESRETRY",
    "PROCMAIL_OVERFLOW",
    "PROCMAIL_VERSION",
    "SHELLMETAS",
    "SUSPEND",
    "SENDMAIL",
    "SENDMAILFLAGS",
    "SHIFT",
];

pub fn parse_umask(value: &str) -> Result<u32, String> {
    if value.is_empty() || value.len() > 4 || !value.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
    {
        return Err("UMASK must be an octal integer from 0000 to 0777".to_owned());
    }
    let mask = u32::from_str_radix(value, 8)
        .map_err(|_| "UMASK must be an octal integer from 0000 to 0777".to_owned())?;
    if mask > 0o777 {
        return Err("UMASK must be an octal integer from 0000 to 0777".to_owned());
    }
    Ok(mask)
}

pub fn validate_trap_command(value: &str) -> Result<(), String> {
    if value.as_bytes().contains(&0) {
        Err("TRAP command must not contain NUL".to_owned())
    } else {
        Ok(())
    }
}

pub fn validate_lock_ext(value: &str) -> Result<(), String> {
    if value.as_bytes().contains(&0) {
        Err("LOCKEXT must not contain NUL".to_owned())
    } else if value.contains('/') {
        Err("LOCKEXT must not contain '/'".to_owned())
    } else {
        Ok(())
    }
}

pub fn validate_log_abstract(value: &str) -> Result<(), String> {
    if value == "no" {
        Ok(())
    } else {
        Err(
            "LOGABSTRACT supports only 'no'; other values could log sensitive header values"
                .to_owned(),
        )
    }
}

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
    LogAbstract,
    Verbose,
    Durability,
    LockMethod,
    LockFile,
    LockExt,
    LockTimeout,
    LineBuf,
    ProcessTimeout,
    Umask,
    Trap,
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
    System,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariablePolicy {
    RcOnly(AssignmentTarget),
    RcOrCommandLine(AssignmentTarget),
    RuntimeOnly,
    Unsupported,
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
        if policy == VariablePolicy::Unsupported {
            return Err(SuppliedVariableError::new(format!(
                "procmail variable {name} is not supported"
            )));
        }
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

    pub fn from_system_hostname(value: String) -> Result<Self, SuppliedVariableError> {
        if value.is_empty() || value.len() > crate::hostname::MAX_HOSTNAME_LEN {
            return Err(SuppliedVariableError::new(format!(
                "system HOST must contain from 1 through {} bytes",
                crate::hostname::MAX_HOSTNAME_LEN
            )));
        }
        Ok(Self {
            name: "HOST".to_owned(),
            value,
            source: VariableSource::System,
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
        "LOGABSTRACT" => VariablePolicy::RcOnly(AssignmentTarget::LogAbstract),
        "VERBOSE" => VariablePolicy::RcOnly(AssignmentTarget::Verbose),
        "DURABILITY" => VariablePolicy::RcOnly(AssignmentTarget::Durability),
        "LOCKMETHOD" => VariablePolicy::RcOnly(AssignmentTarget::LockMethod),
        "LOCKFILE" => VariablePolicy::RcOnly(AssignmentTarget::LockFile),
        "LOCKEXT" => VariablePolicy::RcOnly(AssignmentTarget::LockExt),
        "LOCKTIMEOUT" => VariablePolicy::RcOnly(AssignmentTarget::LockTimeout),
        "LINEBUF" => VariablePolicy::RcOnly(AssignmentTarget::LineBuf),
        "TIMEOUT" => VariablePolicy::RcOnly(AssignmentTarget::ProcessTimeout),
        "UMASK" => VariablePolicy::RcOnly(AssignmentTarget::Umask),
        "TRAP" => VariablePolicy::RcOnly(AssignmentTarget::Trap),
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
        name if UNSUPPORTED_PROCMAIL_VARIABLES.contains(&name) => VariablePolicy::Unsupported,
        _ => VariablePolicy::RcOrCommandLine(AssignmentTarget::User),
    }
}

pub fn assignment_value_limit(target: AssignmentTarget) -> usize {
    match target {
        AssignmentTarget::Maildir
        | AssignmentTarget::LogFile
        | AssignmentTarget::LockFile
        | AssignmentTarget::LockExt => super::MAX_PATH_EXPRESSION_LEN,
        AssignmentTarget::Shell | AssignmentTarget::ShellFlags | AssignmentTarget::Path => {
            MAX_SHELL_SETTING_LEN
        }
        AssignmentTarget::LogDetail
        | AssignmentTarget::LogAbstract
        | AssignmentTarget::Verbose
        | AssignmentTarget::Durability
        | AssignmentTarget::LockMethod
        | AssignmentTarget::LockTimeout
        | AssignmentTarget::LineBuf
        | AssignmentTarget::ProcessTimeout
        | AssignmentTarget::Umask
        | AssignmentTarget::Trap
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
mod tests;
