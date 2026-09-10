// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#[test]
fn header_edit_updates_following_header_rules_without_buffering_body() {
    let config = config::parse(
        ":0\nheaders {\n set X-State new\n}\n:0\n* ^X-State: new$\nmaildir:selected\n",
    )
    .unwrap()
    .expand(&[])
    .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
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
fn decoded_header_extraction_drives_a_following_variable_condition() {
    let config = config::parse(
        ":0\nheaders {\n extract decoded Subject into MSG_SUBJECT\n}\n:0\n* MSG_SUBJECT ?? ^Scholar.*\nmaildir:selected\n",
    )
    .unwrap()
    .expand(&[])
    .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let mut head = head(b"Subject: =?US-ASCII?Q?Scholar_alert?=\n\nbody");
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
}

#[test]
fn ordered_header_edit_updates_later_delivery_bytes() {
    let config = config::parse(
        ":0 B\n* needle\nheaders {\n add X-Body-Matched yes\n}\n:0\n* ^X-Body-Matched: yes$\nmaildir:selected\n",
    )
    .unwrap()
    .expand(&[])
    .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"Subject: test\n\nneedle body";
    let mut runtime = RuntimeVariables::default();
    let mut delivered = Vec::new();

    plan.execute_ordered(
        MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
        &mut runtime,
        ExecutionServices::new(
            &mut |destination, message, _, _, _, _| {
                delivered.push((destination.path().to_owned(), message.to_vec()));
                Ok::<_, DeliveryAttemptError<&str>>(())
            },
            &mut NoTrace,
        ),
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
fn ordered_tree_binds_runtime_values_between_actual_actions() {
    let plan = compile("BOX=first\n:0 c\nmaildir:$BOX\n:0\nmaildir:${LASTFOLDER}.second\n");
    let raw = b"Subject: test\n\nbody";
    let mut runtime = RuntimeVariables::default();
    let mut trace = NoTrace;
    let mut attempted = Vec::new();

    let outcome = plan
        .execute_ordered(
            MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
            &mut runtime,
            ExecutionServices::new(
                &mut |destination, _, _, _, runtime, _| {
                    let destination = destination
                        .resolve_with(|name| runtime.get(name).map(str::to_owned))
                        .unwrap();
                    attempted.push(destination.path().to_owned());
                    runtime.set("LASTFOLDER", destination.path());
                    Ok::<_, DeliveryAttemptError<&str>>(())
                },
                &mut trace,
            ),
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
        .execute_ordered(
            MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
            &mut runtime,
            ExecutionServices::new(
                &mut |destination, _, _, _, _, _| {
                    attempted.push(destination.path().to_owned());
                    if destination.path() == "primary" {
                        Err(DeliveryAttemptError::Recoverable("primary failed"))
                    } else {
                        Ok(())
                    }
                },
                &mut trace,
            ),
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
        .execute_ordered(
            MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
            &mut runtime,
            ExecutionServices::new(
                &mut |destination, _, _, _, _, _| {
                    attempted.push(destination.path().to_owned());
                    if destination.path() == "primary" {
                        Err(DeliveryAttemptError::Recoverable("primary failed"))
                    } else {
                        Ok(())
                    }
                },
                &mut trace,
            ),
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
        .execute_ordered(
            MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
            &mut runtime,
            ExecutionServices::new(
                &mut |destination, _, _, _, _, _| {
                    attempted.push(destination.path().to_owned());
                    Err(DeliveryAttemptError::Fatal("durability failed"))
                },
                &mut trace,
            ),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        OrderedExecutionError::Delivery("durability failed")
    ));
    assert_eq!(attempted, ["primary"]);
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

    plan.resume_with_trace(
        continuation,
        MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
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
