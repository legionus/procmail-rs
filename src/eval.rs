// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use regex::bytes::Regex;

use crate::config::{ConditionKind, Config, Destination, Recipe, Statement};
use crate::message::{Message, MessageHead, StreamedMessage};
use crate::runtime::RuntimeVariables;
use crate::trace::{
    ConditionKind as TraceConditionKind, NoTrace, RecipeDecision, TraceEvent, TraceName, TraceSink,
    VariableSource as TraceVariableSource,
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
    recipes: Vec<CompiledRecipe>,
    suffix_requirements: Vec<InputRequirements>,
    requirements: InputRequirements,
    requires_ordered_delivery: bool,
}

#[derive(Debug)]
struct CompiledRecipe {
    line: usize,
    assignments: Vec<CompiledAssignment>,
    conditions: Vec<CompiledCondition>,
    destination: Destination,
    copy: bool,
}

#[derive(Debug)]
struct CompiledAssignment {
    name: String,
    value: String,
    line: Option<usize>,
    source: TraceVariableSource,
}

#[derive(Debug)]
struct CompiledCondition {
    negated: bool,
    kind: CompiledConditionKind,
}

#[derive(Debug)]
enum CompiledConditionKind {
    HeaderRegex(Regex),
    BodyRegex(Regex),
    MessageRegex(Regex),
    SmallerThan(usize),
    LargerThan(usize),
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
    original_delivered: bool,
}

impl DeliveryPlan {
    pub fn destinations(&self) -> &[Destination] {
        &self.destinations
    }

    pub fn original_delivered(&self) -> bool {
        self.original_delivered
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
    recipe_index: usize,
    destinations: Vec<Destination>,
    requirements: InputRequirements,
}

impl Continuation {
    pub fn requirements(&self) -> InputRequirements {
        self.requirements
    }

    pub fn pending_destinations(&self) -> &[Destination] {
        &self.destinations
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

impl ExecutionPlan {
    pub fn compile(config: &Config) -> Self {
        let mut recipes = Vec::new();
        let mut assignments = config
            .initial_variables()
            .iter()
            .map(|(name, value)| CompiledAssignment {
                name: name.clone(),
                value: value.clone(),
                line: None,
                source: TraceVariableSource::CommandLine,
            })
            .collect::<Vec<_>>();
        for statement in &config.statements {
            match statement {
                Statement::Assignment(assignment) => {
                    assignments.push(CompiledAssignment {
                        name: assignment.name.clone(),
                        value: assignment.value.clone(),
                        line: Some(assignment.line),
                        source: TraceVariableSource::RcFile,
                    });
                }
                Statement::Recipe(recipe) => {
                    recipes.push(CompiledRecipe::compile(
                        recipe,
                        std::mem::take(&mut assignments),
                    ));
                }
            }
        }

        let mut suffix_requirements = vec![InputRequirements::default(); recipes.len() + 1];
        for index in (0..recipes.len()).rev() {
            suffix_requirements[index] = recipes[index]
                .requirements()
                .union(suffix_requirements[index + 1]);
        }
        let requirements = suffix_requirements[0];
        let requires_ordered_delivery = recipes
            .iter()
            .any(|recipe| recipe.destination.needs_runtime_variables());

        Self {
            recipes,
            suffix_requirements,
            requirements,
            requires_ordered_delivery,
        }
    }

    pub fn requirements(&self) -> InputRequirements {
        if self.requires_ordered_delivery {
            self.requirements.union(InputRequirements {
                needs_end_of_message: true,
                ..InputRequirements::default()
            })
        } else {
            self.requirements
        }
    }

    pub fn requires_ordered_delivery(&self) -> bool {
        self.requires_ordered_delivery
    }

    pub fn explain(&self) -> PlanExplanation {
        // Explain only execution shape. Values, patterns, thresholds, and
        // paths can contain private configuration data and are unnecessary
        // for deciding which message sections and delivery phases are used.
        let recipes = self.recipes.iter().map(CompiledRecipe::explain).collect();
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
        let mut destinations = Vec::new();

        for (index, recipe) in self.recipes.iter().enumerate() {
            recipe.apply_assignments(runtime, trace);
            match recipe.matches_headers(head, trace) {
                PartialMatch::False => {
                    trace.record(TraceEvent::RecipeEvaluated {
                        line: recipe.line,
                        decision: RecipeDecision::Skipped,
                    });
                    continue;
                }
                PartialMatch::Deferred => {
                    trace.record(TraceEvent::RecipeEvaluated {
                        line: recipe.line,
                        decision: RecipeDecision::Deferred,
                    });
                    return HeaderEvaluation::NeedsMessage(Continuation {
                        recipe_index: index,
                        destinations,
                        requirements: self.suffix_requirements[index],
                    });
                }
                PartialMatch::True => {
                    trace.record(TraceEvent::RecipeEvaluated {
                        line: recipe.line,
                        decision: RecipeDecision::Selected,
                    });
                    let destination = match recipe
                        .destination
                        .bind_with(|name| runtime.get(name).map(str::to_owned))
                    {
                        Ok(destination) => destination,
                        Err(error) => return HeaderEvaluation::Error(EvalError::Expansion(error)),
                    };
                    destinations.push(destination);
                    if !recipe.copy {
                        return HeaderEvaluation::Decided(DeliveryPlan {
                            destinations,
                            original_delivered: true,
                        });
                    }
                }
            }
        }

        HeaderEvaluation::Decided(DeliveryPlan {
            destinations,
            original_delivered: false,
        })
    }

    pub fn resume_buffered(
        &self,
        continuation: Continuation,
        message: &Message,
    ) -> Result<DeliveryPlan, EvalError> {
        self.resume(continuation, CompleteMessage::Buffered(message))
    }

    pub fn resume_streamed(
        &self,
        continuation: Continuation,
        message: &StreamedMessage,
    ) -> Result<DeliveryPlan, EvalError> {
        if continuation.requirements.needs_body_contents {
            return Err(EvalError::BodyWasNotBuffered);
        }
        self.resume(continuation, CompleteMessage::Streamed(message))
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
        self.resume_with_runtime(
            continuation,
            CompleteMessage::Mapped { raw, header_len },
            runtime,
            true,
            trace,
        )
    }

    pub fn evaluate_full(&self, message: &Message) -> Result<DeliveryPlan, EvalError> {
        self.resume_with_runtime(
            Continuation {
                recipe_index: 0,
                destinations: Vec::new(),
                requirements: self.requirements,
            },
            CompleteMessage::Buffered(message),
            &mut RuntimeVariables::default(),
            false,
            &mut NoTrace,
        )
    }

    fn resume(
        &self,
        continuation: Continuation,
        message: CompleteMessage<'_>,
    ) -> Result<DeliveryPlan, EvalError> {
        self.resume_with_runtime(
            continuation,
            message,
            &mut RuntimeVariables::default(),
            true,
            &mut NoTrace,
        )
    }

    fn resume_with_runtime(
        &self,
        continuation: Continuation,
        message: CompleteMessage<'_>,
        runtime: &mut RuntimeVariables,
        first_assignments_applied: bool,
        trace: &mut impl TraceSink,
    ) -> Result<DeliveryPlan, EvalError> {
        let mut destinations = continuation.destinations;

        for (offset, recipe) in self.recipes[continuation.recipe_index..].iter().enumerate() {
            if offset != 0 || !first_assignments_applied {
                recipe.apply_assignments(runtime, trace);
            }
            if !recipe.matches_complete(message, trace)? {
                trace.record(TraceEvent::RecipeEvaluated {
                    line: recipe.line,
                    decision: RecipeDecision::Skipped,
                });
                continue;
            }
            trace.record(TraceEvent::RecipeEvaluated {
                line: recipe.line,
                decision: RecipeDecision::Selected,
            });
            destinations.push(
                recipe
                    .destination
                    .bind_with(|name| runtime.get(name).map(str::to_owned))
                    .map_err(EvalError::Expansion)?,
            );
            if !recipe.copy {
                return Ok(DeliveryPlan {
                    destinations,
                    original_delivered: true,
                });
            }
        }

        Ok(DeliveryPlan {
            destinations,
            original_delivered: false,
        })
    }
}

impl CompiledRecipe {
    fn compile(recipe: &Recipe, assignments: Vec<CompiledAssignment>) -> Self {
        let area = match (recipe.has_flag('H'), recipe.has_flag('B')) {
            (false, true) => RegexArea::Body,
            (true, true) => RegexArea::Message,
            _ => RegexArea::Headers,
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
                negated: condition.negated,
                kind,
            });
        }

        Self {
            line: recipe.line,
            assignments,
            conditions,
            destination: recipe.destination.clone(),
            copy: recipe.has_flag('c'),
        }
    }

