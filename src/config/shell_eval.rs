// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use crate::bounded_bytes::{BoundedBytes, BoundedBytesError};

use super::shell_pattern::{self, Edge, PatternError, Selection};
use super::{CaseDirection, MAX_EXPANSION_DEPTH, ParameterOperation, ShellExpression, ShellPart};

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
    pub(crate) assignments: Vec<(String, Vec<u8>)>,
}

pub(crate) trait EvaluationContext {
    type Error;

    fn depth_mode(&self) -> EvaluationDepth;
    fn variable(&mut self, name: &str) -> Result<Option<VariableValue>, Self::Error>;
    fn command(&mut self, command: &str, remaining: usize) -> Result<Vec<u8>, Self::Error>;
    fn regex_quoted(&mut self, name: &str, remaining: usize) -> Result<Vec<u8>, Self::Error>;
    fn missing_variable(&self, name: &str) -> Self::Error;
    fn required_parameter(&self, name: &str) -> Self::Error;
    fn pattern_error(&self, error: PatternError) -> Self::Error;
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
    let mut assignments = Vec::new();
    let mut result = evaluate_at(expression, limit, 0, context, &mut assignments)?;
    result.assignments = assignments;
    Ok(result)
}

fn evaluate_at<C: EvaluationContext>(
    expression: &ShellExpression,
    limit: usize,
    nesting: usize,
    context: &mut C,
    assignments: &mut Vec<(String, Vec<u8>)>,
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
                let assigned = assignments
                    .iter()
                    .rev()
                    .find(|(assigned, _)| assigned == name)
                    .map(|(_, bytes)| VariableValue {
                        bytes: bytes.clone(),
                        depth: 0,
                    });
                let found = match assigned {
                    Some(value) => Some(value),
                    None => context.variable(name)?,
                };
                let is_set = found.is_some();
                let is_empty = found.as_ref().is_some_and(|value| value.bytes.is_empty());
                if matches!(operation, ParameterOperation::Length) {
                    let value = found.ok_or_else(|| context.missing_variable(name))?;
                    result_depth = result_depth.max(value.depth);
                    append(
                        &mut output,
                        value.bytes.len().to_string().as_bytes(),
                        limit,
                        context,
                    )?;
                    continue;
                }
                if let Some((pattern, edge, selection)) = removal_operation(operation) {
                    let value = found.ok_or_else(|| context.missing_variable(name))?;
                    let pattern =
                        evaluate_word(pattern, &output, limit, nesting, context, assignments)?;
                    let selected =
                        shell_pattern::remove(&value.bytes, &pattern.bytes, edge, selection)
                            .map_err(|error| context.pattern_error(error))?;
                    result_depth = result_depth.max(value.depth).max(pattern.depth);
                    append(&mut output, &selected, limit, context)?;
                    continue;
                }
                if let ParameterOperation::ChangeCase {
                    pattern,
                    direction,
                    all,
                } = operation
                {
                    let value = found.ok_or_else(|| context.missing_variable(name))?;
                    let pattern =
                        evaluate_word(pattern, &output, limit, nesting, context, assignments)?;
                    let transformed =
                        change_case(&value.bytes, &pattern.bytes, *direction, *all, context)?;
                    result_depth = result_depth.max(value.depth).max(pattern.depth);
                    append(&mut output, &transformed, limit, context)?;
                    continue;
                }
                if matches!(operation, ParameterOperation::ErrorIfUnsetOrEmpty(_))
                    && (!is_set || is_empty)
                {
                    return Err(context.required_parameter(name));
                }
                let (selected, used_word, empty_value) =
                    if let Some(word) = operation.selected_word(is_set, is_empty) {
                        (
                            evaluate_word(word, &output, limit, nesting, context, assignments)?,
                            true,
                            false,
                        )
                    } else if operation.uses_value_when_word_is_not_selected() {
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
                if matches!(operation, ParameterOperation::AssignIfUnsetOrEmpty(_))
                    && (!is_set || is_empty)
                {
                    if let Some((_, value)) = assignments
                        .iter_mut()
                        .find(|(assigned, _)| assigned == name)
                    {
                        *value = selected.bytes.clone();
                    } else {
                        assignments.push((name.clone(), selected.bytes.clone()));
                    }
                }
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
            ShellPart::PatternQuote(expression) => {
                let remaining = remaining(&output, limit, context)?;
                let value = evaluate_at(expression, remaining, nesting, context, assignments)?;
                let quoted = quote_pattern_bytes(&value.bytes, remaining, context)?;
                append(&mut output, &quoted, limit, context)?;
                result_depth = result_depth.max(value.depth);
            }
        }
    }
    Ok(EvaluationResult {
        bytes: output.into_vec(),
        depth: result_depth,
        assignments: Vec::new(),
    })
}

fn change_case<C: EvaluationContext>(
    value: &[u8],
    pattern: &[u8],
    direction: CaseDirection,
    all: bool,
    context: &C,
) -> Result<Vec<u8>, C::Error> {
    shell_pattern::transform_matching_bytes(value, pattern, all, |byte| match direction {
        CaseDirection::Upper => byte.to_ascii_uppercase(),
        CaseDirection::Lower => byte.to_ascii_lowercase(),
    })
    .map_err(|error| context.pattern_error(error))
}

fn removal_operation(
    operation: &ParameterOperation,
) -> Option<(&ShellExpression, Edge, Selection)> {
    match operation {
        ParameterOperation::RemovePrefix { pattern, longest } => Some((
            pattern,
            Edge::Prefix,
            if *longest {
                Selection::Longest
            } else {
                Selection::Shortest
            },
        )),
        ParameterOperation::RemoveSuffix { pattern, longest } => Some((
            pattern,
            Edge::Suffix,
            if *longest {
                Selection::Longest
            } else {
                Selection::Shortest
            },
        )),
        _ => None,
    }
}

fn quote_pattern_bytes<C: EvaluationContext>(
    bytes: &[u8],
    limit: usize,
    context: &C,
) -> Result<Vec<u8>, C::Error> {
    let mut quoted = BoundedBytes::with_capacity(limit, 0);
    for &byte in bytes {
        if matches!(byte, b'*' | b'?' | b'[' | b'\\') {
            let current = quoted.len();
            quoted
                .try_extend(b"\\")
                .map_err(|error| context.length_error(error, current, limit))?;
        }
        let current = quoted.len();
        quoted
            .try_extend(&[byte])
            .map_err(|error| context.length_error(error, current, limit))?;
    }
    Ok(quoted.into_vec())
}

fn evaluate_word<C: EvaluationContext>(
    word: &ShellExpression,
    output: &BoundedBytes,
    limit: usize,
    nesting: usize,
    context: &mut C,
    assignments: &mut Vec<(String, Vec<u8>)>,
) -> Result<VariableValue, C::Error> {
    let remaining = remaining(output, limit, context)?;
    let nested = nesting
        .checked_add(1)
        .ok_or_else(|| context.depth_overflow())?;
    let value = evaluate_at(word, remaining, nested, context, assignments)?;
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
