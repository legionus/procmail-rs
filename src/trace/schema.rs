// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Event data and bounded values shared by every trace sink and renderer.

use crate::config::{HeaderExtractionMode, MAX_ASSIGNMENT_NAME_LEN};
use crate::source_location::SourceLocation;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceRecord {
    pub event: TraceEvent,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcFileStatement {
    Include,
    Switch,
}

impl RcFileStatement {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Include => "INCLUDERC",
            Self::Switch => "SWITCHRC",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcFileStage {
    Loaded,
    Empty,
    Failed,
}

impl RcFileStage {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Empty => "empty",
            Self::Failed => "failed",
        }
    }
}

impl From<TraceEvent> for TraceRecord {
    fn from(event: TraceEvent) -> Self {
        let location = SourceLocation::unknown(event.source_line().unwrap_or(0));
        Self { event, location }
    }
}

impl TraceEvent {
    pub fn at(self, location: &SourceLocation, detail: TraceDetail) -> TraceRecord {
        let location = location.at_line(self.source_line().unwrap_or(location.line()));
        TraceRecord {
            event: self,
            location: location.for_trace(detail.includes_variable_values()),
        }
    }

    fn source_line(&self) -> Option<usize> {
        match self {
            Self::VariableAssigned { line, .. } | Self::VariableUnset { line, .. } => *line,
            Self::ConditionEvaluated { condition_line, .. } => Some(*condition_line),
            Self::RecipeEvaluated { line, .. }
            | Self::ExternalCommandExecuting { line, .. }
            | Self::HeaderOperation { line, .. }
            | Self::Log { line, .. }
            | Self::RcFile { line, .. } => Some(*line),
            Self::Delivery { recipe_line, .. }
            | Self::ExternalCommand { recipe_line, .. }
            | Self::ExternalFilterReplaced { recipe_line, .. }
            | Self::DeliveryAbstract { recipe_line, .. } => Some(*recipe_line),
            Self::SessionStarted { .. } | Self::LastFolderUpdated => None,
        }
    }
}

pub const MAX_TRACE_EVENT_SIZE: usize = 1024;
pub const MAX_TRACE_EVENTS: usize = 16 * 1024;
pub const MAX_TRACE_BYTES: usize = 1024 * 1024;
pub const MAX_TRACE_VALUE_SIZE: usize = 256;
pub const MAX_MEMORY_TRACE_EVENTS: usize = MAX_TRACE_EVENTS;
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogAbstractMode {
    #[default]
    No,
    Yes,
    All,
}

impl LogAbstractMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "no" => Some(Self::No),
            "yes" => Some(Self::Yes),
            "all" => Some(Self::All),
            _ => None,
        }
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TraceFormat {
    #[default]
    Text,
    Json,
}

/// One filtering event in execution order.
///
/// Metadata events omit values, expressions, commands, and paths before they
/// reach a sink. Explicit values detail uses bounded prefixes, but never
/// includes message bodies or extracted header values. Rc source positions
/// belong to the enclosing `TraceRecord` and follow the same detail policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    RcFile {
        line: usize,
        statement: RcFileStatement,
        stage: RcFileStage,
        target: Option<TraceValue>,
    },
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
    VariableUnset {
        line: Option<usize>,
        name: TraceName,
        source: VariableSource,
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
    ExternalFilterReplaced {
        recipe_line: usize,
        bytes: usize,
    },
    HeaderOperation {
        line: usize,
        kind: HeaderOperationKind,
        name: TraceName,
        argument: Option<TraceName>,
        extraction_mode: Option<HeaderExtractionMode>,
    },
    Log {
        line: usize,
        value: TraceValue,
    },
    DeliveryAbstract {
        recipe_line: usize,
        destination: DestinationKind,
        path: Option<TraceValue>,
    },
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
pub struct TraceName(pub(super) String);

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
    Address,
    Identifier,
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
    // Retain only the typed cause: an arbitrary I/O error string may contain
    // private paths or input bytes even when metadata-only tracing is active.
    Failure(crate::delivery::DeliveryFailure),
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
