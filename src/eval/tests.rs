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
fn header_edit_updates_following_header_rules_without_buffering_body() {
    let config = config::parse(
        ":0\nheaders {\n set X-State: new\n}\n:0\n* ^X-State: new$\nmaildir:selected\n",
    )
    .unwrap()
    .expand()
    .unwrap();
    let plan = ExecutionPlan::compile(&config);
    let mut head = head(b"X-State: old\n\nbody");
    let mut runtime = RuntimeVariables::default();

    assert!(!plan.requirements().needs_end_of_message);
    let result = plan.evaluate_headers_editing_with_trace(&mut head, &mut runtime, &mut NoTrace);
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected a header-only decision");
    };
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("selected".into())]
    );
    assert_eq!(head.as_bytes(), b"X-State: new\n\n");
}

#[test]
fn ordered_header_edit_updates_later_delivery_bytes() {
    let config = config::parse(
        ":0 B\n* needle\nheaders {\n add X-Body-Matched: yes\n}\n:0\n* ^X-Body-Matched: yes$\nmaildir:selected\n",
    )
    .unwrap()
    .expand()
    .unwrap();
    let plan = ExecutionPlan::compile(&config);
    let raw = b"Subject: test\n\nneedle body";
    let mut runtime = RuntimeVariables::default();
    let mut delivered = Vec::new();

    plan.execute_mapped_ordered_with_trace(
        raw,
        b"Subject: test\n\n".len(),
        &mut runtime,
        &mut NoTrace,
        &mut |destination, message, _, _, _, _| {
            delivered.push((destination.path().to_owned(), message.to_vec()));
            Ok::<_, DeliveryAttemptError<&str>>(())
        },
    )
    .unwrap();

    assert_eq!(
        delivered,
        [(
            "selected".to_owned(),
            b"Subject: test\nX-Body-Matched: yes\n\nneedle body".to_vec(),
        )]
    );
}