    fn apply_assignments(&self, runtime: &mut RuntimeVariables, trace: &mut impl TraceSink) {
        for assignment in &self.assignments {
            runtime.set(assignment.name.clone(), assignment.value.clone());
            if let Ok(name) = TraceName::new(&assignment.name) {
                trace.record(TraceEvent::VariableAssigned {
                    line: assignment.line,
                    name,
                    source: assignment.source,
                });
            }
        }
    }

    fn explain(&self) -> RecipeExplanation {
        let conditions = self
            .conditions
            .iter()
            .map(CompiledCondition::explain)
            .collect();
        let destination = match &self.destination {
            Destination::Maildir(_) => DestinationKind::Maildir,
            Destination::Mbox(_) => DestinationKind::Mbox,
        };
        RecipeExplanation {
            line: self.line,
            assignment_count: self.assignments.len(),
            conditions,
            destination,
            copy: self.copy,
            defers_destination: self.destination.needs_runtime_variables(),
        }
    }

    fn requirements(&self) -> InputRequirements {
        self.conditions
            .iter()
            .fold(InputRequirements::default(), |requirements, condition| {
                requirements.union(condition.requirements())
            })
    }

    fn matches_headers(&self, head: &MessageHead, trace: &mut impl TraceSink) -> PartialMatch {
        let mut deferred = false;
        for (index, condition) in self.conditions.iter().enumerate() {
            let result = condition.matches_headers(head);
            match result {
                PartialMatch::False => {
                    condition.trace_result(self.line, index, result, trace);
                    return PartialMatch::False;
                }
                PartialMatch::Deferred => deferred = true,
                PartialMatch::True => {
                    condition.trace_result(self.line, index, result, trace);
                }
            }
        }
        if deferred || self.destination.needs_runtime_variables() {
            PartialMatch::Deferred
        } else {
            PartialMatch::True
        }
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
    let delivery_plan = plan.evaluate_full(message)?;
    execute_deliveries(&delivery_plan, message, delivery)
}

fn execute_deliveries(
    plan: &DeliveryPlan,
    message: &Message,
    delivery: &mut impl Delivery,
) -> Result<Outcome, EvalError> {
    for destination in &plan.destinations {
        delivery
            .deliver(destination, message)
            .map_err(|error| EvalError::Delivery {
                destination: destination_name(destination).to_owned(),
                message: error,
            })?;
    }

    if plan.original_delivered {
        Ok(Outcome::Delivered {
            deliveries: plan.destinations.len(),
        })
    } else {
        Ok(Outcome::Undelivered {
            copies: plan.destinations.len(),
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
                },
                TraceEvent::ConditionEvaluated {
                    recipe_line: 2,
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
}
