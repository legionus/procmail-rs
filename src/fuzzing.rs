// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Narrow entry points used only by the separately built fuzz package.

use crate::bounded_bytes::BoundedBytesError;
use crate::config::expand::{parse_assignment_word, parse_shell_condition_expression};
use crate::config::shell_eval::{
    self, EvaluationContext, EvaluationDepth, UnsupportedPart, VariableValue,
};
use crate::config::shell_pattern::{self, Edge, PatternError, Selection};

const MAX_FUZZ_OUTPUT: usize = 4096;

pub struct ShellEvaluationSummary {
    pub limit: usize,
    pub output_len: usize,
    pub assignment_lengths: Vec<usize>,
}

pub fn shell_expression(data: &[u8]) -> Option<ShellEvaluationSummary> {
    let (selector, limit, source) = split_expression_input(data)?;
    let source = std::str::from_utf8(source).ok()?;
    let expression = if selector & 1 == 0 {
        parse_assignment_word(source, 1).ok()?.expression
    } else {
        parse_shell_condition_expression(source, 1).ok()?
    };
    let mut context = FuzzContext { selector, data };
    let evaluated = shell_eval::evaluate(&expression, limit, &mut context).ok()?;
    Some(ShellEvaluationSummary {
        limit,
        output_len: evaluated.bytes.len(),
        assignment_lengths: evaluated
            .assignments
            .iter()
            .map(|(_, value)| value.len())
            .collect(),
    })
}

pub fn shell_condition(data: &[u8]) -> Option<ShellEvaluationSummary> {
    let (selector, limit, source) = split_expression_input(data)?;
    let source = std::str::from_utf8(source).ok()?;
    let expression = parse_shell_condition_expression(source, 1).ok()?;
    let mut context = FuzzContext { selector, data };
    let evaluated = shell_eval::evaluate(&expression, limit, &mut context).ok()?;
    if let Ok(expanded) = std::str::from_utf8(&evaluated.bytes) {
        let _ =
            crate::config::parse_reparsed_condition(expanded.trim_start(), 1, selector & 2 != 0);
    }
    Some(ShellEvaluationSummary {
        limit,
        output_len: evaluated.bytes.len(),
        assignment_lengths: evaluated
            .assignments
            .iter()
            .map(|(_, value)| value.len())
            .collect(),
    })
}

pub fn shell_pattern(data: &[u8]) {
    let split = data
        .first()
        .map_or(0, |byte| usize::from(*byte) % data.len().max(1));
    let (value, pattern) = data.split_at(split);

    // Exercise every selection direction from the same arbitrary byte input.
    // Keeping this matrix here prevents individual targets from drifting away
    // from operations used by parameter expansion.
    for edge in [Edge::Prefix, Edge::Suffix] {
        for selection in [Selection::Shortest, Selection::Longest] {
            let _ = shell_pattern::remove(value, pattern, edge, selection);
        }
    }
    for all in [false, true] {
        let _ = shell_pattern::transform_matching_bytes(value, pattern, all, |byte| {
            if byte.is_ascii_lowercase() {
                byte.to_ascii_uppercase()
            } else {
                byte
            }
        });
    }
}

fn split_expression_input(data: &[u8]) -> Option<(u8, usize, &[u8])> {
    let (&selector, rest) = data.split_first()?;
    let (&limit_byte, source) = rest.split_first()?;
    let limit = usize::from(limit_byte)
        .checked_mul(MAX_FUZZ_OUTPUT)?
        .checked_div(usize::from(u8::MAX))?;
    Some((selector, limit, source))
}

struct FuzzContext<'a> {
    selector: u8,
    data: &'a [u8],
}

impl EvaluationContext for FuzzContext<'_> {
    type Error = ();

    fn depth_mode(&self) -> EvaluationDepth {
        match self.selector % 3 {
            0 => EvaluationDepth::None,
            1 => EvaluationDepth::ExpansionChain,
            _ => EvaluationDepth::SyntaxNesting,
        }
    }

    fn variable(&mut self, name: &str) -> Result<Option<VariableValue>, Self::Error> {
        let choice = name
            .bytes()
            .fold(self.selector, |state, byte| state.wrapping_add(byte))
            % 3;
        Ok(match choice {
            0 => None,
            1 => Some(VariableValue {
                bytes: Vec::new(),
                depth: 0,
            }),
            _ => Some(VariableValue {
                bytes: self.data.to_vec(),
                depth: usize::from(self.selector % 4),
            }),
        })
    }

    fn command(&mut self, _command: &str, remaining: usize) -> Result<Vec<u8>, Self::Error> {
        Ok(self.data[..self.data.len().min(remaining)].to_vec())
    }

    fn regex_quoted(&mut self, _name: &str, remaining: usize) -> Result<Vec<u8>, Self::Error> {
        const MOCK_QUOTED_VALUE: &[u8] = b"(?:fuzz)";
        Ok(MOCK_QUOTED_VALUE[..MOCK_QUOTED_VALUE.len().min(remaining)].to_vec())
    }

    fn missing_variable(&self, _name: &str) -> Self::Error {}

    fn required_parameter(&self, _name: &str) -> Self::Error {}

    fn pattern_error(&self, _error: PatternError) -> Self::Error {}

    fn unsupported_part(&self, _part: UnsupportedPart) -> Self::Error {}

    fn depth_exceeded(&self) -> Self::Error {}

    fn depth_overflow(&self) -> Self::Error {}

    fn length_error(
        &self,
        _error: BoundedBytesError,
        _current: usize,
        _limit: usize,
    ) -> Self::Error {
    }
}
