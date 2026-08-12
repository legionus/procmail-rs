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
                line.map_or(0, |value| value),
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
        ConditionKind::SmallerThan => "smaller-than",
        ConditionKind::LargerThan => "larger-than",
    }
}

fn variable_source_name(source: VariableSource) -> &'static str {
    match source {
        VariableSource::RcFile => "rc-file",
        VariableSource::CommandLine => "command-line",
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
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionKind {
    HeaderRegex,
    BodyRegex,
    MessageRegex,
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
mod tests {
    use super::*;

    fn variable_event(name: &str) -> TraceEvent {
        TraceEvent::VariableAssigned {
            line: Some(7),
            name: TraceName::new(name).unwrap(),
            source: VariableSource::RcFile,
            value: None,
        }
    }

    #[test]
    fn variable_event_contains_a_name_but_no_value_slot() {
        let event = variable_event("MAILBOX");

        let rendered = format!("{event:?}");
        assert!(rendered.contains("MAILBOX"));
        assert!(!rendered.contains("secret-value"));
    }

    #[test]
    fn bounds_and_validates_variable_names() {
        assert!(TraceName::new("NAME_1").is_ok());
        assert!(TraceName::new("1NAME").is_err());
        assert!(TraceName::new(&"N".repeat(MAX_ASSIGNMENT_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn models_each_required_execution_decision() {
        let events = [
            TraceEvent::LastFolderUpdated,
            TraceEvent::ConditionEvaluated {
                recipe_line: 2,
                condition_line: 3,
                condition_index: 0,
                kind: ConditionKind::HeaderRegex,
                negated: false,
                matched: false,
            },
            TraceEvent::RecipeEvaluated {
                line: 2,
                decision: RecipeDecision::Skipped,
            },
            TraceEvent::Delivery {
                recipe_line: 5,
                destination: DestinationKind::Maildir,
                stage: DeliveryStage::Failed(FailureClass::Transient),
            },
            TraceEvent::ExternalCommand {
                recipe_line: 9,
                stage: ExternalCommandStage::Starting,
            },
        ];

        assert_eq!(events.len(), 5);
    }

    #[test]
    fn memory_trace_preserves_order_and_stops_at_its_limit() {
        let mut trace = MemoryTrace::default();
        for line in 0..=MAX_MEMORY_TRACE_EVENTS {
            trace.record(TraceEvent::RecipeEvaluated {
                line,
                decision: RecipeDecision::Skipped,
            });
        }

        assert_eq!(trace.events().len(), MAX_MEMORY_TRACE_EVENTS);
        assert_eq!(
            trace.events().first(),
            Some(&TraceEvent::RecipeEvaluated {
                line: 0,
                decision: RecipeDecision::Skipped,
            })
        );
        assert_eq!(
            trace.events().last(),
            Some(&TraceEvent::RecipeEvaluated {
                line: MAX_MEMORY_TRACE_EVENTS - 1,
                decision: RecipeDecision::Skipped,
            })
        );
        assert!(trace.was_truncated());
    }

    #[test]
    fn escapes_record_delimiters_control_bytes_and_non_utf8_input() {
        let hostile = b"line\nnext\r\t'\"\\\0\x1f\x7f\x80\xff";
        assert_eq!(
            EscapedBytes::new(hostile).to_string(),
            "line\\nnext\\r\\t\\'\\\"\\\\\\x00\\x1f\\x7f\\x80\\xff"
        );
        assert!(!EscapedBytes::new(hostile).to_string().contains('\n'));
    }

    #[test]
    fn leaves_only_safe_printable_ascii_unescaped() {
        let printable = (b' '..=b'~')
            .filter(|byte| !matches!(byte, b'\\' | b'\'' | b'"'))
            .collect::<Vec<_>>();
        assert_eq!(
            EscapedBytes::new(&printable).to_string().as_bytes(),
            printable
        );
    }

    #[test]
    fn bounded_writer_emits_complete_records_with_accounted_sizes() {
        let mut trace = BoundedTraceWriter::new(Vec::new());
        trace.record(variable_event("MAILBOX"));
        trace.record(TraceEvent::LastFolderUpdated);

        assert_eq!(trace.event_count(), 2);
        assert_eq!(trace.stop_reason(), None);
        assert_eq!(trace.byte_count(), trace.writer.len());
        let output = String::from_utf8(trace.into_inner()).unwrap();
        assert_eq!(output.lines().count(), 2);
        assert!(output.contains("name=\"MAILBOX\""));
    }

    #[test]
    fn renders_stable_event_fixture_with_source_lines() {
        let events = [
            variable_event("BOX"),
            TraceEvent::ConditionEvaluated {
                recipe_line: 10,
                condition_line: 11,
                condition_index: 0,
                kind: ConditionKind::BodyRegex,
                negated: true,
                matched: false,
            },
            TraceEvent::RecipeEvaluated {
                line: 10,
                decision: RecipeDecision::Deferred,
            },
            TraceEvent::Delivery {
                recipe_line: 12,
                destination: DestinationKind::Maildir,
                stage: DeliveryStage::Failed(FailureClass::Transient),
            },
            TraceEvent::LastFolderUpdated,
            TraceEvent::ExternalCommand {
                recipe_line: 20,
                stage: ExternalCommandStage::Succeeded,
            },
        ];
        let mut trace = BoundedTraceWriter::new(Vec::new());
        for event in events {
            trace.record(event);
        }

        let rendered = String::from_utf8(trace.into_inner()).unwrap();
        assert_eq!(
            rendered,
            concat!(
                "event=variable-assigned line=7 name=\"BOX\" source=rc-file\n",
                "event=condition recipe_line=10 condition_line=11 condition_index=0 kind=body-regex negated=true matched=false\n",
                "event=recipe line=10 decision=deferred\n",
                "event=delivery recipe_line=12 destination=maildir stage=failed failure_class=transient\n",
                "event=last-folder-updated\n",
                "event=external-command recipe_line=20 stage=succeeded\n",
            )
        );
    }

    #[test]
    fn bounded_writer_stops_before_exceeding_total_byte_limit() {
        let mut trace = BoundedTraceWriter::new(Vec::new());
        let event = variable_event(&"N".repeat(MAX_ASSIGNMENT_NAME_LEN));
        while trace.stop_reason().is_none() {
            trace.record(event.clone());
        }

        assert_eq!(trace.stop_reason(), Some(TraceStopReason::ByteLimit));
        assert!(trace.byte_count() <= MAX_TRACE_BYTES);
        assert_eq!(trace.byte_count(), trace.writer.len());
    }

    #[test]
    fn bounded_writer_stops_at_event_count_limit() {
        let mut trace = BoundedTraceWriter::new(io::sink());
        for _ in 0..=MAX_TRACE_EVENTS {
            trace.record(TraceEvent::LastFolderUpdated);
        }

        assert_eq!(trace.event_count(), MAX_TRACE_EVENTS);
        assert_eq!(trace.stop_reason(), Some(TraceStopReason::EventLimit));
    }

    #[test]
    fn bounded_text_rejects_a_record_before_partial_growth() {
        let mut output = BoundedText::new(4);
        assert!(output.write_str("1234").is_ok());
        assert!(output.write_str("5").is_err());
        assert_eq!(output.as_bytes(), b"1234");
    }

    #[test]
    fn reads_procmail_style_trace_controls_in_statement_order() {
        let config = crate::config::parse(
            "MAILDIR=/mail\nVERBOSE=off\nLOGFILE=logs/first\nLOGDETAIL=metadata\nVERBOSE=YesPlease\nLOGFILE=$MAILDIR/log\nLOGDETAIL=values\n",
        )
        .unwrap()
        .expand()
        .unwrap();

        let settings = TraceConfig::from_config(&config).unwrap();
        assert!(settings.enabled());
        assert!(settings.verbose());
        assert_eq!(settings.logfile(), Some("/mail/log"));
        assert_eq!(settings.detail(), TraceDetail::Values);
    }

    #[test]
    fn tracing_is_disabled_without_an_explicit_verbose_assignment() {
        let empty = crate::config::parse("").unwrap().expand().unwrap();
        let logfile_only = crate::config::parse("LOGFILE=/mail/filter.log\n")
            .unwrap()
            .expand()
            .unwrap();

        for config in [&empty, &logfile_only] {
            let settings = TraceConfig::from_config(config).unwrap();
            assert!(!settings.verbose());
            assert!(!settings.enabled());
        }
        assert_eq!(
            TraceConfig::from_config(&logfile_only).unwrap().logfile(),
            Some("/mail/filter.log")
        );
        assert_eq!(
            TraceConfig::from_config(&logfile_only)
                .unwrap()
                .failure_policy(),
            LogFailurePolicy::Advisory
        );
    }

    #[test]
    fn accepts_documented_procmail_boolean_prefixes() {
        for value in ["1", "9anything", "on", "yes", "true", "enable"] {
            assert_eq!(parse_procmail_boolean(value), Some(true), "{value:?}");
        }
        for value in ["0", "0anything", "off", "no", "false", "disable"] {
            assert_eq!(parse_procmail_boolean(value), Some(false), "{value:?}");
        }
        for value in ["", "maybe", "-1"] {
            assert_eq!(parse_procmail_boolean(value), None, "{value:?}");
        }
    }

    #[test]
    fn rejects_invalid_verbose_value_with_its_source_line() {
        let config = crate::config::parse("VERBOSE=maybe\n:0\nmaildir:inbox\n")
            .unwrap()
            .expand()
            .unwrap();

        let error = TraceConfig::from_config(&config).unwrap_err();
        assert_eq!(error.line, 1);
        assert_eq!(error.name, "VERBOSE");
    }

    #[test]
    fn rejects_unknown_log_detail_with_its_source_line() {
        let config = crate::config::parse("LOGDETAIL=everything\n:0\nmaildir:inbox\n")
            .unwrap()
            .expand()
            .unwrap();

        let error = TraceConfig::from_config(&config).unwrap_err();
        assert_eq!(error.line, 1);
        assert_eq!(error.name, "LOGDETAIL");
    }

    #[test]
    fn high_detail_writer_escapes_and_truncates_variable_values() {
        let mut trace = BoundedTraceWriter::with_detail(Vec::new(), TraceDetail::Values);
        let value = [b"secret\n".as_slice(), &vec![b'x'; MAX_TRACE_VALUE_SIZE]].concat();
        trace.record(TraceEvent::VariableAssigned {
            line: Some(1),
            name: TraceName::new("TOKEN").unwrap(),
            source: VariableSource::RcFile,
            value: Some(TraceValue::new(&value)),
        });

        let rendered = String::from_utf8(trace.into_inner()).unwrap();
        assert!(rendered.contains("value=\"secret\\n"));
        assert!(rendered.contains("value_truncated=true"));
        assert_eq!(rendered.lines().count(), 1);
    }
}
