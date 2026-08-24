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
        let message = MappedMessageInput::new(raw, header_len, matching)
            .complete_message(self.needs_message_contents())
            .ok_or(EvalError::BodyWasNotBuffered)?;
        self.resume_tree(continuation, message, runtime, trace)
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

impl CompiledSequence {
    fn plan_complete(
        &self,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut FanoutPlanState,
        context: RcExecutionContext<'_>,
    ) -> Result<SequenceControl, EvalError> {
        self.plan_complete_with_context(message, runtime, trace, execution, context)
    }

    fn plan_complete_with_context(
        &self,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut FanoutPlanState,
        context: RcExecutionContext<'_>,
    ) -> Result<SequenceControl, EvalError> {
        self.plan_complete_from(
            SequenceCursor::default(),
            message,
            runtime,
            trace,
            execution,
            context,
        )
    }

    fn plan_complete_from(
        &self,
        cursor: SequenceCursor,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut FanoutPlanState,
        context: RcExecutionContext<'_>,
    ) -> Result<SequenceControl, EvalError> {
        let mut state = cursor.state;
        for (index, recipe) in self.recipes.iter().enumerate().skip(cursor.index) {
            let statement_control = plan_statements_complete(
                &recipe.preceding_statements,
                message,
                runtime,
                trace,
                execution,
                context,
            )?;
            if statement_control != SequenceControl::Continue {
                return Ok(statement_control);
            }
            let conditions_matched =
                recipe.planning_gate(state) && recipe.matches_complete(message, runtime, trace)?;
            let else_handled = recipe.else_handled(state, conditions_matched);
            let has_error_handler = self.has_error_handler(index);

            let control = if conditions_matched {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                recipe.plan_action(
                    message,
                    runtime,
                    trace,
                    execution,
                    has_error_handler,
                    context,
                )?
            } else {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Skipped,
                });
                SequenceControl::Continue
            };
            state.record(
                recipe.control,
                conditions_matched,
                if conditions_matched {
                    ActionExecution::Succeeded
                } else {
                    ActionExecution::NotAttempted
                },
                else_handled,
            );
            if control != SequenceControl::Continue {
                return Ok(control);
            }
        }

        let statement_control = plan_statements_complete(
            &self.trailing_statements,
            message,
            runtime,
            trace,
            execution,
            context,
        )?;
        if statement_control != SequenceControl::Continue {
            return Ok(statement_control);
        }
        Ok(SequenceControl::Continue)
    }

    fn plan_headers(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        planning: &mut HeaderPlanState,
        following: InputRequirements,
        context: RcExecutionContext<'_>,
    ) -> Result<HeaderControl, EvalError> {
        let mut state = SequenceState::default();

        for (index, recipe) in self.recipes.iter().enumerate() {
            let statement_following = self.requirements_from(index).union(following);
            let statement_control = plan_statements_headers(
                &recipe.preceding_statements,
                head,
                runtime,
                trace,
                planning,
                statement_following,
                context,
            )?;
            if statement_control != HeaderControl::Continue {
                return Ok(statement_control);
            }
            let gate = recipe.planning_gate(state);
            let (matched, condition_results) = if gate {
                recipe.matches_headers(head, runtime, trace)?
            } else {
                (PartialMatch::False, Vec::new())
            };
            if matched == PartialMatch::Deferred {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Deferred,
                });
                planning.frames.push(ContinuationFrame {
                    recipe_index: index,
                    state,
                    condition_results,
                    assignments_applied: true,
                });
                planning.requirements = self.requirements_from(index).union(following);
                return Ok(HeaderControl::Deferred);
            }

            let conditions_matched = matched == PartialMatch::True;
            let else_handled = recipe.else_handled(state, conditions_matched);
            let has_error_handler = self.has_error_handler(index);
            if conditions_matched && recipe.delivery_defers_header() {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Deferred,
                });
                planning.frames.push(ContinuationFrame {
                    recipe_index: index,
                    state,
                    condition_results,
                    assignments_applied: true,
                });
                planning.requirements = self.requirements_from(index).union(following);
                return Ok(HeaderControl::Deferred);
            }
            let control = if conditions_matched {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                match &recipe.action {
                    CompiledAction::Pipe { .. } => {
                        return Err(EvalError::ExternalActionUnsupported { line: recipe.line });
                    }
                    CompiledAction::Deliver { .. } => {
                        let control = recipe.plan_delivery(
                            runtime,
                            &mut planning.execution,
                            has_error_handler,
                        )?;
                        HeaderControl::from(control)
                    }
                    CompiledAction::Block(children) => {
                        // Store the parent before descending so the path is
                        // ordered from the root and never exceeds the parser's
                        // recipe nesting limit.
                        planning.frames.push(ContinuationFrame {
                            recipe_index: index,
                            state,
                            condition_results: Vec::new(),
                            assignments_applied: true,
                        });
                        let child_following = self.requirements_from(index + 1).union(following);
                        let child = children.plan_headers(
                            head,
                            runtime,
                            trace,
                            planning,
                            child_following,
                            context,
                        )?;
                        if child != HeaderControl::Deferred {
                            planning.frames.pop();
                        }
                        child
                    }
                }
            } else {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Skipped,
                });
                HeaderControl::Continue
            };
            if control == HeaderControl::Deferred {
                return Ok(control);
            }
            state.record(
                recipe.control,
                conditions_matched,
                ActionExecution::Succeeded,
                else_handled,
            );
            if control != HeaderControl::Continue {
                return Ok(control);
            }
        }

        let statement_control = plan_statements_headers(
            &self.trailing_statements,
            head,
            runtime,
            trace,
            planning,
            following,
            context,
        )?;
        if statement_control != HeaderControl::Continue {
            return Ok(statement_control);
        }
        Ok(HeaderControl::Continue)
    }

    fn resume_from_frames(
        &self,
        cursor: ResumeCursor<'_>,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut FanoutPlanState,
        context: RcExecutionContext<'_>,
    ) -> Result<SequenceControl, EvalError> {
        let frame = cursor
            .frames
            .get(cursor.depth)
            .ok_or(EvalError::BodyWasNotBuffered)?;
        let recipe = self
            .recipes
            .get(frame.recipe_index)
            .ok_or(EvalError::BodyWasNotBuffered)?;
        let mut state = frame.state;
        if !frame.assignments_applied {
            let control = execute_statements(&recipe.preceding_statements, runtime, trace)?;
            if control != SequenceControl::Continue {
                return Ok(control);
            }
        }

        let (conditions_matched, control) = if cursor.depth + 1 < cursor.frames.len() {
            let CompiledAction::Block(children) = &recipe.action else {
                return Err(EvalError::BodyWasNotBuffered);
            };
            let control = children.resume_from_frames(
                ResumeCursor {
                    frames: cursor.frames,
                    depth: cursor.depth + 1,
                },
                message,
                runtime,
                trace,
                execution,
                context,
            )?;
            (true, control)
        } else {
            let conditions_matched = recipe.planning_gate(state)
                && recipe.matches_resumed(message, &frame.condition_results, runtime, trace)?;
            let control = if conditions_matched {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                recipe.plan_action(
                    message,
                    runtime,
                    trace,
                    execution,
                    self.has_error_handler(frame.recipe_index),
                    context,
                )?
            } else {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Skipped,
                });
                SequenceControl::Continue
            };
            (conditions_matched, control)
        };

        if control != SequenceControl::Continue {
            return Ok(control);
        }
        state.record(
            recipe.control,
            conditions_matched,
            if conditions_matched {
                ActionExecution::Succeeded
            } else {
                ActionExecution::NotAttempted
            },
            recipe.else_handled(state, conditions_matched),
        );
        self.plan_complete_from(
            SequenceCursor {
                index: frame.recipe_index + 1,
                state,
            },
            message,
            runtime,
            trace,
            execution,
            context,
        )
    }
}

