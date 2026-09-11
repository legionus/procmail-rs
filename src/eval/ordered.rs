// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::services::OrderedExecutionHost;
use super::*;
use crate::bounded_bytes::BoundedBytesError;
use crate::config::shell_eval::{
    self, EvaluationContext, EvaluationDepth, UnsupportedPart, VariableValue,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) const MAX_BACKGROUND_COPY_BRANCHES: usize = 128;

pub(super) struct BackgroundCopyBudget {
    started: AtomicUsize,
}

impl BackgroundCopyBudget {
    pub(super) fn new() -> Self {
        Self {
            started: AtomicUsize::new(0),
        }
    }

    pub(super) fn reserve(&self, line: usize) -> Result<(), EvalError> {
        // Rust newer than the supported 1.85 release renamed this operation
        // to `try_update`. Keep the older spelling until the minimum compiler
        // can provide the replacement.
        #[allow(deprecated)]
        let reservation =
            self.started
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |started| {
                    (started < MAX_BACKGROUND_COPY_BRANCHES).then_some(started + 1)
                });
        reservation
            .map(|_| ())
            .map_err(|_| EvalError::BackgroundCopyUnavailable {
                line,
                reason: format!(
                    "the limit of {MAX_BACKGROUND_COPY_BRANCHES} branches per message was reached"
                ),
            })
    }
}

struct OrderedTreeExecution<'a, H: OrderedExecutionHost> {
    message: CompleteMessage<'a>,
    current_message: CurrentMessage,
    runtime: &'a mut RuntimeVariables,
    host: &'a mut H,
    published: usize,
    original_delivered: bool,
    pending_error: Option<H::Error>,
    rc: RcExecutionContext<'a>,
    limits: MessageLimits,
    copy_budget: Arc<BackgroundCopyBudget>,
}

type OrderedActionResult<H> = Result<
    (ActionExecution, SequenceControl),
    OrderedExecutionError<<H as OrderedExecutionHost>::Error>,
>;

impl<'a, H: OrderedExecutionHost + Send> OrderedTreeExecution<'a, H> {
    fn replace_message(&mut self, message: Message) {
        self.current_message.replace(message);
    }

    fn action_succeeded(&mut self, control: SequenceControl) -> OrderedActionResult<H> {
        self.pending_error = None;
        Ok((ActionExecution::Succeeded, control))
    }

    fn action_failed(&mut self, error: H::Error) -> OrderedActionResult<H> {
        self.pending_error = Some(error);
        Ok((ActionExecution::Failed, SequenceControl::Continue))
    }

    fn action_failed_fatally(&mut self, error: H::Error) -> OrderedActionResult<H> {
        Err(OrderedExecutionError::Delivery(error))
    }

