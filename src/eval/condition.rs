// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use regex::bytes::Regex;

use super::message::CompleteMessage;
use super::{
    ConditionExplanation, ConditionKindExplanation, EvalError, InputRequirements, PlanProperties,
};
use crate::config::{
    CaseMode, Condition, ConditionInput, ConditionKind, Recipe, RegexCondition,
    ShellExpandedCondition,
};
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
    ShellExpanded {
        condition: ShellExpandedCondition,
        area: RegexArea,
        case_sensitive: bool,
    },
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
    recipe
        .conditions
        .iter()
        .map(|condition| {
            compile_condition(
                condition,
                area,
                recipe.options.case_mode == CaseMode::Sensitive,
            )
        })
        .collect()
}

fn compile_condition(
    condition: &Condition,
    area: RegexArea,
    case_sensitive: bool,
) -> CompiledCondition {
    let regex_condition = match &condition.kind {
        ConditionKind::Regex(regex)
        | ConditionKind::AreaRegex { regex, .. }
        | ConditionKind::VariableRegex { regex, .. } => Some(regex),
        ConditionKind::ShellExpanded(_)
        | ConditionKind::Program(_)
        | ConditionKind::SmallerThan(_)
        | ConditionKind::LargerThan(_) => None,
    };
    let kind = match &condition.kind {
        ConditionKind::ShellExpanded(condition) => CompiledConditionKind::ShellExpanded {
            condition: condition.clone(),
            area,
            case_sensitive,
        },
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
            input: match area {
                RegexArea::Headers => ConditionInput::Headers,
                RegexArea::Body => ConditionInput::Body,
                RegexArea::Message => ConditionInput::Message,
            },
        },
    };
    CompiledCondition {
        line: condition.line,
        negated: condition.negated,
        kind,
        match_capture: regex_condition.and_then(RegexCondition::match_capture),
        capture_indexes: regex_condition
            .map(|regex| regex.capture_indexes().to_vec())
            .unwrap_or_default(),
    }
}

impl CompiledCondition {
    pub(super) fn properties(&self) -> PlanProperties {
        let (requirements, ordered, message_contents, external) = match self.kind {
            CompiledConditionKind::ShellExpanded { .. } => (
                InputRequirements {
                    needs_headers: true,
                    needs_body_contents: true,
                    needs_end_of_message: true,
                },
                true,
                true,
                true,
            ),
            CompiledConditionKind::HeaderRegex(_) => (
                InputRequirements {
                    needs_headers: true,
                    ..InputRequirements::default()
                },
                false,
                false,
                false,
            ),
            CompiledConditionKind::BodyRegex(_) => (
                InputRequirements {
                    needs_headers: true,
                    needs_body_contents: true,
                    needs_end_of_message: true,
                },
                false,
                false,
                false,
            ),
            CompiledConditionKind::MessageRegex(_) => (
                InputRequirements {
                    needs_headers: true,
                    needs_body_contents: true,
                    needs_end_of_message: true,
                },
                false,
                true,
                false,
            ),
            CompiledConditionKind::VariableRegex { .. } => {
                (InputRequirements::default(), false, false, false)
            }
            CompiledConditionKind::Program { input, .. } => (
                match input {
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
                true,
                false,
                true,
            ),
            CompiledConditionKind::SmallerThan(_) | CompiledConditionKind::LargerThan(_) => (
                InputRequirements {
                    needs_end_of_message: true,
                    ..InputRequirements::default()
                },
                false,
                false,
                false,
            ),
        };
        PlanProperties {
            requirements,
            requires_ordered_delivery: ordered,
            requires_preemptive_ordered_delivery: ordered,
            needs_message_contents: message_contents,
            has_external_commands: external,
        }
    }

    pub(super) fn resolve_shell_expansion(
        &self,
        runtime: &RuntimeVariables,
    ) -> Result<Option<Self>, EvalError> {
        self.resolve_shell_expansion_with(
            |condition, line| {
                crate::config::expand::expand_shell_condition(condition, line, runtime)
                    .map_err(EvalError::Expansion)
            },
            |error| error,
        )
    }

    pub(super) fn resolve_shell_expansion_with<X>(
        &self,
        mut expand: impl FnMut(&crate::config::ShellExpandedCondition, usize) -> Result<String, X>,
        mut map_error: impl FnMut(EvalError) -> X,
    ) -> Result<Option<Self>, X> {
        let CompiledConditionKind::ShellExpanded {
            condition,
            area,
            case_sensitive,
        } = &self.kind
        else {
            return Ok(None);
        };
        let mut condition = condition.clone();
        let mut negated = self.negated;

        // Reparsed text may itself begin with the expansion marker when a
        // variable supplies a complete condition. Bound those repeated passes
        // so hostile runtime values cannot create unbounded reparsing work.
        for _ in 0..=crate::config::MAX_EXPANSION_DEPTH {
            let expanded = expand(&condition, self.line)?;
            let parsed = crate::config::parse_reparsed_condition(
                expanded.trim_start(),
                self.line,
                *case_sensitive,
            )
            .map_err(|error| {
                map_error(EvalError::RuntimeCondition {
                    line: error.line,
                    message: error.message,
                })
            })?;
            negated ^= parsed.negated;
            if let ConditionKind::ShellExpanded(next) = parsed.kind {
                condition = next;
                continue;
            }
            let mut compiled = compile_condition(&parsed, *area, *case_sensitive);
            compiled.negated = negated;
            return Ok(Some(compiled));
        }
        Err(map_error(EvalError::RuntimeCondition {
            line: self.line,
            message: format!(
                "condition expansion exceeds the hard depth limit of {}",
                crate::config::MAX_EXPANSION_DEPTH
            ),
        }))
    }

    pub(super) fn program(&self) -> Option<(&str, ConditionInput)> {
        match &self.kind {
            CompiledConditionKind::ShellExpanded { .. } => None,
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
            CompiledConditionKind::ShellExpanded { .. } => TraceConditionKind::ShellExpanded,
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
            CompiledConditionKind::ShellExpanded { .. } => ConditionKindExplanation::ShellExpanded,
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

    pub(super) fn matches_headers(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
    ) -> Result<PartialMatch, EvalError> {
        let matched = match &self.kind {
            CompiledConditionKind::ShellExpanded { .. } => return Ok(PartialMatch::Deferred),
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
            CompiledConditionKind::ShellExpanded { .. } => {
                let resolved = self.resolve_shell_expansion(runtime)?.ok_or_else(|| {
                    EvalError::RuntimeCondition {
                        line: self.line,
                        message: "expanded condition did not resolve".to_owned(),
                    }
                })?;
                return resolved.matches_complete(message, runtime);
            }
            CompiledConditionKind::HeaderRegex(regex) => self.regex_matches(
                regex,
                message
                    .matching_input(ConditionInput::Headers)
                    .ok_or(EvalError::BodyWasNotBuffered)?,
                runtime,
            )?,
            CompiledConditionKind::BodyRegex(regex) => self.regex_matches(
                regex,
                message
                    .matching_input(ConditionInput::Body)
                    .ok_or(EvalError::BodyWasNotBuffered)?,
                runtime,
            )?,
            CompiledConditionKind::MessageRegex(regex) => self.regex_matches(
                regex,
                message
                    .matching_input(ConditionInput::Message)
                    .ok_or(EvalError::BodyWasNotBuffered)?,
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
