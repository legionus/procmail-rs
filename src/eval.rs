// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use regex::bytes::Regex;

use crate::config::{
    ConditionInput, ConditionKind, Config, ContinuationMode, ControlFlow, Destination, Recipe,
    RecipeAction, Statement,
};
use crate::message::{Message, MessageHead, StreamedMessage};
use crate::runtime::RuntimeVariables;
use crate::trace::{
    ConditionKind as TraceConditionKind, NoTrace, RecipeDecision, TraceEvent, TraceName, TraceSink,
    TraceValue, VariableSource as TraceVariableSource,
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
}

#[derive(Debug)]
struct CompiledSequence {
    recipes: Vec<CompiledNode>,
}

#[derive(Debug)]
struct CompiledNode {
    line: usize,
    assignments: Vec<CompiledAssignment>,
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
    Block(CompiledSequence),
}

#[derive(Debug, Clone)]
struct CompiledAssignment {
    name: String,
    value: String,
    line: Option<usize>,
    source: TraceVariableSource,
}

#[derive(Debug, Clone)]
struct CompiledCondition {
    line: usize,
    negated: bool,
    kind: CompiledConditionKind,
}

#[derive(Debug, Clone)]
enum CompiledConditionKind {
    HeaderRegex(Regex),
    BodyRegex(Regex),
    MessageRegex(Regex),
    SmallerThan(usize),
    LargerThan(usize),
}

