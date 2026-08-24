// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FanoutPlanState {
    pub(super) deliveries: Vec<PlannedDelivery>,
    pub(super) original_delivered: bool,
}

#[derive(Debug, Default)]
pub(super) struct HeaderPlanState {
    pub(super) execution: FanoutPlanState,
    pub(super) frames: Vec<ContinuationFrame>,
    pub(super) requirements: InputRequirements,
    pub(super) restart: bool,
}

// A resumed sequence must move its position and prior-recipe state together.
// Keeping them in one value prevents a caller from advancing to another node
// while accidentally retaining state that belongs to the old position.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SequenceCursor {
    pub(super) index: usize,
    pub(super) state: SequenceState,
}

// The frame slice owns the complete bounded path and depth selects one entry
// on it. Passing them together keeps recursive descent tied to that same path
// instead of allowing independently supplied frame and depth values.
#[derive(Debug, Clone, Copy)]
pub(super) struct ResumeCursor<'a> {
    pub(super) frames: &'a [ContinuationFrame],
    pub(super) depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeaderControl {
    Continue,
    Stop,
    EndRcFile,
    Deferred,
}

impl From<SequenceControl> for HeaderControl {
    fn from(control: SequenceControl) -> Self {
        match control {
            SequenceControl::Continue => Self::Continue,
            SequenceControl::Stop => Self::Stop,
            SequenceControl::EndRcFile => Self::EndRcFile,
        }
    }
}

impl ExecutionPlan {
    pub fn evaluate_headers(&self, head: &MessageHead) -> HeaderEvaluation {
        self.evaluate_headers_with_runtime(head, &mut RuntimeVariables::default())
    }

    pub fn evaluate_headers_with_runtime(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
    ) -> HeaderEvaluation {
        self.evaluate_headers_with_trace(head, runtime, &mut NoTrace)
    }

