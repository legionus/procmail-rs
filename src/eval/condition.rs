// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use regex::bytes::Regex;

use super::message::CompleteMessage;
use super::{ConditionExplanation, ConditionKindExplanation, EvalError, InputRequirements};
use crate::config::{ConditionInput, ConditionKind, Recipe, RegexCondition};
use crate::message::MessageHead;
use crate::runtime::RuntimeVariables;
use crate::trace::{ConditionKind as TraceConditionKind, TraceEvent, TraceSink};

#[derive(Debug, Clone)]
pub(super) struct CompiledCondition {
    pub(super) line: usize,
    negated: bool,
    kind: CompiledConditionKind,
    match_capture: Option<usize>,
    capture_indexes: Vec<usize>,
}

#[derive(Debug, Clone)]
enum CompiledConditionKind {
    HeaderRegex(Regex),
    BodyRegex(Regex),
    MessageRegex(Regex),
    VariableRegex {
        name: String,
        regex: Regex,
    },
    Program {
        command: String,
        input: ConditionInput,
    },
    SmallerThan(usize),
    LargerThan(usize),
}

pub(super) fn compile_conditions(recipe: &Recipe) -> Vec<CompiledCondition> {
    let area = match recipe.options.condition_input {
        ConditionInput::Headers => RegexArea::Headers,
        ConditionInput::Body => RegexArea::Body,
        ConditionInput::Message => RegexArea::Message,
    };
    let mut conditions = Vec::with_capacity(recipe.conditions.len());

    for condition in &recipe.conditions {
        let regex_condition = match &condition.kind {
            ConditionKind::Regex(regex)
            | ConditionKind::AreaRegex { regex, .. }
            | ConditionKind::VariableRegex { regex, .. } => Some(regex),
            ConditionKind::Program(_)
            | ConditionKind::SmallerThan(_)
            | ConditionKind::LargerThan(_) => None,
        };
        let kind = match &condition.kind {
            ConditionKind::SmallerThan(size) => CompiledConditionKind::SmallerThan(*size),
            ConditionKind::LargerThan(size) => CompiledConditionKind::LargerThan(*size),
            ConditionKind::Regex(regex) => {
                // Parsing already validated and compiled this expression.
                // Cloning Regex shares its read-only compiled program, so
                // execution planning cannot repeat attacker-controlled
                // compilation work after configuration validation.
                let regex = regex.compiled().clone();
                match area {
                    RegexArea::Headers => CompiledConditionKind::HeaderRegex(regex),
                    RegexArea::Body => CompiledConditionKind::BodyRegex(regex),
                    RegexArea::Message => CompiledConditionKind::MessageRegex(regex),
                }
            }
            ConditionKind::AreaRegex { area, regex } => {
                let regex = regex.compiled().clone();
                match area {
                    ConditionInput::Headers => CompiledConditionKind::HeaderRegex(regex),
                    ConditionInput::Body => CompiledConditionKind::BodyRegex(regex),
                    ConditionInput::Message => CompiledConditionKind::MessageRegex(regex),
                }
            }
            ConditionKind::VariableRegex { name, regex } => CompiledConditionKind::VariableRegex {
                name: name.clone(),
                regex: regex.compiled().clone(),
            },
            ConditionKind::Program(command) => CompiledConditionKind::Program {
                command: command.clone(),
                input: recipe.options.condition_input,
            },
        };
        conditions.push(CompiledCondition {
            line: condition.line,
            negated: condition.negated,
            kind,
            match_capture: regex_condition.and_then(RegexCondition::match_capture),
            capture_indexes: regex_condition
                .map(|regex| regex.capture_indexes().to_vec())
                .unwrap_or_default(),
        });
    }

    conditions
}

impl CompiledCondition {
    pub(super) fn requires_ordered_execution(&self) -> bool {
        matches!(self.kind, CompiledConditionKind::Program { .. })
    }

    pub(super) fn needs_message_contents(&self) -> bool {
        matches!(self.kind, CompiledConditionKind::MessageRegex(_))
    }

    pub(super) fn program(&self) -> Option<(&str, ConditionInput)> {
        match &self.kind {
            CompiledConditionKind::Program { command, input } => Some((command, *input)),
            _ => None,
        }
    }

