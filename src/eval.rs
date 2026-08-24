// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use crate::config::{
    Assignment, AssignmentTarget, ConditionInput, Config, ContinuationMode, ControlFlow,
    Destination, OutputEnding, PipeAction, RecipeOptions,
};
use crate::limits::MessageLimits;
use crate::message::{Message, MessageHead, StreamedMessage};
use crate::rc_file::RcFileLoader;
use crate::runtime::RuntimeVariables;
use crate::trace::{
    NoTrace, RecipeDecision, TraceEvent, TraceName, TraceSink, TraceValue,
    VariableSource as TraceVariableSource,
};

mod condition;
mod explanation;
mod header;
mod message;
mod ordered;
mod result;
mod runtime_rc;
mod simple;
mod tree;

use condition::PartialMatch;
pub use explanation::{
    ActionKindExplanation, ConditionExplanation, ConditionKindExplanation,
    HeaderOperationExplanation, PlanExplanation, RecipeExplanation,
};
use header::FanoutPlanState;
use message::{CompleteMessage, OwnedCompleteMessage, current_ordered_message};
pub use message::{ExternalActionInput, FinalMessage, MappedMessageInput, MatchingMessage};
pub use ordered::RecipeLockGuard;
pub use result::{
    CompletionState, Continuation, DeliveryAttemptError, DeliveryOutcome, DeliveryPlan, EvalError,
    HeaderEvaluation, OrderedExecutionError, Outcome, PlannedDelivery,
};
use result::{ContinuationFrame, DeliveryContinuation};
pub use runtime_rc::MAX_RUNTIME_RC_WARNINGS;
use runtime_rc::{LoadedRuntimeRc, RcExecutionContext, RuntimeRcState};
pub use simple::evaluate;
use tree::{
    ActionExecution, CompiledAction, CompiledAssignment, CompiledNode, CompiledSequence,
    CompiledStatement, SequenceState,
};

pub trait Delivery {
    fn deliver(&mut self, destination: &Destination, message: &Message) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputRequirements {
    pub needs_headers: bool,
    pub needs_body_contents: bool,
    pub needs_end_of_message: bool,
}

impl InputRequirements {
    fn union(self, other: Self) -> Self {
        Self {
            needs_headers: self.needs_headers || other.needs_headers,
            needs_body_contents: self.needs_body_contents || other.needs_body_contents,
            needs_end_of_message: self.needs_end_of_message || other.needs_end_of_message,
        }
    }
}

#[derive(Debug)]
pub struct ExecutionPlan {
    root: CompiledSequence,
    requires_ordered_delivery: bool,
    requires_preemptive_ordered_delivery: bool,
    message_limits: Result<MessageLimits, String>,
    runtime_rc: RuntimeRcState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceControl {
    Continue,
    Stop,
    EndRcFile,
}

impl CompiledNode {
    fn resolve_lock(
        &self,
        runtime: &RuntimeVariables,
    ) -> Result<Option<String>, crate::config::ExpansionError> {
        self.lock
            .as_ref()
            .map(|expression| expression.resolve_with(|name| runtime.get(name).map(str::to_owned)))
            .transpose()
    }

