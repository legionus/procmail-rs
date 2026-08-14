// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeMap;
use std::fmt;

use crate::config::{
    MAX_ASSIGNMENT_NAME_LEN, MAX_ASSIGNMENT_VALUE_LEN, assignment_value_limit, variable_policy,
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

impl fmt::Display for ProcessEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProcessEnvironmentError {}

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
}