    pub(super) fn apply_negation(&self, matched: bool) -> bool {
        matched ^ self.negated
    }

    pub(super) fn trace_result(
        &self,
        recipe_line: usize,
        condition_index: usize,
        result: PartialMatch,
        trace: &mut impl TraceSink,
    ) {
        let matched = match result {
            PartialMatch::True => true,
            PartialMatch::False => false,
            PartialMatch::Deferred => return,
        };
        let kind = match &self.kind {
            CompiledConditionKind::HeaderRegex(_) => TraceConditionKind::HeaderRegex,
            CompiledConditionKind::BodyRegex(_) => TraceConditionKind::BodyRegex,
            CompiledConditionKind::MessageRegex(_) => TraceConditionKind::MessageRegex,
            CompiledConditionKind::VariableRegex { .. } => TraceConditionKind::VariableRegex,
            CompiledConditionKind::Program { .. } => TraceConditionKind::Program,
            CompiledConditionKind::SmallerThan(_) => TraceConditionKind::SmallerThan,
            CompiledConditionKind::LargerThan(_) => TraceConditionKind::LargerThan,
        };
        trace.record(TraceEvent::ConditionEvaluated {
            recipe_line,
            condition_line: self.line,
            condition_index,
            kind,
            negated: self.negated,
            matched,
        });
    }

    pub(super) fn explain(&self) -> ConditionExplanation {
        let kind = match &self.kind {
            CompiledConditionKind::HeaderRegex(_) => ConditionKindExplanation::HeaderRegex,
            CompiledConditionKind::BodyRegex(_) => ConditionKindExplanation::BodyRegex,
            CompiledConditionKind::MessageRegex(_) => ConditionKindExplanation::MessageRegex,
            CompiledConditionKind::VariableRegex { .. } => ConditionKindExplanation::VariableRegex,
            CompiledConditionKind::Program { .. } => ConditionKindExplanation::Program,
            CompiledConditionKind::SmallerThan(_) => ConditionKindExplanation::SmallerThan,
            CompiledConditionKind::LargerThan(_) => ConditionKindExplanation::LargerThan,
        };
        ConditionExplanation {
            negated: self.negated,
            kind,
        }
    }

    pub(super) fn requirements(&self) -> InputRequirements {
        match self.kind {
            CompiledConditionKind::HeaderRegex(_) => InputRequirements {
                needs_headers: true,
                ..InputRequirements::default()
            },
            CompiledConditionKind::BodyRegex(_) | CompiledConditionKind::MessageRegex(_) => {
                InputRequirements {
                    needs_headers: true,
                    needs_body_contents: true,
                    needs_end_of_message: true,
                }
            }
            CompiledConditionKind::VariableRegex { .. } => InputRequirements::default(),
            CompiledConditionKind::Program { input, .. } => match input {
                ConditionInput::Headers => InputRequirements {
                    needs_headers: true,
                    needs_end_of_message: true,
                    ..InputRequirements::default()
                },
                ConditionInput::Body | ConditionInput::Message => InputRequirements {
                    needs_headers: true,
                    needs_body_contents: true,
                    needs_end_of_message: true,
                },
            },
            CompiledConditionKind::SmallerThan(_) | CompiledConditionKind::LargerThan(_) => {
                InputRequirements {
                    needs_end_of_message: true,
                    ..InputRequirements::default()
                }
            }
        }
    }

