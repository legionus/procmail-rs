// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io::Cursor;

use procmail_rs::config;
use procmail_rs::eval::{ExecutionPlan, HeaderEvaluation};
use procmail_rs::limits::MessageLimits;
use procmail_rs::message::Message;
use procmail_rs::runtime::RuntimeVariables;
use procmail_rs::trace::{ConditionKind, MemoryTrace, RecipeDecision, TraceEvent, VariableSource};

const DIRECTORY: &str = "tests/fixtures/differential_trace/header_fallback";

#[test]
fn header_fallback_matches_reference_procmail_decisions() {
    let source = include_str!("fixtures/differential_trace/header_fallback/procmail-rs.rc");
    let message = include_bytes!("fixtures/differential_trace/header_fallback/message.eml");
    let expected = include_str!("fixtures/differential_trace/header_fallback/expected.events");
    let config = config::parse(source).unwrap().expand().unwrap();
    let plan = ExecutionPlan::compile(&config);
    let limits = MessageLimits::from_config(&config).unwrap();
    let head = Message::read_headers(&mut Cursor::new(message), limits).unwrap();
    let mut runtime = RuntimeVariables::default();
    let mut trace = MemoryTrace::default();

    assert!(matches!(
        plan.evaluate_headers_with_trace(&head, &mut runtime, &mut trace),
        HeaderEvaluation::Decided(_)
    ));

    // Keep the shared format limited to facts available from both traces.
    // Source lines and patterns remain covered by native trace tests because
    // procmail's text log does not expose equivalent structured fields.
    let actual = trace
        .events()
        .iter()
        .filter_map(common_event)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(actual, expected, "fixture directory: {DIRECTORY}");
}

fn common_event(event: &TraceEvent) -> Option<String> {
    match event {
        TraceEvent::VariableAssigned {
            name,
            source: VariableSource::RcFile,
            ..
        } if name.as_str() == "BOX" => Some(format!("assignment {}", name.as_str())),
        TraceEvent::ConditionEvaluated { kind, matched, .. } => {
            Some(format!("condition {} {matched}", condition_kind(*kind)))
        }
        TraceEvent::RecipeEvaluated { decision, .. } => {
            Some(format!("recipe {}", recipe_decision(*decision)))
        }
        _ => None,
    }
}

fn condition_kind(kind: ConditionKind) -> &'static str {
    match kind {
        ConditionKind::HeaderRegex => "header-regex",
        ConditionKind::BodyRegex => "body-regex",
        ConditionKind::MessageRegex => "message-regex",
        ConditionKind::VariableRegex => "variable-regex",
        ConditionKind::Program => "program",
        ConditionKind::SmallerThan => "smaller-than",
        ConditionKind::LargerThan => "larger-than",
    }
}

fn recipe_decision(decision: RecipeDecision) -> &'static str {
    match decision {
        RecipeDecision::Selected => "selected",
        RecipeDecision::Deferred => "deferred",
        RecipeDecision::Skipped => "skipped",
    }
}
