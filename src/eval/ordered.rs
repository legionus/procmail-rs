// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

struct OrderedTreeExecution<'a, E, D, T> {
    message: CompleteMessage<'a>,
    replacement: Option<OwnedCompleteMessage>,
    runtime: &'a mut RuntimeVariables,
    trace: &'a mut T,
    deliver: &'a mut D,
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

pub trait RecipeLockGuard {}

impl<T> RecipeLockGuard for T {}

type ExternalActionExecutor<'a, E, T> = dyn FnMut(
        &PipeAction,
        RecipeOptions,
        Option<&str>,
        ExternalActionInput<'_>,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<Option<Message>, DeliveryAttemptError<E>>
    + 'a;

type ExternalConditionExecutor<'a, E, T> = dyn FnMut(&str, &[u8], &mut RuntimeVariables, &mut T) -> Result<bool, DeliveryAttemptError<E>>
    + 'a;

type CommandCaptureExecutor<'a, E, T> = dyn FnMut(
        &str,
        &[u8],
        OutputEnding,
        Option<RecipeOptions>,
        usize,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<CapturedCommand, DeliveryAttemptError<E>>
    + 'a;

type GlobalLockExecutor<'a, E> = dyn FnMut(&str, &mut RuntimeVariables) -> Result<(), E> + 'a;

type LocalLockExecutor<'a, E> = dyn FnMut(&str, &mut RuntimeVariables) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<E>>
    + 'a;

type CompletionExecutor<'a, E, T> =
    dyn FnMut(FinalMessage<'_>, &mut RuntimeVariables, &mut T, CompletionState<'_, E>) + 'a;

struct OptionalOrderedExecutors<'a, E, T> {
    external: Option<&'a mut ExternalActionExecutor<'a, E, T>>,
    capture: Option<&'a mut CommandCaptureExecutor<'a, E, T>>,
    external_condition: Option<&'a mut ExternalConditionExecutor<'a, E, T>>,
    global_lock: Option<&'a mut GlobalLockExecutor<'a, E>>,
    local_lock: Option<&'a mut LocalLockExecutor<'a, E>>,
}

impl<E, D, T> OrderedTreeExecution<'_, E, D, T> {
    fn replace_message(&mut self, message: Message) {
        let matching_full = message.matching_message();
        self.replacement = Some(OwnedCompleteMessage {
            message,
            matching_full,
        });
    }
}