    pub(super) fn matches_headers(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
    ) -> Result<PartialMatch, EvalError> {
        let matched = match &self.kind {
            CompiledConditionKind::HeaderRegex(regex) => {
                self.regex_matches(regex, head.matching_header(), runtime)?
            }
            CompiledConditionKind::BodyRegex(_) | CompiledConditionKind::MessageRegex(_) => {
                return Ok(PartialMatch::Deferred);
            }
            CompiledConditionKind::Program { .. } => return Ok(PartialMatch::Deferred),
            CompiledConditionKind::VariableRegex { name, regex } => {
                let value = runtime.get(name).unwrap_or_default().to_owned();
                if value.len() > crate::config::MAX_ASSIGNMENT_VALUE_LEN {
                    return Err(EvalError::VariableValueTooLarge {
                        name: name.clone(),
                        size: value.len(),
                    });
                }
                self.regex_matches(regex, value.as_bytes(), runtime)?
            }
            CompiledConditionKind::SmallerThan(size) => {
                if head.len() >= *size {
                    false
                } else {
                    return Ok(PartialMatch::Deferred);
                }
            }
            CompiledConditionKind::LargerThan(size) => {
                if head.len() > *size {
                    true
                } else {
                    return Ok(PartialMatch::Deferred);
                }
            }
        };
        Ok(PartialMatch::from_bool(self.apply_negation(matched)))
    }

    pub(super) fn matches_complete(
        &self,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
    ) -> Result<bool, EvalError> {
        let matched = match &self.kind {
            CompiledConditionKind::HeaderRegex(regex) => {
                self.regex_matches(regex, message.header_bytes(), runtime)?
            }
            CompiledConditionKind::BodyRegex(regex) => self.regex_matches(
                regex,
                message.body().ok_or(EvalError::BodyWasNotBuffered)?,
                runtime,
            )?,
            CompiledConditionKind::MessageRegex(regex) => self.regex_matches(
                regex,
                message.full().ok_or(EvalError::BodyWasNotBuffered)?,
                runtime,
            )?,
            CompiledConditionKind::VariableRegex { name, regex } => {
                let value = runtime.get(name).unwrap_or_default().to_owned();
                if value.len() > crate::config::MAX_ASSIGNMENT_VALUE_LEN {
                    return Err(EvalError::VariableValueTooLarge {
                        name: name.clone(),
                        size: value.len(),
                    });
                }
                self.regex_matches(regex, value.as_bytes(), runtime)?
            }
            CompiledConditionKind::Program { .. } => {
                return Err(EvalError::ExternalConditionUnsupported { line: self.line });
            }
            CompiledConditionKind::SmallerThan(size) => message.len() < *size,
            CompiledConditionKind::LargerThan(size) => message.len() > *size,
        };
        Ok(self.apply_negation(matched))
    }

    fn regex_matches(
        &self,
        regex: &Regex,
        input: &[u8],
        runtime: &mut RuntimeVariables,
    ) -> Result<bool, EvalError> {
        if self.match_capture.is_none() && self.capture_indexes.is_empty() {
            return Ok(regex.is_match(input));
        }

        // Captures are runtime variables, so stale values must disappear even
        // when this condition does not match. Validate the complete set before
        // updating the table so no later recipe can observe partial results.
        runtime.clear_match_values();
        let Some(captures) = regex.captures(input) else {
            return Ok(false);
        };
        if self.negated {
            return Ok(true);
        }
        let mut values = Vec::with_capacity(self.capture_indexes.len() + 1);
        if let Some(index) = self.match_capture {
            values.push(("MATCH".to_owned(), capture_value(&captures, index)?));
        }
        for (number, index) in self.capture_indexes.iter().copied().enumerate() {
            values.push((
                format!("MATCH{}", number + 1),
                capture_value(&captures, index)?,
            ));
        }
        let size = values
            .iter()
            .try_fold(0usize, |total, (_, value)| total.checked_add(value.len()));
        let Some(size) = size else {
            return Err(EvalError::MatchValuesTooLarge { size: usize::MAX });
        };
        if size > crate::config::MAX_MATCH_BYTES {
            return Err(EvalError::MatchValuesTooLarge { size });
        }
        for (name, value) in values {
            runtime.set_match_value(name, value);
        }
        Ok(true)
    }
}

fn capture_value(captures: &regex::bytes::Captures<'_>, index: usize) -> Result<String, EvalError> {
    let bytes = captures
        .get(index)
        .map_or(&[][..], |matched| matched.as_bytes());
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| EvalError::MatchValueIsNotUtf8)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PartialMatch {
    True,
    False,
    Deferred,
}

impl PartialMatch {
    pub(super) fn from_bool(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }
}

#[derive(Debug, Clone, Copy)]
enum RegexArea {
    Headers,
    Body,
    Message,
}
