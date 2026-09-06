// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

type HeaderCaptureExecutor<'a, E, T> = dyn FnMut(
        &str,
        &[u8],
        OutputEnding,
        Option<RecipeOptions>,
        usize,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<CapturedCommand, DeliveryAttemptError<E>>
    + 'a;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FanoutPlanState {
    pub(super) deliveries: Vec<PlannedDelivery>,
    pub(super) original_delivered: bool,
}

#[derive(Debug)]
pub(super) struct HeaderPlanState<E> {
    pub(super) frames: Vec<ContinuationFrame>,
    pub(super) requirements: InputRequirements,
    pub(super) restart: bool,
    pub(super) pending_error: Option<E>,
}

impl<E> Default for HeaderPlanState<E> {
    fn default() -> Self {
        Self {
            frames: Vec::new(),
            requirements: InputRequirements::default(),
            restart: false,
            pending_error: None,
        }
    }
}

// Header planning can stop before the body is read, while complete planning
// and resume always have a stable complete-message view. Keep their state in
// separate context types so recursive rc-file traversal carries all mutable
// planning data together without erasing those different stopping points.
struct HeaderPlanContext<'a, 'executor, E, T> {
    head: &'a mut MessageHead,
    runtime: &'a mut RuntimeVariables,
    trace: &'a mut T,
    planning: &'a mut HeaderPlanState<E>,
    execution: &'a mut FanoutPlanState,
    rc: RcExecutionContext<'a>,
    capture: &'a mut Option<&'executor mut HeaderCaptureExecutor<'executor, E, T>>,
}

struct CompletePlanContext<'a, 'message, T> {
    message: CompleteMessage<'message>,
    runtime: &'a mut RuntimeVariables,
    trace: &'a mut T,
    execution: &'a mut FanoutPlanState,
    rc: RcExecutionContext<'a>,
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
    pub fn evaluate_headers_editing_with_capture_trace<E, K, T>(
        &self,
        head: &mut MessageHead,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
        capture: &mut K,
    ) -> Result<HeaderEvaluation, OrderedExecutionError<E>>
    where
        K: FnMut(
            &str,
            &[u8],
            OutputEnding,
            Option<RecipeOptions>,
            usize,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<CapturedCommand, DeliveryAttemptError<E>>,
        T: TraceSink,
    {
        self.evaluate_headers_editing_inner(head, runtime, trace, Some(capture))
    }

    pub(super) fn evaluate_headers_editing_inner<'a, E, T>(
        &self,
        head: &mut MessageHead,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
        mut capture: Option<&'a mut HeaderCaptureExecutor<'a, E, T>>,
    ) -> Result<HeaderEvaluation, OrderedExecutionError<E>>
    where
        T: TraceSink,
    {
        let initial_runtime = runtime.clone();
        // Actions such as pipes and locked delivery need the complete message
        // before any recipe is executed. Header editing is deliberately not
        // included here: it can safely update the bounded MessageHead and let
        // the existing streaming path forward the untouched body afterwards.
        if self.root.properties().requires_preemptive_ordered_delivery {
            return Ok(HeaderEvaluation::NeedsMessage(Continuation {
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
            }));
        }
        let mut planning = HeaderPlanState::default();
        let mut execution = FanoutPlanState::default();
        match self.root.plan_headers(
            &mut HeaderPlanContext {
                head,
                runtime,
                trace,
                planning: &mut planning,
                execution: &mut execution,
                rc: self.rc_context(),
                capture: &mut capture,
            },
            InputRequirements::default(),
        ) {
            Ok(HeaderControl::Deferred) => Ok(HeaderEvaluation::NeedsMessage(Continuation {
                frames: planning.frames,
                execution: if planning.restart {
                    FanoutPlanState::default()
                } else {
                    execution
                },
                runtime: if planning.restart {
                    initial_runtime
                } else {
                    runtime.clone()
                },
                requirements: planning.requirements,
                restart: planning.restart,
            })),
            Ok(HeaderControl::Continue | HeaderControl::Stop | HeaderControl::EndRcFile) => {
                if let Some(error) = planning.pending_error {
                    return Err(OrderedExecutionError::Delivery(error));
                }
                Ok(HeaderEvaluation::Decided(DeliveryPlan {
                    deliveries: execution.deliveries,
                    original_delivered: execution.original_delivered,
                }))
            }
            Err(error) => Err(error),
        }
    }

