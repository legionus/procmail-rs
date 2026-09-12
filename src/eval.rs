// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use crate::config::{
    ActionInput, Assignment, AssignmentTarget, Config, ContinuationMode, ControlFlow, Destination,
    OutputEnding, PipeAction, RecipeOptions,
};
use crate::limits::MessageLimits;
#[cfg(test)]
use crate::message::StreamedMessage;
use crate::message::{Message, MessageHead};
use crate::rc_file::{RcFileLoader, RuntimeRcLoader};
use crate::runtime::{RuntimeSettingError, RuntimeSettings, RuntimeVariables};
#[cfg(test)]
use crate::trace::NoTrace;
use crate::trace::{RecipeDecision, TraceEvent, TraceSink, VariableSource as TraceVariableSource};

mod condition;
mod explanation;
mod header;
#[cfg(test)]
mod header_test_support;
mod message;
mod ordered;
mod result;
mod runtime_rc;
mod services;
#[cfg(test)]
mod test_services;
mod tree;

fn runtime_setting_eval_error(error: RuntimeSettingError) -> EvalError {
    EvalError::RuntimeSettingUnavailable {
        line: error.line().unwrap_or(0),
        name: error.name(),
    }
}

use condition::PartialMatch;
pub use explanation::{
    ActionKindExplanation, ConditionExplanation, ConditionKindExplanation,
    HeaderOperationExplanation, PlanExplanation, RecipeExplanation,
};
use header::FanoutPlanState;
use message::{CompleteMessage, CurrentMessage};
pub use message::{
    ExternalActionInput, FinalMessage, MappedMessageInput, MatchingMessage, PreparedMatchingMessage,
};
pub use result::{
    CompletionState, Continuation, DeliveryAttemptError, DeliveryOutcome, DeliveryPlan, EvalError,
    HeaderEvaluation, OrderedExecutionError, PlannedDelivery,
};
use result::{ContinuationFrame, DeliveryContinuation};
pub use runtime_rc::MAX_RUNTIME_RC_WARNINGS;
use runtime_rc::{RcExecutionContext, RuntimeRcState};
pub use services::{OrderedExecutionHost, RecipeLockGuard};
#[cfg(test)]
pub use test_services::ExecutionServices;
use tree::{
    ActionExecution, CompiledAction, CompiledAssignment, CompiledNode, CompiledSequence,
    CompiledStatement, SequenceState,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedCommand {
    output: Vec<u8>,
}

impl CapturedCommand {
    pub fn new(output: Vec<u8>) -> Self {
        Self { output }
    }

    pub fn output(&self) -> &[u8] {
        &self.output
    }

    pub fn into_output(self) -> Vec<u8> {
        self.output
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapturedNewlineRule {
    StripOne,
    StripAll,
}

fn validate_captured_value(
    mut output: Vec<u8>,
    limit: usize,
    variable: &str,
    newline_rule: CapturedNewlineRule,
) -> Result<Vec<u8>, EvalError> {
    // Check the bytes returned by the executor independently of the process
    // runner. Alternate executors and future call paths must not bypass the
    // allocation ceiling merely because the normal runner enforces it while
    // reading stdout.
    if output.len() > limit {
        return Err(EvalError::VariableValueTooLarge {
            name: variable.to_owned(),
            size: output.len(),
        });
    }
    match newline_rule {
        CapturedNewlineRule::StripOne => {
            if output.last() == Some(&b'\n') {
                output.pop();
            }
        }
        CapturedNewlineRule::StripAll => {
            while output.last() == Some(&b'\n') {
                output.pop();
            }
        }
    }
    Ok(output)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputRequirements {
    pub needs_headers: bool,
    pub needs_body_contents: bool,
    pub needs_end_of_message: bool,
}

impl InputRequirements {
    pub(super) fn union(self, other: Self) -> Self {
        Self {
            needs_headers: self.needs_headers || other.needs_headers,
            needs_body_contents: self.needs_body_contents || other.needs_body_contents,
            needs_end_of_message: self.needs_end_of_message || other.needs_end_of_message,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PlanProperties {
    requirements: InputRequirements,
    requires_ordered_delivery: bool,
    requires_preemptive_ordered_delivery: bool,
    needs_message_contents: bool,
    has_external_commands: bool,
}

impl PlanProperties {
    fn union(self, other: Self) -> Self {
        Self {
            requirements: self.requirements.union(other.requirements),
            requires_ordered_delivery: self.requires_ordered_delivery
                || other.requires_ordered_delivery,
            requires_preemptive_ordered_delivery: self.requires_preemptive_ordered_delivery
                || other.requires_preemptive_ordered_delivery,
            needs_message_contents: self.needs_message_contents || other.needs_message_contents,
            has_external_commands: self.has_external_commands || other.has_external_commands,
        }
    }
}

#[derive(Debug)]
pub struct ExecutionPlan {
    root: CompiledSequence,
    message_limits: Result<MessageLimits, String>,
    runtime_rc: RuntimeRcState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceControl {
    Continue,
    Stop,
    EndRcFile,
    SequenceComplete,
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
            CompiledStatement::CommandAssignment(assignment) => {
                return Err(EvalError::ExternalActionUnsupported {
                    line: assignment.line,
                });
            }
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
    ResolvedAssignment {
        name: assignment.assignment.name.clone(),
        target: assignment.assignment.target,
        value: value.into_bytes(),
        source_line: assignment.assignment.line,
        trace_line: assignment.line,
        source: assignment.source,
    }
    .apply(runtime, trace)
}

struct ResolvedAssignment {
    name: String,
    target: AssignmentTarget,
    value: Vec<u8>,
    source_line: usize,
    trace_line: Option<usize>,
    source: TraceVariableSource,
}

impl ResolvedAssignment {
    fn apply(
        self,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> Result<(), EvalError> {
        if self.target == AssignmentTarget::Shift {
            let text =
                std::str::from_utf8(&self.value).map_err(|_| EvalError::RuntimeCondition {
                    line: self.source_line,
                    message: "SHIFT must be a positive decimal integer".to_owned(),
                })?;
            let amount = crate::config::parse_shift(text).map_err(|message| {
                EvalError::RuntimeCondition {
                    line: self.source_line,
                    message,
                }
            })?;
            runtime.set_bytes_with_trace(
                self.name,
                self.value,
                self.trace_line,
                self.source,
                trace,
            );
            runtime.remove("SHIFT");
            runtime.shift_positionals(amount);
            return Ok(());
        }
        runtime.set_bytes_with_trace(self.name, self.value, self.trace_line, self.source, trace);
        Ok(())
    }
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
    pub fn compile(config: &Config, loader: Option<RcFileLoader>) -> Self {
        Self::compile_with_loader(
            config,
            loader.map(|loader| Box::new(loader) as Box<dyn RuntimeRcLoader>),
        )
    }

    fn compile_with_loader(config: &Config, loader: Option<Box<dyn RuntimeRcLoader>>) -> Self {
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

        Self {
            root,
            message_limits: MessageLimits::from_config(config).map_err(|error| error.to_string()),
            runtime_rc: RuntimeRcState::new(loader),
        }
    }

    #[cfg(any(feature = "fuzzing", test))]
    pub(crate) fn compile_for_fuzzing(
        config: &Config,
        loader: impl RuntimeRcLoader + 'static,
    ) -> Self {
        Self::compile_with_loader(config, Some(Box::new(loader)))
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
        if self.root.properties().requires_preemptive_ordered_delivery {
            requirements.union(InputRequirements {
                needs_end_of_message: true,
                ..InputRequirements::default()
            })
        } else {
            requirements
        }
    }

    pub fn requires_ordered_delivery(&self) -> bool {
        self.root.properties().requires_ordered_delivery
            || self.runtime_rc.requires_ordered_delivery()
    }

    pub fn needs_message_contents(&self) -> bool {
        self.root.properties().needs_message_contents || self.runtime_rc.needs_message_contents()
    }

    pub fn has_external_commands(&self) -> bool {
        self.root.properties().has_external_commands
    }

    pub fn explain(&self) -> PlanExplanation {
        // Explain only execution shape. Values, patterns, thresholds, and
        // paths can contain private configuration data and are unnecessary
        // for deciding which message sections and delivery phases are used.
        let mut recipes = Vec::new();
        self.root.collect_explanations(&[], 0, &mut recipes);
        PlanExplanation {
            requirements: self.requirements(),
            requires_ordered_delivery: self.root.properties().requires_ordered_delivery,
            recipes,
        }
    }
}

#[cfg(test)]
#[path = "tests/eval/mod.rs"]
mod tests;