    fn matches_complete(
        &self,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> Result<bool, EvalError> {
        for (index, condition) in self.conditions.iter().enumerate() {
            let matched = condition.matches_complete(message, runtime)?;
            condition.trace_result(self.line, index, PartialMatch::from_bool(matched), trace);
            if !matched {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn execution_gate(&self, state: SequenceState) -> bool {
        match self.control {
            ControlFlow::Independent => true,
            ControlFlow::AfterChainMatch => state.chain_base_matched.unwrap_or(false),
            ControlFlow::AfterPreviousSuccess => state.previous.is_some_and(|result| {
                result.conditions_matched && result.action == ActionExecution::Succeeded
            }),
            ControlFlow::Else => state.previous.is_none_or(|result| !result.else_handled),
            ControlFlow::AfterPreviousError => state
                .previous
                .is_some_and(|result| result.action == ActionExecution::Failed),
        }
    }

    fn else_handled(&self, state: SequenceState, conditions_matched: bool) -> bool {
        if self.control == ControlFlow::Else {
            state.previous.is_some_and(|result| result.else_handled) || conditions_matched
        } else {
            conditions_matched
        }
    }
}

fn execute_statements(
    statements: &[CompiledStatement],
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<SequenceControl, EvalError> {
    for statement in statements {
        match statement {
            CompiledStatement::Assignment(assignment) => {
                execute_assignment(assignment, runtime, trace)?;
            }
            CompiledStatement::Host(assignment) => {
                if !execute_host_assignment(assignment, runtime, trace)? {
                    return Ok(SequenceControl::EndRcFile);
                }
            }
            CompiledStatement::Include(include) => {
                return Err(EvalError::RuntimeRcLoaderUnavailable {
                    line: include.line(),
                    statement: "INCLUDERC",
                });
            }
            CompiledStatement::Switch(switch) => {
                return Err(EvalError::RuntimeRcLoaderUnavailable {
                    line: switch.line(),
                    statement: "SWITCHRC",
                });
            }
        }
    }
    Ok(SequenceControl::Continue)
}

fn execute_assignment(
    assignment: &CompiledAssignment,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), EvalError> {
    let value = assignment
        .assignment
        .resolve_with(|name| runtime.get(name).map(str::to_owned))
        .map_err(EvalError::Expansion)?;
    runtime.set(assignment.assignment.name.clone(), value.clone());
    if let Ok(name) = TraceName::new(&assignment.assignment.name) {
        trace.record(TraceEvent::VariableAssigned {
            line: assignment.line,
            name,
            source: assignment.source,
            value: trace
                .detail()
                .includes_variable_values()
                .then(|| TraceValue::new(value.as_bytes())),
        });
    }
    Ok(())
}

fn execute_host_assignment(
    assignment: &CompiledAssignment,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<bool, EvalError> {
    execute_assignment(assignment, runtime, trace)?;
    let configured = runtime.get("HOST").unwrap_or_default();
    let current = runtime
        .system_hostname()
        .ok_or(EvalError::RuntimeSettingUnavailable {
            line: assignment.assignment.line,
            name: "HOST",
        })?;
    Ok(configured == current)
}

impl ExecutionPlan {
    pub fn compile(config: &Config) -> Self {
        Self::compile_with_optional_loader(config, None)
    }

    pub fn compile_with_loader(config: &Config, loader: RcFileLoader) -> Self {
        Self::compile_with_optional_loader(config, Some(loader))
    }

    fn compile_with_optional_loader(config: &Config, loader: Option<RcFileLoader>) -> Self {
        let mut initial_statements = config
            .initial_variables()
            .iter()
            .map(|(name, value, source)| {
                CompiledStatement::Assignment(CompiledAssignment {
                    assignment: Assignment {
                        line: 0,
                        name: name.clone(),
                        value: value.clone(),
                        target: AssignmentTarget::User,
                        expansion: None,
                    },
                    line: None,
                    source: match source {
                        crate::config::VariableSource::RcFile => TraceVariableSource::RcFile,
                        crate::config::VariableSource::CommandLine => {
                            TraceVariableSource::CommandLine
                        }
                        crate::config::VariableSource::Environment => {
                            TraceVariableSource::Environment
                        }
                        crate::config::VariableSource::System => TraceVariableSource::System,
                        crate::config::VariableSource::Runtime => TraceVariableSource::Runtime,
                    },
                })
            })
            .collect::<Vec<_>>();
        let root = CompiledSequence::compile(&config.statements, &mut initial_statements);
        let requires_ordered_delivery = root.requires_ordered_delivery();
        let requires_preemptive_ordered_delivery = root.requires_preemptive_ordered_delivery();

        Self {
            root,
            requires_ordered_delivery,
            requires_preemptive_ordered_delivery,
            message_limits: MessageLimits::from_config(config).map_err(|error| error.to_string()),
            runtime_rc: RuntimeRcState::new(loader),
        }
    }

    fn rc_context(&self) -> RcExecutionContext<'_> {
        self.runtime_rc.context()
    }

    pub fn take_rc_diagnostics(&self) -> Vec<String> {
        self.runtime_rc.take_diagnostics()
    }

    pub fn requirements(&self) -> InputRequirements {
        let mut requirements = self.root.requirements();
        if self.runtime_rc.needs_message_contents() {
            requirements.needs_body_contents = true;
            requirements.needs_end_of_message = true;
        }
        if self.requires_preemptive_ordered_delivery {
            requirements.union(InputRequirements {
                needs_end_of_message: true,
                ..InputRequirements::default()
            })
        } else {
            requirements
        }
    }

    pub fn requires_ordered_delivery(&self) -> bool {
        self.requires_ordered_delivery || self.runtime_rc.requires_ordered_delivery()
    }

    pub fn needs_message_contents(&self) -> bool {
        self.root.needs_message_contents() || self.runtime_rc.needs_message_contents()
    }

    pub fn explain(&self) -> PlanExplanation {
        // Explain only execution shape. Values, patterns, thresholds, and
        // paths can contain private configuration data and are unnecessary
        // for deciding which message sections and delivery phases are used.
        let mut recipes = Vec::new();
        self.root.collect_explanations(&[], 0, &mut recipes);
        PlanExplanation {
            requirements: self.requirements(),
            requires_ordered_delivery: self.requires_ordered_delivery,
            recipes,
        }
    }
}

#[cfg(test)]
mod tests;
