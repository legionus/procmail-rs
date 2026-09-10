// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Narrow entry points used only by the separately built fuzz package.

use crate::bounded_bytes::BoundedBytesError;
use crate::config::expand::{parse_assignment_word, parse_shell_condition_expression};
use crate::config::shell_eval::{
    self, EvaluationContext, EvaluationDepth, UnsupportedPart, VariableValue,
};
use crate::config::shell_pattern::{self, Edge, PatternError, Selection};
use crate::config::{RecipeAction, Statement};
use crate::eval::{
    CapturedCommand, CompletionState, DeliveryAttemptError, ExecutionPlan, ExternalActionInput,
    FinalMessage, MappedMessageInput, OrderedExecutionHost, PreparedMatchingMessage,
    RecipeLockGuard,
};
use crate::limits::MessageLimits;
use crate::message::Message;
use crate::rc_file::{LoadedRcConfig, RcFileError, RuntimeRcLoader};
use crate::runtime::RuntimeVariables;
use crate::trace::MemoryTrace;
use std::io::{BufReader, Cursor};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const MAX_FUZZ_OUTPUT: usize = 4096;
const MESSAGE_LIMIT_COUNT: usize = 5;
const MAX_FUZZ_SERVICE_CALLS: usize = 8192;

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

pub fn regex(data: &[u8]) {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    let (pattern, input) = rest
        .iter()
        .position(|byte| *byte == 0)
        .map_or((rest, &[][..]), |separator| {
            (&rest[..separator], &rest[separator + 1..])
        });
    let Ok(pattern) = std::str::from_utf8(pattern) else {
        return;
    };
    crate::config::exercise_condition_regex(pattern, input, selector & 1 != 0);
}

pub fn header_edit(data: &[u8]) {
    let Some(separator) = data.iter().position(|byte| *byte == 0) else {
        return;
    };
    let Ok(operations) = std::str::from_utf8(&data[..separator]) else {
        return;
    };
    let message_bytes = &data[separator + 1..];
    let source = format!(":0\nheaders {{\n{operations}\n}}\n");
    let Ok(config) = crate::config::parse(&source) else {
        return;
    };
    let Some(Statement::Recipe(recipe)) = config.statements.first() else {
        return;
    };
    let RecipeAction::Headers(action) = &recipe.action else {
        return;
    };
    let limits = fuzz_message_limits(message_bytes);
    let Ok(message) = Message::read_from(&mut BufReader::new(Cursor::new(message_bytes)), limits)
    else {
        return;
    };
    let Ok(applied) = crate::header_edit::apply_header_action(
        message.header(),
        message.body().len(),
        action,
        limits,
    ) else {
        return;
    };
    let (edited, extractions) = applied.into_parts();
    let replacement = Message::from_edited_header(edited, message.body())
        .expect("a validated header edit must form a bounded message");

    // Reparse the serialized result through bounded ingestion. This checks
    // that a successful edit preserved message framing and did not conceal a
    // limit violation that only the normal input path would detect.
    let reparsed = Message::read_from(
        &mut BufReader::new(Cursor::new(replacement.as_bytes())),
        limits,
    )
    .expect("a validated edited message must pass the same input limits");
    assert_eq!(reparsed.as_bytes(), replacement.as_bytes());
    assert_eq!(reparsed.body(), message.body());
    assert!(
        extractions
            .iter()
            .all(|extraction| extraction.value.len() <= crate::config::MAX_ASSIGNMENT_VALUE_LEN)
    );
}

pub fn rc_configuration(data: &[u8]) {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let supplied = [
        crate::config::SuppliedVariable::from_environment("HOME", "/fuzz/home".to_owned())
            .expect("fixed HOME value must be accepted"),
        crate::config::SuppliedVariable::from_environment("LOGNAME", "fuzzer".to_owned())
            .expect("fixed LOGNAME value must be accepted"),
        crate::config::SuppliedVariable::from_system_hostname("fuzz-host".to_owned())
            .expect("fixed hostname must be accepted"),
        crate::config::SuppliedVariable::from_program_version()
            .expect("package version must be accepted"),
    ];
    let Ok(config) = crate::config::parse(source) else {
        return;
    };
    let Ok(config) = config.expand(&supplied) else {
        return;
    };
    if crate::configuration::validate(&config).is_err() {
        return;
    }

    // Compile and traverse the same lazy tree used by check and filter. A
    // missing rc loader deliberately prevents this target from touching the
    // filesystem while retaining include and switch nodes in the plan.
    let plan = crate::eval::ExecutionPlan::compile(&config, None);
    let _ = plan.requirements();
    let _ = plan.requires_ordered_delivery();
    let _ = plan.needs_message_contents();
    let _ = plan.has_external_commands();
    let _ = plan.explain();
}

