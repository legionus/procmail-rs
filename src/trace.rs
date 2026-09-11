// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Typed, value-free events used to explain filtering decisions.

use std::fmt;
use std::fmt::Write as _;
use std::io::{self, Write};

use crate::config::MAX_ASSIGNMENT_NAME_LEN;
use crate::config::{
    AssignmentTarget, Config, HeaderAction, HeaderExtractionMode, HeaderOperation, Statement,
};

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

pub fn record_external_command(line: usize, command: &str, trace: &mut impl TraceSink) {
    let command = trace
        .detail()
        .includes_variable_values()
        .then(|| TraceValue::new(command.as_bytes()));
    trace.record(TraceEvent::ExternalCommandExecuting { line, command });
}

pub fn record_session_start(trace: &mut impl TraceSink) {
    let timestamp = format_session_timestamp(std::time::SystemTime::now());
    trace.record(TraceEvent::SessionStarted {
        pid: std::process::id(),
        timestamp,
    });
}

pub fn record_header_action(action: &HeaderAction, trace: &mut impl TraceSink) {
    for operation in &action.operations {
        let (line, kind, name, argument, extraction_mode) = match operation {
            HeaderOperation::Remove { line, name } => {
                (*line, HeaderOperationKind::Remove, name, None, None)
            }
            HeaderOperation::Set { line, name, .. } => {
                (*line, HeaderOperationKind::Set, name, None, None)
            }
            HeaderOperation::Add { line, name, .. } => {
                (*line, HeaderOperationKind::Add, name, None, None)
            }
            HeaderOperation::Prepend { line, name, .. } => {
                (*line, HeaderOperationKind::Prepend, name, None, None)
            }
            HeaderOperation::Rename { line, from, to } => (
                *line,
                HeaderOperationKind::Rename,
                from,
                Some(to.as_str()),
                None,
            ),
            HeaderOperation::Extract {
                line,
                name,
                target,
                mode,
            } => (
                *line,
                HeaderOperationKind::Extract,
                name,
                Some(target.as_str()),
                Some(*mode),
            ),
        };

        // The parser has already bounded and validated header and variable
        // names. Retain only those names here; header values must never enter
        // a trace event, including in high-detail mode.
        trace.record(TraceEvent::HeaderOperation {
            line,
            kind,
            name: TraceName(name.clone()),
            argument: argument.map(|name| TraceName(name.to_owned())),
            extraction_mode,
        });
    }
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
    format: TraceFormat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TraceFormat {
    #[default]
    Text,
    Json,
}

impl<W> BoundedTraceWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            events: 0,
            bytes: 0,
            stopped: None,
            detail: TraceDetail::Metadata,
            format: TraceFormat::Json,
        }
    }

    pub fn with_detail(writer: W, detail: TraceDetail) -> Self {
        Self {
            writer,
            events: 0,
            bytes: 0,
            stopped: None,
            detail,
            format: TraceFormat::Json,
        }
    }

    pub fn formatted(writer: W, detail: TraceDetail, format: TraceFormat) -> Self {
        Self {
            writer,
            events: 0,
            bytes: 0,
            stopped: None,
            detail,
            format,
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
        if self.format == TraceFormat::Text
            && matches!(
                event,
                TraceEvent::RecipeEvaluated { .. } | TraceEvent::LastFolderUpdated
            )
        {
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
        let formatted = match self.format {
            TraceFormat::Json => render_json_event(&mut rendered, &event),
            TraceFormat::Text => render_human_event(&mut rendered, &event),
        };
        if formatted.is_err() || rendered.write_char('\n').is_err() {
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

fn render_json_event(output: &mut impl fmt::Write, event: &TraceEvent) -> fmt::Result {
    match event {
        TraceEvent::SessionStarted { pid, timestamp } => {
            write!(
                output,
                "{{\"event\":\"session-start\",\"pid\":{pid},\"timestamp\":"
            )?;
            render_json_string(output, timestamp.as_bytes())?;
            output.write_char('}')
        }
        TraceEvent::VariableAssigned {
            line,
            name,
            source,
            value,
        } => {
            write!(
                output,
                "{{\"event\":\"variable-assigned\",\"line\":{},\"name\":",
                line.unwrap_or(0),
            )?;
            render_json_string(output, name.as_str().as_bytes())?;
            write!(output, ",\"source\":\"{}\"", variable_source_name(*source))?;
            if let Some(value) = value {
                output.write_str(",\"value\":")?;
                render_json_string(output, value.as_bytes())?;
                write!(output, ",\"value_truncated\":{}", value.was_truncated())?;
            }
            output.write_char('}')
        }
        TraceEvent::LastFolderUpdated => output.write_str("{\"event\":\"last-folder-updated\"}"),
        TraceEvent::ConditionEvaluated {
            recipe_line,
            condition_line,
            condition_index,
            kind,
            negated,
            matched,
            expression,
        } => {
            write!(
                output,
                "{{\"event\":\"condition\",\"recipe_line\":{recipe_line},\"condition_line\":{condition_line},\"condition_index\":{condition_index},\"kind\":\"{}\",\"negated\":{negated},\"matched\":{matched}",
                condition_kind_name(*kind)
            )?;
            if let Some(expression) = expression {
                output.write_str(",\"expression\":")?;
                render_json_string(output, expression.as_bytes())?;
                write!(
                    output,
                    ",\"expression_truncated\":{}",
                    expression.was_truncated()
                )?;
            }
            output.write_char('}')
        }
        TraceEvent::RecipeEvaluated { line, decision } => {
            write!(
                output,
                "{{\"event\":\"recipe\",\"line\":{line},\"decision\":\"{}\"}}",
                recipe_decision_name(*decision)
            )
        }
        TraceEvent::Delivery {
            recipe_line,
            destination,
            stage,
            path,
        } => {
            write!(
                output,
                "{{\"event\":\"delivery\",\"recipe_line\":{recipe_line},\"destination\":\"{}\",\"stage\":\"",
                destination_kind_name(*destination)
            )?;
            render_delivery_stage(output, *stage)?;
            if let Some(path) = path {
                output.write_str("\",\"path\":")?;
                render_json_string(output, path.as_bytes())?;
                write!(output, ",\"path_truncated\":{}}}", path.was_truncated())
            } else {
                output.write_str("\"}")
            }
        }
        TraceEvent::ExternalCommand { recipe_line, stage } => {
            write!(
                output,
                "{{\"event\":\"external-command\",\"recipe_line\":{recipe_line},\"stage\":\""
            )?;
            render_external_stage(output, *stage).and_then(|()| output.write_str("\"}"))
        }
        TraceEvent::ExternalCommandExecuting { line, command } => {
            write!(
                output,
                "{{\"event\":\"external-command-executing\",\"line\":{line}"
            )?;
            if let Some(command) = command {
                output.write_str(",\"command\":")?;
                render_json_string(output, command.as_bytes())?;
                write!(output, ",\"command_truncated\":{}", command.was_truncated())?;
            }
            output.write_char('}')
        }
        TraceEvent::HeaderOperation {
            line,
            kind,
            name,
            argument,
            extraction_mode,
        } => {
            write!(
                output,
                "{{\"event\":\"header-operation\",\"line\":{line},\"operation\":\"{}\",\"name\":",
                header_operation_kind_name(*kind)
            )?;
            render_json_string(output, name.as_str().as_bytes())?;
            if let Some(argument) = argument {
                output.write_str(",\"argument\":")?;
                render_json_string(output, argument.as_str().as_bytes())?;
            }
            if let Some(mode) = extraction_mode {
                write!(
                    output,
                    ",\"extraction_mode\":\"{}\"",
                    header_extraction_mode_name(*mode)
                )?;
            }
            output.write_char('}')
        }
    }
}

fn render_json_string(output: &mut impl fmt::Write, value: &[u8]) -> fmt::Result {
    output.write_char('"')?;
    for byte in value {
        match byte {
            b'"' => output.write_str("\\\"")?,
            b'\\' => output.write_str("\\\\")?,
            b'\n' => output.write_str("\\n")?,
            b'\r' => output.write_str("\\r")?,
            b'\t' => output.write_str("\\t")?,
            b' '..=b'~' => output.write_char(char::from(*byte))?,
            _ => write!(output, "\\u00{byte:02x}")?,
        }
    }
    output.write_char('"')
}

fn render_human_event(output: &mut impl fmt::Write, event: &TraceEvent) -> fmt::Result {
    match event {
        TraceEvent::SessionStarted { pid, timestamp } => {
            write!(output, "procmail-rs: [{pid}] {timestamp}")
        }
        TraceEvent::VariableAssigned {
            line, name, value, ..
        } => {
            match line {
                Some(line) => write!(
                    output,
                    "procmail-rs: Assigning at line {line} \"{}",
                    name.as_str()
                )?,
                None => write!(output, "procmail-rs: Assigning \"{}", name.as_str())?,
            }
            match value {
                Some(value) => write!(
                    output,
                    "={}\"{}",
                    EscapedBytes::new(value.as_bytes()),
                    if value.was_truncated() {
                        " (truncated)"
                    } else {
                        ""
                    }
                ),
                None => output.write_str("\" (value hidden)"),
            }
        }
        TraceEvent::LastFolderUpdated => output.write_str("procmail-rs: Updated LASTFOLDER"),
        TraceEvent::ConditionEvaluated {
            recipe_line,
            condition_line,
            condition_index,
            kind,
            negated,
            matched,
            expression,
        } => {
            write!(
                output,
                "procmail-rs: {} on line {condition_line}",
                if *matched { "Match" } else { "No match" }
            )?;
            if let Some(expression) = expression {
                write!(
                    output,
                    " on \"{}\"",
                    EscapedBytes::new(expression.as_bytes())
                )?;
            } else {
                write!(
                    output,
                    " (condition {} of recipe at line {recipe_line}: {}{})",
                    condition_index + 1,
                    human_condition_kind(*kind),
                    if *negated { ", negated" } else { "" }
                )?;
            }
            Ok(())
        }
        TraceEvent::RecipeEvaluated { line, decision } => write!(
            output,
            "procmail-rs: Recipe at line {line}: {}",
            match decision {
                RecipeDecision::Selected => "selected",
                RecipeDecision::Deferred => "waiting for more message data",
                RecipeDecision::Skipped => "skipped",
            }
        ),
        TraceEvent::Delivery {
            recipe_line,
            destination,
            stage,
            path,
        } => match stage {
            DeliveryStage::DryRun => {
                write!(
                    output,
                    "procmail-rs: Would deliver to {}",
                    human_destination_kind(*destination)
                )?;
                if let Some(path) = path {
                    write!(output, " \"{}\"", EscapedBytes::new(path.as_bytes()))?;
                }
                write!(output, " (recipe at line {recipe_line})")
            }
            DeliveryStage::Preparing => write!(
                output,
                "procmail-rs: Recipe at line {recipe_line}: preparing {} delivery",
                human_destination_kind(*destination)
            ),
            DeliveryStage::Published => {
                write!(
                    output,
                    "procmail-rs: Delivered to {}",
                    human_destination_kind(*destination)
                )?;
                if let Some(path) = path {
                    write!(output, " \"{}\"", EscapedBytes::new(path.as_bytes()))?;
                }
                write!(output, " (recipe at line {recipe_line})")
            }
            DeliveryStage::Failed(class) => write!(
                output,
                "procmail-rs: Recipe at line {recipe_line}: {} delivery failed ({})",
                human_destination_kind(*destination),
                failure_class_name(*class)
            ),
        },
        TraceEvent::ExternalCommand { recipe_line, stage } => write!(
            output,
            "procmail-rs: Recipe at line {recipe_line}: external command {}",
            match stage {
                ExternalCommandStage::Starting => "started",
                ExternalCommandStage::Succeeded => "succeeded",
                ExternalCommandStage::Failed(_) => "failed",
            }
        ),
        TraceEvent::ExternalCommandExecuting { line, command } => {
            write!(output, "procmail-rs: Executing at line {line}")?;
            if let Some(command) = command {
                write!(output, " \"{}\"", EscapedBytes::new(command.as_bytes()))?;
            } else {
                output.write_str(" external command")?;
            }
            Ok(())
        }
        TraceEvent::HeaderOperation {
            line,
            kind,
            name,
            argument,
            extraction_mode,
        } => {
            write!(
                output,
                "procmail-rs: {} header \"{}\"",
                human_header_operation(*kind),
                name.as_str()
            )?;
            if let Some(argument) = argument {
                match kind {
                    HeaderOperationKind::Rename => write!(output, " to \"{}\"", argument.as_str())?,
                    HeaderOperationKind::Extract => {
                        write!(output, " into \"{}\"", argument.as_str())?
                    }
                    _ => {}
                }
            }
            if let Some(mode) = extraction_mode {
                write!(output, " ({})", header_extraction_mode_name(*mode))?;
            }
            write!(output, " at line {line}")
        }
    }
}

fn human_header_operation(kind: HeaderOperationKind) -> &'static str {
    match kind {
        HeaderOperationKind::Remove => "Removing",
        HeaderOperationKind::Set => "Setting",
        HeaderOperationKind::Add => "Adding",
        HeaderOperationKind::Prepend => "Prepending",
        HeaderOperationKind::Rename => "Renaming",
        HeaderOperationKind::Extract => "Extracting",
    }
}

fn header_operation_kind_name(kind: HeaderOperationKind) -> &'static str {
    match kind {
        HeaderOperationKind::Remove => "remove",
        HeaderOperationKind::Set => "set",
        HeaderOperationKind::Add => "add",
        HeaderOperationKind::Prepend => "prepend",
        HeaderOperationKind::Rename => "rename",
        HeaderOperationKind::Extract => "extract",
    }
}

fn header_extraction_mode_name(mode: HeaderExtractionMode) -> &'static str {
    match mode {
        HeaderExtractionMode::Raw => "raw",
        HeaderExtractionMode::Unfolded => "unfolded",
        HeaderExtractionMode::Decoded => "decoded",
    }
}

fn human_condition_kind(kind: ConditionKind) -> &'static str {
    match kind {
        ConditionKind::ShellExpanded => "expanded condition",
        ConditionKind::HeaderRegex => "header regular expression",
        ConditionKind::BodyRegex => "body regular expression",
        ConditionKind::MessageRegex => "message regular expression",
        ConditionKind::VariableRegex => "variable regular expression",
        ConditionKind::Program => "external program",
        ConditionKind::SmallerThan => "message size is smaller than",
        ConditionKind::LargerThan => "message size is larger than",
    }
}

