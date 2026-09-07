// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#[test]
fn executes_assignments_after_the_final_recipe() {
    let config = config::parse(":0\n* ^X-Never: yes$\nmaildir:unused\nAFTER=tail\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let mut runtime = RuntimeVariables::default();

    let result = plan.evaluate_headers_with_runtime(&head(b"Subject: test\n\nbody"), &mut runtime);

    assert!(matches!(result, HeaderEvaluation::Decided(_)));
    assert_eq!(runtime.get("AFTER"), Some("tail"));
}

#[test]
fn skipped_block_does_not_apply_its_assignment() {
    let config = config::parse(":0\n* ^X-Select: yes$\n{\nBOX=selected\n}\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
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
            .expand(&[])
            .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
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