#[derive(Debug)]
struct SequenceExecution {
    deliveries: usize,
    original_delivered: bool,
    pending_error: Option<EvalError>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PlanningExecution {
    destinations: Vec<Destination>,
    after_error: Vec<bool>,
    copies: Vec<bool>,
    original_delivered: bool,
}

#[derive(Debug, Default)]
struct HeaderPlanning {
    execution: PlanningExecution,
    frames: Vec<ContinuationFrame>,
    requirements: InputRequirements,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderControl {
    Continue,
    Stop,
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
    SmallerThan,
    LargerThan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationKind {
    Maildir,
    Mbox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPlan {
    destinations: Vec<Destination>,
    after_error: Vec<bool>,
    copies: Vec<bool>,
    original_delivered: bool,
}

impl DeliveryPlan {
    pub fn destinations(&self) -> &[Destination] {
        &self.destinations
    }

    pub fn original_delivered(&self) -> bool {
        self.original_delivered
    }

    pub fn runs_after_previous_error(&self, index: usize) -> bool {
        self.after_error.get(index).copied().unwrap_or(false)
    }

    pub fn destination_is_copy(&self, index: usize) -> bool {
        self.copies.get(index).copied().unwrap_or(false)
    }

    pub fn has_error_fallback(&self) -> bool {
        self.after_error.iter().any(|enabled| *enabled)
    }

    pub fn copies(&self) -> usize {
        self.destinations
            .len()
            .saturating_sub(usize::from(self.original_delivered))
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
    execution: PlanningExecution,
    runtime: RuntimeVariables,
    requirements: InputRequirements,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContinuationFrame {
    recipe_index: usize,
    state: SequenceState,
    condition_results: Vec<Option<bool>>,
}

impl Continuation {
    pub fn requirements(&self) -> InputRequirements {
        self.requirements
    }

    pub fn pending_destinations(&self) -> &[Destination] {
        &self.execution.destinations
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
    Expansion(crate::config::ExpansionError),
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
            Self::Expansion(error) => write!(formatter, "cannot resolve destination: {error}"),
            Self::Delivery {
                destination,
                message,
            } => write!(formatter, "cannot deliver to {destination}: {message}"),
        }
    }
}

impl std::error::Error for EvalError {}

impl CompiledSequence {
    fn compile(statements: &[Statement], assignments: &mut Vec<CompiledAssignment>) -> Self {
        let mut recipes = Vec::new();
        for statement in statements {
            match statement {
                Statement::Assignment(assignment) => assignments.push(CompiledAssignment {
                    name: assignment.name.clone(),
                    value: assignment.value.clone(),
                    line: Some(assignment.line),
                    source: TraceVariableSource::RcFile,
                }),
                Statement::Recipe(recipe) => {
                    recipes.push(CompiledNode::compile(recipe, std::mem::take(assignments)));
                }
            }
        }
        Self { recipes }
    }

    fn requirements(&self) -> InputRequirements {
        self.recipes
            .iter()
            .fold(InputRequirements::default(), |requirements, recipe| {
                requirements.union(recipe.requirements())
            })
    }

    fn requires_ordered_delivery(&self) -> bool {
        self.recipes
            .iter()
            .any(CompiledNode::requires_ordered_delivery)
    }

    fn execute(
        &self,
        message: &Message,
        delivery: &mut impl Delivery,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut SequenceExecution,
    ) -> Result<SequenceControl, EvalError> {
        let mut state = SequenceState::default();

        for recipe in &self.recipes {
            apply_assignments(&recipe.assignments, runtime, trace);

            // Control-flow flags inspect only results produced at this block
            // level. Child sequences therefore cannot overwrite the state
            // used by the next sibling recipe.
            let gate = match recipe.control {
                ControlFlow::Independent => true,
                ControlFlow::AfterChainMatch => state.chain_base_matched.unwrap_or(false),
                ControlFlow::AfterPreviousSuccess => {
                    state.previous.is_some_and(|result: RecipeExecution| {
                        result.conditions_matched && result.action == ActionExecution::Succeeded
                    })
                }
                ControlFlow::Else => state
                    .previous
                    .is_none_or(|result: RecipeExecution| !result.else_handled),
                ControlFlow::AfterPreviousError => {
                    state.previous.is_some_and(|result: RecipeExecution| {
                        result.action == ActionExecution::Failed
                    })
                }
            };

            let conditions_matched = gate && recipe.matches(message, trace)?;
            let else_handled = if recipe.control == ControlFlow::Else {
                state.previous.is_some_and(|result| result.else_handled) || conditions_matched
            } else {
                conditions_matched
            };

            let (action, control) = if conditions_matched {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                recipe.execute_action(message, delivery, runtime, trace, execution)?
            } else {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Skipped,
                });
                (ActionExecution::NotAttempted, SequenceControl::Continue)
            };

            let result = RecipeExecution {
                conditions_matched,
                else_handled,
                action,
            };
            state.previous = Some(result);
            if !matches!(
                recipe.control,
                ControlFlow::AfterChainMatch | ControlFlow::AfterPreviousSuccess
            ) {
                state.chain_base_matched = Some(conditions_matched);
            }
            if control == SequenceControl::Stop {
                return Ok(SequenceControl::Stop);
            }
        }

        Ok(SequenceControl::Continue)
    }

    fn plan_complete(
        &self,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut PlanningExecution,
    ) -> Result<SequenceControl, EvalError> {
        self.plan_complete_from(
            0,
            SequenceState::default(),
            message,
            runtime,
            trace,
            execution,
        )
    }

    fn plan_complete_from(
        &self,
        start: usize,
        mut state: SequenceState,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut PlanningExecution,
    ) -> Result<SequenceControl, EvalError> {
        for (index, recipe) in self.recipes.iter().enumerate().skip(start) {
            apply_assignments(&recipe.assignments, runtime, trace);
            let conditions_matched =
                recipe.planning_gate(state) && recipe.matches_complete(message, trace)?;
            let else_handled = recipe.else_handled(state, conditions_matched);
            let has_error_handler = self.has_error_handler(index);

            // Planning retains actions whose final selection depends on a
            // preceding delivery. Ordered publication later resolves a/a and
            // e from the actual result without discarding their destinations.
            let control = if conditions_matched {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Selected,
                });
                recipe.plan_action(message, runtime, trace, execution, has_error_handler)?
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
            if control == SequenceControl::Stop {
                return Ok(SequenceControl::Stop);
            }
        }

        Ok(SequenceControl::Continue)
    }

    fn plan_headers(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        planning: &mut HeaderPlanning,
        following: InputRequirements,
    ) -> Result<HeaderControl, EvalError> {
        let mut state = SequenceState::default();

        for (index, recipe) in self.recipes.iter().enumerate() {
            apply_assignments(&recipe.assignments, runtime, trace);
            let gate = recipe.planning_gate(state);
            let (matched, condition_results) = if gate {
                recipe.matches_headers(head, trace)
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
                        });
                        let child_following = self.requirements_from(index + 1).union(following);
                        let child = children.plan_headers(
                            head,
                            runtime,
                            trace,
                            planning,
                            child_following,
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
            if control == HeaderControl::Stop {
                return Ok(control);
            }
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
            let assignment_count = inherited_assignments + recipe.assignments.len();
            match &recipe.action {
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
        execution: &mut PlanningExecution,
    ) -> Result<SequenceControl, EvalError> {
        let frame = frames.get(depth).ok_or(EvalError::BodyWasNotBuffered)?;
        let recipe = self
            .recipes
            .get(frame.recipe_index)
            .ok_or(EvalError::BodyWasNotBuffered)?;
        let mut state = frame.state;

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
            )?;
            (true, control)
        } else {
            let conditions_matched = recipe.planning_gate(state)
                && recipe.matches_resumed(message, &frame.condition_results, trace)?;
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

        if control == SequenceControl::Stop {
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
        )
    }
}

impl From<SequenceControl> for HeaderControl {
    fn from(control: SequenceControl) -> Self {
        match control {
            SequenceControl::Continue => Self::Continue,
            SequenceControl::Stop => Self::Stop,
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
    fn compile(recipe: &Recipe, assignments: Vec<CompiledAssignment>) -> Self {
        let conditions = compile_conditions(recipe);
        let action = match &recipe.action {
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
            assignments,
            control: recipe.options.control,
            conditions,
            action,
        }
    }

    fn requirements(&self) -> InputRequirements {
        let action = match &self.action {
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
        let action_requires_order = match &self.action {
            CompiledAction::Deliver {
                destination,
                continuation,
            } => {
                destination.needs_runtime_variables()
                    || matches!(destination, Destination::Mbox(_))
                    || *continuation == ContinuationMode::BranchBlock
            }
            CompiledAction::Block(sequence) => sequence.requires_ordered_delivery(),
        };
        action_requires_order
            || matches!(
                self.control,
                ControlFlow::AfterPreviousSuccess | ControlFlow::AfterPreviousError
            )
    }

    fn matches(&self, message: &Message, trace: &mut impl TraceSink) -> Result<bool, EvalError> {
        for (index, condition) in self.conditions.iter().enumerate() {
            let matched = condition.matches_complete(CompleteMessage::Buffered(message))?;
            condition.trace_result(self.line, index, PartialMatch::from_bool(matched), trace);
            if !matched {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn matches_complete(
        &self,
        message: CompleteMessage<'_>,
        trace: &mut impl TraceSink,
    ) -> Result<bool, EvalError> {
        for (index, condition) in self.conditions.iter().enumerate() {
            let matched = condition.matches_complete(message)?;
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
        trace: &mut impl TraceSink,
    ) -> (PartialMatch, Vec<Option<bool>>) {
        let mut result = PartialMatch::True;
        let mut condition_results = Vec::with_capacity(self.conditions.len());
        for (index, condition) in self.conditions.iter().enumerate() {
            let matched = condition.matches_headers(head);
            if matched != PartialMatch::Deferred {
                condition.trace_result(self.line, index, matched, trace);
            }
            match matched {
                PartialMatch::False => {
                    condition_results.push(Some(false));
                    return (PartialMatch::False, condition_results);
                }
                PartialMatch::Deferred => {
                    condition_results.push(None);
                    result = PartialMatch::Deferred;
                }
                PartialMatch::True => condition_results.push(Some(true)),
            }
        }
        (result, condition_results)
    }

    fn matches_resumed(
        &self,
        message: CompleteMessage<'_>,
        header_results: &[Option<bool>],
        trace: &mut impl TraceSink,
    ) -> Result<bool, EvalError> {
        for (index, condition) in self.conditions.iter().enumerate() {
            let matched = match header_results.get(index).copied().flatten() {
                Some(matched) => matched,
                None => {
                    let matched = condition.matches_complete(message)?;
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
        delivery: &mut impl Delivery,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut SequenceExecution,
    ) -> Result<(ActionExecution, SequenceControl), EvalError> {
        match &self.action {
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
                let control = children.execute(message, delivery, runtime, trace, execution)?;
                let action = if execution.pending_error.is_some() {
                    ActionExecution::Failed
                } else {
                    ActionExecution::Succeeded
                };
                Ok((action, control))
            }
        }
    }

    fn plan_action(
        &self,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
        execution: &mut PlanningExecution,
        has_error_handler: bool,
    ) -> Result<SequenceControl, EvalError> {
        match &self.action {
            CompiledAction::Deliver { .. } => {
                self.plan_delivery(runtime, execution, has_error_handler)
            }
            CompiledAction::Block(children) => {
                children.plan_complete(message, runtime, trace, execution)
            }
        }
    }

    fn plan_delivery(
        &self,
        runtime: &RuntimeVariables,
        execution: &mut PlanningExecution,
        has_error_handler: bool,
    ) -> Result<SequenceControl, EvalError> {
        let CompiledAction::Deliver {
            destination,
            continuation,
        } = &self.action
        else {
            return Ok(SequenceControl::Continue);
        };
        execution.destinations.push(
            destination
                .bind_with(|name| runtime.get(name).map(str::to_owned))
                .map_err(EvalError::Expansion)?,
        );
        execution
            .after_error
            .push(self.control == ControlFlow::AfterPreviousError);
        let copy = *continuation == ContinuationMode::Continue;
        execution.copies.push(copy);
        execution.original_delivered |= !copy;
        if copy || has_error_handler {
            Ok(SequenceControl::Continue)
        } else {
            Ok(SequenceControl::Stop)
        }
    }
}

fn apply_assignments(
    assignments: &[CompiledAssignment],
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) {
    for assignment in assignments {
        runtime.set(assignment.name.clone(), assignment.value.clone());
        if let Ok(name) = TraceName::new(&assignment.name) {
            trace.record(TraceEvent::VariableAssigned {
                line: assignment.line,
                name,
                source: assignment.source,
                value: trace
                    .detail()
                    .includes_variable_values()
                    .then(|| TraceValue::new(assignment.value.as_bytes())),
            });
        }
    }
}

impl ExecutionPlan {
    pub fn compile(config: &Config) -> Self {
        let initial_assignments = config
            .initial_variables()
            .iter()
            .map(|(name, value)| CompiledAssignment {
                name: name.clone(),
                value: value.clone(),
                line: None,
                source: TraceVariableSource::CommandLine,
            })
            .collect::<Vec<_>>();
        let root = CompiledSequence::compile(&config.statements, &mut initial_assignments.clone());
        let requires_ordered_delivery = root.requires_ordered_delivery();

        Self {
            root,
            requires_ordered_delivery,
        }
    }

    pub fn requirements(&self) -> InputRequirements {
        if self.requires_ordered_delivery {
            self.root.requirements().union(InputRequirements {
                needs_end_of_message: true,
                ..InputRequirements::default()
            })
        } else {
            self.root.requirements()
        }
    }

    pub fn requires_ordered_delivery(&self) -> bool {
        self.requires_ordered_delivery
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
        let mut planning = HeaderPlanning::default();
        match self.root.plan_headers(
            head,
            runtime,
            trace,
            &mut planning,
            InputRequirements::default(),
        ) {
            Ok(HeaderControl::Deferred) => HeaderEvaluation::NeedsMessage(Continuation {
                frames: planning.frames,
                execution: planning.execution,
                runtime: runtime.clone(),
                requirements: planning.requirements,
            }),
            Ok(HeaderControl::Continue | HeaderControl::Stop) => {
                HeaderEvaluation::Decided(DeliveryPlan {
                    destinations: planning.execution.destinations,
                    after_error: planning.execution.after_error,
                    copies: planning.execution.copies,
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
        self.resume_tree(
            continuation,
            CompleteMessage::Buffered(message),
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
        if header_len > raw.len() {
            return Err(EvalError::BodyWasNotBuffered);
        }
        self.resume_tree(
            continuation,
            CompleteMessage::Mapped { raw, header_len },
            runtime,
            trace,
        )
    }

    pub fn evaluate_full(&self, message: &Message) -> Result<DeliveryPlan, EvalError> {
        let mut execution = PlanningExecution::default();
        self.root.plan_complete(
            CompleteMessage::Buffered(message),
            &mut RuntimeVariables::default(),
            &mut NoTrace,
            &mut execution,
        )?;
        Ok(DeliveryPlan {
            destinations: execution.destinations,
            after_error: execution.after_error,
            copies: execution.copies,
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
        if continuation.frames.is_empty() {
            return Err(EvalError::BodyWasNotBuffered);
        }
        *runtime = continuation.runtime;
        let mut execution = continuation.execution;

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
        )?;
        Ok(DeliveryPlan {
            destinations: execution.destinations,
            after_error: execution.after_error,
            copies: execution.copies,
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
        };
        conditions.push(CompiledCondition {
            line: condition.line,
            negated: condition.negated,
            kind,
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
            CompiledConditionKind::SmallerThan(_) | CompiledConditionKind::LargerThan(_) => {
                InputRequirements {
                    needs_end_of_message: true,
                    ..InputRequirements::default()
                }
            }
        }
    }

    fn matches_headers(&self, head: &MessageHead) -> PartialMatch {
        let matched = match &self.kind {
            CompiledConditionKind::HeaderRegex(regex) => {
                return PartialMatch::from_bool(regex.is_match(head.as_bytes()) ^ self.negated);
            }
            CompiledConditionKind::BodyRegex(_) | CompiledConditionKind::MessageRegex(_) => {
                return PartialMatch::Deferred;
            }
            CompiledConditionKind::SmallerThan(size) => {
                if head.len() >= *size {
                    false
                } else {
                    return PartialMatch::Deferred;
                }
            }
            CompiledConditionKind::LargerThan(size) => {
                if head.len() > *size {
                    true
                } else {
                    return PartialMatch::Deferred;
                }
            }
        };
        PartialMatch::from_bool(matched ^ self.negated)
    }

    fn matches_complete(&self, message: CompleteMessage<'_>) -> Result<bool, EvalError> {
        let matched = match &self.kind {
            CompiledConditionKind::HeaderRegex(regex) => regex.is_match(message.header_bytes()),
            CompiledConditionKind::BodyRegex(regex) => {
                regex.is_match(message.body().ok_or(EvalError::BodyWasNotBuffered)?)
            }
            CompiledConditionKind::MessageRegex(regex) => {
                regex.is_match(message.full().ok_or(EvalError::BodyWasNotBuffered)?)
            }
            CompiledConditionKind::SmallerThan(size) => message.len() < *size,
            CompiledConditionKind::LargerThan(size) => message.len() > *size,
        };
        Ok(matched ^ self.negated)
    }
}

#[derive(Debug, Clone, Copy)]
enum CompleteMessage<'a> {
    Buffered(&'a Message),
    Streamed(&'a StreamedMessage),
    Mapped { raw: &'a [u8], header_len: usize },
}

impl<'a> CompleteMessage<'a> {
    fn header_bytes(self) -> &'a [u8] {
        match self {
            Self::Buffered(message) => message.header(),
            Self::Streamed(message) => message.header(),
            Self::Mapped { raw, header_len } => &raw[..header_len],
        }
    }

    fn body(self) -> Option<&'a [u8]> {
        match self {
            Self::Buffered(message) => Some(message.body()),
            Self::Streamed(_) => None,
            Self::Mapped { raw, header_len } => Some(&raw[header_len..]),
        }
    }

    fn full(self) -> Option<&'a [u8]> {
        match self {
            Self::Buffered(message) => Some(message.as_bytes()),
            Self::Streamed(_) => None,
            Self::Mapped { raw, .. } => Some(raw),
        }
    }

    fn len(self) -> usize {
        match self {
            Self::Buffered(message) => message.len(),
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
    let mut execution = SequenceExecution {
        deliveries: 0,
        original_delivered: false,
        pending_error: None,
    };
    plan.root.execute(
        message,
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
            delivery.destinations(),
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
            delivery.destinations(),
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
            selected.destinations(),
            [Destination::Maildir("list".into())]
        );

        let HeaderEvaluation::Decided(skipped) =
            plan.evaluate_headers(&head(b"List-Id: other\nSubject: report\n\nbody"))
        else {
            panic!("expected fallback delivery");
        };
        assert_eq!(
            skipped.destinations(),
            [Destination::Maildir("fallback".into())]
        );
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
            delivery.destinations(),
            [
                Destination::Maildir("copy".into()),
                Destination::Maildir("final".into())
            ]
        );
        assert!(delivery.original_delivered());
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
            delivery.destinations(),
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
        assert!(unmatched.destinations().is_empty());
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
        assert_eq!(selected.destinations().len(), 3);

        let skipped = plan
            .evaluate_full(&Message::from_bytes(b"Subject: wanted\n\nbody".to_vec()))
            .unwrap();
        assert_eq!(
            skipped.destinations(),
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
    fn chain_without_a_preceding_recipe_never_executes() {
        for flag in ['A', 'a'] {
            let plan = compile(&format!(":0 {flag}\nmaildir:unreachable\n"));
            let HeaderEvaluation::Decided(delivery) =
                plan.evaluate_headers(&head(b"Subject: test\n\nbody"))
            else {
                panic!("expected a complete decision");
            };
            assert!(delivery.destinations().is_empty());
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
        assert_eq!(delivery.destinations().len(), 66);
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
            assert_eq!(delivery.destinations()[0].path(), expected);
            assert_eq!(delivery.destinations().len(), 1);
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
        assert_eq!(delivery.destinations()[0].path(), "fallback");
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
            delivery.destinations(),
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
            continuation.pending_destinations(),
            [Destination::Maildir("copy".into())]
        );

        let delivery = plan
            .resume_buffered(continuation, &Message::from_bytes(raw.to_vec()))
            .unwrap();
        assert_eq!(
            delivery.destinations(),
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
            delivery.destinations(),
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
            delivery.destinations(),
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
            delivery.destinations(),
            [
                Destination::Maildir("copy".into()),
                Destination::Maildir("final".into())
            ]
        );
        assert!(delivery.original_delivered());
    }
}