    pub fn resume_with_trace(
        &self,
        continuation: Continuation,
        message: MappedMessageInput<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> Result<DeliveryPlan, EvalError> {
        self.resume_input(continuation, ResumeInput::Mapped(message), runtime, trace)
    }

    pub(super) fn resume_input(
        &self,
        continuation: Continuation,
        input: ResumeInput<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> Result<DeliveryPlan, EvalError> {
        #[cfg(test)]
        let matching;
        let message = match input {
            #[cfg(test)]
            ResumeInput::Buffered(message) => {
                matching = PreparedMatchingMessage::new(message, self.needs_message_contents());
                matching.complete(message)
            }
            #[cfg(test)]
            ResumeInput::Streamed(message) => {
                if continuation.requirements.needs_body_contents {
                    return Err(EvalError::BodyWasNotBuffered);
                }
                CompleteMessage::Streamed(message)
            }
            ResumeInput::Mapped(message) => message
                .complete_message(self.needs_message_contents())
                .ok_or(EvalError::BodyWasNotBuffered)?,
        };
        if continuation.frames.is_empty() && !continuation.restart {
            return Err(EvalError::BodyWasNotBuffered);
        }
        *runtime = continuation.runtime;
        let mut execution = continuation.execution;

        if continuation.restart {
            self.runtime_rc.reset_transitions();
            self.root.plan_complete(&mut CompletePlanContext {
                message,
                runtime,
                trace,
                execution: &mut execution,
                rc: self.rc_context(),
            })?;
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
            &mut CompletePlanContext {
                message,
                runtime,
                trace,
                execution: &mut execution,
                rc: self.rc_context(),
            },
        )?;
        Ok(DeliveryPlan {
            deliveries: execution.deliveries,
            original_delivered: execution.original_delivered,
        })
    }
}

pub(super) enum ResumeInput<'a> {
    #[cfg(test)]
    Buffered(&'a Message),
    #[cfg(test)]
    Streamed(&'a StreamedMessage),
    Mapped(MappedMessageInput<'a>),
}

impl CompiledSequence {
    fn plan_complete<T: TraceSink>(
        &self,
        context: &mut CompletePlanContext<'_, '_, T>,
    ) -> Result<SequenceControl, EvalError> {
        self.plan_complete_with_context(context)
    }

    fn plan_complete_with_context<T: TraceSink>(
        &self,
        context: &mut CompletePlanContext<'_, '_, T>,
    ) -> Result<SequenceControl, EvalError> {
        self.plan_complete_from(SequenceCursor::default(), context)
    }

    fn plan_complete_from<T: TraceSink>(
        &self,
        cursor: SequenceCursor,
        context: &mut CompletePlanContext<'_, '_, T>,
    ) -> Result<SequenceControl, EvalError> {
        let mut state = cursor.state;
        for (index, recipe) in self.recipes.iter().enumerate().skip(cursor.index) {
            let statement_control =
                plan_statements_complete(&recipe.preceding_statements, context)?;
            if statement_control != SequenceControl::Continue {
                return Ok(statement_control);
            }
            let conditions_matched = recipe.planning_gate(state)
                && recipe.matches_complete(context.message, context.runtime, context.trace)?;
            let else_handled = recipe.else_handled(state, conditions_matched);
            let has_error_handler = self.has_error_handler(index);

            let control = if conditions_matched {
                context.trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                recipe.plan_action(has_error_handler, context)?
            } else {
                context.trace.record(TraceEvent::RecipeEvaluated {
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

        let statement_control = plan_statements_complete(&self.trailing_statements, context)?;
        if statement_control != SequenceControl::Continue {
            return Ok(statement_control);
        }
        Ok(SequenceControl::Continue)
    }

    fn plan_headers<E, T>(
        &self,
        context: &mut HeaderPlanContext<'_, '_, E, T>,
        following: InputRequirements,
    ) -> Result<HeaderControl, OrderedExecutionError<E>>
    where
        T: TraceSink,
    {
        let mut state = SequenceState::default();

        for (index, recipe) in self.recipes.iter().enumerate() {
            let statement_following = self.requirements_from(index).union(following);
            let statement_control = plan_statements_headers(
                &recipe.preceding_statements,
                context,
                statement_following,
            )?;
            if statement_control != HeaderControl::Continue {
                return Ok(statement_control);
            }
            let gate = recipe.planning_gate(state);
            let (matched, condition_results) = if gate {
                recipe.matches_headers(context.head, context.runtime, context.trace)?
            } else {
                (PartialMatch::False, Vec::new())
            };
            if matched == PartialMatch::Deferred {
                context.trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Deferred,
                });
                context.planning.frames.push(ContinuationFrame {
                    recipe_index: index,
                    state,
                    condition_results,
                    assignments_applied: true,
                });
                context.planning.requirements = self.requirements_from(index).union(following);
                return Ok(HeaderControl::Deferred);
            }

            let conditions_matched = matched == PartialMatch::True;
            let else_handled = recipe.else_handled(state, conditions_matched);
            let has_error_handler = self.has_error_handler(index);
            if conditions_matched && recipe.delivery_defers_header(context.capture.is_some()) {
                context.trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Deferred,
                });
                context.planning.frames.push(ContinuationFrame {
                    recipe_index: index,
                    state,
                    condition_results,
                    assignments_applied: true,
                });
                context.planning.requirements = self.requirements_from(index).union(following);
                return Ok(HeaderControl::Deferred);
            }
            let mut action_execution = ActionExecution::NotAttempted;
            let control = if conditions_matched {
                action_execution = ActionExecution::Succeeded;
                context.trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                match &recipe.action {
                    CompiledAction::Pipe { .. } => {
                        return Err(
                            EvalError::ExternalActionUnsupported { line: recipe.line }.into()
                        );
                    }
                    CompiledAction::Capture { action, options } => {
                        let limit = super::ordered::active_command_value_limit::<E>(
                            context.runtime,
                            action.target,
                            action.line,
                        )?;
                        let executor = context
                            .capture
                            .as_deref_mut()
                            .ok_or(EvalError::ExternalActionUnsupported { line: recipe.line })?;
                        match executor(
                            &action.command,
                            context.head.as_bytes(),
                            options.output_ending,
                            Some(*options),
                            limit,
                            context.runtime,
                            context.trace,
                        ) {
                            Ok(captured) => {
                                let value = validate_captured_value(
                                    captured.into_output(),
                                    limit,
                                    &action.name,
                                    CapturedNewlineRule::StripOne,
                                )?;
                                context.runtime.set_bytes_with_trace(
                                    action.name.clone(),
                                    value,
                                    Some(action.line),
                                    TraceVariableSource::RcFile,
                                    context.trace,
                                );
                                context.planning.pending_error = None;
                                HeaderControl::Continue
                            }
                            Err(DeliveryAttemptError::Recoverable(error)) => {
                                context.planning.pending_error = Some(error);
                                action_execution = ActionExecution::Failed;
                                HeaderControl::Continue
                            }
                            Err(DeliveryAttemptError::Fatal(error)) => {
                                return Err(OrderedExecutionError::Delivery(error));
                            }
                        }
                    }
                    CompiledAction::Headers(action) => {
                        let action = action
                            .resolve_with(|name| context.runtime.get(name).map(str::to_owned))
                            .map_err(EvalError::Expansion)?;
                        let edited = crate::header_edit::apply_header_action(
                            context.head.as_bytes(),
                            0,
                            &action,
                            context.head.limits(),
                        )
                        .map_err(|error| EvalError::HeaderEdit {
                            line: recipe.line,
                            message: error.to_string(),
                        })?;
                        context.head.replace_edited_header(edited);
                        HeaderControl::Continue
                    }
                    CompiledAction::Deliver { .. } => {
                        let control = recipe.plan_delivery(
                            context.runtime,
                            context.execution,
                            has_error_handler,
                        )?;
                        HeaderControl::from(control)
                    }
                    CompiledAction::Block(children) => {
                        // Store the parent before descending so the path is
                        // ordered from the root and never exceeds the parser's
                        // recipe nesting limit.
                        context.planning.frames.push(ContinuationFrame {
                            recipe_index: index,
                            state,
                            condition_results: Vec::new(),
                            assignments_applied: true,
                        });
                        let child_following = self.requirements_from(index + 1).union(following);
                        let child = children.plan_headers(context, child_following)?;
                        if child != HeaderControl::Deferred {
                            context.planning.frames.pop();
                        }
                        child
                    }
                }
            } else {
                context.trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Skipped,
                });
                HeaderControl::Continue
            };
            if control == HeaderControl::Deferred {
                return Ok(control);
            }
            if action_execution == ActionExecution::Succeeded {
                context.planning.pending_error = None;
            }
            state.record(
                recipe.control,
                conditions_matched,
                action_execution,
                else_handled,
            );
            if control != HeaderControl::Continue {
                return Ok(control);
            }
        }

        let statement_control =
            plan_statements_headers(&self.trailing_statements, context, following)?;
        if statement_control != HeaderControl::Continue {
            return Ok(statement_control);
        }
        Ok(HeaderControl::Continue)
    }

    fn resume_from_frames<T: TraceSink>(
        &self,
        cursor: ResumeCursor<'_>,
        context: &mut CompletePlanContext<'_, '_, T>,
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
            let control =
                execute_statements(&recipe.preceding_statements, context.runtime, context.trace)?;
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
                context,
            )?;
            (true, control)
        } else {
            let conditions_matched = recipe.planning_gate(state)
                && recipe.matches_resumed(
                    context.message,
                    &frame.condition_results,
                    context.runtime,
                    context.trace,
                )?;
            let control = if conditions_matched {
                context.trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                recipe.plan_action(self.has_error_handler(frame.recipe_index), context)?
            } else {
                context.trace.record(TraceEvent::RecipeEvaluated {
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
        // Paths whose result is unavailable are moved to ordered execution
        // before reaching this function. Header capture is different: its
        // process has already finished, so a/e must use that actual result.
        match self.control {
            ControlFlow::Independent => true,
            ControlFlow::AfterChainMatch => state.chain_base_matched.unwrap_or(false),
            ControlFlow::AfterPreviousSuccess => state.previous.is_some_and(|result| {
                result.conditions_matched && result.action == ActionExecution::Succeeded
            }),
            ControlFlow::AfterPreviousError => state.previous.is_some_and(|result| {
                result.conditions_matched && result.action == ActionExecution::Failed
            }),
            ControlFlow::Else => state.previous.is_none_or(|result| !result.else_handled),
        }
    }

    fn delivery_defers_header(&self, capture_available: bool) -> bool {
        match &self.action {
            CompiledAction::Pipe { .. } => true,
            CompiledAction::Capture { options, .. } => {
                !capture_available || options.action_input != ActionInput::Headers
            }
            CompiledAction::Deliver { destination, .. } => {
                destination.needs_runtime_variables() || destination.requires_ordered_delivery()
            }
            CompiledAction::Block(_) => false,
            CompiledAction::Headers(_) => false,
        }
    }

    fn plan_action<T: TraceSink>(
        &self,
        has_error_handler: bool,
        context: &mut CompletePlanContext<'_, '_, T>,
    ) -> Result<SequenceControl, EvalError> {
        match &self.action {
            CompiledAction::Pipe { .. } | CompiledAction::Capture { .. } => {
                Err(EvalError::ExternalActionUnsupported { line: self.line })
            }
            CompiledAction::Deliver { .. } => {
                self.plan_delivery(context.runtime, context.execution, has_error_handler)
            }
            CompiledAction::Block(children) => {
                if self.lock.is_some() {
                    return Err(EvalError::LocalLockExecutorUnavailable { line: self.line });
                }
                children.plan_complete(context)
            }
            CompiledAction::Headers(_) => {
                Err(EvalError::HeaderActionUnsupported { line: self.line })
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
        let umask = RuntimeSettings::at_line(runtime, self.line)
            .umask()
            .map_err(runtime_setting_eval_error)?;
        execution.deliveries.push(PlannedDelivery {
            destination,
            continuation: if copy {
                DeliveryContinuation::Continue
            } else {
                DeliveryContinuation::Stop
            },
            output_ending: *output_ending,
            lock,
            umask,
        });
        execution.original_delivered |= !copy;
        if copy || has_error_handler {
            Ok(SequenceControl::Continue)
        } else {
            Ok(SequenceControl::Stop)
        }
    }
}

fn plan_statements_complete<T: TraceSink>(
    statements: &[CompiledStatement],
    context: &mut CompletePlanContext<'_, '_, T>,
) -> Result<SequenceControl, EvalError> {
    for statement in statements {
        match statement {
            CompiledStatement::CommandAssignment(assignment) => {
                return Err(EvalError::ExternalActionUnsupported {
                    line: assignment.line,
                });
            }
            CompiledStatement::Assignment(assignment) => {
                execute_assignment(assignment, context.runtime, context.trace)?;
            }
            CompiledStatement::Host(assignment) => {
                if !execute_host_assignment(assignment, context.runtime, context.trace)? {
                    context.execution.original_delivered = true;
                    return Ok(SequenceControl::EndRcFile);
                }
            }
            CompiledStatement::Include(include) => {
                let entered = include.enter(context.runtime, context.rc)?;
                if let Some((sequence, child_rc)) = entered.sequence()? {
                    let mut child = CompletePlanContext {
                        message: context.message,
                        runtime: context.runtime,
                        trace: context.trace,
                        execution: context.execution,
                        rc: child_rc,
                    };
                    if sequence.plan_complete_with_context(&mut child)? == SequenceControl::Stop {
                        return Ok(SequenceControl::Stop);
                    }
                }
            }
            CompiledStatement::Switch(switch) => {
                // Run the replacement as a separate rc-file scope, then use
                // EndRcFile to unwind every enclosing recipe block. An
                // INCLUDERC boundary consumes that result and resumes its
                // caller, while the root treats it as end of processing.
                let entered = switch.enter(context.runtime, context.rc)?;
                if entered.is_empty() {
                    return Ok(SequenceControl::EndRcFile);
                }
                if let Some((sequence, child_rc)) = entered.sequence()? {
                    let mut child = CompletePlanContext {
                        message: context.message,
                        runtime: context.runtime,
                        trace: context.trace,
                        execution: context.execution,
                        rc: child_rc,
                    };
                    let control = sequence.plan_complete_with_context(&mut child)?;
                    return Ok(if control == SequenceControl::Stop {
                        SequenceControl::Stop
                    } else {
                        SequenceControl::EndRcFile
                    });
                }
            }
        }
    }
    Ok(SequenceControl::Continue)
}

fn plan_statements_headers<E, T>(
    statements: &[CompiledStatement],
    context: &mut HeaderPlanContext<'_, '_, E, T>,
    following: InputRequirements,
) -> Result<HeaderControl, OrderedExecutionError<E>>
where
    T: TraceSink,
{
    for statement in statements {
        match statement {
            CompiledStatement::CommandAssignment(assignment) => {
                return Err(EvalError::ExternalActionUnsupported {
                    line: assignment.line,
                }
                .into());
            }
            CompiledStatement::Assignment(assignment) => {
                execute_assignment(assignment, context.runtime, context.trace)?;
            }
            CompiledStatement::Host(assignment) => {
                if !execute_host_assignment(assignment, context.runtime, context.trace)? {
                    context.execution.original_delivered = true;
                    return Ok(HeaderControl::EndRcFile);
                }
            }
            CompiledStatement::Include(include) => {
                let entered = include.enter(context.runtime, context.rc)?;
                if let Some((sequence, child_rc)) = entered.sequence()? {
                    if sequence.requires_preemptive_ordered_delivery() {
                        context.planning.frames.clear();
                        context.planning.restart = true;
                        context.planning.requirements = sequence
                            .requirements()
                            .union(following)
                            .union(InputRequirements {
                                needs_end_of_message: true,
                                ..InputRequirements::default()
                            });
                        return Ok(HeaderControl::Deferred);
                    }
                    let parent_rc = context.rc;
                    context.rc = child_rc;
                    let child = sequence.plan_headers(context, following);
                    context.rc = parent_rc;
                    let child = child?;
                    if child == HeaderControl::Deferred {
                        // Continuation frames point into the static root tree.
                        // A dynamically loaded child cannot be represented by
                        // that path, so replay the still-private plan once the
                        // selected message sections have been staged.
                        context.planning.frames.clear();
                        context.planning.restart = true;
                        context.planning.requirements = context
                            .planning
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
                let entered = switch.enter(context.runtime, context.rc)?;
                if entered.is_empty() {
                    return Ok(HeaderControl::EndRcFile);
                }
                if let Some((sequence, child_rc)) = entered.sequence()? {
                    if sequence.requires_preemptive_ordered_delivery() {
                        context.planning.frames.clear();
                        context.planning.restart = true;
                        context.planning.requirements =
                            sequence.requirements().union(InputRequirements {
                                needs_end_of_message: true,
                                ..InputRequirements::default()
                            });
                        return Ok(HeaderControl::Deferred);
                    }
                    let parent_rc = context.rc;
                    context.rc = child_rc;
                    let child = sequence.plan_headers(context, InputRequirements::default());
                    context.rc = parent_rc;
                    let child = child?;
                    if child == HeaderControl::Deferred {
                        // Replaying from the root reconstructs the dynamic
                        // target without retaining pointers into its tree.
                        // Nothing after SWITCHRC remains reachable.
                        context.planning.frames.clear();
                        context.planning.restart = true;
                        context.planning.requirements =
                            context.planning.requirements.union(sequence.requirements());
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
    Ok(HeaderControl::Continue)
}
