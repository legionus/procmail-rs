// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Typed, value-free events used to explain filtering decisions.

use std::fmt;
use std::fmt::Write as _;
use std::io::{self, Write};

use crate::config::MAX_ASSIGNMENT_NAME_LEN;
use crate::config::{AssignmentTarget, Config, Statement};

pub const MAX_TRACE_EVENT_SIZE: usize = 1024;
pub const MAX_TRACE_EVENTS: usize = 16 * 1024;
pub const MAX_TRACE_BYTES: usize = 1024 * 1024;
pub const MAX_TRACE_VALUE_SIZE: usize = 256;
pub const MAX_MEMORY_TRACE_EVENTS: usize = MAX_TRACE_EVENTS;

pub struct EscapedBytes<'a>(&'a [u8]);

impl<'a> EscapedBytes<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for EscapedBytes<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Render one byte at a time so malformed UTF-8 can never enter the
        // output unchecked. Keeping only a conservative printable ASCII set
        // literal also makes every record boundary visible to line-oriented
        // log consumers.
        for byte in self.0 {
            match byte {
                b'\n' => formatter.write_str("\\n")?,
                b'\r' => formatter.write_str("\\r")?,
                b'\t' => formatter.write_str("\\t")?,
                b'\\' => formatter.write_str("\\\\")?,
                b'\'' => formatter.write_str("\\'")?,
                b'"' => formatter.write_str("\\\"")?,
                b' '..=b'~' => formatter.write_str(char::from(*byte).encode_utf8(&mut [0; 4]))?,
                _ => write!(formatter, "\\x{byte:02x}")?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFailurePolicy {
    Advisory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceConfig {
    verbose: bool,
    logfile: Option<String>,
    detail: TraceDetail,
    failure_policy: LogFailurePolicy,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            verbose: false,
            logfile: None,
            detail: TraceDetail::Metadata,
            failure_policy: LogFailurePolicy::Advisory,
        }
    }
}

impl TraceConfig {
    pub fn from_config(config: &Config) -> Result<Self, TraceConfigError> {
        let mut settings = Self::default();
        for statement in &config.statements {
            let Statement::Assignment(assignment) = statement else {
                continue;
            };
            match assignment.target {
                AssignmentTarget::Verbose => {
                    settings.verbose =
                        parse_procmail_boolean(&assignment.value).ok_or_else(|| {
                            TraceConfigError {
                                line: assignment.line,
                                name: assignment.name.clone(),
                                reason: "expected a procmail boolean value".to_owned(),
                            }
                        })?;
                }
                AssignmentTarget::LogFile => {
                    settings.logfile =
                        (!assignment.value.is_empty()).then(|| assignment.value.clone());
                }
                AssignmentTarget::LogDetail => {
                    settings.detail = match assignment.value.as_str() {
                        "metadata" => TraceDetail::Metadata,
                        "values" => TraceDetail::Values,
                        _ => {
                            return Err(TraceConfigError {
                                line: assignment.line,
                                name: assignment.name.clone(),
                                reason: "expected 'metadata' or 'values'".to_owned(),
                            });
                        }
                    };
                }
                _ => {}
            }
        }
        Ok(settings)
    }

    pub fn verbose(&self) -> bool {
        self.verbose
    }

    pub fn logfile(&self) -> Option<&str> {
        self.logfile.as_deref()
    }

    pub fn enabled(&self) -> bool {
        self.verbose
    }

    pub fn failure_policy(&self) -> LogFailurePolicy {
        self.failure_policy
    }

    pub fn detail(&self) -> TraceDetail {
        self.detail
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceConfigError {
    pub line: usize,
    pub name: String,
    pub reason: String,
}

impl fmt::Display for TraceConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "line {}: invalid {}: {}",
            self.line, self.name, self.reason
        )
    }
}

impl std::error::Error for TraceConfigError {}

