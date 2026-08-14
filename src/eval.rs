// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::cell::{Cell, RefCell};
use std::fmt;

use regex::bytes::Regex;

use crate::config::{
    Assignment, AssignmentTarget, ConditionInput, ConditionKind, Config, ContinuationMode,
    ControlFlow, Destination, PipeAction, RcFileExpression, Recipe, RecipeAction, RecipeOptions,
    RegexCondition, Statement,
};
use crate::message::{Message, MessageHead, StreamedMessage};
use crate::rc_file::{MAX_RC_TRANSITIONS, RcFileLoader};
use crate::runtime::RuntimeVariables;
use crate::trace::{
    ConditionKind as TraceConditionKind, NoTrace, RecipeDecision, TraceEvent, TraceName, TraceSink,
    TraceValue, VariableSource as TraceVariableSource,
};

const MAX_RC_DIAGNOSTIC_LEN: usize = 1024;

pub trait Delivery {
    fn deliver(&mut self, destination: &Destination, message: &Message) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputRequirements {
    pub needs_headers: bool,
    pub needs_body_contents: bool,
    pub needs_end_of_message: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct MatchingMessage<'a> {
    header: &'a [u8],
    full: Option<&'a [u8]>,
}

impl<'a> MatchingMessage<'a> {
    pub fn new(header: &'a [u8], full: Option<&'a [u8]>) -> Self {
        Self { header, full }
    }
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
    runtime_rc: RefCell<Option<RcFileLoader>>,
    rc_transitions: Cell<usize>,
    dynamic_ordered_delivery: Cell<bool>,
    dynamic_message_contents: Cell<bool>,
    rc_diagnostics: RefCell<Vec<String>>,
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
    control: ControlFlow,
    conditions: Vec<CompiledCondition>,
    action: CompiledAction,
}

#[derive(Debug)]
enum CompiledAction {
    Deliver {
        destination: Destination,
        continuation: ContinuationMode,
    },
    Pipe {
        _action: PipeAction,
        _options: RecipeOptions,
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
    Include(CompiledInclude),
    Switch(CompiledSwitch),
}

#[derive(Debug)]
struct CompiledInclude {
    expression: RcFileExpression,
    loaded: RefCell<LoadedRuntimeRc>,
}

#[derive(Debug)]
struct CompiledSwitch {
    expression: RcFileExpression,
    loaded: RefCell<LoadedRuntimeRc>,
}

#[derive(Debug, Default)]
enum LoadedRuntimeRc {
    #[default]
    Unloaded,
    Empty,
    Failed,
    Sequence(Box<CompiledSequence>),
}

#[derive(Clone, Copy)]
struct RcExecutionContext<'a> {
    loader: &'a RefCell<Option<RcFileLoader>>,
    transitions: &'a Cell<usize>,
    dynamic_ordered_delivery: &'a Cell<bool>,
    dynamic_message_contents: &'a Cell<bool>,
    diagnostics: &'a RefCell<Vec<String>>,
    depth: usize,
}

#[derive(Debug, Clone)]
struct CompiledCondition {
    line: usize,
    negated: bool,
    kind: CompiledConditionKind,
    match_capture: Option<usize>,
    capture_indexes: Vec<usize>,
}

#[derive(Debug, Clone)]
enum CompiledConditionKind {
    HeaderRegex(Regex),
    BodyRegex(Regex),
    MessageRegex(Regex),
    VariableRegex { name: String, regex: Regex },
    SmallerThan(usize),
    LargerThan(usize),
}

#[derive(Debug)]
struct SequenceExecution {
    deliveries: usize,
    original_delivered: bool,
    pending_error: Option<EvalError>,
}

struct OrderedTreeExecution<'a, E, D, T> {
    message: CompleteMessage<'a>,
    runtime: &'a mut RuntimeVariables,
    trace: &'a mut T,
    deliver: &'a mut D,
    published: usize,
    original_delivered: bool,
    pending_error: Option<E>,
    rc: RcExecutionContext<'a>,
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
    RuntimeRc(String),
    ExternalActionUnsupported {
        line: usize,
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
            Self::RuntimeRc(message) => formatter.write_str(message),
            Self::ExternalActionUnsupported { line } => {
                write!(
                    formatter,
                    "line {line}: external action is not executable yet"
                )
            }
            Self::Delivery {
                destination,
                message,
            } => write!(formatter, "cannot deliver to {destination}: {message}"),
        }
    }
}

impl std::error::Error for EvalError {}

fn truncate_utf8(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

impl RcExecutionContext<'_> {
    fn descend(self) -> Result<Self, EvalError> {
        let depth = self
            .depth
            .checked_add(1)
            .ok_or_else(|| EvalError::RuntimeRc("rc include depth overflows".to_owned()))?;
        Ok(Self { depth, ..self })
    }

    fn record_transition(self) -> Result<(), EvalError> {
        let transitions = self
            .transitions
            .get()
            .checked_add(1)
            .ok_or_else(|| EvalError::RuntimeRc("rc transition count overflows".to_owned()))?;
        if transitions > MAX_RC_TRANSITIONS {
            return Err(EvalError::RuntimeRc(format!(
                "rc transitions exceed the hard limit of {MAX_RC_TRANSITIONS}"
            )));
        }
        self.transitions.set(transitions);
        Ok(())
    }
}

impl CompiledInclude {
    fn ensure_loaded(
        &self,
        runtime: &RuntimeVariables,
        context: RcExecutionContext<'_>,
    ) -> Result<(), EvalError> {
        load_runtime_rc(
            &self.expression,
            &self.loaded,
            "INCLUDERC",
            runtime,
            context,
        )
    }
}

impl CompiledSwitch {
    fn ensure_loaded(
        &self,
        runtime: &RuntimeVariables,
        context: RcExecutionContext<'_>,
    ) -> Result<(), EvalError> {
        load_runtime_rc(&self.expression, &self.loaded, "SWITCHRC", runtime, context)
    }
}

fn load_runtime_rc(
    expression: &RcFileExpression,
    loaded_state: &RefCell<LoadedRuntimeRc>,
    statement: &'static str,
    runtime: &RuntimeVariables,
    context: RcExecutionContext<'_>,
) -> Result<(), EvalError> {
    context.record_transition()?;
    if !matches!(*loaded_state.borrow(), LoadedRuntimeRc::Unloaded) {
        return Ok(());
    }
    let child_context = context.descend()?;
    let loaded = context
        .loader
        .borrow_mut()
        .as_mut()
        .ok_or(EvalError::RuntimeRcLoaderUnavailable {
            line: expression.line,
            statement,
        })?
        .load_config(expression, runtime, child_context.depth);
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(error) if error.is_resource_limit() => {
            return Err(EvalError::RuntimeRc(format!(
                "line {}: {statement} resource limit: {}",
                expression.line,
                error.safe_message()
            )));
        }
        Err(error) => {
            let mut diagnostic = format!(
                "line {}: {statement} failed: {}",
                expression.line,
                error.safe_message()
            );
            truncate_utf8(&mut diagnostic, MAX_RC_DIAGNOSTIC_LEN);
            context.diagnostics.borrow_mut().push(diagnostic);
            *loaded_state.borrow_mut() = LoadedRuntimeRc::Failed;
            return Ok(());
        }
    };
    let Some(loaded) = loaded else {
        *loaded_state.borrow_mut() = LoadedRuntimeRc::Empty;
        return Ok(());
    };
    let mut preceding = Vec::new();
    let sequence = CompiledSequence::compile(&loaded.into_config().statements, &mut preceding);
    let requirements = sequence.requirements();
    if requirements.needs_body_contents {
        context.dynamic_message_contents.set(true);
    }
    if sequence.requires_ordered_delivery() {
        context.dynamic_ordered_delivery.set(true);
    }
    *loaded_state.borrow_mut() = LoadedRuntimeRc::Sequence(Box::new(sequence));
    Ok(())
}

