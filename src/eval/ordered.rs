// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::services::{
    CommandCaptureExecutor, DeliveryExecutor, ExternalActionExecutor, ExternalConditionExecutor,
    GlobalLockExecutor, LocalLockExecutor,
};
use super::*;
use crate::bounded_bytes::{BoundedBytes, BoundedBytesError};

struct OrderedTreeExecution<'a, E, T> {
    message: CompleteMessage<'a>,
    replacement: Option<OwnedCompleteMessage>,
    runtime: &'a mut RuntimeVariables,
    trace: &'a mut T,
    deliver: &'a mut DeliveryExecutor<'a, E, T>,
    published: usize,
    original_delivered: bool,
    pending_error: Option<E>,
    external: Option<&'a mut ExternalActionExecutor<'a, E, T>>,
    capture: Option<&'a mut CommandCaptureExecutor<'a, E, T>>,
    external_condition: Option<&'a mut ExternalConditionExecutor<'a, E, T>>,
    global_lock: Option<&'a mut GlobalLockExecutor<'a, E>>,
    local_lock: Option<&'a mut LocalLockExecutor<'a, E>>,
    rc: RcExecutionContext<'a>,
    limits: MessageLimits,
}

type OrderedActionResult<E> = Result<(ActionExecution, SequenceControl), OrderedExecutionError<E>>;

impl<'a, E, T> OrderedTreeExecution<'a, E, T> {
    fn replace_message(&mut self, message: Message) {
        let matching = PreparedMatchingMessage::new(&message, true);
        self.replacement = Some(OwnedCompleteMessage { message, matching });
    }

    fn action_succeeded(&mut self, control: SequenceControl) -> OrderedActionResult<E> {
        self.pending_error = None;
        Ok((ActionExecution::Succeeded, control))
    }

    fn action_failed(&mut self, error: E) -> OrderedActionResult<E> {
        self.pending_error = Some(error);
        Ok((ActionExecution::Failed, SequenceControl::Continue))
    }

    fn action_failed_fatally(&mut self, error: E) -> OrderedActionResult<E> {
        Err(OrderedExecutionError::Delivery(error))
    }

    fn execute_runtime_rc(
        &mut self,
        sequence: &CompiledSequence,
        child_context: RcExecutionContext<'a>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<E>>
    where
        T: TraceSink,
    {
        // The mutable execution object carries the active rc context for
        // nested actions. Restore its caller value before propagating either
        // success or failure so an included file cannot leak its depth into
        // the statements that follow it.
        let caller_context = std::mem::replace(&mut self.rc, child_context);
        let result = sequence.execute_ordered(self);
        self.rc = caller_context;
        result
    }
}

impl CompiledSequence {
    fn execute_ordered<E, T>(
        &self,
        context: &mut OrderedTreeExecution<'_, E, T>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<E>>
    where
        T: TraceSink,
    {
        let mut state = SequenceState::default();
        let mut sequence_action = ActionExecution::Succeeded;

        // A block reports its latest attempted child action to its parent.
        // An unhandled copy failure therefore escapes the block, while a
        // successful child error handler replaces that failure.
        for recipe in &self.recipes {
            let statement_control =
                execute_statements_ordered(&recipe.preceding_statements, context)?;
            if statement_control != SequenceControl::Continue {
                return Ok((sequence_action, statement_control));
            }
            let conditions_matched =
                recipe.execution_gate(state) && recipe.matches_ordered(context)?;
            let else_handled = recipe.else_handled(state, conditions_matched);
            let (action, control) = if conditions_matched {
                context.trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                recipe.execute_ordered_action(context)?
            } else {
                context.trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Skipped,
                });
                (ActionExecution::NotAttempted, SequenceControl::Continue)
            };
            state.record(recipe.control, conditions_matched, action, else_handled);
            if action == ActionExecution::Failed {
                sequence_action = ActionExecution::Failed;
            } else if action == ActionExecution::Succeeded {
                sequence_action = ActionExecution::Succeeded;
            }
            if control != SequenceControl::Continue {
                return Ok((sequence_action, control));
            }
        }

        let statement_control = execute_statements_ordered(&self.trailing_statements, context)?;
        if statement_control != SequenceControl::Continue {
            return Ok((sequence_action, statement_control));
        }
        Ok((sequence_action, SequenceControl::Continue))
    }
}

