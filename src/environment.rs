// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::os::unix::ffi::OsStrExt;

use crate::config::{
    AssignmentTarget, MAX_ASSIGNMENT_NAME_LEN, MAX_ASSIGNMENT_VALUE_LEN, MAX_POSITIONAL_ARGUMENTS,
    MAX_SHELL_SETTING_LEN, variable_policy,
};
use crate::runtime::RuntimeVariables;

pub const DEFAULT_SHELL: &str = "/bin/sh";
pub const DEFAULT_SHELL_FLAGS: &str = "-c";
pub const DEFAULT_PATH: &str = "/usr/bin:/bin";
pub const MAX_CHILD_ENVIRONMENT_VARIABLES: usize = 512;
pub const MAX_CHILD_ENVIRONMENT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEnvironment {
    values: BTreeMap<String, Vec<u8>>,
    positional_arguments: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEnvironmentError {
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPolicy {
    approved_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellInvocation<'a> {
    path: &'a str,
    flags: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPolicyError {
    message: String,
}

impl ProcessEnvironment {
    pub fn from_runtime(runtime: &RuntimeVariables) -> Result<Self, ProcessEnvironmentError> {
        let mut values = BTreeMap::new();

        // Build a fresh map instead of starting from std::env. This makes the
        // future spawn path independent of secrets and behavior-changing
        // values inherited by the procmail-rs process.
        for (name, value) in runtime.byte_values() {
            if crate::config::is_positional_parameter_name(name) {
                continue;
            }
            validate_entry(name, value)?;
            values.insert(name.to_owned(), value.to_owned());
        }
        for (name, value) in [
            ("SHELL", DEFAULT_SHELL),
            ("SHELLFLAGS", DEFAULT_SHELL_FLAGS),
            ("PATH", DEFAULT_PATH),
        ] {
            values
                .entry(name.to_owned())
                .or_insert_with(|| value.as_bytes().to_vec());
        }
        validate_aggregate(&values)?;

        // Keep positional values out of the child environment while retaining
        // their argv boundaries. A later "$@" expansion can then pass empty
        // and whitespace-containing arguments to the shell without quoting
        // user data into the command text.
        let positional_count = runtime
            .get("#")
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|_| error("invalid positional argument count"))?;
        if positional_count > MAX_POSITIONAL_ARGUMENTS {
            return Err(error("positional argument count exceeds its hard limit"));
        }
        let mut positional_arguments = Vec::new();
        positional_arguments
            .try_reserve_exact(positional_count)
            .map_err(|_| error("cannot reserve positional arguments"))?;
        for index in 1..=positional_count {
            let value = runtime.get_bytes(&index.to_string()).unwrap_or(b"");
            if value.contains(&0) {
                return Err(error("positional argument contains NUL"));
            }
            positional_arguments.push(value.to_vec());
        }
        Ok(Self {
            values,
            positional_arguments,
        })
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|value| std::str::from_utf8(value).ok())
    }

    pub fn values(&self) -> impl Iterator<Item = (&str, &OsStr)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), OsStr::from_bytes(value)))
    }

    pub(crate) fn positional_arguments(&self) -> impl Iterator<Item = &OsStr> {
        self.positional_arguments
            .iter()
            .map(|value| OsStr::from_bytes(value))
    }
}

impl ShellPolicy {
    pub fn disabled() -> Self {
        Self {
            approved_path: None,
        }
    }

    pub fn approve(path: &str) -> Result<Self, ShellPolicyError> {
        validate_shell_path(path)?;
        Ok(Self {
            approved_path: Some(path.to_owned()),
        })
    }

    pub fn authorize<'a>(
        &self,
        environment: &'a ProcessEnvironment,
    ) -> Result<ShellInvocation<'a>, ShellPolicyError> {
        let approved = self
            .approved_path
            .as_deref()
            .ok_or_else(|| shell_policy_error("shell execution is disabled by operator policy"))?;
        let configured = environment
            .get("SHELL")
            .expect("process environment always supplies SHELL");
        if configured != approved {
            return Err(shell_policy_error(
                "configured SHELL does not match the operator-approved shell",
            ));
        }
        let flags = environment
            .get("SHELLFLAGS")
            .expect("process environment always supplies SHELLFLAGS");
        Ok(ShellInvocation {
            path: configured,
            flags,
        })
    }
}

impl ShellInvocation<'_> {
    pub fn path(&self) -> &str {
        self.path
    }

    pub fn flags(&self) -> &str {
        self.flags
    }
}

impl fmt::Display for ProcessEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProcessEnvironmentError {}

impl fmt::Display for ShellPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ShellPolicyError {}

fn validate_shell_path(path: &str) -> Result<(), ShellPolicyError> {
    if path.is_empty() || path.len() > MAX_SHELL_SETTING_LEN || path.as_bytes().contains(&0) {
        return Err(shell_policy_error(
            "approved shell must be a non-empty bounded path without NUL",
        ));
    }
    if !path.starts_with('/')
        || path.ends_with('/')
        || path[1..]
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(shell_policy_error(
            "approved shell must be an absolute path without '.' or '..' components",
        ));
    }
    Ok(())
}

fn validate_entry(name: &str, value: &[u8]) -> Result<(), ProcessEnvironmentError> {
    if name.is_empty()
        || name.len() > MAX_ASSIGNMENT_NAME_LEN
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphanumeric() && (index > 0 || !byte.is_ascii_digit())
        })
    {
        return Err(error("child environment contains an invalid variable name"));
    }
    if value.contains(&0) {
        return Err(error(format!(
            "child environment variable {name} contains NUL"
        )));
    }
    let limit = variable_policy(name)
        .assignment_target(crate::config::VariableSource::RcFile)
        .map_or(MAX_ASSIGNMENT_VALUE_LEN, AssignmentTarget::value_limit);
    if value.len() > limit {
        return Err(error(format!(
            "child environment variable {name} exceeds the hard limit of {limit} bytes"
        )));
    }
    Ok(())
}

fn validate_aggregate(values: &BTreeMap<String, Vec<u8>>) -> Result<(), ProcessEnvironmentError> {
    if values.len() > MAX_CHILD_ENVIRONMENT_VARIABLES {
        return Err(error(format!(
            "child environment variable count exceeds the hard limit of {MAX_CHILD_ENVIRONMENT_VARIABLES}"
        )));
    }
    let mut bytes = 0usize;
    for (name, value) in values {
        bytes = bytes
            .checked_add(name.len())
            .and_then(|size| size.checked_add(value.len()))
            .and_then(|size| size.checked_add(2))
            .ok_or_else(|| error("child environment size overflows"))?;
        if bytes > MAX_CHILD_ENVIRONMENT_BYTES {
            return Err(error(format!(
                "child environment exceeds the hard limit of {MAX_CHILD_ENVIRONMENT_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

fn error(message: impl Into<String>) -> ProcessEnvironmentError {
    ProcessEnvironmentError {
        message: message.into(),
    }
}

fn shell_policy_error(message: impl Into<String>) -> ShellPolicyError {
    ShellPolicyError {
        message: message.into(),
    }
}

#[cfg(test)]
#[path = "tests/environment.rs"]
mod tests;