fn human_destination_kind(kind: DestinationKind) -> &'static str {
    match kind {
        DestinationKind::Maildir => "Maildir",
        DestinationKind::Mbox => "mbox",
        DestinationKind::File => "file",
        DestinationKind::Discard => "/dev/null",
    }
}

fn condition_kind_name(kind: ConditionKind) -> &'static str {
    match kind {
        ConditionKind::ShellExpanded => "shell-expanded",
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
        DestinationKind::File => "file",
        DestinationKind::Discard => "discard",
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
        DeliveryStage::DryRun => output.write_str("dry-run"),
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
    SessionStarted {
        pid: u32,
        timestamp: String,
    },
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
        expression: Option<TraceValue>,
    },
    RecipeEvaluated {
        line: usize,
        decision: RecipeDecision,
    },
    Delivery {
        recipe_line: usize,
        destination: DestinationKind,
        stage: DeliveryStage,
        path: Option<TraceValue>,
    },
    ExternalCommand {
        recipe_line: usize,
        stage: ExternalCommandStage,
    },
    ExternalCommandExecuting {
        line: usize,
        command: Option<TraceValue>,
    },
    HeaderOperation {
        line: usize,
        kind: HeaderOperationKind,
        name: TraceName,
        argument: Option<TraceName>,
        extraction_mode: Option<HeaderExtractionMode>,
    },
}

fn format_session_timestamp(time: std::time::SystemTime) -> String {
    let seconds = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_date_from_days(days as i64);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days % 7) as usize];
    let month_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][month as usize - 1];
    format!(
        "{weekday} {month_name} {day:2} {:02}:{:02}:{:02} {year}",
        day_seconds / 3600,
        (day_seconds / 60) % 60,
        day_seconds % 60
    )
}

// Convert days since 1970-01-01 to a Gregorian date without libc or locale
// state. Trace output must remain available in the executable's restricted
// environment, and a small deterministic formatter is sufficient here.
fn civil_date_from_days(days: i64) -> (i32, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderOperationKind {
    Remove,
    Set,
    Add,
    Prepend,
    Rename,
    Extract,
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
    ShellExpanded,
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
    File,
    Discard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStage {
    Preparing,
    DryRun,
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
#[path = "tests/trace.rs"]
mod tests;
