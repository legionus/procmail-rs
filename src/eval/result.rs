// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use super::{FanoutPlanState, InputRequirements, SequenceState};
use crate::config::{Destination, OutputEnding};
use crate::runtime::RuntimeVariables;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPlan {
    pub(super) deliveries: Vec<PlannedDelivery>,
    pub(super) original_delivered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDelivery {
    pub(super) destination: Destination,
    pub(super) continuation: DeliveryContinuation,
    pub(super) output_ending: OutputEnding,
    pub(super) lock: Option<String>,
    pub(super) umask: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryAttemptError<E> {
    Recoverable(E),
    Fatal(E),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedExecutionError<E> {
    Evaluation(EvalError),
    Delivery(E),
}

#[derive(Debug, Clone, Copy)]
pub enum CompletionState<'a, E> {
    Completed(DeliveryOutcome),
    Failed(&'a OrderedExecutionError<E>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryOutcome {
    pub(super) published: usize,
    pub(super) original_delivered: bool,
}

impl DeliveryOutcome {
    pub fn published(self) -> usize {
        self.published
    }

    pub fn original_delivered(self) -> bool {
        self.original_delivered
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryContinuation {
    Stop,
    Continue,
}

impl PlannedDelivery {
    pub fn destination(&self) -> &Destination {
        &self.destination
    }

    pub fn is_copy(&self) -> bool {
        self.continuation == DeliveryContinuation::Continue
    }

    pub fn output_ending(&self) -> OutputEnding {
        self.output_ending
    }

    pub fn lock(&self) -> Option<&str> {
        self.lock.as_deref()
    }

    pub fn umask(&self) -> &str {
        &self.umask
    }
}

impl DeliveryPlan {
    pub fn deliveries(&self) -> &[PlannedDelivery] {
        &self.deliveries
    }

    pub fn original_delivered(&self) -> bool {
        self.original_delivered
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderEvaluation {
    Decided(DeliveryPlan),
    NeedsMessage(Continuation),
    Error(EvalError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continuation {
    pub(super) frames: Vec<ContinuationFrame>,
    pub(super) execution: FanoutPlanState,
    pub(super) runtime: RuntimeVariables,
    pub(super) requirements: InputRequirements,
    pub(super) restart: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ContinuationFrame {
    pub(super) recipe_index: usize,
    pub(super) state: SequenceState,
    pub(super) condition_results: Vec<Option<bool>>,
    pub(super) assignments_applied: bool,
}

impl Continuation {
    pub fn requirements(&self) -> InputRequirements {
        self.requirements
    }

    pub fn pending_deliveries(&self) -> &[PlannedDelivery] {
        &self.execution.deliveries
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Delivered { deliveries: usize },
    Undelivered { copies: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    BodyWasNotBuffered,
    VariableValueTooLarge {
        name: String,
        size: usize,
    },
    MatchValueIsNotUtf8,
    MatchValuesTooLarge {
        size: usize,
    },
    Expansion(crate::config::ExpansionError),
    RuntimeRcLoaderUnavailable {
        line: usize,
        statement: &'static str,
    },
    RuntimeSettingUnavailable {
        line: usize,
        name: &'static str,
    },
    LocalLockExecutorUnavailable {
        line: usize,
    },
    RuntimeRc(String),
    ExternalActionUnsupported {
        line: usize,
    },
    ExternalConditionUnsupported {
        line: usize,
    },
    InvalidExternalActionResult {
        line: usize,
        reason: &'static str,
    },
    Delivery {
        destination: String,
        message: String,
    },
}

impl fmt::Display for EvalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyWasNotBuffered => {
                formatter.write_str("execution plan requires body contents that were not buffered")
            }
            Self::VariableValueTooLarge { name, size } => write!(
                formatter,
                "variable {name} has {size} bytes, exceeding the hard limit of {} bytes",
                crate::config::MAX_ASSIGNMENT_VALUE_LEN
            ),
            Self::MatchValueIsNotUtf8 => {
                formatter.write_str("regular expression capture is not valid UTF-8")
            }
            Self::MatchValuesTooLarge { size } => write!(
                formatter,
                "regular expression captures require {size} bytes, exceeding the hard limit of {} bytes",
                crate::config::MAX_MATCH_BYTES
            ),
            Self::Expansion(error) => {
                write!(formatter, "cannot expand configuration value: {error}")
            }
            Self::RuntimeRcLoaderUnavailable { line, statement } => write!(
                formatter,
                "line {line}: {statement} requires the runtime rc loader"
            ),
            Self::RuntimeSettingUnavailable { line, name } => write!(
                formatter,
                "line {line}: {name} requires runtime setting support"
            ),
            Self::LocalLockExecutorUnavailable { line } => {
                write!(
                    formatter,
                    "line {line}: recipe block requires local lock support"
                )
            }
            Self::RuntimeRc(message) => formatter.write_str(message),
            Self::ExternalActionUnsupported { line } => {
                write!(
                    formatter,
                    "line {line}: external action is not executable yet"
                )
            }
            Self::ExternalConditionUnsupported { line } => {
                write!(
                    formatter,
                    "line {line}: program condition is not executable in this evaluation mode"
                )
            }
            Self::InvalidExternalActionResult { line, reason } => write!(
                formatter,
                "line {line}: invalid external action result: {reason}"
            ),
            Self::Delivery {
                destination,
                message,
            } => write!(formatter, "cannot deliver to {destination}: {message}"),
        }
    }
}

impl std::error::Error for EvalError {}
