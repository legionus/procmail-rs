// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#[test]
fn waited_copy_block_continues_branch_and_parent_with_local_variables() {
    for flags in ["cw", "cW"] {
        let source = format!(
            "BRANCH=parent\n:0 {flags}\n{{\nBRANCH=copy\n:0 c\nmaildir:${{BRANCH}}\n}}\n:0 c\nmaildir:${{BRANCH}}\n"
        );
        let plan = compile(&source);
        let raw = b"Subject: test\n\nbody\n";
        let mut runtime = RuntimeVariables::default();
        let mut trace = NoTrace;
        let mut destinations = Vec::new();
        let outcome = plan
            .execute_ordered(
                MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
                &mut runtime,
                ExecutionServices::new(
                    &mut |destination, _, _, _, runtime, _| {
                        let destination = destination
                            .resolve_with(|name| runtime.get(name).map(str::to_owned))
                            .unwrap();
                        destinations.push(destination.path().to_owned());
                        Ok::<_, DeliveryAttemptError<&str>>(())
                    },
                    &mut trace,
                ),
            )
            .unwrap();

        assert_eq!(outcome.published(), 3, "{flags}");
        assert!(!outcome.original_delivered(), "{flags}");
        assert_eq!(
            destinations,
            ["copy", "copy", "parent"],
            "{flags}"
        );
    }
}

#[test]
fn waited_copy_block_keeps_header_edits_in_the_branch() {
    let source = ":0 cw\n{\n:0\nheaders {\n set X-Branch: copy\n}\n}\n:0 c\n* ^X-Branch: copy$\nmaildir:changed\n:0 c\n* ! ^X-Branch: copy$\nmaildir:original\n";
    let (outcome, recorder) = evaluate_config(source, b"Subject: test\n\nbody\n");

    assert_eq!(outcome, Outcome::Undelivered { copies: 2 });
    assert_eq!(
        recorder
            .destinations
            .iter()
            .map(Destination::path)
            .collect::<Vec<_>>(),
        ["changed", "original"]
    );
}

#[test]
fn delivery_inside_waited_copy_block_stops_only_the_branch() {
    let source = ":0 cw\n{\n:0\nmaildir:block\n}\n:0 c\nmaildir:after\n";
    let (outcome, recorder) = evaluate_config(source, b"Subject: test\n\nbody\n");

    assert_eq!(outcome, Outcome::Undelivered { copies: 2 });
    assert_eq!(
        recorder
            .destinations
            .iter()
            .map(Destination::path)
            .collect::<Vec<_>>(),
        ["block", "after"]
    );
}

#[test]
fn delivery_after_waited_copy_block_runs_in_both_branches() {
    let source = ":0 cw\n{\nVALUE=copy\n}\n:0\nmaildir:after\n";
    let (outcome, recorder) = evaluate_config(source, b"Subject: test\n\nbody\n");

    assert_eq!(
        outcome,
        Outcome::Delivered {
            deliveries: 2
        }
    );
    assert_eq!(
        recorder
            .destinations
            .iter()
            .map(Destination::path)
            .collect::<Vec<_>>(),
        ["after", "after"]
    );
}

#[test]
fn waited_copy_block_preserves_parent_and_branch_chain_state() {
    let source = ":0 cw\n{\n:0 c\nmaildir:inside\n}\n:0 Ac\nmaildir:after-a-upper\n:0 ac\nmaildir:after-a-lower\n:0 Ec\nmaildir:never-else\n";
    let (outcome, recorder) = evaluate_config(source, b"Subject: test\n\nbody\n");

    assert_eq!(outcome, Outcome::Undelivered { copies: 5 });
    assert_eq!(
        recorder
            .destinations
            .iter()
            .map(Destination::path)
            .collect::<Vec<_>>(),
        [
            "inside",
            "after-a-upper",
            "after-a-lower",
            "after-a-upper",
            "after-a-lower"
        ]
    );
}

#[test]
fn branch_error_handler_recovers_before_waited_status_reaches_parent() {
    let config = config::parse(
        ":0 cw\n{\n:0\nmaildir:failure\n}\n:0 ec\nmaildir:recovered\n",
    )
    .unwrap();
    let message = Message::from_bytes(b"Subject: test\n\nbody\n".to_vec());
    let mut recorder = FailingRecorder {
        fail_paths: &["failure"],
        attempted: Vec::new(),
    };

    let outcome = evaluate(&config, &message, &mut recorder).unwrap();

    assert_eq!(recorder.attempted, ["failure", "recovered"]);
    assert_eq!(outcome, Outcome::Undelivered { copies: 1 });
}

#[test]
fn parser_accepts_an_unwaited_copy_block() {
    config::parse(":0 c\n{\n}\n").unwrap();
}

#[test]
fn background_copy_limit_checks_both_boundaries() {
    let budget = crate::eval::ordered::BackgroundCopyBudget::new();

    for _ in 0..crate::eval::ordered::MAX_BACKGROUND_COPY_BRANCHES {
        budget.reserve(1).unwrap();
    }
    let error = budget.reserve(1).unwrap_err();
    assert!(error.to_string().contains("128 branches"));
}
