// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeSet;
use std::fs;
use std::io::Cursor;
use std::path::Path;

use procmail_rs::config::{self, Destination, OutputEnding, PipeAction, RecipeOptions};
use procmail_rs::eval::{
    CapturedCommand, CompletionState, DeliveryAttemptError, DeliveryOutcome, ExecutionPlan,
    ExternalActionInput, FinalMessage, MappedMessageInput, OrderedExecutionHost,
    PreparedMatchingMessage, RecipeLockGuard,
};
use procmail_rs::limits::MessageLimits;
use procmail_rs::message::Message;
use procmail_rs::runtime::RuntimeVariables;
use procmail_rs::trace::NoTrace;

const FIXTURES: &str = "tests/fixtures/differential_eval";

#[derive(Default)]
struct Recorder {
    selected: Vec<String>,
    failures: BTreeSet<String>,
}

struct RecorderHost<'a> {
    recorder: &'a mut Recorder,
    trace: NoTrace,
}

impl OrderedExecutionHost for RecorderHost<'_> {
    type Error = String;
    type Trace = NoTrace;

    fn trace(&mut self) -> &mut Self::Trace {
        &mut self.trace
    }

    fn deliver(
        &mut self,
        destination: &Destination,
        _: &[u8],
        _: OutputEnding,
        _: Option<&str>,
        _: &mut RuntimeVariables,
    ) -> Result<(), DeliveryAttemptError<Self::Error>> {
        self.recorder
            .deliver(destination)
            .map_err(DeliveryAttemptError::Recoverable)
    }

    fn external_action(
        &mut self,
        _: &PipeAction,
        _: RecipeOptions,
        _: Option<&str>,
        _: ExternalActionInput<'_>,
        _: &mut RuntimeVariables,
    ) -> Result<Option<Message>, DeliveryAttemptError<Self::Error>> {
        panic!("fixture unexpectedly requested an external action")
    }

    fn capture(
        &mut self,
        _: &str,
        _: &[u8],
        _: OutputEnding,
        _: Option<RecipeOptions>,
        _: usize,
        _: &mut RuntimeVariables,
    ) -> Result<CapturedCommand, DeliveryAttemptError<Self::Error>> {
        panic!("fixture unexpectedly requested command capture")
    }

    fn external_condition(
        &mut self,
        _: &str,
        _: &[u8],
        _: &mut RuntimeVariables,
    ) -> Result<bool, DeliveryAttemptError<Self::Error>> {
        panic!("fixture unexpectedly requested an external condition")
    }

    fn replace_global_lock(
        &mut self,
        _: &str,
        _: &mut RuntimeVariables,
    ) -> Result<(), Self::Error> {
        panic!("fixture unexpectedly requested a global lock")
    }

    fn acquire_local_lock(
        &mut self,
        _: &str,
        _: &mut RuntimeVariables,
    ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<Self::Error>> {
        panic!("fixture unexpectedly requested a local lock")
    }

    fn complete(
        &mut self,
        _: FinalMessage<'_>,
        _: &mut RuntimeVariables,
        _: CompletionState<'_, Self::Error>,
    ) {
    }
}

impl Recorder {
    fn deliver(&mut self, destination: &Destination) -> Result<(), String> {
        let name = Path::new(destination.path())
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "fixture destination has no UTF-8 basename".to_owned())?;
        if self.failures.contains(name) {
            return Err("injected reference action failure".to_owned());
        }
        self.selected.push(name.to_owned());
        Ok(())
    }
}

#[test]
fn supported_milestone_7_behavior_matches_reference_procmail() {
    for case in fixture_cases() {
        let directory = Path::new(FIXTURES).join(&case);
        let source = fs::read_to_string(directory.join("procmail-rs.rc")).unwrap();
        let message = fs::read(directory.join("message.eml")).unwrap();
        let expected = fs::read_to_string(directory.join("expected.destinations"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let expected_outcome = fs::read_to_string(directory.join("expected.outcome")).unwrap();
        let config = config::parse(&source).unwrap().expand().unwrap();
        let failures = fs::read_to_string(directory.join("fail.destinations"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        let mut recorder = Recorder {
            failures,
            ..Recorder::default()
        };

        let message =
            Message::read_from(&mut Cursor::new(message), MessageLimits::default()).unwrap();
        let outcome = evaluate(&config, &message, &mut recorder);

        assert_eq!(recorder.selected, expected, "fixture: {case}");
        assert_eq!(render_outcome(outcome), expected_outcome, "fixture: {case}");
    }
}

fn evaluate(
    config: &config::Config,
    message: &Message,
    recorder: &mut Recorder,
) -> DeliveryOutcome {
    let plan = ExecutionPlan::compile(config);
    let mut runtime = RuntimeVariables::default();
    let prepared_matching = PreparedMatchingMessage::new(message, plan.needs_message_contents());
    let matching = Some(prepared_matching.views(message));
    let host = RecorderHost {
        recorder,
        trace: NoTrace,
    };
    plan.execute_ordered(
        MappedMessageInput::new(message.as_bytes(), message.header().len(), matching),
        &mut runtime,
        host,
    )
    .unwrap()
}

fn render_outcome(outcome: DeliveryOutcome) -> String {
    if outcome.original_delivered() {
        format!("delivered {}\n", outcome.published())
    } else {
        format!("undelivered {}\n", outcome.published())
    }
}

fn fixture_cases() -> Vec<String> {
    let mut cases = fs::read_dir(FIXTURES)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            entry.file_type().unwrap().is_dir().then(|| {
                entry
                    .file_name()
                    .into_string()
                    .expect("fixture directory name must be UTF-8")
            })
        })
        .collect::<Vec<_>>();
    cases.sort();
    assert!(!cases.is_empty());
    cases
}