impl CompiledNode {
    fn matches_headers(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> Result<(PartialMatch, Vec<Option<bool>>), EvalError> {
        let mut result = PartialMatch::True;
        let mut condition_results = Vec::with_capacity(self.conditions.len());
        for (index, condition) in self.conditions.iter().enumerate() {
            let matched = condition.matches_headers(head, runtime)?;
            if matched != PartialMatch::Deferred {
                condition.trace_result(self.line, index, matched, trace);
            }
            match matched {
                PartialMatch::False => {
                    condition_results.push(Some(false));
                    return Ok((PartialMatch::False, condition_results));
                }
                PartialMatch::Deferred => {
                    condition_results.push(None);
                    result = PartialMatch::Deferred;
                }
                PartialMatch::True => condition_results.push(Some(true)),
            }
        }
        Ok((result, condition_results))
    }

    fn matches_resumed(
        &self,
        message: CompleteMessage<'_>,
        header_results: &[Option<bool>],
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> Result<bool, EvalError> {
        for (index, condition) in self.conditions.iter().enumerate() {
            let matched = match header_results.get(index).copied().flatten() {
                Some(matched) => matched,
                None => {
                    let matched = condition.matches_complete(message, runtime)?;
                    condition.trace_result(
                        self.line,
                        index,
                        PartialMatch::from_bool(matched),
                        trace,
                    );
                    matched
                }
            };
            if !matched {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn planning_gate(&self, state: SequenceState) -> bool {
        // Fan-out planning never observes a publication result. Treat a/e as
        // reachable after a matching predecessor so header analysis can
        // defer before either branch is discarded; ordered execution later
        // selects the branch from the real action result.
        match self.control {
            ControlFlow::Independent => true,
            ControlFlow::AfterChainMatch => state.chain_base_matched.unwrap_or(false),
            ControlFlow::AfterPreviousSuccess | ControlFlow::AfterPreviousError => state
                .previous
                .is_some_and(|result| result.conditions_matched),
            ControlFlow::Else => state.previous.is_none_or(|result| !result.else_handled),
        }
    }

    fn delivery_defers_header(&self) -> bool {
        match &self.action {
            CompiledAction::Pipe { .. } => true,
            CompiledAction::Deliver { destination, .. } => {
                destination.needs_runtime_variables()
                    || matches!(destination, Destination::Mbox(_))
                    || matches!(
                        self.control,
                        ControlFlow::AfterPreviousSuccess | ControlFlow::AfterPreviousError
                    )
            }
            CompiledAction::Block(_) => false,
        }
    }

    fn plan_action(
        &self,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut FanoutPlanState,
        has_error_handler: bool,
        context: RcExecutionContext<'_>,
    ) -> Result<SequenceControl, EvalError> {
        match &self.action {
            CompiledAction::Pipe { .. } => {
                Err(EvalError::ExternalActionUnsupported { line: self.line })
            }
            CompiledAction::Deliver { .. } => {
                self.plan_delivery(runtime, execution, has_error_handler)
            }
            CompiledAction::Block(children) => {
                if self.lock.is_some() {
                    return Err(EvalError::LocalLockExecutorUnavailable { line: self.line });
                }
                children.plan_complete(message, runtime, trace, execution, context)
            }
        }
    }

    fn plan_delivery(
        &self,
        runtime: &mut RuntimeVariables,
        execution: &mut FanoutPlanState,
        has_error_handler: bool,
    ) -> Result<SequenceControl, EvalError> {
        let CompiledAction::Deliver {
            destination,
            continuation,
            output_ending,
        } = &self.action
        else {
            return Ok(SequenceControl::Continue);
        };
        let destination = destination
            .bind_with(|name| runtime.get(name).map(str::to_owned))
            .map_err(EvalError::Expansion)?;
        let lock = self.resolve_lock(runtime).map_err(EvalError::Expansion)?;
        let copy = *continuation == ContinuationMode::Continue;
        execution.deliveries.push(PlannedDelivery {
            destination,
            continuation: if copy {
                DeliveryContinuation::Continue
            } else {
                DeliveryContinuation::Stop
            },
            output_ending: *output_ending,
            lock,
            umask: runtime.get("UMASK").unwrap_or("077").to_owned(),
        });
        execution.original_delivered |= !copy;
        if copy || has_error_handler {
            Ok(SequenceControl::Continue)
        } else {
            Ok(SequenceControl::Stop)
        }
    }
}

fn plan_statements_complete(
    statements: &[CompiledStatement],
    message: CompleteMessage<'_>,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
    execution: &mut FanoutPlanState,
    context: RcExecutionContext<'_>,
) -> Result<SequenceControl, EvalError> {
    for statement in statements {
        match statement {
            CompiledStatement::Assignment(assignment) => {
                execute_assignment(assignment, runtime, trace)?;
            }
            CompiledStatement::Host(assignment) => {
                if !execute_host_assignment(assignment, runtime, trace)? {
                    execution.original_delivered = true;
                    return Ok(SequenceControl::EndRcFile);
                }
            }
            CompiledStatement::Include(include) => {
                include.ensure_loaded(runtime, context)?;
                if let LoadedRuntimeRc::Sequence(sequence) = &*include.loaded()
                    && sequence.plan_complete_with_context(
                        message,
                        runtime,
                        trace,
                        execution,
                        context.descend()?,
                    )? == SequenceControl::Stop
                {
                    return Ok(SequenceControl::Stop);
                }
            }
            CompiledStatement::Switch(switch) => {
                // Run the replacement as a separate rc-file scope, then use
                // EndRcFile to unwind every enclosing recipe block. An
                // INCLUDERC boundary consumes that result and resumes its
                // caller, while the root treats it as end of processing.
                switch.ensure_loaded(runtime, context)?;
                match &*switch.loaded() {
                    LoadedRuntimeRc::Unloaded => unreachable!(),
                    LoadedRuntimeRc::Failed => {}
                    LoadedRuntimeRc::Empty => return Ok(SequenceControl::EndRcFile),
                    LoadedRuntimeRc::Sequence(sequence) => {
                        let control = sequence.plan_complete_with_context(
                            message,
                            runtime,
                            trace,
                            execution,
                            context.descend()?,
                        )?;
                        return Ok(if control == SequenceControl::Stop {
                            SequenceControl::Stop
                        } else {
                            SequenceControl::EndRcFile
                        });
                    }
                }
            }
        }
    }
    Ok(SequenceControl::Continue)
}

fn plan_statements_headers(
    statements: &[CompiledStatement],
    head: &MessageHead,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
    planning: &mut HeaderPlanState,
    following: InputRequirements,
    context: RcExecutionContext<'_>,
) -> Result<HeaderControl, EvalError> {
    for statement in statements {
        match statement {
            CompiledStatement::Assignment(assignment) => {
                execute_assignment(assignment, runtime, trace)?;
            }
            CompiledStatement::Host(assignment) => {
                if !execute_host_assignment(assignment, runtime, trace)? {
                    planning.execution.original_delivered = true;
                    return Ok(HeaderControl::EndRcFile);
                }
            }
            CompiledStatement::Include(include) => {
                include.ensure_loaded(runtime, context)?;
                if let LoadedRuntimeRc::Sequence(sequence) = &*include.loaded() {
                    if sequence.requires_ordered_delivery() {
                        planning.frames.clear();
                        planning.restart = true;
                        planning.requirements =
                            sequence
                                .requirements()
                                .union(following)
                                .union(InputRequirements {
                                    needs_end_of_message: true,
                                    ..InputRequirements::default()
                                });
                        return Ok(HeaderControl::Deferred);
                    }
                    let child = sequence.plan_headers(
                        head,
                        runtime,
                        trace,
                        planning,
                        following,
                        context.descend()?,
                    )?;
                    if child == HeaderControl::Deferred {
                        // Continuation frames point into the static root tree.
                        // A dynamically loaded child cannot be represented by
                        // that path, so replay the still-private plan once the
                        // selected message sections have been staged.
                        planning.frames.clear();
                        planning.restart = true;
                        planning.requirements = planning
                            .requirements
                            .union(sequence.requirements())
                            .union(following);
                        return Ok(HeaderControl::Deferred);
                    }
                    if child == HeaderControl::Stop {
                        return Ok(HeaderControl::Stop);
                    }
                }
            }
            CompiledStatement::Switch(switch) => {
                // Requirements after this statement are unreachable after a
                // successful switch. If the dynamic target needs the body,
                // restart from the private root plan after staging it.
                switch.ensure_loaded(runtime, context)?;
                match &*switch.loaded() {
                    LoadedRuntimeRc::Unloaded => unreachable!(),
                    LoadedRuntimeRc::Failed => {}
                    LoadedRuntimeRc::Empty => return Ok(HeaderControl::EndRcFile),
                    LoadedRuntimeRc::Sequence(sequence) => {
                        if sequence.requires_ordered_delivery() {
                            planning.frames.clear();
                            planning.restart = true;
                            planning.requirements =
                                sequence.requirements().union(InputRequirements {
                                    needs_end_of_message: true,
                                    ..InputRequirements::default()
                                });
                            return Ok(HeaderControl::Deferred);
                        }
                        let child = sequence.plan_headers(
                            head,
                            runtime,
                            trace,
                            planning,
                            InputRequirements::default(),
                            context.descend()?,
                        )?;
                        if child == HeaderControl::Deferred {
                            // Replaying from the root reconstructs the dynamic
                            // target without retaining pointers into its tree.
                            // Nothing after SWITCHRC remains reachable.
                            planning.frames.clear();
                            planning.restart = true;
                            planning.requirements =
                                planning.requirements.union(sequence.requirements());
                            return Ok(HeaderControl::Deferred);
                        }
                        return Ok(if child == HeaderControl::Stop {
                            HeaderControl::Stop
                        } else {
                            HeaderControl::EndRcFile
                        });
                    }
                }
            }
        }
    }
    Ok(HeaderControl::Continue)
}
