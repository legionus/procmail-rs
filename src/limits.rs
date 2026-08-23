// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use crate::config::{AssignmentTarget, Config, MessageLimitVariable, Statement};

pub use crate::config::MAX_RC_SIZE;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const GIB: usize = 1024 * MIB;

pub const MAX_MESSAGE_SIZE: usize = 256 * MIB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageLimits {
    pub(crate) message_size: usize,
    pub(crate) headers_size: usize,
    pub(crate) body_size: usize,
    pub(crate) header_line_size: usize,
    pub(crate) header_field_size: usize,
}

impl Default for MessageLimits {
    fn default() -> Self {
        Self {
            message_size: 64 * MIB,
            headers_size: 256 * KIB,
            body_size: 64 * MIB,
            header_line_size: 64 * KIB,
            header_field_size: 256 * KIB,
        }
    }
}

impl MessageLimits {
    pub fn from_config(config: &Config) -> Result<Self, LimitConfigError> {
        let mut limits = Self::default();

        for statement in &config.statements {
            let Statement::Assignment(assignment) = statement else {
                continue;
            };
            let AssignmentTarget::MessageLimit(variable) = assignment.target else {
                continue;
            };
            let (target, ceiling) = limit_target(&mut limits, variable);
            let value = parse_size(&assignment.value).map_err(|reason| LimitConfigError {
                line: assignment.line,
                name: assignment.name.clone(),
                reason,
            })?;
            if value > ceiling {
                return Err(LimitConfigError {
                    line: assignment.line,
                    name: assignment.name.clone(),
                    reason: format!("value exceeds the hard ceiling of {ceiling} bytes"),
                });
            }
            *target = value;
        }

        Ok(limits)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitConfigError {
    pub line: usize,
    pub name: String,
    pub reason: String,
}

impl fmt::Display for LimitConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "line {}: invalid {}: {}",
            self.line, self.name, self.reason
        )
    }
}

impl std::error::Error for LimitConfigError {}

fn limit_target(limits: &mut MessageLimits, variable: MessageLimitVariable) -> (&mut usize, usize) {
    match variable {
        MessageLimitVariable::MessageSize => (&mut limits.message_size, MAX_MESSAGE_SIZE),
        MessageLimitVariable::HeadersSize => (&mut limits.headers_size, 16 * MIB),
        MessageLimitVariable::BodySize => (&mut limits.body_size, MAX_MESSAGE_SIZE),
        MessageLimitVariable::HeaderLineSize => (&mut limits.header_line_size, MIB),
        MessageLimitVariable::HeaderFieldSize => (&mut limits.header_field_size, 16 * MIB),
    }
}

fn parse_size(input: &str) -> Result<usize, String> {
    if input.is_empty() || input.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err("expected a non-negative byte count with an optional K, M, or G suffix".into());
    }

    let (digits, multiplier) = match input.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&input[..input.len() - 1], KIB as u64),
        Some(b'm' | b'M') => (&input[..input.len() - 1], MIB as u64),
        Some(b'g' | b'G') => (&input[..input.len() - 1], GIB as u64),
        Some(byte) if byte.is_ascii_digit() => (input, 1),
        _ => return Err("unknown size suffix; use K, M, or G".into()),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("expected a non-negative integer before the size suffix".into());
    }

    let value = digits
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "size overflows the supported range".to_owned())?;
    Ok(value)
}

#[cfg(test)]
mod tests;