pub fn message(data: &[u8]) {
    const CONTROL_SIZE: usize = MESSAGE_LIMIT_COUNT * size_of::<u16>();

    let (control, message) = if data.len() >= CONTROL_SIZE {
        data.split_at(CONTROL_SIZE)
    } else {
        (&[][..], data)
    };

    // Test one independently selected limit at each neighboring value. This
    // keeps every allocation bounded by the fuzz input while making rejection
    // edges much denser than five unrelated, freely varying limits would be.
    for selected in 0..MESSAGE_LIMIT_COUNT {
        let encoded = control
            .get(selected * 2..selected * 2 + 2)
            .map_or(0, |bytes| u16::from_le_bytes([bytes[0], bytes[1]]));
        let pivot = usize::from(encoded) % message.len().saturating_add(2);
        for limit in [pivot.saturating_sub(1), pivot, pivot.saturating_add(1)] {
            let mut limits = MessageLimits::default();
            set_message_limit(&mut limits, selected, limit);
            let result = Message::read_from(&mut BufReader::new(Cursor::new(message)), limits);
            if let Ok(parsed) = result {
                assert!(parsed.len() <= limits.message_size);
                assert!(parsed.header().len() <= limits.headers_size);
                assert!(parsed.body().len() <= limits.body_size);
            }
        }
    }
}

pub fn ordered_evaluation(data: &[u8]) {
    let Some((&selector, input)) = data.split_first() else {
        return;
    };
    let Some(separator) = input.iter().position(|byte| *byte == 0) else {
        return;
    };
    let Ok(source) = std::str::from_utf8(&input[..separator]) else {
        return;
    };
    let message_bytes = &input[separator + 1..];
    let supplied = fuzz_supplied_variables();
    let Ok(config) = crate::config::parse(source) else {
        return;
    };
    let Ok(config) = config.expand(&supplied) else {
        return;
    };
    let Ok(settings) = crate::configuration::validate(&config) else {
        return;
    };
    let Ok(message) = Message::read_from(
        &mut BufReader::new(Cursor::new(message_bytes)),
        settings.message_limits,
    ) else {
        return;
    };
    let child = crate::config::parse("FUZZ_INCLUDED=yes\n:0 c\n/dev/null\n")
        .expect("fixed mock include syntax must parse")
        .expand(&supplied)
        .expect("fixed mock include configuration must compile");
    let plan = ExecutionPlan::compile_for_fuzzing(&config, FuzzRcLoader { config: child });
    let matching = PreparedMatchingMessage::new(&message, plan.needs_message_contents());
    let mut runtime = RuntimeVariables::default();
    runtime.set_system_hostname("fuzz-host".to_owned());
    let host = FuzzExecutionHost::new(selector);

    // Drive the real ordered tree with memory-only services. Every service
    // shares one finite call budget, so copy branches and repeated runtime rc
    // transitions cannot turn a small fuzz input into unbounded mock work.
    let _ = plan.execute_ordered(
        MappedMessageInput::new(
            message.as_bytes(),
            message.header().len(),
            Some(matching.views(&message)),
        ),
        &mut runtime,
        host,
    );
    let _ = plan.take_rc_diagnostics();
}