impl CompiledSequence {
    fn compile(statements: &[Statement], preceding: &mut Vec<CompiledStatement>) -> Self {
        let mut recipes = Vec::new();
        for statement in statements {
            match statement {
                Statement::Assignment(assignment) => {
                    preceding.push(CompiledStatement::Assignment(CompiledAssignment {
                        assignment: assignment.clone(),
                        line: Some(assignment.line),
                        source: TraceVariableSource::RcFile,
                    }))
                }
                Statement::Recipe(recipe) => {
                    recipes.push(CompiledNode::compile(recipe, std::mem::take(preceding)));
                }
                Statement::Include(expression) => {
                    preceding.push(CompiledStatement::Include(CompiledInclude {
                        expression: expression.clone(),
                        loaded: RefCell::new(LoadedRuntimeRc::Unloaded),
                    }));
                }
                Statement::Switch(expression) => {
                    preceding.push(CompiledStatement::Switch(CompiledSwitch {
                        expression: expression.clone(),
                        loaded: RefCell::new(LoadedRuntimeRc::Unloaded),
                    }));
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
        self.recipes.iter().enumerate().any(|(index, recipe)| {
            recipe.requires_ordered_delivery()
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
            execute_statements(&recipe.preceding_statements, runtime, trace)?;

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
            if control == SequenceControl::Stop {
                return Ok(SequenceControl::Stop);
            }
        }

        execute_statements(&self.trailing_statements, runtime, trace)?;
        Ok(SequenceControl::Continue)
    }

    fn execute_ordered<E, D, T>(
        &self,
        context: &mut OrderedTreeExecution<'_, E, D, T>,
    ) -> Result<(ActionExecution, SequenceControl), OrderedExecutionError<E>>
    where
        D: FnMut(
            &Destination,
            &[u8],
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
            let conditions_matched = recipe.execution_gate(state)
                && recipe
                    .matches_complete(context.message, context.runtime, context.trace)
                    .map_err(OrderedExecutionError::Evaluation)?;
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
            0,
            SequenceState::default(),
            message,
            runtime,
            trace,
            execution,
            context,
        )
    }

    fn plan_complete_from(
        &self,
        start: usize,
        mut state: SequenceState,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut FanoutPlanState,
        context: RcExecutionContext<'_>,
    ) -> Result<SequenceControl, EvalError> {
        for (index, recipe) in self.recipes.iter().enumerate().skip(start) {
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
        frames: &[ContinuationFrame],
        depth: usize,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut FanoutPlanState,
        context: RcExecutionContext<'_>,
    ) -> Result<SequenceControl, EvalError> {
        let frame = frames.get(depth).ok_or(EvalError::BodyWasNotBuffered)?;
        let recipe = self
            .recipes
            .get(frame.recipe_index)
            .ok_or(EvalError::BodyWasNotBuffered)?;
        let mut state = frame.state;
        if !frame.assignments_applied {
            execute_statements(&recipe.preceding_statements, runtime, trace)?;
        }

        let (conditions_matched, control) = if depth + 1 < frames.len() {
            let CompiledAction::Block(children) = &recipe.action else {
                return Err(EvalError::BodyWasNotBuffered);
            };
            let control = children.resume_from_frames(
                frames,
                depth + 1,
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
            frame.recipe_index + 1,
            state,
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
                _action: action.clone(),
                _options: recipe.options,
            },
            RecipeAction::Deliver(destination) => CompiledAction::Deliver {
                destination: destination.clone(),
                continuation: recipe.options.continuation,
            },
            RecipeAction::Block(statements) => {
                CompiledAction::Block(CompiledSequence::compile(statements, &mut Vec::new()))
            }
        };
        Self {
            line: recipe.line,
            preceding_statements,
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
        match &self.action {
            CompiledAction::Pipe { .. } => true,
            CompiledAction::Deliver {
                destination,
                continuation: _,
            } => {
                destination.needs_runtime_variables() || matches!(destination, Destination::Mbox(_))
            }
            CompiledAction::Block(sequence) => sequence.requires_ordered_delivery(),
        }
    }

    fn needs_message_contents(&self) -> bool {
        self.conditions
            .iter()
            .any(|condition| matches!(condition.kind, CompiledConditionKind::MessageRegex(_)))
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
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
        T: TraceSink,
    {
        match &self.action {
            CompiledAction::Pipe { .. } => Err(OrderedExecutionError::Evaluation(
                EvalError::ExternalActionUnsupported { line: self.line },
            )),
            CompiledAction::Deliver {
                destination,
                continuation,
            } => {
                let destination = destination
                    .bind_with(|name| context.runtime.get(name).map(str::to_owned))
                    .map_err(EvalError::Expansion)
                    .map_err(OrderedExecutionError::Evaluation)?;
                let message = context
                    .message
                    .full()
                    .ok_or(EvalError::BodyWasNotBuffered)
                    .map_err(OrderedExecutionError::Evaluation)?;
                match (context.deliver)(&destination, message, context.runtime, context.trace) {
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
        } = &self.action
        else {
            return Ok(SequenceControl::Continue);
        };
        let destination = destination
            .bind_with(|name| runtime.get(name).map(str::to_owned))
            .map_err(EvalError::Expansion)?;
        let copy = *continuation == ContinuationMode::Continue;
        execution.deliveries.push(PlannedDelivery {
            destination,
            continuation: if copy {
                DeliveryContinuation::Continue
            } else {
                DeliveryContinuation::Stop
            },
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
) -> Result<(), EvalError> {
    for statement in statements {
        match statement {
            CompiledStatement::Assignment(assignment) => {
                execute_assignment(assignment, runtime, trace)?;
            }
            CompiledStatement::Include(include) => {
                return Err(EvalError::RuntimeRcLoaderUnavailable {
                    line: include.expression.line,
                    statement: "INCLUDERC",
                });
            }
            CompiledStatement::Switch(switch) => {
                return Err(EvalError::RuntimeRcLoaderUnavailable {
                    line: switch.expression.line,
                    statement: "SWITCHRC",
                });
            }
        }
    }
    Ok(())
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
            CompiledStatement::Include(include) => {
                include.ensure_loaded(runtime, context)?;
                if let LoadedRuntimeRc::Sequence(sequence) = &*include.loaded.borrow()
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
                match &*switch.loaded.borrow() {
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
            }
            CompiledStatement::Include(include) => {
                include
                    .ensure_loaded(context.runtime, context.rc)
                    .map_err(OrderedExecutionError::Evaluation)?;
                if let LoadedRuntimeRc::Sequence(sequence) = &*include.loaded.borrow() {
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
                match &*switch.loaded.borrow() {
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
            CompiledStatement::Include(include) => {
                include.ensure_loaded(runtime, context)?;
                if let LoadedRuntimeRc::Sequence(sequence) = &*include.loaded.borrow() {
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
                match &*switch.loaded.borrow() {
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
            .map(|(name, value)| {
                CompiledStatement::Assignment(CompiledAssignment {
                    assignment: Assignment {
                        line: 0,
                        name: name.clone(),
                        value: value.clone(),
                        target: AssignmentTarget::User,
                        expansion: None,
                    },
                    line: None,
                    source: TraceVariableSource::CommandLine,
                })
            })
            .collect::<Vec<_>>();
        let root = CompiledSequence::compile(&config.statements, &mut initial_statements);
        let requires_ordered_delivery = root.requires_ordered_delivery();

        Self {
            root,
            requires_ordered_delivery,
            runtime_rc: RefCell::new(loader),
            rc_transitions: Cell::new(0),
            dynamic_ordered_delivery: Cell::new(false),
            dynamic_message_contents: Cell::new(false),
            rc_diagnostics: RefCell::new(Vec::new()),
        }
    }

    fn rc_context(&self) -> RcExecutionContext<'_> {
        RcExecutionContext {
            loader: &self.runtime_rc,
            transitions: &self.rc_transitions,
            dynamic_ordered_delivery: &self.dynamic_ordered_delivery,
            dynamic_message_contents: &self.dynamic_message_contents,
            diagnostics: &self.rc_diagnostics,
            depth: 0,
        }
    }

    pub fn take_rc_diagnostics(&self) -> Vec<String> {
        std::mem::take(&mut *self.rc_diagnostics.borrow_mut())
    }

    pub fn requirements(&self) -> InputRequirements {
        let mut requirements = self.root.requirements();
        if self.dynamic_message_contents.get() {
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
        self.requires_ordered_delivery || self.dynamic_ordered_delivery.get()
    }

    pub fn needs_message_contents(&self) -> bool {
        self.root.needs_message_contents() || self.dynamic_message_contents.get()
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
            .map(|message| (Some(message.header), message.full))
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
            &mut RuntimeVariables,
            &mut T,
        ) -> Result<(), DeliveryAttemptError<E>>,
        T: TraceSink,
    {
        if header_len > raw.len() {
            return Err(OrderedExecutionError::Evaluation(
                EvalError::BodyWasNotBuffered,
            ));
        }
        let (matching_header, matching_raw) = matching
            .map(|message| (Some(message.header), message.full))
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
            runtime,
            trace,
            deliver,
            published: 0,
            original_delivered: false,
            pending_error: None,
            rc: self.rc_context(),
        };
        self.root.execute_ordered(&mut context)?;
        if let Some(error) = context.pending_error {
            return Err(OrderedExecutionError::Delivery(error));
        }
        Ok(DeliveryOutcome {
            published: context.published,
            original_delivered: context.original_delivered,
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
            self.rc_transitions.set(0);
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
            &continuation.frames,
            0,
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

fn compile_conditions(recipe: &Recipe) -> Vec<CompiledCondition> {
    let area = match recipe.options.condition_input {
        ConditionInput::Headers => RegexArea::Headers,
        ConditionInput::Body => RegexArea::Body,
        ConditionInput::Message => RegexArea::Message,
    };
    let mut conditions = Vec::with_capacity(recipe.conditions.len());

    for condition in &recipe.conditions {
        let regex_condition = match &condition.kind {
            ConditionKind::Regex(regex)
            | ConditionKind::AreaRegex { regex, .. }
            | ConditionKind::VariableRegex { regex, .. } => Some(regex),
            ConditionKind::SmallerThan(_) | ConditionKind::LargerThan(_) => None,
        };
        let kind = match &condition.kind {
            ConditionKind::SmallerThan(size) => CompiledConditionKind::SmallerThan(*size),
            ConditionKind::LargerThan(size) => CompiledConditionKind::LargerThan(*size),
            ConditionKind::Regex(regex) => {
                // Parsing already validated and compiled this expression.
                // Cloning Regex shares its read-only compiled program, so
                // execution planning cannot repeat attacker-controlled
                // compilation work after configuration validation.
                let regex = regex.compiled().clone();
                match area {
                    RegexArea::Headers => CompiledConditionKind::HeaderRegex(regex),
                    RegexArea::Body => CompiledConditionKind::BodyRegex(regex),
                    RegexArea::Message => CompiledConditionKind::MessageRegex(regex),
                }
            }
            ConditionKind::AreaRegex { area, regex } => {
                let regex = regex.compiled().clone();
                match area {
                    ConditionInput::Headers => CompiledConditionKind::HeaderRegex(regex),
                    ConditionInput::Body => CompiledConditionKind::BodyRegex(regex),
                    ConditionInput::Message => CompiledConditionKind::MessageRegex(regex),
                }
            }
            ConditionKind::VariableRegex { name, regex } => CompiledConditionKind::VariableRegex {
                name: name.clone(),
                regex: regex.compiled().clone(),
            },
        };
        conditions.push(CompiledCondition {
            line: condition.line,
            negated: condition.negated,
            kind,
            match_capture: regex_condition.and_then(RegexCondition::match_capture),
            capture_indexes: regex_condition
                .map(|regex| regex.capture_indexes().to_vec())
                .unwrap_or_default(),
        });
    }

    conditions
}

impl CompiledCondition {
    fn trace_result(
        &self,
        recipe_line: usize,
        condition_index: usize,
        result: PartialMatch,
        trace: &mut impl TraceSink,
    ) {
        let matched = match result {
            PartialMatch::True => true,
            PartialMatch::False => false,
            PartialMatch::Deferred => return,
        };
        let kind = match &self.kind {
            CompiledConditionKind::HeaderRegex(_) => TraceConditionKind::HeaderRegex,
            CompiledConditionKind::BodyRegex(_) => TraceConditionKind::BodyRegex,
            CompiledConditionKind::MessageRegex(_) => TraceConditionKind::MessageRegex,
            CompiledConditionKind::VariableRegex { .. } => TraceConditionKind::VariableRegex,
            CompiledConditionKind::SmallerThan(_) => TraceConditionKind::SmallerThan,
            CompiledConditionKind::LargerThan(_) => TraceConditionKind::LargerThan,
        };
        trace.record(TraceEvent::ConditionEvaluated {
            recipe_line,
            condition_line: self.line,
            condition_index,
            kind,
            negated: self.negated,
            matched,
        });
    }

    fn explain(&self) -> ConditionExplanation {
        let kind = match &self.kind {
            CompiledConditionKind::HeaderRegex(_) => ConditionKindExplanation::HeaderRegex,
            CompiledConditionKind::BodyRegex(_) => ConditionKindExplanation::BodyRegex,
            CompiledConditionKind::MessageRegex(_) => ConditionKindExplanation::MessageRegex,
            CompiledConditionKind::VariableRegex { .. } => ConditionKindExplanation::VariableRegex,
            CompiledConditionKind::SmallerThan(_) => ConditionKindExplanation::SmallerThan,
            CompiledConditionKind::LargerThan(_) => ConditionKindExplanation::LargerThan,
        };
        ConditionExplanation {
            negated: self.negated,
            kind,
        }
    }

    fn requirements(&self) -> InputRequirements {
        match self.kind {
            CompiledConditionKind::HeaderRegex(_) => InputRequirements {
                needs_headers: true,
                ..InputRequirements::default()
            },
            CompiledConditionKind::BodyRegex(_) | CompiledConditionKind::MessageRegex(_) => {
                InputRequirements {
                    needs_headers: true,
                    needs_body_contents: true,
                    needs_end_of_message: true,
                }
            }
            CompiledConditionKind::VariableRegex { .. } => InputRequirements::default(),
            CompiledConditionKind::SmallerThan(_) | CompiledConditionKind::LargerThan(_) => {
                InputRequirements {
                    needs_end_of_message: true,
                    ..InputRequirements::default()
                }
            }
        }
    }

    fn matches_headers(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
    ) -> Result<PartialMatch, EvalError> {
        let matched = match &self.kind {
            CompiledConditionKind::HeaderRegex(regex) => {
                self.regex_matches(regex, head.matching_header(), runtime)?
            }
            CompiledConditionKind::BodyRegex(_) | CompiledConditionKind::MessageRegex(_) => {
                return Ok(PartialMatch::Deferred);
            }
            CompiledConditionKind::VariableRegex { name, regex } => {
                let value = runtime.get(name).unwrap_or_default().to_owned();
                if value.len() > crate::config::MAX_ASSIGNMENT_VALUE_LEN {
                    return Err(EvalError::VariableValueTooLarge {
                        name: name.clone(),
                        size: value.len(),
                    });
                }
                self.regex_matches(regex, value.as_bytes(), runtime)?
            }
            CompiledConditionKind::SmallerThan(size) => {
                if head.len() >= *size {
                    false
                } else {
                    return Ok(PartialMatch::Deferred);
                }
            }
            CompiledConditionKind::LargerThan(size) => {
                if head.len() > *size {
                    true
                } else {
                    return Ok(PartialMatch::Deferred);
                }
            }
        };
        Ok(PartialMatch::from_bool(matched ^ self.negated))
    }

    fn matches_complete(
        &self,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
    ) -> Result<bool, EvalError> {
        let matched = match &self.kind {
            CompiledConditionKind::HeaderRegex(regex) => {
                self.regex_matches(regex, message.header_bytes(), runtime)?
            }
            CompiledConditionKind::BodyRegex(regex) => self.regex_matches(
                regex,
                message.body().ok_or(EvalError::BodyWasNotBuffered)?,
                runtime,
            )?,
            CompiledConditionKind::MessageRegex(regex) => self.regex_matches(
                regex,
                message.full().ok_or(EvalError::BodyWasNotBuffered)?,
                runtime,
            )?,
            CompiledConditionKind::VariableRegex { name, regex } => {
                let value = runtime.get(name).unwrap_or_default().to_owned();
                if value.len() > crate::config::MAX_ASSIGNMENT_VALUE_LEN {
                    return Err(EvalError::VariableValueTooLarge {
                        name: name.clone(),
                        size: value.len(),
                    });
                }
                self.regex_matches(regex, value.as_bytes(), runtime)?
            }
            CompiledConditionKind::SmallerThan(size) => message.len() < *size,
            CompiledConditionKind::LargerThan(size) => message.len() > *size,
        };
        Ok(matched ^ self.negated)
    }

    fn regex_matches(
        &self,
        regex: &Regex,
        input: &[u8],
        runtime: &mut RuntimeVariables,
    ) -> Result<bool, EvalError> {
        if self.match_capture.is_none() && self.capture_indexes.is_empty() {
            return Ok(regex.is_match(input));
        }

        // Captures are runtime variables, so stale values must disappear even
        // when this condition does not match. Validate the complete set before
        // updating the table so no later recipe can observe partial results.
        runtime.clear_match_values();
        let Some(captures) = regex.captures(input) else {
            return Ok(false);
        };
        if self.negated {
            return Ok(true);
        }
        let mut values = Vec::with_capacity(self.capture_indexes.len() + 1);
        if let Some(index) = self.match_capture {
            values.push(("MATCH".to_owned(), capture_value(&captures, index)?));
        }
        for (number, index) in self.capture_indexes.iter().copied().enumerate() {
            values.push((
                format!("MATCH{}", number + 1),
                capture_value(&captures, index)?,
            ));
        }
        let size = values
            .iter()
            .try_fold(0usize, |total, (_, value)| total.checked_add(value.len()));
        let Some(size) = size else {
            return Err(EvalError::MatchValuesTooLarge { size: usize::MAX });
        };
        if size > crate::config::MAX_MATCH_BYTES {
            return Err(EvalError::MatchValuesTooLarge { size });
        }
        for (name, value) in values {
            runtime.set_match_value(name, value);
        }
        Ok(true)
    }
}

fn capture_value(captures: &regex::bytes::Captures<'_>, index: usize) -> Result<String, EvalError> {
    let bytes = captures
        .get(index)
        .map_or(&[][..], |matched| matched.as_bytes());
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| EvalError::MatchValueIsNotUtf8)
}

#[derive(Debug, Clone, Copy)]
enum CompleteMessage<'a> {
    Buffered {
        message: &'a Message,
        matching_full: Option<&'a [u8]>,
    },
    Streamed(&'a StreamedMessage),
    Mapped {
        raw: &'a [u8],
        header_len: usize,
        matching_header: Option<&'a [u8]>,
        matching_raw: Option<&'a [u8]>,
    },
}

fn matching_views_are_valid(
    raw_len: usize,
    header_len: usize,
    matching_header: Option<&[u8]>,
    matching_raw: Option<&[u8]>,
    needs_matching_raw: bool,
) -> bool {
    // Normalizing CRLF folding can shorten the header, so validate the two
    // borrowed views by their independently known pieces rather than reusing
    // the raw header offset. A full HB view is mandatory whenever a changed
    // header could otherwise make matching fall back to delivery bytes.
    match (matching_header, matching_raw) {
        (None, None) => true,
        (Some(_), None) => !needs_matching_raw,
        (Some(header), Some(full)) => header
            .len()
            .checked_add(raw_len - header_len)
            .is_some_and(|expected| expected == full.len()),
        (None, Some(_)) => false,
    }
}

impl<'a> CompleteMessage<'a> {
    fn header_bytes(self) -> &'a [u8] {
        match self {
            Self::Buffered { message, .. } => message.matching_header(),
            Self::Streamed(message) => message.matching_header(),
            Self::Mapped {
                raw,
                header_len,
                matching_header,
                matching_raw: _,
            } => matching_header.unwrap_or(&raw[..header_len]),
        }
    }

    fn body(self) -> Option<&'a [u8]> {
        match self {
            Self::Buffered { message, .. } => Some(message.body()),
            Self::Streamed(_) => None,
            Self::Mapped {
                raw, header_len, ..
            } => Some(&raw[header_len..]),
        }
    }

    fn full(self) -> Option<&'a [u8]> {
        match self {
            Self::Buffered {
                message,
                matching_full,
            } => Some(matching_full.unwrap_or_else(|| message.as_bytes())),
            Self::Streamed(_) => None,
            Self::Mapped {
                raw, matching_raw, ..
            } => Some(matching_raw.unwrap_or(raw)),
        }
    }

    fn len(self) -> usize {
        match self {
            Self::Buffered { message, .. } => message.len(),
            Self::Streamed(message) => message.len(),
            Self::Mapped { raw, .. } => raw.len(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartialMatch {
    True,
    False,
    Deferred,
}

impl PartialMatch {
    fn from_bool(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }
}

#[derive(Debug, Clone, Copy)]
enum RegexArea {
    Headers,
    Body,
    Message,
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
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::config;
    use crate::limits::MessageLimits;
    use crate::trace::{
        BoundedTraceWriter, ConditionKind as TraceConditionKind, MemoryTrace, RecipeDecision,
        TraceEvent, TraceName, VariableSource as TraceVariableSource,
    };

    #[derive(Default)]
    struct Recorder {
        destinations: Vec<Destination>,
    }

    struct FailingRecorder {
        fail_paths: &'static [&'static str],
        attempted: Vec<String>,
    }

    impl Delivery for FailingRecorder {
        fn deliver(&mut self, destination: &Destination, _: &Message) -> Result<(), String> {
            self.attempted.push(destination.path().to_owned());
            if self.fail_paths.contains(&destination.path()) {
                Err("injected delivery failure".to_owned())
            } else {
                Ok(())
            }
        }
    }

    impl Delivery for Recorder {
        fn deliver(&mut self, destination: &Destination, _: &Message) -> Result<(), String> {
            self.destinations.push(destination.clone());
            Ok(())
        }
    }

    fn compile(source: &str) -> ExecutionPlan {
        ExecutionPlan::compile(&config::parse(source).unwrap())
    }

    fn destinations(plan: &DeliveryPlan) -> Vec<Destination> {
        plan.deliveries()
            .iter()
            .map(|delivery| delivery.destination().clone())
            .collect()
    }

    fn pending_destinations(continuation: &Continuation) -> Vec<Destination> {
        continuation
            .pending_deliveries()
            .iter()
            .map(|delivery| delivery.destination().clone())
            .collect()
    }

    fn evaluate_config(source: &str, raw: &[u8]) -> (Outcome, Recorder) {
        let config = config::parse(source).unwrap();
        let message = Message::from_bytes(raw.to_vec());
        let mut recorder = Recorder::default();
        let outcome = evaluate(&config, &message, &mut recorder).unwrap();
        (outcome, recorder)
    }

    fn head(raw: &[u8]) -> MessageHead {
        Message::read_headers(&mut Cursor::new(raw), MessageLimits::default()).unwrap()
    }

    #[test]
    fn computes_static_input_requirements() {
        let header_only = compile(":0\n* ^Subject:\ninbox/\n");
        assert_eq!(
            header_only.requirements(),
            InputRequirements {
                needs_headers: true,
                needs_body_contents: false,
                needs_end_of_message: false,
            }
        );

        let body = compile(":0 B\n* needle\ninbox/\n");
        assert!(body.requirements().needs_body_contents);
        assert!(body.requirements().needs_end_of_message);

        let size = compile(":0\n* < 100\ninbox/\n");
        assert!(!size.requirements().needs_body_contents);
        assert!(size.requirements().needs_end_of_message);
    }

    #[test]
    fn computes_nested_requirements_from_the_compiled_tree() {
        let plan = compile(":0\n* ^List-Id:\n{\n:0 B\n* body-marker\nmaildir:body\n}\n");

        assert_eq!(plan.root.recipes.len(), 1);
        let CompiledAction::Block(children) = &plan.root.recipes[0].action else {
            panic!("expected compiled block action");
        };
        assert_eq!(children.recipes.len(), 1);
        assert_eq!(
            plan.requirements(),
            InputRequirements {
                needs_headers: true,
                needs_body_contents: true,
                needs_end_of_message: true,
            }
        );
    }

    #[test]
    fn finds_ordered_delivery_inside_the_compiled_tree() {
        let plan = compile(":0\n{\n:0\nmbox:archive\n}\n");

        assert!(plan.root.requires_ordered_delivery());
        assert!(plan.requires_ordered_delivery());
        assert!(plan.requirements().needs_end_of_message);
    }

    #[test]
    fn forwards_evaluation_events_to_the_selected_sink() {
        let config = config::parse("BOX=inbox\n:0\n* ^Subject: wanted$\nmaildir:$BOX\n")
            .unwrap()
            .expand()
            .unwrap();
        let plan = ExecutionPlan::compile(&config);
        let mut runtime = RuntimeVariables::default();
        let mut trace = MemoryTrace::default();

        let result = plan.evaluate_headers_with_trace(
            &head(b"Subject: wanted\n\nbody"),
            &mut runtime,
            &mut trace,
        );

        assert!(matches!(result, HeaderEvaluation::Decided(_)));
        assert_eq!(
            trace.events(),
            [
                TraceEvent::VariableAssigned {
                    line: Some(1),
                    name: TraceName::new("BOX").unwrap(),
                    source: TraceVariableSource::RcFile,
                    value: None,
                },
                TraceEvent::ConditionEvaluated {
                    recipe_line: 2,
                    condition_line: 3,
                    condition_index: 0,
                    kind: TraceConditionKind::HeaderRegex,
                    negated: false,
                    matched: true,
                },
                TraceEvent::RecipeEvaluated {
                    line: 2,
                    decision: RecipeDecision::Selected,
                },
            ]
        );
        assert!(!trace.was_truncated());
    }

    #[test]
    fn executes_assignments_after_the_final_recipe() {
        let config = config::parse(":0\n* ^X-Never: yes$\nmaildir:unused\nAFTER=tail\n")
            .unwrap()
            .expand()
            .unwrap();
        let plan = ExecutionPlan::compile(&config);
        let mut runtime = RuntimeVariables::default();

        let result =
            plan.evaluate_headers_with_runtime(&head(b"Subject: test\n\nbody"), &mut runtime);

        assert!(matches!(result, HeaderEvaluation::Decided(_)));
        assert_eq!(runtime.get("AFTER"), Some("tail"));
    }

    #[test]
    fn nested_assignment_uses_runtime_capture_before_delivery() {
        let config = config::parse(
            ":0\n* ^Subject: \\/(.*)$\n{\nBOX=${MATCH1:-fallback}\n:0\nmaildir:$BOX\n}\n",
        )
        .unwrap()
        .expand()
        .unwrap();
        let plan = ExecutionPlan::compile(&config);
        let mut runtime = RuntimeVariables::default();

        let raw = b"Subject: selected\n\nbody";
        let HeaderEvaluation::NeedsMessage(continuation) =
            plan.evaluate_headers_with_runtime(&head(raw), &mut runtime)
        else {
            panic!("expected deferred runtime destination");
        };
        let delivery = plan
            .resume_mapped_with_runtime(
                continuation,
                raw,
                b"Subject: selected\n\n".len(),
                &mut runtime,
            )
            .unwrap();

        assert_eq!(runtime.get("BOX"), Some("selected"));
        let destination = delivery.deliveries()[0]
            .destination()
            .resolve_with(|name| runtime.get(name).map(str::to_owned))
            .unwrap();
        assert_eq!(destination.path(), "selected");
    }

    #[test]
    fn skipped_block_does_not_apply_its_assignment() {
        let config = config::parse(":0\n* ^X-Select: yes$\n{\nBOX=selected\n}\n")
            .unwrap()
            .expand()
            .unwrap();
        let plan = ExecutionPlan::compile(&config);
        let mut runtime = RuntimeVariables::default();

        let result =
            plan.evaluate_headers_with_runtime(&head(b"Subject: skipped\n\nbody"), &mut runtime);

        assert!(matches!(result, HeaderEvaluation::Decided(_)));
        assert_eq!(runtime.get("BOX"), None);
    }

    #[test]
    fn nested_maildir_changes_the_base_for_following_destination() {
        let config =
            config::parse("MAILDIR=/srv/mail\n:0\n{\nMAILDIR=selected\n:0\nmaildir:inbox\n}\n")
                .unwrap()
                .expand()
                .unwrap();
        let plan = ExecutionPlan::compile(&config);
        let raw = b"Subject: test\n\nbody";
        let mut runtime = RuntimeVariables::default();
        let HeaderEvaluation::NeedsMessage(continuation) =
            plan.evaluate_headers_with_runtime(&head(raw), &mut runtime)
        else {
            panic!("expected deferred runtime destination");
        };

        let delivery = plan
            .resume_mapped_with_runtime(continuation, raw, b"Subject: test\n\n".len(), &mut runtime)
            .unwrap();

        assert_eq!(runtime.get("MAILDIR"), Some("/srv/mail/selected"));
        let destination = delivery.deliveries()[0]
            .destination()
            .resolve_with(|name| runtime.get(name).map(str::to_owned))
            .unwrap();
        assert_eq!(destination.path(), "/srv/mail/selected/inbox");
    }

    #[test]
    fn rendered_default_trace_excludes_message_and_configuration_values() {
        let config = config::parse(
            "TOKEN=variable-secret\n:0 c\n* ^Subject: header-secret$\nmaildir:path-secret\n:0\nmaildir:final-secret\n",
        )
        .unwrap()
        .expand()
        .unwrap();
        let plan = ExecutionPlan::compile(&config);
        let mut runtime = RuntimeVariables::default();
        let mut trace = BoundedTraceWriter::new(Vec::new());

        let result = plan.evaluate_headers_with_trace(
            &head(b"Subject: header-secret\nAuthorization: credential-secret\n\nbody-secret"),
            &mut runtime,
            &mut trace,
        );
        assert!(matches!(result, HeaderEvaluation::Decided(_)));

        let rendered = String::from_utf8(trace.into_inner()).unwrap();
        for private in [
            "variable-secret",
            "header-secret",
            "credential-secret",
            "body-secret",
            "path-secret",
            "final-secret",
        ] {
            assert!(!rendered.contains(private), "leaked {private:?}");
        }
        assert!(rendered.contains("name=\"TOKEN\""));
        assert!(rendered.contains("event=condition"));
        assert!(rendered.contains("event=recipe"));
    }

    #[test]
    fn variable_values_require_an_explicit_high_detail_sink() {
        let config = config::parse("TOKEN=secret-value\n:0\nmaildir:inbox\n")
            .unwrap()
            .expand()
            .unwrap();
        let plan = ExecutionPlan::compile(&config);
        let mut runtime = RuntimeVariables::default();
        let mut trace =
            BoundedTraceWriter::with_detail(Vec::new(), crate::trace::TraceDetail::Values);

        let result = plan.evaluate_headers_with_trace(
            &head(b"Subject: test\n\nbody"),
            &mut runtime,
            &mut trace,
        );
        assert!(matches!(result, HeaderEvaluation::Decided(_)));

        let rendered = String::from_utf8(trace.into_inner()).unwrap();
        assert!(rendered.contains("value=\"secret-value\""));
    }

    #[test]
    fn explains_plan_shape_without_private_configuration_values() {
        let config = config::parse(
            "PRIVATE_TOKEN=do-not-print\n:0 HBc\n* ! private-pattern\nmaildir:${LASTFOLDER:-private-path}\n",
        )
        .unwrap()
        .expand()
        .unwrap();
        let explanation = ExecutionPlan::compile(&config).explain();

        assert!(explanation.requirements().needs_headers);
        assert!(explanation.requirements().needs_body_contents);
        assert!(explanation.requirements().needs_end_of_message);
        assert!(explanation.requires_ordered_delivery());
        let [recipe] = explanation.recipes() else {
            panic!("expected one recipe");
        };
        assert_eq!(recipe.line(), 2);
        assert_eq!(recipe.assignment_count(), 1);
        assert_eq!(recipe.destination(), DestinationKind::Maildir);
        assert!(recipe.is_copy());
        assert!(recipe.defers_destination());
        assert_eq!(
            recipe.conditions(),
            [ConditionExplanation {
                negated: true,
                kind: ConditionKindExplanation::MessageRegex,
            }]
        );

        let rendered = format!("{explanation:?}");
        for private in [
            "PRIVATE_TOKEN",
            "do-not-print",
            "private-pattern",
            "private-path",
        ] {
            assert!(!rendered.contains(private), "leaked {private:?}");
        }
    }

    #[test]
    fn header_match_decides_before_body() {
        let plan =
            compile(":0\n* ^Subject: wanted$\nmaildir:wanted\n\n:0 B\n* needle\nmaildir:body\n");
        let result = plan.evaluate_headers(&head(b"Subject: wanted\n\nbody"));

        let HeaderEvaluation::Decided(delivery) = result else {
            panic!("expected a header decision");
        };
        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("wanted".into())]
        );
    }

    #[test]
    fn unconditional_recipe_makes_later_body_rule_unreachable() {
        let plan = compile(":0\nmaildir:all\n\n:0 B\n* needle\nmaildir:body\n");
        let result = plan.evaluate_headers(&head(b"Subject: test\n\nbody"));

        let HeaderEvaluation::Decided(delivery) = result else {
            panic!("expected an unconditional decision");
        };
        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("all".into())]
        );
    }

    #[test]
    fn parent_conditions_gate_nested_delivery() {
        let plan = compile(
            ":0\n* ^List-Id: wanted$\n{\n:0\n* ^Subject: report$\nmaildir:list\n}\n:0\nmaildir:fallback\n",
        );

        let HeaderEvaluation::Decided(selected) =
            plan.evaluate_headers(&head(b"List-Id: wanted\nSubject: report\n\nbody"))
        else {
            panic!("expected nested delivery");
        };
        assert_eq!(
            destinations(&selected),
            [Destination::Maildir("list".into())]
        );

        let HeaderEvaluation::Decided(skipped) =
            plan.evaluate_headers(&head(b"List-Id: other\nSubject: report\n\nbody"))
        else {
            panic!("expected fallback delivery");
        };
        assert_eq!(
            destinations(&skipped),
            [Destination::Maildir("fallback".into())]
        );
    }

    #[test]
    fn variable_regex_uses_the_current_bounded_runtime_value() {
        let plan = compile(":0\n* CATEGORY ?? ^alerts$\nmaildir:matched\n");
        let head = head(b"Subject: unrelated\n\nbody");
        let mut runtime = RuntimeVariables::default();
        runtime.set("CATEGORY", "alerts");

        let HeaderEvaluation::Decided(delivery) =
            plan.evaluate_headers_with_runtime(&head, &mut runtime)
        else {
            panic!("expected a header decision");
        };

        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("matched".into())]
        );
        assert_eq!(plan.requirements(), InputRequirements::default());
    }

    #[test]
    fn special_area_condition_overrides_recipe_input_flags() {
        let body_plan = compile(":0 H\n* B ?? needle\nmaildir:body\n");
        assert_eq!(
            body_plan.requirements(),
            InputRequirements {
                needs_headers: true,
                needs_body_contents: true,
                needs_end_of_message: true,
            }
        );
        let body_delivery = body_plan
            .evaluate_full(&Message::from_bytes(
                b"Subject: unrelated\n\nneedle".to_vec(),
            ))
            .unwrap();
        assert_eq!(
            destinations(&body_delivery),
            [Destination::Maildir("body".into())]
        );

        let header_plan = compile(":0 B\n* H ?? ^Subject: wanted$\nmaildir:headers\n");
        assert_eq!(
            header_plan.requirements(),
            InputRequirements {
                needs_headers: true,
                ..InputRequirements::default()
            }
        );
        let HeaderEvaluation::Decided(header_delivery) =
            header_plan.evaluate_headers(&head(b"Subject: wanted\n\nbody"))
        else {
            panic!("expected a header decision");
        };
        assert_eq!(
            destinations(&header_delivery),
            [Destination::Maildir("headers".into())]
        );
    }

    #[test]
    fn procmail_anchors_use_the_whole_selected_area() {
        let start = compile(":0\n* B ?? ^^%!\nmaildir:postscript\n");
        let delivery = start
            .evaluate_full(&Message::from_bytes(
                b"Subject: file\n\n%!PS-Adobe".to_vec(),
            ))
            .unwrap();
        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("postscript".into())]
        );

        let end = compile(":0 B\n* trailer^^\nmaildir:ended\n");
        let delivery = end
            .evaluate_full(&Message::from_bytes(
                b"Subject: file\n\nbody trailer".to_vec(),
            ))
            .unwrap();
        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("ended".into())]
        );
    }

    #[test]
    fn procmail_word_edges_consume_the_surrounding_bytes() {
        let plan = compile(":0\n* ^Subject: \\<word\\/\\>$\nmaildir:matched\n");
        let mut runtime = RuntimeVariables::default();

        let HeaderEvaluation::Decided(delivery) =
            plan.evaluate_headers_with_runtime(&head(b"Subject: !word?\n\nbody"), &mut runtime)
        else {
            panic!("expected a header decision");
        };

        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("matched".into())]
        );
        assert_eq!(runtime.get("MATCH"), Some("?"));
    }

    #[test]
    fn match_marker_and_numbered_groups_feed_later_expansion() {
        let plan =
            compile(":0\n* ^Subject: ([a-z]+)-\\/([a-z]+)$\nmaildir:$MATCH1-$MATCH-$MATCH2\n");
        let mut runtime = RuntimeVariables::default();

        let HeaderEvaluation::Decided(delivery) =
            plan.evaluate_headers_with_runtime(&head(b"Subject: alpha-beta\n\nbody"), &mut runtime)
        else {
            panic!("expected a header decision");
        };

        assert_eq!(runtime.get("MATCH"), Some("beta"));
        assert_eq!(runtime.get("MATCH1"), Some("alpha"));
        assert_eq!(runtime.get("MATCH2"), Some("beta"));
        let resolved = delivery.deliveries()[0]
            .destination()
            .resolve_with(|name| runtime.get(name).map(str::to_owned))
            .unwrap();
        assert_eq!(resolved, Destination::Maildir("alpha-beta-beta".into()));
    }

    #[test]
    fn failed_capture_condition_clears_previous_values() {
        let plan = compile(":0\n* ^Subject: (wanted)$\nmaildir:matched\n");
        let mut runtime = RuntimeVariables::default();
        runtime.set("MATCH1", "stale");

        let HeaderEvaluation::Decided(delivery) =
            plan.evaluate_headers_with_runtime(&head(b"Subject: other\n\nbody"), &mut runtime)
        else {
            panic!("expected a header decision");
        };

        assert!(delivery.deliveries().is_empty());
        assert_eq!(runtime.get("MATCH1"), None);
    }

    #[test]
    fn unmatched_optional_group_becomes_an_empty_value() {
        let plan = compile(":0\n* ^Subject: (wanted)(-extra)?$\nmaildir:matched\n");
        let mut runtime = RuntimeVariables::default();

        let HeaderEvaluation::Decided(_) =
            plan.evaluate_headers_with_runtime(&head(b"Subject: wanted\n\nbody"), &mut runtime)
        else {
            panic!("expected a header decision");
        };

        assert_eq!(runtime.get("MATCH1"), Some("wanted"));
        assert_eq!(runtime.get("MATCH2"), Some(""));
    }

    #[test]
    fn capture_values_obey_the_aggregate_byte_limit() {
        let plan = compile(":0\n* VALUE ?? ^((x+))$\nmaildir:matched\n");
        for length in [
            crate::config::MAX_MATCH_BYTES / 2,
            crate::config::MAX_MATCH_BYTES / 2 + 1,
        ] {
            let mut runtime = RuntimeVariables::default();
            runtime.set("VALUE", "x".repeat(length));
            let result =
                plan.evaluate_headers_with_runtime(&head(b"Subject: test\n\nbody"), &mut runtime);
            if length * 2 <= crate::config::MAX_MATCH_BYTES {
                assert!(matches!(result, HeaderEvaluation::Decided(_)));
            } else {
                assert!(matches!(
                    result,
                    HeaderEvaluation::Error(EvalError::MatchValuesTooLarge { size })
                        if size == length * 2
                ));
            }
        }
    }

    #[test]
    fn non_utf8_capture_is_rejected_without_partial_values() {
        let plan = compile(":0\n* ^X-Binary: (.)$\nmaildir:matched\n");
        let mut runtime = RuntimeVariables::default();
        runtime.set("MATCH1", "stale");
        let result =
            plan.evaluate_headers_with_runtime(&head(b"X-Binary: \xff\n\nbody"), &mut runtime);

        assert!(matches!(
            result,
            HeaderEvaluation::Error(EvalError::MatchValueIsNotUtf8)
        ));
        assert_eq!(runtime.get("MATCH1"), None);
    }

    #[test]
    fn missing_variable_matches_as_an_empty_value() {
        let plan = compile(":0\n* MISSING ?? ^$\nmaildir:matched\n");

        let HeaderEvaluation::Decided(delivery) =
            plan.evaluate_headers(&head(b"Subject: test\n\nbody"))
        else {
            panic!("expected a header decision");
        };

        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("matched".into())]
        );
    }

    #[test]
    fn variable_regex_enforces_the_runtime_value_limit_at_the_boundary() {
        let plan = compile(":0\n* VALUE ?? ^x+$\nmaildir:matched\n");
        for size in [
            crate::config::MAX_ASSIGNMENT_VALUE_LEN - 1,
            crate::config::MAX_ASSIGNMENT_VALUE_LEN,
            crate::config::MAX_ASSIGNMENT_VALUE_LEN + 1,
        ] {
            let mut runtime = RuntimeVariables::default();
            runtime.set("VALUE", "x".repeat(size));
            let result =
                plan.evaluate_headers_with_runtime(&head(b"Subject: test\n\nbody"), &mut runtime);

            if size <= crate::config::MAX_ASSIGNMENT_VALUE_LEN {
                assert!(matches!(result, HeaderEvaluation::Decided(_)));
            } else {
                assert!(matches!(
                    result,
                    HeaderEvaluation::Error(EvalError::VariableValueTooLarge {
                        name,
                        size: actual
                    }) if name == "VALUE" && actual == size
                ));
            }
        }
    }

    #[test]
    fn processing_continues_after_copy_delivery_in_a_block() {
        let plan = compile(":0\n{\n:0 c\nmaildir:copy\n}\n:0\nmaildir:final\n");
        let HeaderEvaluation::Decided(delivery) =
            plan.evaluate_headers(&head(b"Subject: test\n\nbody"))
        else {
            panic!("expected delivery");
        };

        assert_eq!(
            destinations(&delivery),
            [
                Destination::Maildir("copy".into()),
                Destination::Maildir("final".into())
            ]
        );
        assert!(delivery.original_delivered());
    }

    #[test]
    fn failed_copy_makes_its_block_eligible_for_error_handling() {
        let config =
            config::parse(":0\n{\n:0 c\nmaildir:primary\n}\n:0 e\nmaildir:fallback\n").unwrap();
        let message = Message::from_bytes(b"Subject: test\n\nbody".to_vec());
        let mut recorder = FailingRecorder {
            fail_paths: &["primary"],
            attempted: Vec::new(),
        };

        let outcome = evaluate(&config, &message, &mut recorder).unwrap();

        assert_eq!(recorder.attempted, ["primary", "fallback"]);
        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn recovered_child_failure_makes_its_block_succeed() {
        let config = config::parse(
            ":0\n{\n:0 c\nmaildir:primary\n:0 ec\nmaildir:inner-fallback\n}\n:0 ec\nmaildir:outer-fallback\n:0\nmaildir:final\n",
        )
        .unwrap();
        let message = Message::from_bytes(b"Subject: test\n\nbody".to_vec());
        let mut recorder = FailingRecorder {
            fail_paths: &["primary"],
            attempted: Vec::new(),
        };

        let outcome = evaluate(&config, &message, &mut recorder).unwrap();

        assert_eq!(recorder.attempted, ["primary", "inner-fallback", "final"]);
        assert_eq!(outcome, Outcome::Delivered { deliveries: 2 });
    }

    #[test]
    fn uppercase_chain_uses_last_unchained_recipe_at_the_same_level() {
        let plan = compile(
            ":0 c\n* ^Subject: wanted$\nmaildir:first\n:0 Ac\n* ^X-Never: yes$\nmaildir:skipped\n:0 A\nmaildir:final\n",
        );
        let HeaderEvaluation::Decided(delivery) =
            plan.evaluate_headers(&head(b"Subject: wanted\n\nbody"))
        else {
            panic!("expected chained delivery");
        };
        assert_eq!(
            destinations(&delivery),
            [
                Destination::Maildir("first".into()),
                Destination::Maildir("final".into())
            ]
        );

        let HeaderEvaluation::Decided(unmatched) =
            plan.evaluate_headers(&head(b"Subject: other\n\nbody"))
        else {
            panic!("expected a complete decision");
        };
        assert!(destinations(&unmatched).is_empty());
        assert!(!unmatched.original_delivered());
    }

    #[test]
    fn lowercase_chain_requires_the_immediately_preceding_recipe() {
        let plan = compile(
            ":0 c\n* ^Subject: wanted$\nmaildir:first\n:0 Ac\n* ^X-Select: yes$\nmaildir:second\n:0 a\nmaildir:final\n",
        );

        let selected = plan
            .evaluate_full(&Message::from_bytes(
                b"Subject: wanted\nX-Select: yes\n\nbody".to_vec(),
            ))
            .unwrap();
        assert_eq!(destinations(&selected).len(), 3);

        let skipped = plan
            .evaluate_full(&Message::from_bytes(b"Subject: wanted\n\nbody".to_vec()))
            .unwrap();
        assert_eq!(
            destinations(&skipped),
            [Destination::Maildir("first".into())]
        );
        assert!(!skipped.original_delivered());
    }

    #[test]
    fn lowercase_chain_forces_ordered_publication() {
        let plan = compile(":0 c\nmaildir:first\n:0 a\nmaildir:second\n");

        assert!(plan.requires_ordered_delivery());
        assert!(plan.requirements().needs_end_of_message);
    }

    #[test]
    fn ordered_header_evaluation_defers_before_the_first_action() {
        let plan = compile("BOX=first\n:0 c\nmaildir:$BOX\n:0 a\nmaildir:second\n");
        let mut runtime = RuntimeVariables::default();
        let mut trace = MemoryTrace::default();
        let HeaderEvaluation::NeedsMessage(continuation) = plan.evaluate_headers_with_trace(
            &head(b"Subject: test\n\nbody"),
            &mut runtime,
            &mut trace,
        ) else {
            panic!("expected ordered plan to defer before evaluation");
        };

        assert!(continuation.pending_deliveries().is_empty());
        assert!(trace.events().is_empty());
        assert!(runtime.get("BOX").is_none());

        let delivery = plan
            .resume_buffered(
                continuation,
                &Message::from_bytes(b"Subject: test\n\nbody".to_vec()),
            )
            .unwrap();
        assert_eq!(
            delivery
                .deliveries()
                .iter()
                .map(|delivery| delivery.destination().path())
                .collect::<Vec<_>>(),
            ["$BOX", "second"]
        );
    }

    #[test]
    fn chain_without_a_preceding_recipe_never_executes() {
        for flag in ['A', 'a'] {
            let plan = compile(&format!(":0 {flag}\nmaildir:unreachable\n"));
            let HeaderEvaluation::Decided(delivery) =
                plan.evaluate_headers(&head(b"Subject: test\n\nbody"))
            else {
                panic!("expected a complete decision");
            };
            assert!(destinations(&delivery).is_empty());
        }
    }

    #[test]
    fn long_chain_reuses_the_preceding_condition_result() {
        let mut source = ":0 c\n* ^Subject: wanted$\nmaildir:first\n".to_owned();
        for index in 0..64 {
            source.push_str(&format!(":0 Ac\nmaildir:copy-{index}\n"));
        }
        source.push_str(":0 A\nmaildir:final\n");
        let plan = compile(&source);
        let mut runtime = RuntimeVariables::default();
        let mut trace = MemoryTrace::default();

        let result = plan.evaluate_headers_with_trace(
            &head(b"Subject: wanted\n\nbody"),
            &mut runtime,
            &mut trace,
        );
        let HeaderEvaluation::Decided(delivery) = result else {
            panic!("expected chained delivery");
        };
        assert_eq!(destinations(&delivery).len(), 66);
        assert_eq!(
            trace
                .events()
                .iter()
                .filter(|event| matches!(event, TraceEvent::ConditionEvaluated { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn else_chain_selects_only_the_first_available_branch() {
        let plan = compile(
            ":0 c\n* ^Subject: first$\nmaildir:first\n:0 Ec\n* ^Subject: second$\nmaildir:second\n:0 E\nmaildir:fallback\n",
        );

        for (subject, expected) in [
            ("first", "first"),
            ("second", "second"),
            ("other", "fallback"),
        ] {
            let raw = format!("Subject: {subject}\n\nbody");
            let HeaderEvaluation::Decided(delivery) = plan.evaluate_headers(&head(raw.as_bytes()))
            else {
                panic!("expected complete else decision");
            };
            assert_eq!(destinations(&delivery)[0].path(), expected);
            assert_eq!(destinations(&delivery).len(), 1);
        }
    }

    #[test]
    fn first_else_recipe_is_an_unconditional_branch() {
        let plan = compile(":0 E\nmaildir:fallback\n");
        let HeaderEvaluation::Decided(delivery) =
            plan.evaluate_headers(&head(b"Subject: test\n\nbody"))
        else {
            panic!("expected fallback delivery");
        };
        assert_eq!(destinations(&delivery)[0].path(), "fallback");
    }

    #[test]
    fn error_recipe_runs_only_after_a_failed_action() {
        let config = config::parse(":0\nmaildir:primary\n:0 e\nmaildir:fallback\n").unwrap();
        let message = Message::from_bytes(b"Subject: test\n\nbody".to_vec());
        let mut failed = FailingRecorder {
            fail_paths: &["primary"],
            attempted: Vec::new(),
        };

        let outcome = evaluate(&config, &message, &mut failed).unwrap();
        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
        assert_eq!(failed.attempted, ["primary", "fallback"]);

        let mut succeeded = FailingRecorder {
            fail_paths: &[],
            attempted: Vec::new(),
        };
        let outcome = evaluate(&config, &message, &mut succeeded).unwrap();
        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
        assert_eq!(succeeded.attempted, ["primary"]);
    }

    #[test]
    fn failed_error_handler_preserves_its_own_error() {
        let config = config::parse(":0\nmaildir:primary\n:0 e\nmaildir:fallback\n").unwrap();
        let message = Message::from_bytes(b"Subject: test\n\nbody".to_vec());
        let mut recorder = FailingRecorder {
            fail_paths: &["primary", "fallback"],
            attempted: Vec::new(),
        };

        let error = evaluate(&config, &message, &mut recorder).unwrap_err();
        assert!(matches!(
            error,
            EvalError::Delivery { destination, .. } if destination == "fallback"
        ));
        assert_eq!(recorder.attempted, ["primary", "fallback"]);
    }

    #[test]
    fn ordered_tree_binds_runtime_values_between_actual_actions() {
        let plan = compile("BOX=first\n:0 c\nmaildir:$BOX\n:0\nmaildir:${LASTFOLDER}.second\n");
        let raw = b"Subject: test\n\nbody";
        let mut runtime = RuntimeVariables::default();
        let mut trace = NoTrace;
        let mut attempted = Vec::new();

        let outcome = plan
            .execute_mapped_ordered_with_trace(
                raw,
                b"Subject: test\n\n".len(),
                &mut runtime,
                &mut trace,
                &mut |destination, _, runtime, _| {
                    let destination = destination
                        .resolve_with(|name| runtime.get(name).map(str::to_owned))
                        .unwrap();
                    attempted.push(destination.path().to_owned());
                    runtime.set("LASTFOLDER", destination.path());
                    Ok::<_, DeliveryAttemptError<&str>>(())
                },
            )
            .unwrap();

        assert_eq!(attempted, ["first", "first.second"]);
        assert_eq!(runtime.last_folder(), Some("first.second"));
        assert_eq!(outcome.published(), 2);
        assert!(outcome.original_delivered());
    }

    #[test]
    fn ordered_tree_uses_actual_failure_for_lowercase_chain() {
        let plan = compile(":0 c\nmaildir:primary\n:0 a\nmaildir:dependent\n");
        let raw = b"Subject: test\n\nbody";
        let mut runtime = RuntimeVariables::default();
        let mut trace = NoTrace;
        let mut attempted = Vec::new();

        let outcome = plan
            .execute_mapped_ordered_with_trace(
                raw,
                b"Subject: test\n\n".len(),
                &mut runtime,
                &mut trace,
                &mut |destination, _, _, _| {
                    attempted.push(destination.path().to_owned());
                    if destination.path() == "primary" {
                        Err(DeliveryAttemptError::Recoverable("primary failed"))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();

        assert_eq!(attempted, ["primary"]);
        assert!(matches!(
            outcome,
            OrderedExecutionError::Delivery("primary failed")
        ));
    }

    #[test]
    fn ordered_tree_uses_actual_failure_for_error_handler() {
        let plan = compile(":0\nmaildir:primary\n:0 e\nmaildir:fallback\n");
        let raw = b"Subject: test\n\nbody";
        let mut runtime = RuntimeVariables::default();
        let mut trace = NoTrace;
        let mut attempted = Vec::new();

        let outcome = plan
            .execute_mapped_ordered_with_trace(
                raw,
                b"Subject: test\n\n".len(),
                &mut runtime,
                &mut trace,
                &mut |destination, _, _, _| {
                    attempted.push(destination.path().to_owned());
                    if destination.path() == "primary" {
                        Err(DeliveryAttemptError::Recoverable("primary failed"))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap();

        assert_eq!(attempted, ["primary", "fallback"]);
        assert_eq!(outcome.published(), 1);
        assert!(outcome.original_delivered());
    }

    #[test]
    fn ordered_tree_does_not_handle_failure_after_publication() {
        let plan = compile(":0\nmaildir:primary\n:0 e\nmaildir:fallback\n");
        let raw = b"Subject: test\n\nbody";
        let mut runtime = RuntimeVariables::default();
        let mut trace = NoTrace;
        let mut attempted = Vec::new();

        let error = plan
            .execute_mapped_ordered_with_trace(
                raw,
                b"Subject: test\n\n".len(),
                &mut runtime,
                &mut trace,
                &mut |destination, _, _, _| {
                    attempted.push(destination.path().to_owned());
                    Err(DeliveryAttemptError::Fatal("durability failed"))
                },
            )
            .unwrap_err();

        assert!(matches!(
            error,
            OrderedExecutionError::Delivery("durability failed")
        ));
        assert_eq!(attempted, ["primary"]);
    }

    #[test]
    fn consecutive_error_handlers_can_recover_the_latest_failure() {
        let config = config::parse(
            ":0\nmaildir:primary\n:0 e\nmaildir:first-fallback\n:0 e\nmaildir:second-fallback\n",
        )
        .unwrap();
        let message = Message::from_bytes(b"Subject: test\n\nbody".to_vec());
        let mut recorder = FailingRecorder {
            fail_paths: &["primary", "first-fallback"],
            attempted: Vec::new(),
        };

        let outcome = evaluate(&config, &message, &mut recorder).unwrap();
        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
        assert_eq!(
            recorder.attempted,
            ["primary", "first-fallback", "second-fallback"]
        );
    }

    #[test]
    fn else_state_is_local_to_each_recipe_block() {
        let plan = compile(
            ":0\n* ^List-Id: wanted$\n{\n:0 c\n* ^Subject: missing$\nmaildir:child-first\n:0 E\nmaildir:child-fallback\n}\n",
        );
        let delivery = plan
            .evaluate_full(&Message::from_bytes(
                b"List-Id: wanted\nSubject: other\n\nbody".to_vec(),
            ))
            .unwrap();

        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("child-fallback".into())]
        );
    }

    #[test]
    fn failed_header_match_defers_to_reachable_body_recipe() {
        let plan =
            compile(":0\n* ^Subject: wanted$\nmaildir:wanted\n\n:0 B\n* needle\nmaildir:body\n");
        let result = plan.evaluate_headers(&head(b"Subject: other\n\nbody"));

        let HeaderEvaluation::NeedsMessage(continuation) = result else {
            panic!("expected deferred evaluation");
        };
        assert!(continuation.requirements().needs_body_contents);
    }

    #[test]
    fn preserves_header_selected_copies_across_continuation() {
        let plan = compile(":0 c\n* ^List-Id:\nmaildir:copy\n\n:0 B\n* needle\nmaildir:body\n");
        let raw = b"List-Id: users.example\n\nneedle\n";
        let result = plan.evaluate_headers(&head(raw));
        let HeaderEvaluation::NeedsMessage(continuation) = result else {
            panic!("expected deferred evaluation");
        };
        assert_eq!(
            pending_destinations(&continuation),
            [Destination::Maildir("copy".into())]
        );

        let delivery = plan
            .resume_buffered(continuation, &Message::from_bytes(raw.to_vec()))
            .unwrap();
        assert_eq!(
            destinations(&delivery),
            [
                Destination::Maildir("copy".into()),
                Destination::Maildir("body".into())
            ]
        );
        assert!(delivery.original_delivered());
    }

    #[test]
    fn size_only_continuation_can_resume_without_buffered_body() {
        let plan = compile(":0\n* < 100\nmaildir:small\n");
        let raw = b"Subject: test\n\nbody";
        let HeaderEvaluation::NeedsMessage(continuation) = plan.evaluate_headers(&head(raw)) else {
            panic!("expected deferred evaluation");
        };
        assert!(!continuation.requirements().needs_body_contents);

        let mut reader = Cursor::new(raw);
        let head = Message::read_headers(&mut reader, MessageLimits::default()).unwrap();
        let streamed = head.stream_to(&mut reader, &mut Vec::new()).unwrap();
        let delivery = plan.resume_streamed(continuation, &streamed).unwrap();
        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("small".into())]
        );
    }

    #[test]
    fn nested_deferred_recipe_records_a_bounded_tree_path() {
        let plan = compile(":0\n* ^List-Id: wanted$\n{\n:0 B\n* needle\nmaildir:nested\n}\n");
        let HeaderEvaluation::NeedsMessage(continuation) =
            plan.evaluate_headers(&head(b"List-Id: wanted\n\nneedle"))
        else {
            panic!("expected nested body condition to defer");
        };

        assert_eq!(continuation.frames.len(), 2);
        assert!(continuation.requirements().needs_body_contents);
        let delivery = plan
            .resume_buffered(
                continuation,
                &Message::from_bytes(b"List-Id: wanted\n\nneedle".to_vec()),
            )
            .unwrap();
        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("nested".into())]
        );
    }

    #[test]
    fn resume_does_not_repeat_the_header_prefix_trace() {
        let plan = compile("BOX=copy\n:0 c\nmaildir:$BOX\n:0 B\n* needle\nmaildir:body\n");
        let raw = b"Subject: test\n\nneedle";
        let mut runtime = RuntimeVariables::default();
        let mut trace = MemoryTrace::default();
        let HeaderEvaluation::NeedsMessage(continuation) =
            plan.evaluate_headers_with_trace(&head(raw), &mut runtime, &mut trace)
        else {
            panic!("expected body condition to defer");
        };

        plan.resume_mapped_with_trace(
            continuation,
            raw,
            b"Subject: test\n\n".len(),
            &mut runtime,
            &mut trace,
        )
        .unwrap();

        assert_eq!(
            trace
                .events()
                .iter()
                .filter(|event| matches!(event, TraceEvent::VariableAssigned { line: Some(1), .. }))
                .count(),
            1
        );
        assert_eq!(
            trace
                .events()
                .iter()
                .filter(|event| matches!(
                    event,
                    TraceEvent::RecipeEvaluated {
                        line: 2,
                        decision: RecipeDecision::Selected,
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn delivers_first_matching_recipe() {
        let (outcome, recorder) = evaluate_config(
            ":0\n* ^Subject: wanted$\nmaildir:wanted\n\n:0\nmaildir:fallback\n",
            b"Subject: wanted\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
        assert_eq!(
            recorder.destinations,
            [Destination::Maildir("wanted".into())]
        );
    }

    #[test]
    fn defaults_to_case_insensitive_header_matching() {
        let (outcome, _) = evaluate_config(
            ":0\n* ^subject: WANTED$\nmaildir:wanted\n",
            b"Subject: wanted\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn header_regex_uses_normalized_continuations() {
        let (outcome, _) = evaluate_config(
            ":0\n* Subject: alpha  beta\nmaildir:wanted\n",
            b"Subject: alpha\n beta\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn message_regex_can_cross_a_normalized_header_body_boundary() {
        let (outcome, _) = evaluate_config(
            ":0\n* HB ?? beta\\n\\nbody\nmaildir:wanted\n",
            b"Subject: alpha\n beta\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn body_flag_limits_regex_to_body() {
        let (outcome, _) = evaluate_config(
            ":0 B\n* ^needle$\nmaildir:wanted\n",
            b"Subject: no\n\nneedle\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn combines_conditions_with_and_and_supports_negation() {
        let (outcome, _) = evaluate_config(
            ":0\n* ^Subject: wanted$\n* ! ^From: blocked@\nmaildir:wanted\n",
            b"From: allowed@example.org\nSubject: wanted\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn supports_size_conditions() {
        let (outcome, _) = evaluate_config(
            ":0\n* > 10\n* < 100\nmaildir:wanted\n",
            b"Subject: test\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn copy_recipe_continues_to_final_delivery() {
        let (outcome, recorder) = evaluate_config(
            ":0 c\nmaildir:copy\n\n:0\nmbox:final\n",
            b"Subject: test\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 2 });
        assert_eq!(
            recorder.destinations,
            [
                Destination::Maildir("copy".into()),
                Destination::Mbox("final".into())
            ]
        );
    }

    #[test]
    fn reports_copy_only_as_undelivered_original() {
        let (outcome, _) = evaluate_config(":0 c\nmaildir:copy\n", b"Subject: test\n\nbody\n");

        assert_eq!(outcome, Outcome::Undelivered { copies: 1 });
    }

    #[test]
    fn nested_final_delivery_stops_the_parent_sequence() {
        let (outcome, recorder) = evaluate_config(
            ":0\n{\n:0\nmaildir:nested\n}\n:0\nmaildir:unreachable\n",
            b"Subject: test\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
        assert_eq!(
            recorder.destinations,
            [Destination::Maildir("nested".into())]
        );
    }

    #[test]
    fn successful_block_action_enables_lowercase_chain() {
        let (outcome, recorder) = evaluate_config(
            ":0\n{\n:0 c\nmaildir:copy\n}\n:0 a\nmaildir:final\n",
            b"Subject: test\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 2 });
        assert_eq!(
            recorder.destinations,
            [
                Destination::Maildir("copy".into()),
                Destination::Maildir("final".into())
            ]
        );
    }

    #[test]
    fn complete_plan_uses_successful_block_for_lowercase_chain() {
        let plan = compile(":0\n{\n:0 c\nmaildir:copy\n}\n:0 a\nmaildir:final\n");
        let delivery = plan
            .evaluate_full(&Message::from_bytes(b"Subject: test\n\nbody\n".to_vec()))
            .unwrap();

        assert_eq!(
            destinations(&delivery),
            [
                Destination::Maildir("copy".into()),
                Destination::Maildir("final".into())
            ]
        );
        assert!(delivery.original_delivered());
    }
}
