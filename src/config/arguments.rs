// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

pub const MAX_POSITIONAL_ARGUMENTS: usize = 256;
pub const MAX_POSITIONAL_ARGUMENT_LEN: usize = 64 * 1024;
pub const MAX_POSITIONAL_ARGUMENT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PositionalArguments {
    values: Vec<String>,
    bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionalArgumentError {
    message: String,
}

impl PositionalArguments {
    pub fn push(&mut self, value: String) -> Result<(), PositionalArgumentError> {
        if self.values.len() == MAX_POSITIONAL_ARGUMENTS {
            return Err(error(format!(
                "too many -a/--argument values; hard limit is {MAX_POSITIONAL_ARGUMENTS}"
            )));
        }
        if value.len() > MAX_POSITIONAL_ARGUMENT_LEN {
            return Err(error(format!(
                "-a/--argument value exceeds the hard limit of {MAX_POSITIONAL_ARGUMENT_LEN} bytes"
            )));
        }
        let bytes = self
            .bytes
            .checked_add(value.len())
            .ok_or_else(|| error("positional argument size overflows"))?;
        if bytes > MAX_POSITIONAL_ARGUMENT_BYTES {
            return Err(error(format!(
                "positional arguments exceed the aggregate hard limit of {MAX_POSITIONAL_ARGUMENT_BYTES} bytes"
            )));
        }
        self.values.push(value);
        self.bytes = bytes;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub(crate) fn values(&self) -> &[String] {
        &self.values
    }
}

pub(crate) fn is_positional_parameter_name(name: &str) -> bool {
    name == "#"
        || (!name.is_empty()
            && name.bytes().all(|byte| byte.is_ascii_digit())
            && name
                .parse::<usize>()
                .is_ok_and(|index| (1..=MAX_POSITIONAL_ARGUMENTS).contains(&index)))
}

impl fmt::Display for PositionalArgumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PositionalArgumentError {}

fn error(message: impl Into<String>) -> PositionalArgumentError {
    PositionalArgumentError {
        message: message.into(),
    }
}

#[cfg(test)]
#[path = "../tests/config/arguments.rs"]
mod tests;