pub fn destination_path(data: &[u8]) {
    let Some((&selector, input)) = data.split_first() else {
        return;
    };
    let mut parts = input.splitn(3, |byte| *byte == 0);
    let (Some(expression), Some(value), Some(base)) = (parts.next(), parts.next(), parts.next())
    else {
        return;
    };
    let (Ok(expression), Ok(value), Ok(base)) = (
        std::str::from_utf8(expression),
        std::str::from_utf8(value),
        std::str::from_utf8(base),
    ) else {
        return;
    };
    let action = match selector & 3 {
        0 => format!("maildir:{expression}"),
        1 => format!("mbox:{expression}"),
        2 => expression.to_owned(),
        _ => format!("{expression}/"),
    };
    let source = format!(":0\n{action}\n");
    let mut supplied = fuzz_supplied_variables().to_vec();
    let Ok(variable) = crate::config::SuppliedVariable::parse(format!("FUZZ_VALUE={value}")) else {
        return;
    };
    supplied.push(variable);
    if selector & 4 != 0 {
        let Ok(maildir) = crate::config::SuppliedVariable::parse(format!("MAILDIR={base}")) else {
            return;
        };
        supplied.push(maildir);
    }
    let Ok(config) = crate::config::parse(&source) else {
        return;
    };
    let Ok(config) = config.expand(&supplied) else {
        return;
    };

    // Resolve every nested destination through the production API. Command
    // output is evaluated in memory and admitted only as UTF-8, matching the
    // filesystem-path boundary without running a shell or opening any path.
    exercise_destination_statements(&config.statements, selector, value, base, data);
}

fn exercise_destination_statements(
    statements: &[Statement],
    selector: u8,
    value: &str,
    base: &str,
    data: &[u8],
) {
    for statement in statements {
        let Statement::Recipe(recipe) = statement else {
            continue;
        };
        match &recipe.action {
            RecipeAction::Deliver(destination) => {
                let lookup = |name: &str| match name {
                    "FUZZ_VALUE" => Some(value.to_owned()),
                    "MAILDIR" if selector & 4 != 0 => Some(base.to_owned()),
                    _ => None,
                };
                if let Some(expression) = destination.command_expression() {
                    let mut context = FuzzContext { selector, data };
                    if let Ok(evaluated) = shell_eval::evaluate(
                        expression,
                        crate::config::MAX_PATH_EXPRESSION_LEN,
                        &mut context,
                    ) {
                        if let Ok(path) = String::from_utf8(evaluated.bytes) {
                            let runtime_base = (selector & 4 != 0).then_some(base);
                            let _ = destination.resolve_ordered_output(path, runtime_base);
                        }
                    }
                } else {
                    let _ = destination.resolve_with(lookup);
                }
            }
            RecipeAction::Block(children) => {
                exercise_destination_statements(children, selector, value, base, data);
            }
            RecipeAction::Pipe(_) | RecipeAction::Capture(_) | RecipeAction::Headers(_) => {}
        }
    }
}

fn fuzz_supplied_variables() -> [crate::config::SuppliedVariable; 4] {
    [
        crate::config::SuppliedVariable::from_environment("HOME", "/fuzz/home".to_owned())
            .expect("fixed HOME value must be accepted"),
        crate::config::SuppliedVariable::from_environment("LOGNAME", "fuzzer".to_owned())
            .expect("fixed LOGNAME value must be accepted"),
        crate::config::SuppliedVariable::from_system_hostname("fuzz-host".to_owned())
            .expect("fixed hostname must be accepted"),
        crate::config::SuppliedVariable::from_program_version()
            .expect("package version must be accepted"),
    ]
}

#[derive(Debug)]
struct FuzzRcLoader {
    config: crate::config::Config,
}

impl RuntimeRcLoader for FuzzRcLoader {
    fn load_runtime_config(
        &mut self,
        expression: &crate::config::RcFileExpression,
        runtime: &RuntimeVariables,
        _depth: usize,
    ) -> Result<Option<LoadedRcConfig>, RcFileError> {
        let Ok(path) = expression.resolve_with(|name| runtime.get(name).map(str::to_owned)) else {
            return Ok(None);
        };
        if path.is_empty() {
            return Ok(None);
        }
        Ok(Some(LoadedRcConfig::from_config(
            PathBuf::from(path),
            self.config.clone(),
        )))
    }
}

struct FuzzExecutionHost {
    selector: u8,
    calls: Arc<AtomicUsize>,
    trace: MemoryTrace,
}

impl FuzzExecutionHost {
    fn new(selector: u8) -> Self {
        Self {
            selector,
            calls: Arc::new(AtomicUsize::new(0)),
            trace: MemoryTrace::default(),
        }
    }

    fn choice(&self, bytes: &[u8]) -> Result<u8, DeliveryAttemptError<()>> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        if call >= MAX_FUZZ_SERVICE_CALLS {
            return Err(DeliveryAttemptError::Fatal(()));
        }
        Ok(bytes
            .iter()
            .take(16)
            .fold(self.selector.wrapping_add(call as u8), |value, byte| {
                value.wrapping_add(*byte)
            })
            % 8)
    }
}

