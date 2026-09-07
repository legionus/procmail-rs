// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

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

trait Delivery {
    fn deliver(&mut self, destination: &Destination, message: &[u8]) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Delivered { deliveries: usize },
    Undelivered { copies: usize },
}

trait HeaderTestExt {
    fn evaluate_headers_with_runtime(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
    ) -> HeaderEvaluation;

    fn resume_mapped_with_runtime(
        &self,
        continuation: Continuation,
        raw: &[u8],
        header_len: usize,
        runtime: &mut RuntimeVariables,
    ) -> Result<DeliveryPlan, EvalError>;
}

impl HeaderTestExt for ExecutionPlan {
    fn evaluate_headers_with_runtime(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
    ) -> HeaderEvaluation {
        self.evaluate_headers_with_trace(head, runtime, &mut NoTrace)
    }

    fn resume_mapped_with_runtime(
        &self,
        continuation: Continuation,
        raw: &[u8],
        header_len: usize,
        runtime: &mut RuntimeVariables,
    ) -> Result<DeliveryPlan, EvalError> {
        self.resume_with_trace(
            continuation,
            MappedMessageInput::new(raw, header_len, None),
            runtime,
            &mut NoTrace,
        )
    }
}

impl Delivery for FailingRecorder {
    fn deliver(&mut self, destination: &Destination, _: &[u8]) -> Result<(), String> {
        self.attempted.push(destination.path().to_owned());
        if self.fail_paths.contains(&destination.path()) {
            Err("injected delivery failure".to_owned())
        } else {
            Ok(())
        }
    }
}

impl Delivery for Recorder {
    fn deliver(&mut self, destination: &Destination, _: &[u8]) -> Result<(), String> {
        self.destinations.push(destination.clone());
        Ok(())
    }
}

fn evaluate(
    config: &Config,
    message: &Message,
    delivery: &mut impl Delivery,
) -> Result<Outcome, EvalError> {
    let plan = ExecutionPlan::compile(config, None);
    let prepared_matching = PreparedMatchingMessage::new(message, plan.needs_message_contents());
    let matching = Some(prepared_matching.views(message));
    let mut runtime = RuntimeVariables::default();
    let mut trace = NoTrace;
    let mut deliver = |destination: &Destination,
                       bytes: &[u8],
                       _: OutputEnding,
                       _: Option<&str>,
                       _: &mut RuntimeVariables,
                       _: &mut NoTrace| {
        delivery.deliver(destination, bytes).map_err(|message| {
            DeliveryAttemptError::Recoverable(EvalError::Delivery {
                destination: destination.path().to_owned(),
                message,
            })
        })
    };
    let services = ExecutionServices::new(&mut deliver, &mut trace);
    let outcome = plan
        .execute_ordered(
            MappedMessageInput::new(message.as_bytes(), message.header().len(), matching),
            &mut runtime,
            services,
        )
        .map_err(|error| match error {
            OrderedExecutionError::Evaluation(error) | OrderedExecutionError::Delivery(error) => {
                error
            }
        })?;

    if outcome.original_delivered() {
        Ok(Outcome::Delivered {
            deliveries: outcome.published(),
        })
    } else {
        Ok(Outcome::Undelivered {
            copies: outcome.published(),
        })
    }
}

fn compile(source: &str) -> ExecutionPlan {
    ExecutionPlan::compile(&config::parse(source).unwrap(), None)
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

include!("conditions_and_captures.rs");
include!("recipe_control.rs");
include!("ordered_execution.rs");
include!("external_commands.rs");
include!("plan_explanations.rs");