impl CompiledSequence {
    fn execute_ordered<E, D, T>(
        &self,
        context: &mut OrderedTreeExecution<'_, E, D, T>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
            OutputEnding,
            Option<&str>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
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
    fn matches_ordered<E, D, T>(
        &self,
        context: &mut OrderedTreeExecution<'_, E, D, T>,
    ) -> Result<bool, OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
            OutputEnding,
            Option<&str>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
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
                    let limit = context
                        .runtime
                        .get("LINEBUF")
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(crate::config::DEFAULT_LINEBUF);
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
                let input = match input {
                    ConditionInput::Headers => Some(message.raw_header()),
                    ConditionInput::Body => message.body(),
                    ConditionInput::Message => message.raw(),
                }
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

    fn execute_ordered_action<E, D, T>(
        &self,
        context: &mut OrderedTreeExecution<'_, E, D, T>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
            OutputEnding,
            Option<&str>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
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
                        context
                            .runtime
                            .set_bytes(action.name.clone(), value.clone());
                        if let Ok(name) = TraceName::new(&action.name) {
                            context.trace.record(TraceEvent::VariableAssigned {
                                line: Some(action.line),
                                name,
                                source: TraceVariableSource::RcFile,
                                value: context
                                    .trace
                                    .detail()
                                    .includes_variable_values()
                                    .then(|| TraceValue::new(&value)),
                            });
                        }
                        context.pending_error = None;
                        Ok((ActionExecution::Succeeded, SequenceControl::Continue))
                    }
                    Err(DeliveryAttemptError::Recoverable(error)) => {
                        context.pending_error = Some(error);
                        Ok((ActionExecution::Failed, SequenceControl::Continue))
                    }
                    Err(DeliveryAttemptError::Fatal(error)) => {
                        Err(OrderedExecutionError::Delivery(error))
                    }
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
                context.pending_error = None;
                Ok((ActionExecution::Succeeded, SequenceControl::Continue))
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
                        context.pending_error = None;
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
                            Ok((ActionExecution::Succeeded, SequenceControl::Continue))
                        } else if replacement.is_some() {
                            Err(OrderedExecutionError::Evaluation(
                                EvalError::InvalidExternalActionResult {
                                    line: self.line,
                                    reason: "non-filter pipe returned a replacement message",
                                },
                            ))
                        } else if options.continuation == ContinuationMode::Stop {
                            context.original_delivered = true;
                            Ok((ActionExecution::Succeeded, SequenceControl::Stop))
                        } else {
                            Ok((ActionExecution::Succeeded, SequenceControl::Continue))
                        }
                    }
                    Err(DeliveryAttemptError::Recoverable(error)) => {
                        context.pending_error = Some(error);
                        Ok((ActionExecution::Failed, SequenceControl::Continue))
                    }
                    Err(DeliveryAttemptError::Fatal(error)) => {
                        Err(OrderedExecutionError::Delivery(error))
                    }
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
                        destination.command_line(),
                    )?
                    .min(crate::config::MAX_PATH_EXPRESSION_LEN);
                    let bytes = evaluate_shell_expression(
                        ShellExpressionInput {
                            parts: &parts.parts,
                            line: destination.command_line(),
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
                            line: destination.command_line(),
                        })
                        .map_err(OrderedExecutionError::Evaluation)?;
                    destination
                        .resolve_command_output(source, context.runtime.get("MAILDIR"))
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
                        context.pending_error = None;
                        if *continuation == ContinuationMode::Stop {
                            context.original_delivered = true;
                            Ok((ActionExecution::Succeeded, SequenceControl::Stop))
                        } else {
                            Ok((ActionExecution::Succeeded, SequenceControl::Continue))
                        }
                    }
                    Err(DeliveryAttemptError::Recoverable(error)) => {
                        context.pending_error = Some(error);
                        Ok((ActionExecution::Failed, SequenceControl::Continue))
                    }
                    Err(DeliveryAttemptError::Fatal(error)) => {
                        Err(OrderedExecutionError::Delivery(error))
                    }
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
                            context.pending_error = Some(error);
                            return Ok((ActionExecution::Failed, SequenceControl::Continue));
                        }
                        Err(DeliveryAttemptError::Fatal(error)) => {
                            return Err(OrderedExecutionError::Delivery(error));
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

fn execute_statements_ordered<E, D, T>(
    statements: &[CompiledStatement],
    context: &mut OrderedTreeExecution<'_, E, D, T>,
) -> Result<SequenceControl, OrderedExecutionError<E>>
where
    D: FnMut(
        &Destination,
        &[u8],
        OutputEnding,
        Option<&str>,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<(), DeliveryAttemptError<E>>,
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
                include
                    .ensure_loaded(context.runtime, context.rc)
                    .map_err(OrderedExecutionError::Evaluation)?;
                if let LoadedRuntimeRc::Sequence(sequence) = &*include.loaded() {
                    let previous = context.rc;
                    context.rc = previous
                        .descend()
                        .map_err(OrderedExecutionError::Evaluation)?;
                    let result = sequence.execute_ordered(context);
                    context.rc = previous;
                    let (_, control) = result?;
                    if control == SequenceControl::Stop {
                        return Ok(control);
                    }
                }
            }
            CompiledStatement::Switch(switch) => {
                // Preserve the same rc-file boundary while deliveries happen
                // immediately. Restoring the caller context matters when the
                // switch belongs to a file entered through INCLUDERC.
                switch
                    .ensure_loaded(context.runtime, context.rc)
                    .map_err(OrderedExecutionError::Evaluation)?;
                match &*switch.loaded() {
                    LoadedRuntimeRc::Unloaded => unreachable!(),
                    LoadedRuntimeRc::Failed => {}
                    LoadedRuntimeRc::Empty => return Ok(SequenceControl::EndRcFile),
                    LoadedRuntimeRc::Sequence(sequence) => {
                        let previous = context.rc;
                        context.rc = previous
                            .descend()
                            .map_err(OrderedExecutionError::Evaluation)?;
                        let result = sequence.execute_ordered(context);
                        context.rc = previous;
                        let (_, control) = result?;
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

fn execute_command_assignment<E, D, T>(
    assignment: &crate::config::CommandAssignment,
    context: &mut OrderedTreeExecution<'_, E, D, T>,
) -> Result<(), OrderedExecutionError<E>>
where
    D: FnMut(
        &Destination,
        &[u8],
        OutputEnding,
        Option<&str>,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<(), DeliveryAttemptError<E>>,
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

    context
        .runtime
        .set_bytes(assignment.name.clone(), value.clone());
    if let Ok(name) = TraceName::new(&assignment.name) {
        context.trace.record(TraceEvent::VariableAssigned {
            line: Some(assignment.line),
            name,
            source: TraceVariableSource::RcFile,
            value: context
                .trace
                .detail()
                .includes_variable_values()
                .then(|| TraceValue::new(&value)),
        });
    }
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
    let mut value = Vec::new();

    // Build the complete result privately. Commands can fail, time out, or
    // exceed the remaining budget after earlier literal fragments; callers
    // must never observe a partial variable or destination path.
    for part in input.parts {
        let remaining = input.limit.checked_sub(value.len()).ok_or_else(|| {
            OrderedExecutionError::Evaluation(EvalError::VariableValueTooLarge {
                name: input.value_name.to_owned(),
                size: value.len(),
            })
        })?;
        let mut bytes = match part {
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
        if bytes.len() > remaining {
            return Err(OrderedExecutionError::Evaluation(
                EvalError::VariableValueTooLarge {
                    name: input.value_name.to_owned(),
                    size: value.len().saturating_add(bytes.len()),
                },
            ));
        }
        value.append(&mut bytes);
    }
    Ok(value)
}

pub(super) fn active_command_value_limit<E>(
    runtime: &RuntimeVariables,
    target: AssignmentTarget,
    line: usize,
) -> Result<usize, OrderedExecutionError<E>> {
    let linebuf = match runtime.get("LINEBUF") {
        Some(value) => value.parse::<usize>().map_err(|_| {
            OrderedExecutionError::Evaluation(EvalError::RuntimeSettingUnavailable {
                line,
                name: "LINEBUF",
            })
        })?,
        None => crate::config::DEFAULT_LINEBUF,
    };
    Ok(linebuf.min(crate::config::assignment_value_limit(target)))
}

impl ExecutionPlan {
    pub fn execute_mapped_ordered_with_trace<E, D, T>(
        &self,
        raw: &[u8],
        header_len: usize,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
        deliver: &mut D,
    ) -> Result<DeliveryOutcome, OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
            OutputEnding,
            Option<&str>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
        T: TraceSink,
    {
        self.execute_mapped_ordered_with_matching_trace(
            raw, header_len, None, runtime, trace, deliver,
        )
    }

    pub fn execute_mapped_ordered_with_matching_trace<E, D, T>(
        &self,
        raw: &[u8],
        header_len: usize,
        matching: Option<MatchingMessage<'_>>,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
        deliver: &mut D,
    ) -> Result<DeliveryOutcome, OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
            OutputEnding,
            Option<&str>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
        T: TraceSink,
    {
        self.execute_mapped_ordered_inner(
            MappedMessageInput::new(raw, header_len, matching),
            runtime,
            trace,
            deliver,
            OptionalOrderedExecutors {
                external: None,
                capture: None,
                external_condition: None,
                global_lock: None,
                local_lock: None,
            },
            None,
        )
    }

    pub fn execute_mapped_ordered_with_external_trace<E, D, X, T>(
        &self,
        message: MappedMessageInput<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
        deliver: &mut D,
        external: &mut X,
    ) -> Result<DeliveryOutcome, OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
            OutputEnding,
            Option<&str>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
        X: FnMut(
            &PipeAction,
            RecipeOptions,
            Option<&str>,
            ExternalActionInput<'_>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<Option<Message>, DeliveryAttemptError<E>>,
        T: TraceSink,
    {
        self.execute_mapped_ordered_inner(
            message,
            runtime,
            trace,
            deliver,
            OptionalOrderedExecutors {
                external: Some(external),
                capture: None,
                external_condition: None,
                global_lock: None,
                local_lock: None,
            },
            None,
        )
    }

    pub fn execute_mapped_ordered_with_capture_trace<E, D, K, T>(
        &self,
        message: MappedMessageInput<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
        deliver: &mut D,
        capture: &mut K,
    ) -> Result<DeliveryOutcome, OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
            OutputEnding,
            Option<&str>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
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
        self.execute_mapped_ordered_inner(
            message,
            runtime,
            trace,
            deliver,
            OptionalOrderedExecutors {
                external: None,
                capture: Some(capture),
                external_condition: None,
                global_lock: None,
                local_lock: None,
            },
            None,
        )
    }

    pub fn execute_mapped_ordered_with_processes_trace<E, D, C, X, K, G, L, T>(
        &self,
        message: MappedMessageInput<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
        deliver: &mut D,
        executors: (&mut C, &mut X, &mut K, &mut G, &mut L),
    ) -> Result<DeliveryOutcome, OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
            OutputEnding,
            Option<&str>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
        C: FnMut(
            &str,
            &[u8],
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<bool, DeliveryAttemptError<E>>,
        X: FnMut(
            &PipeAction,
            RecipeOptions,
            Option<&str>,
            ExternalActionInput<'_>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<Option<Message>, DeliveryAttemptError<E>>,
        K: FnMut(
            &str,
            &[u8],
            OutputEnding,
            Option<RecipeOptions>,
            usize,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<CapturedCommand, DeliveryAttemptError<E>>,
        G: FnMut(&str, &mut RuntimeVariables) -> Result<(), E>,
        L: FnMut(
            &str,
            &mut RuntimeVariables,
        ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<E>>,
        T: TraceSink,
    {
        let (external_condition, external, capture, global_lock, local_lock) = executors;
        self.execute_mapped_ordered_inner(
            message,
            runtime,
            trace,
            deliver,
            OptionalOrderedExecutors {
                external: Some(external),
                capture: Some(capture),
                external_condition: Some(external_condition),
                global_lock: Some(global_lock),
                local_lock: Some(local_lock),
            },
            None,
        )
    }

    pub fn execute_mapped_ordered_with_processes_and_completion_trace<E, D, C, X, K, G, L, F, T>(
        &self,
        message: MappedMessageInput<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
        deliver: &mut D,
        executors: (&mut C, &mut X, &mut K, &mut G, &mut L),
        completion: &mut F,
    ) -> Result<DeliveryOutcome, OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
            OutputEnding,
            Option<&str>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
        C: FnMut(
            &str,
            &[u8],
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<bool, DeliveryAttemptError<E>>,
        X: FnMut(
            &PipeAction,
            RecipeOptions,
            Option<&str>,
            ExternalActionInput<'_>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<Option<Message>, DeliveryAttemptError<E>>,
        K: FnMut(
            &str,
            &[u8],
            OutputEnding,
            Option<RecipeOptions>,
            usize,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<CapturedCommand, DeliveryAttemptError<E>>,
        G: FnMut(&str, &mut RuntimeVariables) -> Result<(), E>,
        L: FnMut(
            &str,
            &mut RuntimeVariables,
        ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<E>>,
        F: FnMut(FinalMessage<'_>, &mut RuntimeVariables, &mut T, CompletionState<'_, E>),
        T: TraceSink,
    {
        let (external_condition, external, capture, global_lock, local_lock) = executors;
        self.execute_mapped_ordered_inner(
            message,
            runtime,
            trace,
            deliver,
            OptionalOrderedExecutors {
                external: Some(external),
                capture: Some(capture),
                external_condition: Some(external_condition),
                global_lock: Some(global_lock),
                local_lock: Some(local_lock),
            },
            Some(completion),
        )
    }

    fn execute_mapped_ordered_inner<'a, E, D, T>(
        &'a self,
        message: MappedMessageInput<'a>,
        runtime: &'a mut RuntimeVariables,
        trace: &'a mut T,
        deliver: &'a mut D,
        executors: OptionalOrderedExecutors<'a, E, T>,
        mut completion: Option<&mut CompletionExecutor<'_, E, T>>,
    ) -> Result<DeliveryOutcome, OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
            OutputEnding,
            Option<&str>,
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
        T: TraceSink,
    {
        let message = message
            .complete_message(self.needs_message_contents())
            .ok_or(OrderedExecutionError::Evaluation(
                EvalError::BodyWasNotBuffered,
            ))?;
        let mut context = OrderedTreeExecution {
            message,
            replacement: None,
            runtime,
            trace,
            deliver,
            published: 0,
            original_delivered: false,
            pending_error: None,
            external: executors.external,
            capture: executors.capture,
            external_condition: executors.external_condition,
            global_lock: executors.global_lock,
            local_lock: executors.local_lock,
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