fn parse_procmail_boolean(value: &str) -> Option<bool> {
    let value = value.to_ascii_lowercase();
    if value.starts_with(|character: char| character.is_ascii_digit() && character != '0')
        || ["on", "y", "t", "e"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
    {
        Some(true)
    } else if value.starts_with('0')
        || ["off", "n", "f", "d"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
    {
        Some(false)
    } else {
        None
    }
}

pub trait TraceSink {
    fn detail(&self) -> TraceDetail {
        TraceDetail::Metadata
    }

    fn record(&mut self, event: TraceEvent);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TraceDetail {
    #[default]
    Metadata,
    Values,
}

impl TraceDetail {
    pub fn includes_variable_values(self) -> bool {
        self == Self::Values
    }
}

#[derive(Debug, Default)]
pub struct NoTrace;

impl TraceSink for NoTrace {
    fn record(&mut self, _: TraceEvent) {}
}

#[derive(Debug)]
pub struct BoundedTraceWriter<W> {
    writer: W,
    events: usize,
    bytes: usize,
    stopped: Option<TraceStopReason>,
    detail: TraceDetail,
}

impl<W> BoundedTraceWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            events: 0,
            bytes: 0,
            stopped: None,
            detail: TraceDetail::Metadata,
        }
    }

    pub fn with_detail(writer: W, detail: TraceDetail) -> Self {
        Self {
            writer,
            events: 0,
            bytes: 0,
            stopped: None,
            detail,
        }
    }

    pub fn event_count(&self) -> usize {
        self.events
    }

    pub fn byte_count(&self) -> usize {
        self.bytes
    }

    pub fn stop_reason(&self) -> Option<TraceStopReason> {
        self.stopped
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: Write> TraceSink for BoundedTraceWriter<W> {
    fn detail(&self) -> TraceDetail {
        self.detail
    }

    fn record(&mut self, event: TraceEvent) {
        if self.stopped.is_some() {
            return;
        }
        if self.events >= MAX_TRACE_EVENTS {
            self.stopped = Some(TraceStopReason::EventLimit);
            return;
        }

        // Format into a fixed-capacity builder before touching the output.
        // This prevents both a partial record and allocation beyond the
        // per-event budget when an event contains hostile future fields.
        let mut rendered = BoundedText::new(MAX_TRACE_EVENT_SIZE);
        if render_event(&mut rendered, &event).is_err() || rendered.write_char('\n').is_err() {
            self.stopped = Some(TraceStopReason::EventSizeLimit);
            return;
        }
        let Some(total) = self.bytes.checked_add(rendered.len()) else {
            self.stopped = Some(TraceStopReason::ByteLimit);
            return;
        };
        if total > MAX_TRACE_BYTES {
            self.stopped = Some(TraceStopReason::ByteLimit);
            return;
        }
        if let Err(error) = self.writer.write_all(rendered.as_bytes()) {
            self.stopped = Some(TraceStopReason::Io(error.kind()));
            return;
        }
        self.events += 1;
        self.bytes = total;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceStopReason {
    EventSizeLimit,
    EventLimit,
    ByteLimit,
    Io(io::ErrorKind),
}

struct BoundedText {
    bytes: String,
    limit: usize,
}

impl BoundedText {
    fn new(limit: usize) -> Self {
        Self {
            bytes: String::with_capacity(limit),
            limit,
        }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn as_bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }
}

impl fmt::Write for BoundedText {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let new_len = self
            .bytes
            .len()
            .checked_add(value.len())
            .ok_or(fmt::Error)?;
        if new_len > self.limit {
            return Err(fmt::Error);
        }
        self.bytes.push_str(value);
        Ok(())
    }
}

fn render_event(output: &mut impl fmt::Write, event: &TraceEvent) -> fmt::Result {
    match event {
        TraceEvent::VariableAssigned {
            line,
            name,
            source,
            value,
        } => {
            write!(
                output,
                "event=variable-assigned line={} name=\"{}\" source={}",
                line.unwrap_or(0),
                EscapedBytes::new(name.as_str().as_bytes()),
                variable_source_name(*source)
            )?;
            if let Some(value) = value {
                write!(
                    output,
                    " value=\"{}\" value_truncated={}",
                    EscapedBytes::new(value.as_bytes()),
                    value.was_truncated()
                )?;
            }
            Ok(())
        }
        TraceEvent::LastFolderUpdated => output.write_str("event=last-folder-updated"),
        TraceEvent::ConditionEvaluated {
            recipe_line,
            condition_line,
            condition_index,
            kind,
            negated,
            matched,
        } => write!(
            output,
            "event=condition recipe_line={recipe_line} condition_line={condition_line} condition_index={condition_index} kind={} negated={negated} matched={matched}",
            condition_kind_name(*kind)
        ),
        TraceEvent::RecipeEvaluated { line, decision } => {
            write!(
                output,
                "event=recipe line={line} decision={}",
                recipe_decision_name(*decision)
            )
        }
        TraceEvent::Delivery {
            recipe_line,
            destination,
            stage,
        } => {
            write!(
                output,
                "event=delivery recipe_line={recipe_line} destination={} stage=",
                destination_kind_name(*destination)
            )?;
            render_delivery_stage(output, *stage)
        }
        TraceEvent::ExternalCommand { recipe_line, stage } => {
            write!(
                output,
                "event=external-command recipe_line={recipe_line} stage="
            )?;
            render_external_stage(output, *stage)
        }
    }
}

fn condition_kind_name(kind: ConditionKind) -> &'static str {
    match kind {
        ConditionKind::HeaderRegex => "header-regex",
        ConditionKind::BodyRegex => "body-regex",
        ConditionKind::MessageRegex => "message-regex",
        ConditionKind::VariableRegex => "variable-regex",
        ConditionKind::Program => "program",
        ConditionKind::SmallerThan => "smaller-than",
        ConditionKind::LargerThan => "larger-than",
    }
}

fn variable_source_name(source: VariableSource) -> &'static str {
    match source {
        VariableSource::RcFile => "rc-file",
        VariableSource::CommandLine => "command-line",
        VariableSource::Environment => "environment",
        VariableSource::System => "system",
        VariableSource::Runtime => "runtime",
    }
}

fn recipe_decision_name(decision: RecipeDecision) -> &'static str {
    match decision {
        RecipeDecision::Selected => "selected",
        RecipeDecision::Deferred => "deferred",
        RecipeDecision::Skipped => "skipped",
    }
}

