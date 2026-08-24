// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[derive(Debug)]
struct SequenceExecution {
    deliveries: usize,
    original_delivered: bool,
    pending_error: Option<EvalError>,
}

impl CompiledSequence {
    fn execute(
        &self,
        message: &mut Message,
        limits: MessageLimits,
        delivery: &mut impl Delivery,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut SequenceExecution,
    ) -> Result<SequenceControl, EvalError> {
        let mut state = SequenceState::default();

        for recipe in &self.recipes {
            let control = execute_statements(&recipe.preceding_statements, runtime, trace)?;
            if control != SequenceControl::Continue {
                return Ok(control);
            }

            // Control-flow flags inspect only results produced at this block
            // level. Child sequences therefore cannot overwrite the state
            // used by the next sibling recipe.
            let matching_full = recipe
                .needs_message_contents()
                .then(|| message.matching_message())
                .flatten();
            let matching = CompleteMessage::Buffered {
                message,
                matching_full: matching_full.as_deref(),
            };
            let conditions_matched = recipe.execution_gate(state)
                && recipe.matches_complete(matching, runtime, trace)?;
            let else_handled = recipe.else_handled(state, conditions_matched);

            let (action, control) = if conditions_matched {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                recipe.execute_action(message, limits, delivery, runtime, trace, execution)?
            } else {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Skipped,
                });
                (ActionExecution::NotAttempted, SequenceControl::Continue)
            };

            state.record(recipe.control, conditions_matched, action, else_handled);
            if control != SequenceControl::Continue {
                return Ok(control);
            }
        }

        execute_statements(&self.trailing_statements, runtime, trace)
    }
}

impl CompiledNode {
    fn execute_action(
        &self,
        message: &mut Message,
        limits: MessageLimits,
        delivery: &mut impl Delivery,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut SequenceExecution,
    ) -> Result<(ActionExecution, SequenceControl), EvalError> {
        match &self.action {
            CompiledAction::Pipe { .. } => {
                Err(EvalError::ExternalActionUnsupported { line: self.line })
            }
            CompiledAction::Headers(action) => {
                let action = action
                    .resolve_with(|name| runtime.get(name).map(str::to_owned))
                    .map_err(EvalError::Expansion)?;
                let edited = crate::header_edit::apply_header_action(
                    message.header(),
                    message.body().len(),
                    &action,
                    limits,
                )
                .map_err(|error| EvalError::HeaderEdit {
                    line: self.line,
                    message: error.to_string(),
                })?;
                *message =
                    message
                        .with_edited_header(edited)
                        .map_err(|error| EvalError::HeaderEdit {
                            line: self.line,
                            message: error.to_string(),
                        })?;
                execution.pending_error = None;
                Ok((ActionExecution::Succeeded, SequenceControl::Continue))
            }
            CompiledAction::Deliver {
                destination,
                continuation,
                ..
            } => {
                let destination =
                    match destination.bind_with(|name| runtime.get(name).map(str::to_owned)) {
                        Ok(destination) => destination,
                        Err(error) => {
                            execution.pending_error = Some(EvalError::Expansion(error));
                            return Ok((ActionExecution::Failed, SequenceControl::Continue));
                        }
                    };
                match delivery.deliver(&destination, message) {
                    Ok(()) => {
                        execution.deliveries += 1;
                        execution.pending_error = None;
                        if *continuation == ContinuationMode::Stop {
                            execution.original_delivered = true;
                            Ok((ActionExecution::Succeeded, SequenceControl::Stop))
                        } else {
                            Ok((ActionExecution::Succeeded, SequenceControl::Continue))
                        }
                    }
                    Err(message) => {
                        execution.pending_error = Some(EvalError::Delivery {
                            destination: destination.path().to_owned(),
                            message,
                        });
                        Ok((ActionExecution::Failed, SequenceControl::Continue))
                    }
                }
            }
            CompiledAction::Block(children) => {
                if self.lock.is_some() {
                    return Err(EvalError::LocalLockExecutorUnavailable { line: self.line });
                }
                // A selected block owns the outcome of its child sequence.
                // Discard an older sibling failure before entering it so an
                // empty or fully skipped block can still complete normally.
                execution.pending_error = None;
                let control =
                    children.execute(message, limits, delivery, runtime, trace, execution)?;
                let action = if execution.pending_error.is_some() {
                    ActionExecution::Failed
                } else {
                    ActionExecution::Succeeded
                };
                Ok((action, control))
            }
        }
    }
}

pub fn evaluate(
    config: &Config,
    message: &Message,
    delivery: &mut impl Delivery,
) -> Result<Outcome, EvalError> {
    let plan = ExecutionPlan::compile(config);
    let limits = plan
        .message_limits
        .clone()
        .map_err(EvalError::MessageLimits)?;
    let mut message = message.clone();
    let mut execution = SequenceExecution {
        deliveries: 0,
        original_delivered: false,
        pending_error: None,
    };
    plan.root.execute(
        &mut message,
        limits,
        delivery,
        &mut RuntimeVariables::default(),
        &mut NoTrace,
        &mut execution,
    )?;
    if let Some(error) = execution.pending_error {
        return Err(error);
    }

    if execution.original_delivered {
        Ok(Outcome::Delivered {
            deliveries: execution.deliveries,
        })
    } else {
        Ok(Outcome::Undelivered {
            copies: execution.deliveries,
        })
    }
}