    fn execute_runtime_rc(
        &mut self,
        sequence: &CompiledSequence,
        child_context: RcExecutionContext<'a>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<H::Error>>
    where
        H::Trace: TraceSink,
        H::Error: Send,
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
    fn execute_ordered<H>(
        &self,
        context: &mut OrderedTreeExecution<'_, H>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<H::Error>>
    where
        H: OrderedExecutionHost + Send,
        H::Error: Send,
        H::Trace: TraceSink,
    {
        self.execute_ordered_from(0, SequenceState::default(), context)
    }

    fn execute_ordered_from<H>(
        &self,
        start: usize,
        mut state: SequenceState,
        context: &mut OrderedTreeExecution<'_, H>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<H::Error>>
    where
        H: OrderedExecutionHost + Send,
        H::Error: Send,
        H::Trace: TraceSink,
    {
        let mut sequence_action = ActionExecution::Succeeded;

        // A copy-block branch resumes this same loop after its block while the
        // parent also advances to that recipe independently. Keeping the
        // cursor and chain state explicit prevents the two paths from gaining
        // subtly different handling for statements, A/a/E/e, or termination.
        for (offset, recipe) in self.recipes[start..].iter().enumerate() {
            let index = start + offset;
            let statement_control =
                execute_statements_ordered(&recipe.preceding_statements, context)?;
            if statement_control != SequenceControl::Continue {
                return Ok((sequence_action, statement_control));
            }
            let conditions_matched =
                recipe.execution_gate(state) && recipe.matches_ordered(context)?;
            let else_handled = recipe.else_handled(state, conditions_matched);
            let (action, control) = if conditions_matched {
                context.host.trace().record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                if recipe.is_waited_copy_block() {
                    self.execute_waited_copy_block(index, state, recipe, context)?
                } else if recipe.is_unwaited_copy_block() {
                    self.execute_unwaited_copy_block(index, state, recipe, context)?
                } else {
                    recipe.execute_ordered_action(context)?
                }
            } else {
                context.host.trace().record(TraceEvent::RecipeEvaluated {
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

    fn execute_waited_copy_block<H>(
        &self,
        index: usize,
        mut branch_state: SequenceState,
        recipe: &CompiledNode,
        context: &mut OrderedTreeExecution<'_, H>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<H::Error>>
    where
        H: OrderedExecutionHost + Send,
        H::Error: Send,
        H::Trace: TraceSink,
    {
        let branch_runtime = context.runtime.fork();
        let parent_runtime = std::mem::replace(context.runtime, branch_runtime);
        let parent_message = context.current_message.clone();
        let parent_pending_error = context.pending_error.take();
        let parent_original_delivered = context.original_delivered;
        let parent_rc = context.rc;
        context.host.enter_copy_branch();

        // Run the block and its possible continuation with branch-local mail
        // and variables. Publication accounting remains shared because every
        // successful delivery is externally visible, while delivery status,
        // pending errors, and rc transitions must not leak into the parent.
        let branch_result = recipe
            .execute_ordered_action(context)
            .and_then(|(action, control)| {
                branch_state.record(recipe.control, true, action, false);
                if control == SequenceControl::Continue {
                    self.execute_ordered_from(index + 1, branch_state, context)
                } else {
                    Ok((action, control))
                }
            });
        let branch_pending_error = context.pending_error.take();

        context.host.leave_copy_branch();
        let _branch_runtime = std::mem::replace(context.runtime, parent_runtime);
        context.current_message = parent_message;
        context.pending_error = parent_pending_error;
        context.original_delivered = parent_original_delivered;
        context.rc = parent_rc;

        match branch_result {
            Err(error) => Err(error),
            Ok(_) => match branch_pending_error {
                Some(error) => context.action_failed(error),
                None => context.action_succeeded(SequenceControl::Continue),
            },
        }
    }

    fn execute_unwaited_copy_block<H>(
        &self,
        index: usize,
        state: SequenceState,
        recipe: &CompiledNode,
        context: &mut OrderedTreeExecution<'_, H>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<H::Error>>
    where
        H: OrderedExecutionHost + Send,
        H::Error: Send,
        H::Trace: TraceSink,
    {
        context
            .copy_budget
            .reserve(recipe.line)
            .map_err(OrderedExecutionError::Evaluation)?;
        let mut branch_host = context.host.fork_copy_branch().ok_or_else(|| {
            OrderedExecutionError::Evaluation(EvalError::BackgroundCopyUnavailable {
                line: recipe.line,
                reason: "the execution host cannot create an isolated branch".to_owned(),
            })
        })?;
        let mut branch_runtime = context.runtime.fork();
        let branch_message = context.current_message.clone();
        let branch_rc = context.rc;
        let branch_limits = context.limits;
        let branch_budget = Arc::clone(&context.copy_budget);
        let complete_message = context.message;

        // The branch owns its variables, current message, commands, and locks,
        // while the immutable compiled sequence and staged message are shared.
        // A scoped thread keeps those borrows valid and forces a join before
        // staging can disappear, including when parent evaluation fails.
        std::thread::scope(|scope| {
            let branch = std::thread::Builder::new()
                .name("procmail-rs-copy".to_owned())
                .spawn_scoped(scope, move || {
                    let mut branch_context = OrderedTreeExecution {
                        message: complete_message,
                        current_message: branch_message,
                        runtime: &mut branch_runtime,
                        host: &mut branch_host,
                        published: 0,
                        original_delivered: false,
                        pending_error: None,
                        rc: branch_rc,
                        limits: branch_limits,
                        copy_budget: branch_budget,
                    };
                    let mut branch_state = state;
                    let execution = recipe.execute_ordered_action(&mut branch_context).and_then(
                        |(action, control)| {
                            branch_state.record(recipe.control, true, action, false);
                            if control == SequenceControl::Continue {
                                self.execute_ordered_from(
                                    index + 1,
                                    branch_state,
                                    &mut branch_context,
                                )
                            } else {
                                Ok((action, control))
                            }
                        },
                    );
                    let supervision = branch_context.host.finish_background();
                    (execution, supervision, branch_context.published)
                })
                .map_err(|error| {
                    OrderedExecutionError::Evaluation(EvalError::BackgroundCopyUnavailable {
                        line: recipe.line,
                        reason: error.to_string(),
                    })
                })?;

            let mut parent_state = state;
            parent_state.record(recipe.control, true, ActionExecution::Succeeded, false);
            let parent = self.execute_ordered_from(index + 1, parent_state, context);
            let joined = branch.join().map_err(|_| {
                OrderedExecutionError::Evaluation(EvalError::BackgroundCopyUnavailable {
                    line: recipe.line,
                    reason: "the branch worker terminated unexpectedly".to_owned(),
                })
            })?;
            context.published = context.published.checked_add(joined.2).ok_or_else(|| {
                OrderedExecutionError::Evaluation(EvalError::BackgroundCopyUnavailable {
                    line: recipe.line,
                    reason: "published destination count overflows".to_owned(),
                })
            })?;

            // Plain `c` does not wait for or apply the branch action status in
            // original procmail. We still join for resource supervision, but
            // only a failure of that supervision changes the parent result.
            if let Err(error) = joined.1 {
                return Err(OrderedExecutionError::Delivery(error));
            }
            if let Err(OrderedExecutionError::Evaluation(error)) = joined.0 {
                return Err(OrderedExecutionError::Evaluation(error));
            }
            parent.map(|(action, _)| (action, SequenceControl::SequenceComplete))
        })
    }
}

impl CompiledNode {
    fn is_waited_copy_block(&self) -> bool {
        matches!(self.action, CompiledAction::Block(_))
            && self.continuation == ContinuationMode::Continue
            && (self.child_status != crate::config::ChildStatusMode::Ignore || self.lock.is_some())
    }

    fn is_unwaited_copy_block(&self) -> bool {
        matches!(self.action, CompiledAction::Block(_))
            && self.continuation == ContinuationMode::Continue
            && self.child_status == crate::config::ChildStatusMode::Ignore
            && self.lock.is_none()
    }
}

impl CompiledNode {
    fn matches_ordered<H>(
        &self,
        context: &mut OrderedTreeExecution<'_, H>,
    ) -> Result<bool, OrderedExecutionError<H::Error>>
    where
        H: OrderedExecutionHost + Send,
        H::Error: Send,
        H::Trace: TraceSink,
    {
        for (index, condition) in self.conditions.iter().enumerate() {
            let message = context.current_message.view(context.message);
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
                            expression,
                            line,
                            value_name: "shell-expanded condition",
                            message: raw,
                            limit,
                        },
                        context.runtime,
                        context.host,
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
                crate::trace::record_external_command(
                    condition.line,
                    command,
                    context.host.trace(),
                );
                match context
                    .host
                    .external_condition(command, input, context.runtime)
                {
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
                context.host.trace(),
            );
            if !matched {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn execute_ordered_action<H>(
        &self,
        context: &mut OrderedTreeExecution<'_, H>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<H::Error>>
    where
        H: OrderedExecutionHost + Send,
        H::Error: Send,
        H::Trace: TraceSink,
    {
        match &self.action {
            CompiledAction::Capture { action, options } => {
                let message = context.current_message.view(context.message);
                let input = message
                    .action_input(options.action_input)
                    .ok_or(EvalError::BodyWasNotBuffered)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let limit =
                    active_command_value_limit(context.runtime, action.target, action.line)?;
                crate::trace::record_external_command(
                    action.line,
                    &action.command,
                    context.host.trace(),
                );
                let captured = context.host.capture(
                    &action.command,
                    input,
                    options.output_ending,
                    Some(*options),
                    limit,
                    context.runtime,
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
                            context.host.trace(),
                        );
                        context.action_succeeded(SequenceControl::Continue)
                    }
                    Err(DeliveryAttemptError::Recoverable(error)) => context.action_failed(error),
                    Err(DeliveryAttemptError::Fatal(error)) => context.action_failed_fatally(error),
                }
            }
            CompiledAction::Headers(action) => {
                let message = context.current_message.view(context.message);
                let body = message
                    .body()
                    .ok_or(EvalError::BodyWasNotBuffered)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let action = action
                    .resolve_with(|name| context.runtime.get(name).map(str::to_owned))
                    .map_err(EvalError::Expansion)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let applied = crate::header_edit::apply_header_action(
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
                let (edited, extractions) = applied.into_parts();
                let message = Message::from_edited_header(edited, body)
                    .map_err(|error| EvalError::HeaderEdit {
                        line: self.line,
                        message: error.to_string(),
                    })
                    .map_err(OrderedExecutionError::Evaluation)?;
                context.replace_message(message);
                context
                    .runtime
                    .apply_header_extractions(extractions, context.host.trace());
                context.action_succeeded(SequenceControl::Continue)
            }
            CompiledAction::Pipe { action, options } => {
                let message = context.current_message.view(context.message);
                let input = message
                    .action_input(options.action_input)
                    .ok_or(EvalError::BodyWasNotBuffered)
                    .map_err(OrderedExecutionError::Evaluation)?;
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
                crate::trace::record_external_command(
                    self.line,
                    &action.command,
                    context.host.trace(),
                );
                match context.host.external_action(
                    action,
                    *options,
                    lock.as_deref(),
                    action_input,
                    context.runtime,
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
                let message = context
                    .current_message
                    .view(context.message)
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
                            expression: parts,
                            line: destination.line(),
                            value_name: "destination",
                            message,
                            limit,
                        },
                        context.runtime,
                        context.host,
                    )?;
                    let source = String::from_utf8(bytes)
                        .map_err(|_| EvalError::DestinationCommandOutputIsNotUtf8 {
                            line: destination.line(),
                        })
                        .map_err(OrderedExecutionError::Evaluation)?;
                    destination
                        .resolve_ordered_output(
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
                match context.host.deliver(
                    &destination,
                    message,
                    *output_ending,
                    lock.as_deref(),
                    context.runtime,
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
                    match context.host.acquire_local_lock(path, context.runtime) {
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
                children.execute_ordered(context).map(|(action, control)| {
                    if control == SequenceControl::SequenceComplete {
                        (action, SequenceControl::Continue)
                    } else {
                        (action, control)
                    }
                })
            }
        }
    }
}

fn execute_statements_ordered<H>(
    statements: &[CompiledStatement],
    context: &mut OrderedTreeExecution<'_, H>,
) -> Result<SequenceControl, OrderedExecutionError<H::Error>>
where
    H: OrderedExecutionHost + Send,
    H::Error: Send,
    H::Trace: TraceSink,
{
    for statement in statements {
        match statement {
            CompiledStatement::CommandAssignment(assignment) => {
                execute_command_assignment(assignment, context)?;
            }
            CompiledStatement::Assignment(assignment) => {
                execute_assignment(assignment, context.runtime, context.host.trace())
                    .map_err(OrderedExecutionError::Evaluation)?;
                if assignment.assignment.target == AssignmentTarget::LockFile {
                    let value = context
                        .runtime
                        .get("LOCKFILE")
                        .unwrap_or_default()
                        .to_owned();
                    context
                        .host
                        .replace_global_lock(&value, context.runtime)
                        .map_err(OrderedExecutionError::Delivery)?;
                }
            }
            CompiledStatement::Host(assignment) => {
                if !execute_host_assignment(assignment, context.runtime, context.host.trace())
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

fn execute_command_assignment<H>(
    assignment: &crate::config::CommandAssignment,
    context: &mut OrderedTreeExecution<'_, H>,
) -> Result<(), OrderedExecutionError<H::Error>>
where
    H: OrderedExecutionHost,
    H::Trace: TraceSink,
{
    let message = context
        .current_message
        .view(context.message)
        .raw()
        .ok_or(EvalError::BodyWasNotBuffered)
        .map_err(OrderedExecutionError::Evaluation)?;
    let limit = active_command_value_limit(context.runtime, assignment.target, assignment.line)?;
    let value = evaluate_shell_expression(
        ShellExpressionInput {
            expression: &assignment.expression,
            line: assignment.line,
            value_name: &assignment.name,
            message,
            limit,
        },
        context.runtime,
        context.host,
    )?;

    context.runtime.set_bytes_with_trace(
        assignment.name.clone(),
        value,
        Some(assignment.line),
        TraceVariableSource::RcFile,
        context.host.trace(),
    );
    Ok(())
}

struct ShellExpressionInput<'a> {
    expression: &'a crate::config::ShellExpression,
    line: usize,
    value_name: &'a str,
    message: &'a [u8],
    limit: usize,
}

fn evaluate_shell_expression<E, T>(
    input: ShellExpressionInput<'_>,
    runtime: &mut RuntimeVariables,
    host: &mut dyn OrderedExecutionHost<Error = E, Trace = T>,
) -> Result<Vec<u8>, OrderedExecutionError<E>>
where
    T: TraceSink,
{
    let mut context = OrderedExpressionEvaluation {
        line: input.line,
        value_name: input.value_name,
        message: input.message,
        runtime,
        host,
    };
    let evaluated = shell_eval::evaluate(input.expression, input.limit, &mut context)?;
    for (name, value) in evaluated.assignments {
        context.runtime.set_bytes_with_trace(
            name,
            value,
            Some(input.line),
            TraceVariableSource::RcFile,
            context.host.trace(),
        );
    }
    Ok(evaluated.bytes)
}

struct OrderedExpressionEvaluation<'context, 'input, E, T> {
    line: usize,
    value_name: &'input str,
    message: &'input [u8],
    runtime: &'context mut RuntimeVariables,
    host: &'context mut dyn OrderedExecutionHost<Error = E, Trace = T>,
}

impl<E, T> EvaluationContext for OrderedExpressionEvaluation<'_, '_, E, T>
where
    T: TraceSink,
{
    type Error = OrderedExecutionError<E>;

    fn depth_mode(&self) -> EvaluationDepth {
        EvaluationDepth::None
    }

    fn variable(&mut self, name: &str) -> Result<Option<VariableValue>, Self::Error> {
        Ok(self.runtime.get_bytes(name).map(|bytes| VariableValue {
            bytes: bytes.to_vec(),
            depth: 0,
        }))
    }

    fn command(&mut self, command: &str, remaining: usize) -> Result<Vec<u8>, Self::Error> {
        crate::trace::record_external_command(self.line, command, self.host.trace());
        let captured =
            self.host
                .capture(
                    command,
                    self.message,
                    OutputEnding::Preserve,
                    None,
                    remaining,
                    self.runtime,
                )
                .map_err(|error| match error {
                    DeliveryAttemptError::Recoverable(error)
                    | DeliveryAttemptError::Fatal(error) => OrderedExecutionError::Delivery(error),
                })?;
        validate_captured_value(
            captured.into_output(),
            remaining,
            self.value_name,
            CapturedNewlineRule::StripAll,
        )
        .map_err(OrderedExecutionError::Evaluation)
    }

    fn regex_quoted(&mut self, name: &str, remaining: usize) -> Result<Vec<u8>, Self::Error> {
        let source = self
            .runtime
            .get_bytes(name)
            .ok_or_else(|| self.missing_variable(name))?;
        let mut escaped = Vec::new();
        crate::config::expand::push_regex_escaped(&mut escaped, source, remaining, self.line)
            .map_err(EvalError::Expansion)
            .map_err(OrderedExecutionError::Evaluation)?;
        Ok(escaped)
    }

    fn missing_variable(&self, name: &str) -> Self::Error {
        OrderedExecutionError::Evaluation(EvalError::Expansion(crate::config::ExpansionError {
            line: self.line,
            message: format!("variable {name} is not defined"),
        }))
    }

    fn required_parameter(&self, name: &str) -> Self::Error {
        OrderedExecutionError::Evaluation(EvalError::Expansion(crate::config::ExpansionError {
            line: self.line,
            message: format!("parameter {name} is unset or empty"),
        }))
    }

    fn pattern_error(&self, error: crate::config::shell_pattern::PatternError) -> Self::Error {
        OrderedExecutionError::Evaluation(EvalError::Expansion(crate::config::ExpansionError {
            line: self.line,
            message: error.to_string(),
        }))
    }

    fn unsupported_part(&self, _: UnsupportedPart) -> Self::Error {
        OrderedExecutionError::Evaluation(EvalError::Expansion(crate::config::ExpansionError {
            line: self.line,
            message: "expression part is not supported during ordered evaluation".to_owned(),
        }))
    }

    fn depth_exceeded(&self) -> Self::Error {
        OrderedExecutionError::Evaluation(EvalError::Expansion(crate::config::ExpansionError {
            line: self.line,
            message: format!(
                "variable expansion exceeds the hard depth limit of {}",
                crate::config::MAX_EXPANSION_DEPTH
            ),
        }))
    }

    fn depth_overflow(&self) -> Self::Error {
        OrderedExecutionError::Evaluation(EvalError::Expansion(crate::config::ExpansionError {
            line: self.line,
            message: "variable expansion depth overflows".to_owned(),
        }))
    }

    fn length_error(&self, error: BoundedBytesError, current: usize, _: usize) -> Self::Error {
        OrderedExecutionError::Evaluation(EvalError::VariableValueTooLarge {
            name: self.value_name.to_owned(),
            size: match error {
                BoundedBytesError::LengthOverflow => usize::MAX,
                BoundedBytesError::LimitExceeded { attempted } => attempted.max(current),
            },
        })
    }
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
    Ok(linebuf.min(target.value_limit()))
}

impl ExecutionPlan {
    pub fn execute_ordered<'a, H>(
        &'a self,
        message: MappedMessageInput<'a>,
        runtime: &'a mut RuntimeVariables,
        mut host: H,
    ) -> Result<DeliveryOutcome, OrderedExecutionError<H::Error>>
    where
        H: OrderedExecutionHost + Send,
        H::Error: Send,
    {
        let message = message
            .complete_message(self.needs_message_contents())
            .ok_or(OrderedExecutionError::Evaluation(
                EvalError::BodyWasNotBuffered,
            ))?;
        let mut context = OrderedTreeExecution {
            message,
            current_message: CurrentMessage::default(),
            runtime,
            host: &mut host,
            published: 0,
            original_delivered: false,
            pending_error: None,
            rc: self.rc_context(),
            limits: self
                .message_limits
                .clone()
                .map_err(EvalError::MessageLimits)
                .map_err(OrderedExecutionError::Evaluation)?,
            copy_budget: Arc::new(BackgroundCopyBudget::new()),
        };
        let execution = self.root.execute_ordered(&mut context);
        let background = context.host.finish_background();
        let result = match (execution, background) {
            (_, Err(error)) => Err(OrderedExecutionError::Delivery(error)),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Ok(())) => match context.pending_error.take() {
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
        let Some(message) = context.current_message.view(context.message).raw() else {
            return Err(OrderedExecutionError::Evaluation(
                EvalError::BodyWasNotBuffered,
            ));
        };
        let state = match &result {
            Ok(outcome) => CompletionState::Completed(*outcome),
            Err(error) => CompletionState::Failed(error),
        };
        context
            .host
            .complete(FinalMessage::new(message), context.runtime, state);
        result
    }
}
