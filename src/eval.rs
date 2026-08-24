// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use crate::config::{
    Assignment, AssignmentTarget, ConditionInput, Config, ContinuationMode, ControlFlow,
    Destination, OutputEnding, PipeAction, Recipe, RecipeAction, RecipeOptions, Statement,
};
use crate::message::{Message, MessageHead, StreamedMessage};
use crate::rc_file::RcFileLoader;
use crate::runtime::RuntimeVariables;
use crate::trace::{
    NoTrace, RecipeDecision, TraceEvent, TraceName, TraceSink, TraceValue,
    VariableSource as TraceVariableSource,
};

mod condition;
mod message;
mod runtime_rc;

use condition::{CompiledCondition, PartialMatch, compile_conditions};
use message::{
    CompleteMessage, OwnedCompleteMessage, current_ordered_message, matching_views_are_valid,
};
pub use message::{ExternalActionInput, FinalMessage, MappedMessageInput, MatchingMessage};
pub use runtime_rc::MAX_RUNTIME_RC_WARNINGS;
use runtime_rc::{
    CompiledInclude, CompiledSwitch, LoadedRuntimeRc, RcExecutionContext, RuntimeRcState,
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
    runtime_rc: RuntimeRcState,
}

#[derive(Debug)]
struct CompiledSequence {
    recipes: Vec<CompiledNode>,
    trailing_statements: Vec<CompiledStatement>,
}

#[derive(Debug)]
struct CompiledNode {
    line: usize,
    preceding_statements: Vec<CompiledStatement>,
    lock: Option<crate::config::PathExpression>,
    control: ControlFlow,
    conditions: Vec<CompiledCondition>,
    action: CompiledAction,
}

#[derive(Debug)]
enum CompiledAction {
    Deliver {
        destination: Destination,
        continuation: ContinuationMode,
        output_ending: OutputEnding,
    },
    Pipe {
        action: PipeAction,
        options: RecipeOptions,
    },
    Block(CompiledSequence),
}

#[derive(Debug, Clone)]
struct CompiledAssignment {
    assignment: Assignment,
    line: Option<usize>,
    source: TraceVariableSource,
}

#[derive(Debug)]
enum CompiledStatement {
    Assignment(CompiledAssignment),
    Host(CompiledAssignment),
    Include(CompiledInclude),
    Switch(CompiledSwitch),
}

fn assignment_requires_ordered_message(statement: &CompiledStatement) -> bool {
    matches!(
        statement,
        CompiledStatement::Assignment(assignment)
            if matches!(
                assignment.assignment.target,
                AssignmentTarget::LockFile
            )
                || assignment.assignment.target == AssignmentTarget::Trap
                    && !assignment.assignment.value.is_empty()
    )
}

#[derive(Debug)]
struct SequenceExecution {
    deliveries: usize,
    original_delivered: bool,
    pending_error: Option<EvalError>,
}

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
    external_condition: Option<&'a mut ExternalConditionExecutor<'a, E, T>>,
    global_lock: Option<&'a mut GlobalLockExecutor<'a, E>>,
    local_lock: Option<&'a mut LocalLockExecutor<'a, E>>,
    rc: RcExecutionContext<'a>,
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

type GlobalLockExecutor<'a, E> = dyn FnMut(&str, &mut RuntimeVariables) -> Result<(), E> + 'a;

type LocalLockExecutor<'a, E> = dyn FnMut(&str, &mut RuntimeVariables) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<E>>
    + 'a;

type CompletionExecutor<'a, E, T> =
    dyn FnMut(FinalMessage<'_>, &mut RuntimeVariables, &mut T, CompletionState<'_, E>) + 'a;

struct OptionalOrderedExecutors<'a, E, T> {
    external: Option<&'a mut ExternalActionExecutor<'a, E, T>>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FanoutPlanState {
    deliveries: Vec<PlannedDelivery>,
    original_delivered: bool,
}