#[test]
fn external_action_observes_edited_headers() {
    let config = config::parse(":0\nheaders {\n set X-State: new\n}\n:0 w\n| consume\n")
        .unwrap()
        .expand()
        .unwrap();
    let plan = ExecutionPlan::compile(&config);
    let raw = b"X-State: old\n\nbody";
    let mut runtime = RuntimeVariables::default();
    let mut calls = 0usize;

    plan.execute_mapped_ordered_with_external_trace(
        MappedMessageInput::new(raw, b"X-State: old\n\n".len(), None),
        &mut runtime,
        &mut NoTrace,
        &mut |_, _, _, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(()),
        &mut |_, _, _, input, _, _| {
            calls += 1;
            assert_eq!(input.header(), b"X-State: new\n\n");
            assert_eq!(input.body(), b"body");
            assert_eq!(input.selected(), b"X-State: new\n\nbody");
            Ok::<_, DeliveryAttemptError<&str>>(None)
        },
    )
    .unwrap();

    assert_eq!(calls, 1);
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

    let result = plan.evaluate_headers_with_runtime(&head(b"Subject: test\n\nbody"), &mut runtime);

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
fn rendered_trace_excludes_edited_header_names_and_values() {
    let config = config::parse(
        ":0\nheaders {\n add X-Private-Edited-Name: private-edited-value\n}\n:0\nmaildir:selected\n",
    )
    .unwrap()
    .expand()
    .unwrap();
    let plan = ExecutionPlan::compile(&config);
    let mut head = head(b"Subject: test\n\nbody");
    let mut runtime = RuntimeVariables::default();
    let mut trace = BoundedTraceWriter::new(Vec::new());

    let result = plan.evaluate_headers_editing_with_trace(&mut head, &mut runtime, &mut trace);
    assert!(matches!(result, HeaderEvaluation::Decided(_)));

    let rendered = String::from_utf8(trace.into_inner()).unwrap();
    assert!(!rendered.contains("X-Private-Edited-Name"));
    assert!(!rendered.contains("private-edited-value"));
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
    let mut trace = BoundedTraceWriter::with_detail(Vec::new(), crate::trace::TraceDetail::Values);

    let result =
        plan.evaluate_headers_with_trace(&head(b"Subject: test\n\nbody"), &mut runtime, &mut trace);
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
    assert_eq!(recipe.action(), ActionKindExplanation::Maildir);
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
fn explains_header_operation_kinds_without_private_fields() {
    let config = config::parse(
        ":0\nheaders {\n remove X-Secret-Remove\n set X-Secret-Set: secret-set-value\n add X-Secret-Add: secret-add-value\n add X-Other-Add: other-add-value\n prepend X-Secret-Prepend: secret-prepend-value\n}\n",
    )
    .unwrap()
    .expand()
    .unwrap();
    let explanation = ExecutionPlan::compile(&config).explain();
    let [recipe] = explanation.recipes() else {
        panic!("expected one recipe");
    };

    assert_eq!(recipe.action(), ActionKindExplanation::Headers);
    let operations = recipe.header_operations().unwrap();
    assert_eq!(operations.remove_count(), 1);
    assert_eq!(operations.set_count(), 1);
    assert_eq!(operations.add_count(), 2);
    assert_eq!(operations.prepend_count(), 1);
    let rendered = format!("{explanation:?}");
    for private in [
        "X-Secret-Remove",
        "X-Secret-Set",
        "X-Secret-Add",
        "X-Other-Add",
        "X-Secret-Prepend",
        "secret-set-value",
        "secret-add-value",
        "other-add-value",
        "secret-prepend-value",
    ] {
        assert!(!rendered.contains(private), "leaked {private:?}");
    }
}

#[test]
fn header_match_decides_before_body() {
    let plan = compile(":0\n* ^Subject: wanted$\nmaildir:wanted\n\n:0 B\n* needle\nmaildir:body\n");
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
    let plan = compile(":0\n* ^Subject: ([a-z]+)-\\/([a-z]+)$\nmaildir:$MATCH1-$MATCH-$MATCH2\n");
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
    let result = plan.evaluate_headers_with_runtime(&head(b"X-Binary: \xff\n\nbody"), &mut runtime);

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
    let HeaderEvaluation::NeedsMessage(continuation) =
        plan.evaluate_headers_with_trace(&head(b"Subject: test\n\nbody"), &mut runtime, &mut trace)
    else {
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
            &mut |destination, _, _, _, runtime, _| {
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
            &mut |destination, _, _, _, _, _| {
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
            &mut |destination, _, _, _, _, _| {
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
fn successful_filter_replaces_bytes_for_later_conditions_and_delivery() {
    let plan = compile(":0 fw\n| rewrite\n:0\n* ^X-State: new$\nmaildir:selected\n");
    let original = b"X-State: old\n\nold body";
    let replacement = b"X-State: new\n\nnew body";
    let mut runtime = RuntimeVariables::default();
    let mut trace = NoTrace;
    let mut delivered = Vec::new();
    let mut external_calls = 0usize;

    let outcome = plan
        .execute_mapped_ordered_with_external_trace(
            MappedMessageInput::new(original, b"X-State: old\n\n".len(), None),
            &mut runtime,
            &mut trace,
            &mut |destination, message, _, _, _, _| {
                delivered.push((destination.path().to_owned(), message.to_vec()));
                Ok::<_, DeliveryAttemptError<&str>>(())
            },
            &mut |action, options, _, input, _, _| {
                external_calls += 1;
                assert_eq!(action.command, "rewrite");
                assert_eq!(options.action_mode, crate::config::ActionMode::Filter);
                assert_eq!(input.selected(), original);
                Ok::<_, DeliveryAttemptError<&str>>(Some(Message::from_bytes(replacement.to_vec())))
            },
        )
        .unwrap();

    assert_eq!(external_calls, 1);
    assert_eq!(delivered, [("selected".to_owned(), replacement.to_vec())]);
    assert_eq!(outcome.published(), 1);
    assert!(outcome.original_delivered());
}

#[test]
fn program_condition_uses_child_status_before_entering_block() {
    let plan = compile(":0 W\n* ? test ! -e $LISTDIR\n{\n:0\nmaildir:selected\n}\n");
    let raw = b"Subject: program\n condition\n\nbody";
    let matching_header = b"Subject: program condition\n\n";
    let mut runtime = RuntimeVariables::default();
    let mut trace = NoTrace;
    let mut delivered = Vec::new();
    let mut condition_calls = 0usize;

    let outcome = plan
        .execute_mapped_ordered_with_processes_trace(
            MappedMessageInput::new(
                raw,
                b"Subject: program\n condition\n\n".len(),
                Some(MatchingMessage::new(matching_header, None)),
            ),
            &mut runtime,
            &mut trace,
            &mut |destination, _, _, _, _, _| {
                delivered.push(destination.path().to_owned());
                Ok::<_, DeliveryAttemptError<&str>>(())
            },
            (
                &mut |command, input, _, _| {
                    condition_calls += 1;
                    assert_eq!(command, "test ! -e $LISTDIR");
                    assert_eq!(input, b"Subject: program\n condition\n\n");
                    Ok::<_, DeliveryAttemptError<&str>>(true)
                },
                &mut |_, _, _, _, _, _| {
                    panic!("recipe contains no pipe action");
                },
                &mut |_, _| Ok::<_, &str>(()),
                &mut |_, _| {
                    Ok::<Box<dyn RecipeLockGuard>, DeliveryAttemptError<&str>>(Box::new(()))
                },
            ),
        )
        .unwrap();

    assert_eq!(condition_calls, 1);
    assert_eq!(delivered, ["selected"]);
    assert_eq!(outcome.published(), 1);
    assert!(outcome.original_delivered());
}

#[test]
fn ordered_block_lock_guard_spans_the_complete_child_sequence() {
    struct Guard(std::rc::Rc<std::cell::Cell<bool>>);

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.set(false);
        }
    }

    let config = crate::config::parse(
        "MAILDIR=/mail\nLOCKMETHOD=flock\nLOCKTIMEOUT=7\nUMASK=077\nLOCKNAME=block.lock\n:0 : $LOCKNAME\n{\n:0\nmaildir:selected\n}\n",
    )
    .unwrap()
    .expand()
    .unwrap();
    let plan = ExecutionPlan::compile(&config);
    let raw = b"Subject: lock\n\nbody";
    let held = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = held.clone();
    let released = held.clone();
    let mut runtime = RuntimeVariables::default();
    let mut trace = NoTrace;

    let outcome = plan
        .execute_mapped_ordered_with_processes_trace(
            MappedMessageInput::new(raw, b"Subject: lock\n\n".len(), None),
            &mut runtime,
            &mut trace,
            &mut |destination, message, _, _, runtime, _| {
                assert!(observed.get());
                assert_eq!(
                    destination
                        .resolve_with(|name| runtime.get(name).map(str::to_owned))
                        .unwrap(),
                    Destination::Maildir("/mail/selected".into())
                );
                assert_eq!(message, raw);
                Ok::<_, DeliveryAttemptError<&str>>(())
            },
            (
                &mut |_, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(true),
                &mut |_, _, _, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(None),
                &mut |_, _| Ok::<_, &str>(()),
                &mut |path, runtime| {
                    assert_eq!(path, "/mail/block.lock");
                    assert_eq!(runtime.get("LOCKMETHOD"), Some("flock"));
                    assert_eq!(runtime.get("LOCKTIMEOUT"), Some("7"));
                    assert_eq!(runtime.get("UMASK"), Some("077"));
                    assert!(!held.replace(true));
                    Ok::<Box<dyn RecipeLockGuard>, DeliveryAttemptError<&str>>(Box::new(Guard(
                        held.clone(),
                    )))
                },
            ),
        )
        .unwrap();

    assert!(outcome.original_delivered());
    assert_eq!(outcome.published(), 1);
    assert!(!released.get());
}

#[test]
fn failed_filter_keeps_old_message_for_error_handler() {
    let plan = compile(":0 fw\n| fail\n:0 e\nmaildir:fallback\n");
    let original = b"Subject: original\n\nbody";
    let mut runtime = RuntimeVariables::default();
    let mut trace = NoTrace;
    let mut delivered = Vec::new();

    let outcome = plan
        .execute_mapped_ordered_with_external_trace(
            MappedMessageInput::new(original, b"Subject: original\n\n".len(), None),
            &mut runtime,
            &mut trace,
            &mut |destination, message, _, _, _, _| {
                delivered.push((destination.path().to_owned(), message.to_vec()));
                Ok::<_, DeliveryAttemptError<&str>>(())
            },
            &mut |_, _, _, input, _, _| {
                assert_eq!(input.selected(), original);
                Err(DeliveryAttemptError::Recoverable("filter failed"))
            },
        )
        .unwrap();

    assert_eq!(delivered, [("fallback".to_owned(), original.to_vec())]);
    assert_eq!(outcome.published(), 1);
    assert!(outcome.original_delivered());
}

#[test]
fn pipe_action_receives_only_its_selected_message_area() {
    for (flags, expected) in [
        ("fh", &b"Subject: original\n\n"[..]),
        ("fb", &b"body"[..]),
        ("fhb", &b"Subject: original\n\nbody"[..]),
    ] {
        let plan = compile(&format!(":0 {flags}\n| rewrite\n:0\nmaildir:selected\n"));
        let original = b"Subject: original\n\nbody";
        let mut runtime = RuntimeVariables::default();
        let mut trace = NoTrace;

        let outcome = plan
            .execute_mapped_ordered_with_external_trace(
                MappedMessageInput::new(original, b"Subject: original\n\n".len(), None),
                &mut runtime,
                &mut trace,
                &mut |_, _, _, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(()),
                &mut |_, _, _, input, _, _| {
                    assert_eq!(input.selected(), expected, "flags {flags}");
                    Ok::<_, DeliveryAttemptError<&str>>(Some(Message::from_bytes(
                        original.to_vec(),
                    )))
                },
            )
            .unwrap();
        assert!(outcome.original_delivered());
    }
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
            &mut |destination, _, _, _, _, _| {
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
    let plan = compile(":0\n* ^Subject: wanted$\nmaildir:wanted\n\n:0 B\n* needle\nmaildir:body\n");
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