impl CompiledNode {
    fn matches_ordered<E, T>(
        &self,
        context: &mut OrderedTreeExecution<'_, E, T>,
    ) -> Result<bool, OrderedExecutionError<E>>
    where
        T: TraceSink,
    {
        for (index, condition) in self.conditions.iter().enumerate() {
            let message = current_ordered_message(context.message, context.replacement.as_ref());
            let resolved = condition.resolve_shell_expansion_with(
                |shell, line| {
                    let parsed;
                    let expression = if let Some(expression) = shell.expansion.as_ref() {
                        expression
                    } else {
                        parsed = crate::config::expand::parse_shell_condition_expression(
                            &shell.source,
                            line,
                        )
                        .map_err(EvalError::Expansion)
                        .map_err(OrderedExecutionError::Evaluation)?;
                        &parsed
                    };
                    let limit = RuntimeSettings::at_line(context.runtime, line)
                        .linebuf()
                        .map_err(runtime_setting_eval_error)
                        .map_err(OrderedExecutionError::Evaluation)?;
                    let raw = message
                        .raw()
                        .ok_or(EvalError::BodyWasNotBuffered)
                        .map_err(OrderedExecutionError::Evaluation)?;
                    let bytes = evaluate_shell_expression(
                        ShellExpressionInput {
                            parts: &expression.parts,
                            line,
                            value_name: "shell-expanded condition",
                            message: raw,
                            limit,
                        },
                        context.runtime,
                        context.trace,
                        &mut context.capture,
                    )?;
                    String::from_utf8(bytes)
                        .map_err(|_| EvalError::RuntimeCondition {
                            line,
                            message: "shell-expanded condition contains non-UTF-8 data".to_owned(),
                        })
                        .map_err(OrderedExecutionError::Evaluation)
                },
                OrderedExecutionError::Evaluation,
            )?;
            let condition = resolved.as_ref().unwrap_or(condition);
            let matched = if let Some((command, input)) = condition.program() {
                let input = message
                    .program_input(input)
                    .ok_or(EvalError::BodyWasNotBuffered)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let Some(executor) = context.external_condition.as_deref_mut() else {
                    return Err(OrderedExecutionError::Evaluation(
                        EvalError::ExternalConditionUnsupported {
                            line: condition.line,
                        },
                    ));
                };
                match executor(command, input, context.runtime, context.trace) {
                    Ok(matched) => condition.apply_negation(matched),
                    Err(DeliveryAttemptError::Recoverable(error))
                    | Err(DeliveryAttemptError::Fatal(error)) => {
                        return Err(OrderedExecutionError::Delivery(error));
                    }
                }
            } else {
                condition
                    .matches_complete(message, context.runtime)
                    .map_err(OrderedExecutionError::Evaluation)?
            };
            condition.trace_result(
                self.line,
                index,
                PartialMatch::from_bool(matched),
                context.trace,
            );
            if !matched {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn execute_ordered_action<E, T>(
        &self,
        context: &mut OrderedTreeExecution<'_, E, T>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<E>>
    where
        T: TraceSink,
    {
        match &self.action {
            CompiledAction::Capture { action, options } => {
                let message =
                    current_ordered_message(context.message, context.replacement.as_ref());
                let input = message
                    .action_input(options.action_input)
                    .ok_or(EvalError::BodyWasNotBuffered)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let limit =
                    active_command_value_limit(context.runtime, action.target, action.line)?;
                let executor = context.capture.as_deref_mut().ok_or_else(|| {
                    OrderedExecutionError::Evaluation(EvalError::ExternalActionUnsupported {
                        line: self.line,
                    })
                })?;
                let captured = executor(
                    &action.command,
                    input,
                    options.output_ending,
                    Some(*options),
                    limit,
                    context.runtime,
                    context.trace,
                );
                match captured {
                    Ok(captured) => {
                        let value = validate_captured_value(
                            captured.into_output(),
                            limit,
                            &action.name,
                            CapturedNewlineRule::StripOne,
                        )
                        .map_err(OrderedExecutionError::Evaluation)?;
                        context.runtime.set_bytes_with_trace(
                            action.name.clone(),
                            value,
                            Some(action.line),
                            TraceVariableSource::RcFile,
                            context.trace,
                        );
                        context.action_succeeded(SequenceControl::Continue)
                    }
                    Err(DeliveryAttemptError::Recoverable(error)) => context.action_failed(error),
                    Err(DeliveryAttemptError::Fatal(error)) => context.action_failed_fatally(error),
                }
            }
            CompiledAction::Headers(action) => {
                let message =
                    current_ordered_message(context.message, context.replacement.as_ref());
                let body = message
                    .body()
                    .ok_or(EvalError::BodyWasNotBuffered)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let action = action
                    .resolve_with(|name| context.runtime.get(name).map(str::to_owned))
                    .map_err(EvalError::Expansion)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let edited = crate::header_edit::apply_header_action(
                    message.raw_header(),
                    body.len(),
                    &action,
                    context.limits,
                )
                .map_err(|error| EvalError::HeaderEdit {
                    line: self.line,
                    message: error.to_string(),
                })
                .map_err(OrderedExecutionError::Evaluation)?;
                let message = Message::from_edited_header(edited, body)
                    .map_err(|error| EvalError::HeaderEdit {
                        line: self.line,
                        message: error.to_string(),
                    })
                    .map_err(OrderedExecutionError::Evaluation)?;
                context.replace_message(message);
                context.action_succeeded(SequenceControl::Continue)
            }
            CompiledAction::Pipe { action, options } => {
                let message =
                    current_ordered_message(context.message, context.replacement.as_ref());
                let input = message
                    .action_input(options.action_input)
                    .ok_or(EvalError::BodyWasNotBuffered)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let Some(external) = context.external.as_deref_mut() else {
                    return Err(OrderedExecutionError::Evaluation(
                        EvalError::ExternalActionUnsupported { line: self.line },
                    ));
                };

                // Keep the old message alive until the external executor has
                // completed and validated all output. Only an accepted filter
                // result replaces the owned current version used by later
                // recipes in this sequence.
                let action_input = ExternalActionInput {
                    selected: input,
                    header: message.raw_header(),
                    body: message
                        .body()
                        .ok_or(EvalError::BodyWasNotBuffered)
                        .map_err(OrderedExecutionError::Evaluation)?,
                };
                let lock = self
                    .resolve_lock(context.runtime)
                    .map_err(EvalError::Expansion)
                    .map_err(OrderedExecutionError::Evaluation)?;
                match external(
                    action,
                    *options,
                    lock.as_deref(),
                    action_input,
                    context.runtime,
                    context.trace,
                ) {
                    Ok(replacement) => {
                        if options.action_mode == crate::config::ActionMode::Filter {
                            let message = replacement.ok_or_else(|| {
                                OrderedExecutionError::Evaluation(
                                    EvalError::InvalidExternalActionResult {
                                        line: self.line,
                                        reason: "filter completed without a replacement message",
                                    },
                                )
                            })?;
                            context.replace_message(message);
                            context.action_succeeded(SequenceControl::Continue)
                        } else if replacement.is_some() {
                            Err(OrderedExecutionError::Evaluation(
                                EvalError::InvalidExternalActionResult {
                                    line: self.line,
                                    reason: "non-filter pipe returned a replacement message",
                                },
                            ))
                        } else if options.continuation == ContinuationMode::Stop {
                            context.original_delivered = true;
                            context.action_succeeded(SequenceControl::Stop)
                        } else {
                            context.action_succeeded(SequenceControl::Continue)
                        }
                    }
                    Err(DeliveryAttemptError::Recoverable(error)) => context.action_failed(error),
                    Err(DeliveryAttemptError::Fatal(error)) => context.action_failed_fatally(error),
                }
            }
            CompiledAction::Deliver {
                destination,
                continuation,
                output_ending,
            } => {
                let message =
                    current_ordered_message(context.message, context.replacement.as_ref())
                        .raw()
                        .ok_or(EvalError::BodyWasNotBuffered)
                        .map_err(OrderedExecutionError::Evaluation)?;
                let destination = if let Some(parts) = destination.command_expression() {
                    let limit = active_command_value_limit(
                        context.runtime,
                        AssignmentTarget::User,
                        destination.line(),
                    )?
                    .min(crate::config::MAX_PATH_EXPRESSION_LEN);
                    let bytes = evaluate_shell_expression(
                        ShellExpressionInput {
                            parts: &parts.parts,
                            line: destination.line(),
                            value_name: "destination",
                            message,
                            limit,
                        },
                        context.runtime,
                        context.trace,
                        &mut context.capture,
                    )?;
                    let source = String::from_utf8(bytes)
                        .map_err(|_| EvalError::DestinationCommandOutputIsNotUtf8 {
                            line: destination.line(),
                        })
                        .map_err(OrderedExecutionError::Evaluation)?;
                    destination
                        .resolve_command_output(
                            source,
                            RuntimeSettings::new(context.runtime).maildir(),
                        )
                        .map_err(EvalError::Expansion)
                        .map_err(OrderedExecutionError::Evaluation)?
                } else {
                    destination
                        .bind_with(|name| context.runtime.get(name).map(str::to_owned))
                        .map_err(EvalError::Expansion)
                        .map_err(OrderedExecutionError::Evaluation)?
                };
                let lock = self
                    .resolve_lock(context.runtime)
                    .map_err(EvalError::Expansion)
                    .map_err(OrderedExecutionError::Evaluation)?;
                match (context.deliver)(
                    &destination,
                    message,
                    *output_ending,
                    lock.as_deref(),
                    context.runtime,
                    context.trace,
                ) {
                    Ok(()) => {
                        context.published += 1;
                        if *continuation == ContinuationMode::Stop {
                            context.original_delivered = true;
                            context.action_succeeded(SequenceControl::Stop)
                        } else {
                            context.action_succeeded(SequenceControl::Continue)
                        }
                    }
                    Err(DeliveryAttemptError::Recoverable(error)) => context.action_failed(error),
                    Err(DeliveryAttemptError::Fatal(error)) => context.action_failed_fatally(error),
                }
            }
            CompiledAction::Block(children) => {
                // Do not let an older sibling error determine this block's
                // result. Child actions either leave their latest failure in
                // the context or clear it by completing successfully.
                context.pending_error = None;
                let lock = self
                    .resolve_lock(context.runtime)
                    .map_err(EvalError::Expansion)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let _guard = if let Some(path) = lock.as_deref() {
                    let executor = context.local_lock.as_mut().ok_or_else(|| {
                        OrderedExecutionError::Evaluation(EvalError::LocalLockExecutorUnavailable {
                            line: self.line,
                        })
                    })?;
                    match executor(path, context.runtime) {
                        Ok(guard) => Some(guard),
                        Err(DeliveryAttemptError::Recoverable(error)) => {
                            return context.action_failed(error);
                        }
                        Err(DeliveryAttemptError::Fatal(error)) => {
                            return context.action_failed_fatally(error);
                        }
                    }
                } else {
                    None
                };

                // Keep the guard in this stack frame while every child result
                // propagates upward. Normal completion, delivery failure,
                // HOST/SWITCHRC control flow, and evaluation errors all leave
                // through this scope and therefore release the same lock.
                children.execute_ordered(context)
            }
        }
    }
}

fn execute_statements_ordered<E, T>(
    statements: &[CompiledStatement],
    context: &mut OrderedTreeExecution<'_, E, T>,
) -> Result<SequenceControl, OrderedExecutionError<E>>
where
    T: TraceSink,
{
    for statement in statements {
        match statement {
            CompiledStatement::CommandAssignment(assignment) => {
                execute_command_assignment(assignment, context)?;
            }
            CompiledStatement::Assignment(assignment) => {
                execute_assignment(assignment, context.runtime, context.trace)
                    .map_err(OrderedExecutionError::Evaluation)?;
                if assignment.assignment.target == AssignmentTarget::LockFile {
                    let value = context
                        .runtime
                        .get("LOCKFILE")
                        .unwrap_or_default()
                        .to_owned();
                    let global_lock = context.global_lock.as_mut().ok_or_else(|| {
                        OrderedExecutionError::Evaluation(EvalError::RuntimeSettingUnavailable {
                            line: assignment.assignment.line,
                            name: "LOCKFILE",
                        })
                    })?;
                    global_lock(&value, context.runtime)
                        .map_err(OrderedExecutionError::Delivery)?;
                }
            }
            CompiledStatement::Host(assignment) => {
                if !execute_host_assignment(assignment, context.runtime, context.trace)
                    .map_err(OrderedExecutionError::Evaluation)?
                {
                    context.original_delivered = true;
                    context.pending_error = None;
                    return Ok(SequenceControl::EndRcFile);
                }
            }
            CompiledStatement::Include(include) => {
                let entered = include
                    .enter(context.runtime, context.rc)
                    .map_err(OrderedExecutionError::Evaluation)?;
                if let Some((sequence, child_context)) = entered
                    .sequence()
                    .map_err(OrderedExecutionError::Evaluation)?
                {
                    let (_, control) = context.execute_runtime_rc(sequence, child_context)?;
                    if control == SequenceControl::Stop {
                        return Ok(control);
                    }
                }
            }
            CompiledStatement::Switch(switch) => {
                // Preserve the same rc-file boundary while deliveries happen
                // immediately. Restoring the caller context matters when the
                // switch belongs to a file entered through INCLUDERC.
                let entered = switch
                    .enter(context.runtime, context.rc)
                    .map_err(OrderedExecutionError::Evaluation)?;
                if entered.is_empty() {
                    return Ok(SequenceControl::EndRcFile);
                }
                if let Some((sequence, child_context)) = entered
                    .sequence()
                    .map_err(OrderedExecutionError::Evaluation)?
                {
                    let (_, control) = context.execute_runtime_rc(sequence, child_context)?;
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

fn execute_command_assignment<E, T>(
    assignment: &crate::config::CommandAssignment,
    context: &mut OrderedTreeExecution<'_, E, T>,
) -> Result<(), OrderedExecutionError<E>>
where
    T: TraceSink,
{
    let message = current_ordered_message(context.message, context.replacement.as_ref())
        .raw()
        .ok_or(EvalError::BodyWasNotBuffered)
        .map_err(OrderedExecutionError::Evaluation)?;
    let limit = active_command_value_limit(context.runtime, assignment.target, assignment.line)?;
    let value = evaluate_shell_expression(
        ShellExpressionInput {
            parts: &assignment.expression.parts,
            line: assignment.line,
            value_name: &assignment.name,
            message,
            limit,
        },
        context.runtime,
        context.trace,
        &mut context.capture,
    )?;

    context.runtime.set_bytes_with_trace(
        assignment.name.clone(),
        value,
        Some(assignment.line),
        TraceVariableSource::RcFile,
        context.trace,
    );
    Ok(())
}

struct ShellExpressionInput<'a> {
    parts: &'a [crate::config::ShellPart],
    line: usize,
    value_name: &'a str,
    message: &'a [u8],
    limit: usize,
}

fn evaluate_shell_expression<E, T>(
    input: ShellExpressionInput<'_>,
    runtime: &mut RuntimeVariables,
    trace: &mut T,
    capture: &mut Option<&mut CommandCaptureExecutor<'_, E, T>>,
) -> Result<Vec<u8>, OrderedExecutionError<E>>
where
    T: TraceSink,
{
    let mut value = BoundedBytes::with_capacity(input.limit, 0);

    // Build the complete result privately. Commands can fail, time out, or
    // exceed the remaining budget after earlier literal fragments; callers
    // must never observe a partial variable or destination path.
    for part in input.parts {
        let remaining = value.remaining().map_err(|_| {
            OrderedExecutionError::Evaluation(EvalError::VariableValueTooLarge {
                name: input.value_name.to_owned(),
                size: value.len(),
            })
        })?;
        let bytes = match part {
            crate::config::ShellPart::Literal(source) => source.as_bytes().to_vec(),
            crate::config::ShellPart::Command(command) => {
                let executor = capture.as_deref_mut().ok_or_else(|| {
                    OrderedExecutionError::Evaluation(EvalError::ExternalActionUnsupported {
                        line: input.line,
                    })
                })?;
                let captured = executor(
                    command,
                    input.message,
                    OutputEnding::Preserve,
                    None,
                    remaining,
                    runtime,
                    trace,
                )
                .map_err(|error| match error {
                    DeliveryAttemptError::Recoverable(error)
                    | DeliveryAttemptError::Fatal(error) => OrderedExecutionError::Delivery(error),
                })?;
                validate_captured_value(
                    captured.into_output(),
                    remaining,
                    input.value_name,
                    CapturedNewlineRule::StripAll,
                )
                .map_err(OrderedExecutionError::Evaluation)?
            }
            crate::config::ShellPart::Variable { name, default } => {
                if let Some(bytes) = runtime.get_bytes(name).filter(|value| !value.is_empty()) {
                    bytes.to_vec()
                } else if let Some(default) = default {
                    evaluate_shell_expression(
                        ShellExpressionInput {
                            parts: &default.parts,
                            line: input.line,
                            value_name: input.value_name,
                            message: input.message,
                            limit: remaining,
                        },
                        runtime,
                        trace,
                        capture,
                    )?
                } else if let Some(bytes) = runtime.get_bytes(name) {
                    bytes.to_vec()
                } else {
                    return Err(OrderedExecutionError::Evaluation(EvalError::Expansion(
                        crate::config::ExpansionError {
                            line: input.line,
                            message: format!("variable {name} is not defined"),
                        },
                    )));
                }
            }
            crate::config::ShellPart::RegexQuotedVariable(name) => {
                let source = runtime.get_bytes(name).ok_or_else(|| {
                    OrderedExecutionError::Evaluation(EvalError::Expansion(
                        crate::config::ExpansionError {
                            line: input.line,
                            message: format!("variable {name} is not defined"),
                        },
                    ))
                })?;
                let mut escaped = Vec::new();
                crate::config::expand::push_regex_escaped(
                    &mut escaped,
                    source,
                    remaining,
                    input.line,
                )
                .map_err(EvalError::Expansion)
                .map_err(OrderedExecutionError::Evaluation)?;
                escaped
            }
        };
        value.try_extend(&bytes).map_err(|error| {
            OrderedExecutionError::Evaluation(EvalError::VariableValueTooLarge {
                name: input.value_name.to_owned(),
                size: match error {
                    BoundedBytesError::LengthOverflow => usize::MAX,
                    BoundedBytesError::LimitExceeded { attempted } => attempted,
                },
            })
        })?;
    }
    Ok(value.into_vec())
}

pub(super) fn active_command_value_limit<E>(
    runtime: &RuntimeVariables,
    target: AssignmentTarget,
    line: usize,
) -> Result<usize, OrderedExecutionError<E>> {
    let linebuf = RuntimeSettings::at_line(runtime, line)
        .linebuf()
        .map_err(runtime_setting_eval_error)
        .map_err(OrderedExecutionError::Evaluation)?;
    Ok(linebuf.min(crate::config::assignment_value_limit(target)))
}

impl ExecutionPlan {
    pub fn execute_ordered<'a, E, T>(
        &'a self,
        message: MappedMessageInput<'a>,
        runtime: &'a mut RuntimeVariables,
        services: ExecutionServices<'a, E, T>,
    ) -> Result<DeliveryOutcome, OrderedExecutionError<E>>
    where
        T: TraceSink,
    {
        let message = message
            .complete_message(self.needs_message_contents())
            .ok_or(OrderedExecutionError::Evaluation(
                EvalError::BodyWasNotBuffered,
            ))?;
        let ExecutionServices {
            delivery,
            trace,
            external,
            capture,
            external_condition,
            global_lock,
            local_lock,
            mut completion,
        } = services;
        let mut context = OrderedTreeExecution {
            message,
            replacement: None,
            runtime,
            trace,
            deliver: delivery,
            published: 0,
            original_delivered: false,
            pending_error: None,
            external,
            capture,
            external_condition,
            global_lock,
            local_lock,
            rc: self.rc_context(),
            limits: self
                .message_limits
                .clone()
                .map_err(EvalError::MessageLimits)
                .map_err(OrderedExecutionError::Evaluation)?,
        };
        let execution = self.root.execute_ordered(&mut context);
        let result = match execution {
            Err(error) => Err(error),
            Ok(_) => match context.pending_error.take() {
                Some(error) => Err(OrderedExecutionError::Delivery(error)),
                None => Ok(DeliveryOutcome {
                    published: context.published,
                    original_delivered: context.original_delivered,
                }),
            },
        };

        // The replacement buffer belongs to the evaluator and the original
        // bytes belong to mapped staging. Invoke completion while either
        // owner is still alive so callers such as TRAP can consume the final
        // message without allocating another message-sized buffer.
        if let Some(completion) = completion.as_mut() {
            let Some(message) =
                current_ordered_message(context.message, context.replacement.as_ref()).raw()
            else {
                return Err(OrderedExecutionError::Evaluation(
                    EvalError::BodyWasNotBuffered,
                ));
            };
            let state = match &result {
                Ok(outcome) => CompletionState::Completed(*outcome),
                Err(error) => CompletionState::Failed(error),
            };
            completion(
                FinalMessage::new(message),
                context.runtime,
                context.trace,
                state,
            );
        }
        result
    }
}
