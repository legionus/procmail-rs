// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeMap;
use std::fmt;

use crate::config::{
    MAX_ASSIGNMENT_NAME_LEN, MAX_ASSIGNMENT_VALUE_LEN, MAX_SHELL_SETTING_LEN,
    assignment_value_limit, variable_policy,
};
use crate::runtime::RuntimeVariables;

pub const DEFAULT_SHELL: &str = "/bin/sh";
pub const DEFAULT_SHELL_FLAGS: &str = "-c";
pub const DEFAULT_PATH: &str = "/usr/bin:/bin";
pub const MAX_CHILD_ENVIRONMENT_VARIABLES: usize = 512;
pub const MAX_CHILD_ENVIRONMENT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEnvironment {
    values: BTreeMap<String, String>,
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
        for (name, value) in runtime.values() {
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
                .or_insert_with(|| value.to_owned());
        }
        validate_aggregate(&values)?;
        Ok(Self { values })
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn values(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
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

fn validate_entry(name: &str, value: &str) -> Result<(), ProcessEnvironmentError> {
    if name.is_empty()
        || name.len() > MAX_ASSIGNMENT_NAME_LEN
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphanumeric() && (index > 0 || !byte.is_ascii_digit())
        })
    {
        return Err(error("child environment contains an invalid variable name"));
    }
    if value.as_bytes().contains(&0) {
        return Err(error(format!(
            "child environment variable {name} contains NUL"
        )));
    }
    let limit = variable_policy(name)
        .assignment_target(crate::config::VariableSource::RcFile)
        .map_or(MAX_ASSIGNMENT_VALUE_LEN, assignment_value_limit);
    if value.len() > limit {
        return Err(error(format!(
            "child environment variable {name} exceeds the hard limit of {limit} bytes"
        )));
    }
    Ok(())
}

fn validate_aggregate(values: &BTreeMap<String, String>) -> Result<(), ProcessEnvironmentError> {
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
mod tests {
    use super::*;

    #[test]
    fn builds_only_defaults_and_explicit_runtime_values() {
        let mut runtime = RuntimeVariables::default();
        runtime.set("HOME", "/mail/user");
        runtime.set("PATH", "/approved/bin");

        let environment = ProcessEnvironment::from_runtime(&runtime).unwrap();
        assert_eq!(environment.get("HOME"), Some("/mail/user"));
        assert_eq!(environment.get("PATH"), Some("/approved/bin"));
        assert_eq!(environment.get("SHELL"), Some(DEFAULT_SHELL));
        assert_eq!(environment.get("SHELLFLAGS"), Some(DEFAULT_SHELL_FLAGS));
        assert_eq!(environment.values().count(), 4);
    }

    #[test]
    fn rejects_nul_and_oversized_aggregate_environment() {
        let mut runtime = RuntimeVariables::default();
        runtime.set("VALUE", "contains\0nul");
        assert!(ProcessEnvironment::from_runtime(&runtime).is_err());

        let mut runtime = RuntimeVariables::default();
        for index in 0..MAX_CHILD_ENVIRONMENT_VARIABLES {
            runtime.set(format!("V{index}"), "x");
        }
        assert!(ProcessEnvironment::from_runtime(&runtime).is_err());
    }

    #[test]
    fn shell_policy_requires_an_exact_operator_approved_path() {
        let environment = ProcessEnvironment::from_runtime(&RuntimeVariables::default()).unwrap();
        assert!(ShellPolicy::disabled().authorize(&environment).is_err());

        let invocation = ShellPolicy::approve(DEFAULT_SHELL)
            .unwrap()
            .authorize(&environment)
            .unwrap();
        assert_eq!(invocation.path(), DEFAULT_SHELL);
        assert_eq!(invocation.flags(), DEFAULT_SHELL_FLAGS);

        let mut runtime = RuntimeVariables::default();
        runtime.set("SHELL", "/usr/bin/sh");
        let environment = ProcessEnvironment::from_runtime(&runtime).unwrap();
        assert!(
            ShellPolicy::approve(DEFAULT_SHELL)
                .unwrap()
                .authorize(&environment)
                .is_err()
        );
    }

    #[test]
    fn shell_policy_accepts_only_bounded_absolute_normal_paths() {
        for path in [
            "",
            "bin/sh",
            "/",
            "//bin/sh",
            "/bin//sh",
            "/bin/sh/",
            "/bin/./sh",
            "/bin/../bin/sh",
            "/bin/s\0h",
        ] {
            assert!(ShellPolicy::approve(path).is_err(), "{path:?}");
        }
        assert!(ShellPolicy::approve(&format!("/{}", "x".repeat(MAX_SHELL_SETTING_LEN))).is_err());
        assert!(ShellPolicy::approve("/bin/sh").is_ok());
    }
}