#[derive(Debug, Default)]
struct HeaderPlanState {
    execution: FanoutPlanState,
    frames: Vec<ContinuationFrame>,
    requirements: InputRequirements,
    restart: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SequenceState {
    previous: Option<RecipeExecution>,
    chain_base_matched: Option<bool>,
}

// A resumed sequence must move its position and prior-recipe state together.
// Keeping them in one value prevents a caller from advancing to another node
// while accidentally retaining state that belongs to the old position.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SequenceCursor {
    index: usize,
    state: SequenceState,
}

// The frame slice owns the complete bounded path and depth selects one entry
// on it. Passing them together keeps recursive descent tied to that same path
// instead of allowing independently supplied frame and depth values.
#[derive(Debug, Clone, Copy)]
struct ResumeCursor<'a> {
    frames: &'a [ContinuationFrame],
    depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecipeExecution {
    conditions_matched: bool,
    else_handled: bool,
    action: ActionExecution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionExecution {
    NotAttempted,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceControl {
    Continue,
    Stop,
    EndRcFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderControl {
    Continue,
    Stop,
    EndRcFile,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanExplanation {
    requirements: InputRequirements,
    requires_ordered_delivery: bool,
    recipes: Vec<RecipeExplanation>,
}

impl PlanExplanation {
    pub fn requirements(&self) -> InputRequirements {
        self.requirements
    }

    pub fn requires_ordered_delivery(&self) -> bool {
        self.requires_ordered_delivery
    }

    pub fn recipes(&self) -> &[RecipeExplanation] {
        &self.recipes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeExplanation {
    line: usize,
    assignment_count: usize,
    conditions: Vec<ConditionExplanation>,
    destination: DestinationKind,
    copy: bool,
    defers_destination: bool,
}

impl RecipeExplanation {
    pub fn line(&self) -> usize {
        self.line
    }

    pub fn assignment_count(&self) -> usize {
        self.assignment_count
    }

    pub fn conditions(&self) -> &[ConditionExplanation] {
        &self.conditions
    }

    pub fn destination(&self) -> DestinationKind {
        self.destination
    }

    pub fn is_copy(&self) -> bool {
        self.copy
    }

    pub fn defers_destination(&self) -> bool {
        self.defers_destination
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConditionExplanation {
    negated: bool,
    kind: ConditionKindExplanation,
}

impl ConditionExplanation {
    pub fn is_negated(self) -> bool {
        self.negated
    }

    pub fn kind(self) -> ConditionKindExplanation {
        self.kind
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionKindExplanation {
    HeaderRegex,
    BodyRegex,
    MessageRegex,
    VariableRegex,
    Program,
    SmallerThan,
    LargerThan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationKind {
    Maildir,
    Mbox,
    ExternalProgram,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPlan {
    deliveries: Vec<PlannedDelivery>,
    original_delivered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDelivery {
    destination: Destination,
    continuation: DeliveryContinuation,
    output_ending: OutputEnding,
    lock: Option<String>,
    umask: String,
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
    published: usize,
    original_delivered: bool,
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
enum DeliveryContinuation {
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
    frames: Vec<ContinuationFrame>,
    execution: FanoutPlanState,
    runtime: RuntimeVariables,
    requirements: InputRequirements,
    restart: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContinuationFrame {
    recipe_index: usize,
    state: SequenceState,
    condition_results: Vec<Option<bool>>,
    assignments_applied: bool,
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

impl CompiledSequence {
    fn compile(statements: &[Statement], preceding: &mut Vec<CompiledStatement>) -> Self {
        let mut recipes = Vec::new();
        for statement in statements {
            match statement {
                Statement::Assignment(assignment) => {
                    let compiled = CompiledAssignment {
                        assignment: assignment.clone(),
                        line: Some(assignment.line),
                        source: TraceVariableSource::RcFile,
                    };
                    if assignment.target == AssignmentTarget::Host {
                        preceding.push(CompiledStatement::Host(compiled));
                    } else {
                        preceding.push(CompiledStatement::Assignment(compiled));
                    }
                }
                Statement::Recipe(recipe) => {
                    recipes.push(CompiledNode::compile(recipe, std::mem::take(preceding)));
                }
                Statement::Include(expression) => {
                    preceding.push(CompiledStatement::Include(CompiledInclude::new(
                        expression.clone(),
                    )));
                }
                Statement::Switch(expression) => {
                    preceding.push(CompiledStatement::Switch(CompiledSwitch::new(
                        expression.clone(),
                    )));
                }
            }
        }

        // Keep statements after the final recipe instead of attaching every
        // statement to a following recipe. Include and switch operations may
        // legally terminate a file, so dropping this tail would make their
        // behavior depend on whether an unrelated recipe follows them.
        Self {
            recipes,
            trailing_statements: std::mem::take(preceding),
        }
    }

    fn requirements(&self) -> InputRequirements {
        self.recipes
            .iter()
            .fold(InputRequirements::default(), |requirements, recipe| {
                requirements.union(recipe.requirements())
            })
    }

    fn requires_ordered_delivery(&self) -> bool {
        self.trailing_statements
            .iter()
            .any(assignment_requires_ordered_message)
            || self.recipes.iter().enumerate().any(|(index, recipe)| {
                recipe.requires_ordered_delivery()
                    || recipe
                        .preceding_statements
                        .iter()
                        .any(assignment_requires_ordered_message)
                    || (index != 0
                        && matches!(
                            recipe.control,
                            ControlFlow::AfterPreviousSuccess | ControlFlow::AfterPreviousError
                        ))
            })
    }

    fn needs_message_contents(&self) -> bool {
        self.recipes
            .iter()
            .any(CompiledNode::needs_message_contents)
    }

    fn execute(
        &self,
        message: &Message,
        matching: CompleteMessage<'_>,
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
            let conditions_matched = recipe.execution_gate(state)
                && recipe.matches_complete(matching, runtime, trace)?;
            let else_handled = recipe.else_handled(state, conditions_matched);

            let (action, control) = if conditions_matched {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                recipe.execute_action(message, matching, delivery, runtime, trace, execution)?
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

    fn requirements_from(&self, start: usize) -> InputRequirements {
        let recipes = &self.recipes[start..];
        let requirements = recipes
            .iter()
            .fold(InputRequirements::default(), |requirements, recipe| {
                requirements.union(recipe.requirements())
            });
        if recipes.iter().any(CompiledNode::requires_ordered_delivery) {
            requirements.union(InputRequirements {
                needs_end_of_message: true,
                ..InputRequirements::default()
            })
        } else {
            requirements
        }
    }

    fn has_error_handler(&self, index: usize) -> bool {
        self.recipes
            .get(index + 1)
            .is_some_and(|next| next.control == ControlFlow::AfterPreviousError)
    }

    fn collect_explanations(
        &self,
        inherited_conditions: &[ConditionExplanation],
        inherited_assignments: usize,
        explanations: &mut Vec<RecipeExplanation>,
    ) {
        for recipe in &self.recipes {
            let mut conditions = inherited_conditions.to_vec();
            conditions.extend(recipe.conditions.iter().map(CompiledCondition::explain));
            let assignment_count = inherited_assignments + recipe.preceding_statements.len();
            match &recipe.action {
                CompiledAction::Pipe { .. } => {
                    explanations.push(RecipeExplanation {
                        line: recipe.line,
                        assignment_count,
                        conditions,
                        destination: DestinationKind::ExternalProgram,
                        copy: false,
                        defers_destination: true,
                    });
                }
                CompiledAction::Deliver {
                    destination,
                    continuation,
                    ..
                } => {
                    let destination_kind = match destination {
                        Destination::Maildir(_) => DestinationKind::Maildir,
                        Destination::Mbox(_) => DestinationKind::Mbox,
                    };
                    explanations.push(RecipeExplanation {
                        line: recipe.line,
                        assignment_count,
                        conditions,
                        destination: destination_kind,
                        copy: *continuation == ContinuationMode::Continue,
                        defers_destination: destination.needs_runtime_variables(),
                    });
                }
                CompiledAction::Block(children) => {
                    children.collect_explanations(&conditions, assignment_count, explanations);
                }
            }
        }
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

impl From<SequenceControl> for HeaderControl {
    fn from(control: SequenceControl) -> Self {
        match control {
            SequenceControl::Continue => Self::Continue,
            SequenceControl::Stop => Self::Stop,
            SequenceControl::EndRcFile => Self::EndRcFile,
        }
    }
}

impl SequenceState {
    fn record(
        &mut self,
        control: ControlFlow,
        conditions_matched: bool,
        action: ActionExecution,
        else_handled: bool,
    ) {
        self.previous = Some(RecipeExecution {
            conditions_matched,
            else_handled,
            action,
        });
        if !matches!(
            control,
            ControlFlow::AfterChainMatch | ControlFlow::AfterPreviousSuccess
        ) {
            self.chain_base_matched = Some(conditions_matched);
        }
    }
}

impl CompiledNode {
    fn compile(recipe: &Recipe, preceding_statements: Vec<CompiledStatement>) -> Self {
        let conditions = compile_conditions(recipe);
        let action = match &recipe.action {
            RecipeAction::Pipe(action) => CompiledAction::Pipe {
                action: action.clone(),
                options: recipe.options,
            },
            RecipeAction::Deliver(destination) => CompiledAction::Deliver {
                destination: destination.clone(),
                continuation: recipe.options.continuation,
                output_ending: recipe.options.output_ending,
            },
            RecipeAction::Block(statements) => {
                CompiledAction::Block(CompiledSequence::compile(statements, &mut Vec::new()))
            }
        };
        Self {
            line: recipe.line,
            preceding_statements,
            lock: recipe.lock.clone(),
            control: recipe.options.control,
            conditions,
            action,
        }
    }

    fn requirements(&self) -> InputRequirements {
        let action = match &self.action {
            CompiledAction::Pipe { .. } => InputRequirements {
                needs_headers: true,
                needs_body_contents: true,
                needs_end_of_message: true,
            },
            CompiledAction::Deliver { .. } => InputRequirements::default(),
            CompiledAction::Block(sequence) => sequence.requirements(),
        };
        self.conditions
            .iter()
            .fold(action, |requirements, condition| {
                requirements.union(condition.requirements())
            })
    }

    fn requires_ordered_delivery(&self) -> bool {
        self.lock.is_some()
            || self
                .conditions
                .iter()
                .any(CompiledCondition::requires_ordered_execution)
            || match &self.action {
                CompiledAction::Pipe { .. } => true,
                CompiledAction::Deliver {
                    destination,
                    continuation: _,
                    ..
                } => {
                    destination.needs_runtime_variables()
                        || matches!(destination, Destination::Mbox(_))
                }
                CompiledAction::Block(sequence) => sequence.requires_ordered_delivery(),
            }
    }

    fn needs_message_contents(&self) -> bool {
        self.conditions
            .iter()
            .any(CompiledCondition::needs_message_contents)
            || match &self.action {
                CompiledAction::Pipe { .. } => true,
                CompiledAction::Deliver { .. } => false,
                CompiledAction::Block(sequence) => sequence.needs_message_contents(),
            }
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

    fn else_handled(&self, state: SequenceState, conditions_matched: bool) -> bool {
        if self.control == ControlFlow::Else {
            state.previous.is_some_and(|result| result.else_handled) || conditions_matched
        } else {
            conditions_matched
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

    fn execute_action(
        &self,
        message: &Message,
        matching: CompleteMessage<'_>,
        delivery: &mut impl Delivery,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut SequenceExecution,
    ) -> Result<(ActionExecution, SequenceControl), EvalError> {
        match &self.action {
            CompiledAction::Pipe { .. } => {
                Err(EvalError::ExternalActionUnsupported { line: self.line })
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
                            destination: destination_name(&destination).to_owned(),
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
                    children.execute(message, matching, delivery, runtime, trace, execution)?;
                let action = if execution.pending_error.is_some() {
                    ActionExecution::Failed
                } else {
                    ActionExecution::Succeeded
                };
                Ok((action, control))
            }
        }
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
                    .lock
                    .as_ref()
                    .map(|expression| {
                        expression.resolve_with(|name| context.runtime.get(name).map(str::to_owned))
                    })
                    .transpose()
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
                let destination = destination
                    .bind_with(|name| context.runtime.get(name).map(str::to_owned))
                    .map_err(EvalError::Expansion)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let message =
                    current_ordered_message(context.message, context.replacement.as_ref())
                        .raw()
                        .ok_or(EvalError::BodyWasNotBuffered)
                        .map_err(OrderedExecutionError::Evaluation)?;
                let lock = self
                    .lock
                    .as_ref()
                    .map(|expression| {
                        expression.resolve_with(|name| context.runtime.get(name).map(str::to_owned))
                    })
                    .transpose()
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
                    .lock
                    .as_ref()
                    .map(|expression| {
                        expression.resolve_with(|name| context.runtime.get(name).map(str::to_owned))
                    })
                    .transpose()
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
        let lock = self
            .lock
            .as_ref()
            .map(|expression| expression.resolve_with(|name| runtime.get(name).map(str::to_owned)))
            .transpose()
            .map_err(EvalError::Expansion)?;
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

        Self {
            root,
            requires_ordered_delivery,
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
        if self.requires_ordered_delivery() {
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
                external_condition: None,
                global_lock: None,
                local_lock: None,
            },
            None,
        )
    }

    pub fn execute_mapped_ordered_with_processes_trace<E, D, C, X, G, L, T>(
        &self,
        message: MappedMessageInput<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
        deliver: &mut D,
        executors: (&mut C, &mut X, &mut G, &mut L),
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
        G: FnMut(&str, &mut RuntimeVariables) -> Result<(), E>,
        L: FnMut(
            &str,
            &mut RuntimeVariables,
        ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<E>>,
        T: TraceSink,
    {
        let (external_condition, external, global_lock, local_lock) = executors;
        self.execute_mapped_ordered_inner(
            message,
            runtime,
            trace,
            deliver,
            OptionalOrderedExecutors {
                external: Some(external),
                external_condition: Some(external_condition),
                global_lock: Some(global_lock),
                local_lock: Some(local_lock),
            },
            None,
        )
    }

    pub fn execute_mapped_ordered_with_processes_and_completion_trace<E, D, C, X, G, L, F, T>(
        &self,
        message: MappedMessageInput<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
        deliver: &mut D,
        executors: (&mut C, &mut X, &mut G, &mut L),
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
        G: FnMut(&str, &mut RuntimeVariables) -> Result<(), E>,
        L: FnMut(
            &str,
            &mut RuntimeVariables,
        ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<E>>,
        F: FnMut(FinalMessage<'_>, &mut RuntimeVariables, &mut T, CompletionState<'_, E>),
        T: TraceSink,
    {
        let (external_condition, external, global_lock, local_lock) = executors;
        self.execute_mapped_ordered_inner(
            message,
            runtime,
            trace,
            deliver,
            OptionalOrderedExecutors {
                external: Some(external),
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
        let MappedMessageInput {
            raw,
            header_len,
            matching,
        } = message;
        if header_len > raw.len() {
            return Err(OrderedExecutionError::Evaluation(
                EvalError::BodyWasNotBuffered,
            ));
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
            return Err(OrderedExecutionError::Evaluation(
                EvalError::BodyWasNotBuffered,
            ));
        }
        let mut context = OrderedTreeExecution {
            message: CompleteMessage::Mapped {
                raw,
                header_len,
                matching_header,
                matching_raw,
            },
            replacement: None,
            runtime,
            trace,
            deliver,
            published: 0,
            original_delivered: false,
            pending_error: None,
            external: executors.external,
            external_condition: executors.external_condition,
            global_lock: executors.global_lock,
            local_lock: executors.local_lock,
            rc: self.rc_context(),
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

pub fn evaluate(
    config: &Config,
    message: &Message,
    delivery: &mut impl Delivery,
) -> Result<Outcome, EvalError> {
    let plan = ExecutionPlan::compile(config);
    let matching_full = plan
        .needs_message_contents()
        .then(|| message.matching_message())
        .flatten();
    let matching = CompleteMessage::Buffered {
        message,
        matching_full: matching_full.as_deref(),
    };
    let mut execution = SequenceExecution {
        deliveries: 0,
        original_delivered: false,
        pending_error: None,
    };
    plan.root.execute(
        message,
        matching,
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

fn destination_name(destination: &Destination) -> &str {
    destination.path()
}

#[cfg(test)]
mod tests;
