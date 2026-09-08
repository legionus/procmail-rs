// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use crate::bounded_bytes::{BoundedBytes, BoundedBytesError};

use super::{MAX_EXPANSION_DEPTH, ParameterOperation, ShellExpression, ShellPart};

#[cfg(test)]
#[path = "../tests/config/shell_eval.rs"]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvaluationDepth {
    None,
    ExpansionChain,
    SyntaxNesting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnsupportedPart {
    Command,
    RegexQuotedVariable,
}

pub(crate) struct VariableValue {
    pub(crate) bytes: Vec<u8>,
    pub(crate) depth: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EvaluationResult {
    pub(crate) bytes: Vec<u8>,
    pub(crate) depth: usize,
}

pub(crate) trait EvaluationContext {
    type Error;

    fn depth_mode(&self) -> EvaluationDepth;
    fn variable(&mut self, name: &str) -> Result<Option<VariableValue>, Self::Error>;
    fn command(&mut self, command: &str, remaining: usize) -> Result<Vec<u8>, Self::Error>;
    fn regex_quoted(&mut self, name: &str, remaining: usize) -> Result<Vec<u8>, Self::Error>;
    fn missing_variable(&self, name: &str) -> Self::Error;
    fn unsupported_part(&self, part: UnsupportedPart) -> Self::Error;
    fn depth_exceeded(&self) -> Self::Error;
    fn depth_overflow(&self) -> Self::Error;
    fn length_error(&self, error: BoundedBytesError, current: usize, limit: usize) -> Self::Error;
}

pub(crate) fn evaluate<C: EvaluationContext>(
    expression: &ShellExpression,
    limit: usize,
    context: &mut C,
) -> Result<EvaluationResult, C::Error> {
    evaluate_at(expression, limit, 0, context)
}

fn evaluate_at<C: EvaluationContext>(
    expression: &ShellExpression,
    limit: usize,
    nesting: usize,
    context: &mut C,
) -> Result<EvaluationResult, C::Error> {
    if nesting > MAX_EXPANSION_DEPTH {
        return Err(context.depth_exceeded());
    }
    let mut output = BoundedBytes::with_capacity(limit, 0);
    let mut result_depth = match context.depth_mode() {
        EvaluationDepth::SyntaxNesting => nesting,
        EvaluationDepth::None | EvaluationDepth::ExpansionChain => 0,
    };

    // Every context uses the same private bounded accumulator. A failed
    // default, command, or quoted value therefore cannot expose a partial
    // result, while adapters retain control over which parts are permitted.
    for part in &expression.parts {
        match part {
            ShellPart::Literal(text) => append(&mut output, text.as_bytes(), limit, context)?,
            ShellPart::Variable { name, operation } => {
                let found = context.variable(name)?;
                let is_set = found.is_some();
                let is_empty = found.as_ref().is_some_and(|value| value.bytes.is_empty());
                let (selected, used_word, empty_value) =
                    if let Some(word) = operation.selected_word(is_set, is_empty) {
                        (
                            evaluate_word(word, &output, limit, nesting, context)?,
                            true,
                            false,
                        )
                    } else if operation.requires_value()
                        || matches!(
                            operation,
                            ParameterOperation::DefaultIfUnset(_)
                                | ParameterOperation::DefaultIfUnsetOrEmpty(_)
                        )
                    {
                        match found {
                            Some(value) => {
                                let empty = value.bytes.is_empty();
                                (value, false, empty)
                            }
                            None => return Err(context.missing_variable(name)),
                        }
                    } else {
                        (
                            VariableValue {
                                bytes: Vec::new(),
                                depth: 0,
                            },
                            false,
                            true,
                        )
                    };
                result_depth = result_depth.max(selected_depth(
                    context.depth_mode(),
                    selected.depth,
                    nesting,
                    used_word,
                    empty_value,
                    context,
                )?);
                append(&mut output, &selected.bytes, limit, context)?;
            }
            ShellPart::Command(command) => {
                let remaining = remaining(&output, limit, context)?;
                let value = context.command(command, remaining)?;
                append(&mut output, &value, limit, context)?;
            }
            ShellPart::RegexQuotedVariable(name) => {
                let remaining = remaining(&output, limit, context)?;
                let value = context.regex_quoted(name, remaining)?;
                append(&mut output, &value, limit, context)?;
            }
        }
    }
    Ok(EvaluationResult {
        bytes: output.into_vec(),
        depth: result_depth,
    })
}

fn evaluate_word<C: EvaluationContext>(
    word: &ShellExpression,
    output: &BoundedBytes,
    limit: usize,
    nesting: usize,
    context: &mut C,
) -> Result<VariableValue, C::Error> {
    let remaining = remaining(output, limit, context)?;
    let nested = nesting
        .checked_add(1)
        .ok_or_else(|| context.depth_overflow())?;
    let value = evaluate_at(word, remaining, nested, context)?;
    Ok(VariableValue {
        bytes: value.bytes,
        depth: value.depth,
    })
}

fn selected_depth<C: EvaluationContext>(
    mode: EvaluationDepth,
    value_depth: usize,
    nesting: usize,
    used_word: bool,
    empty_value: bool,
    context: &C,
) -> Result<usize, C::Error> {
    let depth = match mode {
        EvaluationDepth::None => 0,
        EvaluationDepth::ExpansionChain => value_depth
            .checked_add(1)
            .ok_or_else(|| context.depth_overflow())?,
        EvaluationDepth::SyntaxNesting if used_word => value_depth,
        EvaluationDepth::SyntaxNesting if empty_value => nesting,
        EvaluationDepth::SyntaxNesting => nesting
            .checked_add(1)
            .ok_or_else(|| context.depth_overflow())?,
    };
    if depth > MAX_EXPANSION_DEPTH {
        return Err(context.depth_exceeded());
    }
    Ok(depth)
}

fn remaining<C: EvaluationContext>(
    output: &BoundedBytes,
    limit: usize,
    context: &C,
) -> Result<usize, C::Error> {
    output
        .remaining()
        .map_err(|error| context.length_error(error, output.len(), limit))
}

fn append<C: EvaluationContext>(
    output: &mut BoundedBytes,
    value: &[u8],
    limit: usize,
    context: &C,
) -> Result<(), C::Error> {
    output
        .try_extend(value)
        .map_err(|error| context.length_error(error, output.len(), limit))
}