    pub fn evaluate_headers_with_trace(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> HeaderEvaluation {
        let initial_runtime = runtime.clone();
        if self.requires_ordered_delivery {
            return HeaderEvaluation::NeedsMessage(Continuation {
                frames: vec![ContinuationFrame {
                    recipe_index: 0,
                    state: SequenceState::default(),
                    condition_results: Vec::new(),
                    assignments_applied: false,
                }],
                execution: FanoutPlanState::default(),
                runtime: runtime.clone(),
                requirements: self.requirements(),
                restart: false,
            });
        }
        let mut planning = HeaderPlanState::default();
        match self.root.plan_headers(
            head,
            runtime,
            trace,
            &mut planning,
            InputRequirements::default(),
            self.rc_context(),
        ) {
            Ok(HeaderControl::Deferred) => HeaderEvaluation::NeedsMessage(Continuation {
                frames: planning.frames,
                execution: if planning.restart {
                    FanoutPlanState::default()
                } else {
                    planning.execution
                },
                runtime: if planning.restart {
                    initial_runtime
                } else {
                    runtime.clone()
                },
                requirements: planning.requirements,
                restart: planning.restart,
            }),
            Ok(HeaderControl::Continue | HeaderControl::Stop | HeaderControl::EndRcFile) => {
                HeaderEvaluation::Decided(DeliveryPlan {
                    deliveries: planning.execution.deliveries,
                    original_delivered: planning.execution.original_delivered,
                })
            }
            Err(error) => HeaderEvaluation::Error(error),
        }
    }

    pub fn resume_buffered(
        &self,
        continuation: Continuation,
        message: &Message,
    ) -> Result<DeliveryPlan, EvalError> {
        let matching_full = self
            .needs_message_contents()
            .then(|| message.matching_message())
            .flatten();
        self.resume_tree(
            continuation,
            CompleteMessage::Buffered {
                message,
                matching_full: matching_full.as_deref(),
            },
            &mut RuntimeVariables::default(),
            &mut NoTrace,
        )
    }

    pub fn resume_streamed(
        &self,
        continuation: Continuation,
        message: &StreamedMessage,
    ) -> Result<DeliveryPlan, EvalError> {
        if continuation.requirements.needs_body_contents {
            return Err(EvalError::BodyWasNotBuffered);
        }
        self.resume_tree(
            continuation,
            CompleteMessage::Streamed(message),
            &mut RuntimeVariables::default(),
            &mut NoTrace,
        )
    }

    pub fn resume_mapped(
        &self,
        continuation: Continuation,
        raw: &[u8],
        header_len: usize,
    ) -> Result<DeliveryPlan, EvalError> {
        self.resume_mapped_with_runtime(
            continuation,
            raw,
            header_len,
            &mut RuntimeVariables::default(),
        )
    }

    pub fn resume_mapped_with_runtime(
        &self,
        continuation: Continuation,
        raw: &[u8],
        header_len: usize,
        runtime: &mut RuntimeVariables,
    ) -> Result<DeliveryPlan, EvalError> {
        self.resume_mapped_with_trace(continuation, raw, header_len, runtime, &mut NoTrace)
    }

    pub fn resume_mapped_with_trace(
        &self,
        continuation: Continuation,
        raw: &[u8],
        header_len: usize,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> Result<DeliveryPlan, EvalError> {
        self.resume_mapped_with_matching_trace(continuation, raw, header_len, None, runtime, trace)
    }

    pub fn resume_mapped_with_matching_trace(
        &self,
        continuation: Continuation,
        raw: &[u8],
        header_len: usize,
        matching: Option<MatchingMessage<'_>>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> Result<DeliveryPlan, EvalError> {
        if header_len > raw.len() {
            return Err(EvalError::BodyWasNotBuffered);
        }
        let (matching_header, matching_raw) = matching
            .map(|message| {
                let (header, full) = message.into_parts();
                (Some(header), full)
            })
            .unwrap_or((None, None));
        if !matching_views_are_valid(
            raw.len(),
            header_len,
            matching_header,
            matching_raw,
            self.needs_message_contents(),
        ) {
            return Err(EvalError::BodyWasNotBuffered);
        }
        self.resume_tree(
            continuation,
            CompleteMessage::Mapped {
                raw,
                header_len,
                matching_header,
                matching_raw,
            },
            runtime,
            trace,
        )
    }

    pub fn evaluate_full(&self, message: &Message) -> Result<DeliveryPlan, EvalError> {
        let mut execution = FanoutPlanState::default();
        let matching_full = self
            .needs_message_contents()
            .then(|| message.matching_message())
            .flatten();
        self.root.plan_complete(
            CompleteMessage::Buffered {
                message,
                matching_full: matching_full.as_deref(),
            },
            &mut RuntimeVariables::default(),
            &mut NoTrace,
            &mut execution,
            self.rc_context(),
        )?;
        Ok(DeliveryPlan {
            deliveries: execution.deliveries,
            original_delivered: execution.original_delivered,
        })
    }

    fn resume_tree(
        &self,
        continuation: Continuation,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> Result<DeliveryPlan, EvalError> {
        if continuation.frames.is_empty() && !continuation.restart {
            return Err(EvalError::BodyWasNotBuffered);
        }
        *runtime = continuation.runtime;
        let mut execution = continuation.execution;

        if continuation.restart {
            self.runtime_rc.reset_transitions();
            self.root
                .plan_complete(message, runtime, trace, &mut execution, self.rc_context())?;
            return Ok(DeliveryPlan {
                deliveries: execution.deliveries,
                original_delivered: execution.original_delivered,
            });
        }

        // Resume at the deepest pending recipe and then unwind through its
        // parent sequences. Earlier siblings are neither evaluated nor
        // logged again, and their selected destinations remain unchanged.
        self.root.resume_from_frames(
            ResumeCursor {
                frames: &continuation.frames,
                depth: 0,
            },
            message,
            runtime,
            trace,
            &mut execution,
            self.rc_context(),
        )?;
        Ok(DeliveryPlan {
            deliveries: execution.deliveries,
            original_delivered: execution.original_delivered,
        })
    }
}