impl OrderedExecutionHost for FuzzExecutionHost {
    type Error = ();
    type Trace = MemoryTrace;

    fn trace(&mut self) -> &mut Self::Trace {
        &mut self.trace
    }

    fn deliver(
        &mut self,
        destination: &crate::config::Destination,
        _message: &[u8],
        _output_ending: crate::config::OutputEnding,
        _lock: Option<&str>,
        runtime: &mut RuntimeVariables,
    ) -> Result<(), DeliveryAttemptError<Self::Error>> {
        match self.choice(destination.path().as_bytes())? {
            0 => Err(DeliveryAttemptError::Fatal(())),
            1 => Err(DeliveryAttemptError::Recoverable(())),
            _ => {
                runtime.set("LASTFOLDER", destination.path());
                Ok(())
            }
        }
    }

    fn external_action(
        &mut self,
        action: &crate::config::PipeAction,
        _options: crate::config::RecipeOptions,
        _lock: Option<&str>,
        input: ExternalActionInput<'_>,
        _runtime: &mut RuntimeVariables,
    ) -> Result<Option<Message>, DeliveryAttemptError<Self::Error>> {
        match self.choice(action.command.as_bytes())? {
            0 => Err(DeliveryAttemptError::Fatal(())),
            1 => Err(DeliveryAttemptError::Recoverable(())),
            2 => Message::read_from(
                &mut BufReader::new(Cursor::new(input.selected())),
                MessageLimits::default(),
            )
            .map(Some)
            .map_err(|_| DeliveryAttemptError::Recoverable(())),
            _ => Ok(None),
        }
    }

    fn capture(
        &mut self,
        command: &str,
        input: &[u8],
        _output_ending: crate::config::OutputEnding,
        _options: Option<crate::config::RecipeOptions>,
        limit: usize,
        _runtime: &mut RuntimeVariables,
    ) -> Result<CapturedCommand, DeliveryAttemptError<Self::Error>> {
        match self.choice(command.as_bytes())? {
            0 => Err(DeliveryAttemptError::Fatal(())),
            1 => Err(DeliveryAttemptError::Recoverable(())),
            _ => Ok(CapturedCommand::new(
                input[..input.len().min(limit).min(64)].to_vec(),
            )),
        }
    }

    fn external_condition(
        &mut self,
        command: &str,
        _input: &[u8],
        _runtime: &mut RuntimeVariables,
    ) -> Result<bool, DeliveryAttemptError<Self::Error>> {
        self.choice(command.as_bytes())
            .map(|choice| choice & 1 != 0)
    }

    fn replace_global_lock(
        &mut self,
        path: &str,
        _runtime: &mut RuntimeVariables,
    ) -> Result<(), Self::Error> {
        self.choice(path.as_bytes()).map_err(|_| ())?;
        Ok(())
    }

    fn acquire_local_lock(
        &mut self,
        path: &str,
        _runtime: &mut RuntimeVariables,
    ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<Self::Error>> {
        self.choice(path.as_bytes()).map(|_| Box::new(()) as _)
    }

    fn fork_copy_branch(&mut self) -> Option<Self> {
        Some(Self {
            selector: self.selector,
            calls: Arc::clone(&self.calls),
            trace: MemoryTrace::default(),
        })
    }

    fn complete(
        &mut self,
        _message: FinalMessage<'_>,
        _runtime: &mut RuntimeVariables,
        _state: CompletionState<'_, Self::Error>,
    ) {
    }
}

fn set_message_limit(limits: &mut MessageLimits, selected: usize, value: usize) {
    match selected {
        0 => limits.message_size = value,
        1 => limits.headers_size = value,
        2 => limits.body_size = value,
        3 => limits.header_line_size = value,
        4 => limits.header_field_size = value,
        _ => unreachable!("the caller selects one of the five message limits"),
    }
}

fn fuzz_message_limits(data: &[u8]) -> MessageLimits {
    let limit = |index: usize| {
        data.get(index)
            .copied()
            .map_or(0, |byte| usize::from(byte) * 32)
    };
    let mut limits = MessageLimits::default();
    for selected in 0..MESSAGE_LIMIT_COUNT {
        set_message_limit(&mut limits, selected, limit(selected));
    }
    limits
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

#[cfg(test)]
#[path = "tests/fuzzing.rs"]
mod tests;