fn destination_kind_name(kind: DestinationKind) -> &'static str {
    match kind {
        DestinationKind::Maildir => "maildir",
        DestinationKind::Mbox => "mbox",
    }
}

fn failure_class_name(class: FailureClass) -> &'static str {
    match class {
        FailureClass::InputLimit => "input-limit",
        FailureClass::Transient => "transient",
        FailureClass::Permanent => "permanent",
        FailureClass::Internal => "internal",
    }
}

fn render_delivery_stage(output: &mut impl fmt::Write, stage: DeliveryStage) -> fmt::Result {
    match stage {
        DeliveryStage::Preparing => output.write_str("preparing"),
        DeliveryStage::Published => output.write_str("published"),
        DeliveryStage::Failed(class) => {
            write!(output, "failed failure_class={}", failure_class_name(class))
        }
    }
}

fn render_external_stage(output: &mut impl fmt::Write, stage: ExternalCommandStage) -> fmt::Result {
    match stage {
        ExternalCommandStage::Starting => output.write_str("starting"),
        ExternalCommandStage::Succeeded => output.write_str("succeeded"),
        ExternalCommandStage::Failed(class) => {
            write!(output, "failed failure_class={}", failure_class_name(class))
        }
    }
}

#[derive(Debug, Default)]
pub struct MemoryTrace {
    events: Vec<TraceEvent>,
    truncated: bool,
}

impl MemoryTrace {
    pub fn events(&self) -> &[TraceEvent] {
        &self.events
    }

    pub fn was_truncated(&self) -> bool {
        self.truncated
    }
}

impl TraceSink for MemoryTrace {
    fn record(&mut self, event: TraceEvent) {
        // Test traces still consume configuration-controlled events. Stop at
        // a fixed count instead of allowing a forgotten test sink to grow
        // without a limit during adversarial or fuzz-style execution.
        if self.events.len() < MAX_MEMORY_TRACE_EVENTS {
            self.events.push(event);
        } else {
            self.truncated = true;
        }
    }
}

/// One filtering event in execution order.
///
/// Events intentionally contain no message bytes, variable values, regular
/// expression text, command arguments, or destination paths. A later renderer
/// can therefore format the default trace without first trying to redact
/// hostile or sensitive values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    VariableAssigned {
        line: Option<usize>,
        name: TraceName,
        source: VariableSource,
        value: Option<TraceValue>,
    },
    LastFolderUpdated,
    ConditionEvaluated {
        recipe_line: usize,
        condition_line: usize,
        condition_index: usize,
        kind: ConditionKind,
        negated: bool,
        matched: bool,
    },
    RecipeEvaluated {
        line: usize,
        decision: RecipeDecision,
    },
    Delivery {
        recipe_line: usize,
        destination: DestinationKind,
        stage: DeliveryStage,
    },
    ExternalCommand {
        recipe_line: usize,
        stage: ExternalCommandStage,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceValue {
    bytes: Vec<u8>,
    truncated: bool,
}

impl TraceValue {
    pub fn new(value: &[u8]) -> Self {
        let length = value.len().min(MAX_TRACE_VALUE_SIZE);
        Self {
            bytes: value[..length].to_vec(),
            truncated: value.len() > length,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn was_truncated(&self) -> bool {
        self.truncated
    }
}

/// A bounded variable name taken from already validated configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceName(String);

impl TraceName {
    pub fn new(name: &str) -> Result<Self, TraceNameError> {
        if name.len() > MAX_ASSIGNMENT_NAME_LEN {
            return Err(TraceNameError);
        }
        let mut bytes = name.bytes();
        let valid = bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
            && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
        if !valid {
            return Err(TraceNameError);
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceNameError;

impl fmt::Display for TraceNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("trace variable name is invalid or exceeds its size limit")
    }
}

impl std::error::Error for TraceNameError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableSource {
    RcFile,
    CommandLine,
    Environment,
    System,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionKind {
    HeaderRegex,
    BodyRegex,
    MessageRegex,
    VariableRegex,
    Program,
    SmallerThan,
    LargerThan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeDecision {
    Selected,
    Deferred,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationKind {
    Maildir,
    Mbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStage {
    Preparing,
    Published,
    Failed(FailureClass),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalCommandStage {
    Starting,
    Succeeded,
    Failed(FailureClass),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    InputLimit,
    Transient,
    Permanent,
    Internal,
}

#[cfg(test)]
mod tests;
