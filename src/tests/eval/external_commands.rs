// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#[test]
fn external_action_observes_edited_headers() {
    let config = config::parse(":0\nheaders {\n set X-State: new\n}\n:0 w\n| consume\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"X-State: old\n\nbody";
    let mut runtime = RuntimeVariables::default();
    let mut calls = 0usize;

    plan.execute_ordered(
        MappedMessageInput::new(raw, b"X-State: old\n\n".len(), None),
        &mut runtime,
        ExecutionServices::new(
            &mut |_, _, _, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(()),
            &mut NoTrace,
        )
        .with_external_action(&mut |_, _, _, input, _, _| {
            calls += 1;
            assert_eq!(input.header(), b"X-State: new\n\n");
            assert_eq!(input.body(), b"body");
            assert_eq!(input.selected(), b"X-State: new\n\nbody");
            Ok::<_, DeliveryAttemptError<&str>>(None)
        }),
    )
    .unwrap();

    assert_eq!(calls, 1);
}

#[test]
fn backquoted_assignments_require_the_complete_message() {
    for source in [
        "VALUE=`extract`\n:0\nmaildir:selected\n",
        ":0\n{\nVALUE=`extract`\n:0\nmaildir:selected\n}\n",
        ":0\n* ^Never:\nmaildir:selected\nVALUE=`extract`\n",
    ] {
        assert_eq!(
            compile(source).requirements(),
            InputRequirements {
                needs_headers: true,
                needs_body_contents: true,
                needs_end_of_message: true,
            },
            "source: {source:?}",
        );
    }
}

#[test]
fn ordered_backquoted_assignment_preserves_bytes_and_strips_all_trailing_lf() {
    let config = config::parse("VALUE=pre`first`mid`second`post\n:0\nmaildir:selected\n").unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"Subject: test\n\nbody";
    let mut runtime = RuntimeVariables::default();
    let mut trace = NoTrace;
    let mut commands = Vec::new();

    plan.execute_ordered(
        MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
        &mut runtime,
        ExecutionServices::new(
            &mut |_, _, _, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(()),
            &mut trace,
        )
        .with_capture(&mut |command, input, _, options, _, _, _| {
            commands.push(command.to_owned());
            assert_eq!(input, raw);
            assert_eq!(options, None);
            let output = if command == "first" {
                b"a\xff\n\n".to_vec()
            } else {
                b"z\n".to_vec()
            };
            Ok::<_, DeliveryAttemptError<&str>>(CapturedCommand::new(output))
        }),
    )
    .unwrap();

    assert_eq!(commands, ["first", "second"]);
    assert_eq!(runtime.get_bytes("VALUE"), Some(&b"prea\xffmidzpost"[..]));
}

#[test]
fn pattern_command_output_respects_its_quote_mode() {
    let config = config::parse(concat!(
        "VALUE=*tail\n",
        "ACTIVE=${VALUE##`pattern`}\n",
        "QUOTED=${VALUE#\"`pattern`\"}\n",
        ":0\nmaildir:selected\n",
    ))
    .unwrap()
    .expand(&[])
    .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"Subject: test\n\nbody";
    let mut runtime = RuntimeVariables::default();
    let mut calls = 0;

    plan.execute_ordered(
        MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
        &mut runtime,
        ExecutionServices::new(
            &mut |_, _, _, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(()),
            &mut NoTrace,
        )
        .with_capture(&mut |command, input, _, _, _, _, _| {
            calls += 1;
            assert_eq!(command, "pattern");
            assert_eq!(input, raw);
            Ok::<_, DeliveryAttemptError<&str>>(CapturedCommand::new(b"*\n".to_vec()))
        }),
    )
    .unwrap();

    assert_eq!(calls, 2);
    assert_eq!(runtime.get_bytes("ACTIVE"), Some(&b""[..]));
    assert_eq!(runtime.get_bytes("QUOTED"), Some(&b"tail"[..]));
}

#[test]
fn parameter_assignment_is_visible_in_the_expression_and_later_statements() {
    let config = config::parse(
        "VALUE=${SIDE:=selected}-$SIDE\nNEXT=$SIDE\n:0\nmaildir:selected\n",
    )
    .unwrap()
    .expand(&[])
    .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"Subject: test\n\nbody";
    let mut runtime = RuntimeVariables::default();

    plan.execute_ordered(
        MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
        &mut runtime,
        ExecutionServices::new(
            &mut |_, _, _, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(()),
            &mut NoTrace,
        ),
    )
    .unwrap();

    assert_eq!(runtime.get_bytes("SIDE"), Some(&b"selected"[..]));
    assert_eq!(runtime.get_bytes("VALUE"), Some(&b"selected-selected"[..]));
    assert_eq!(runtime.get_bytes("NEXT"), Some(&b"selected"[..]));
}

#[test]
fn destination_parameter_assignment_is_visible_to_later_path_parts() {
    let config = config::parse(":0\nmbox:${SIDE:=selected}-$SIDE\n")
    .unwrap()
    .expand(&[])
    .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"Subject: test\n\nbody";
    let mut runtime = RuntimeVariables::default();
    let mut destinations = Vec::new();

    plan.execute_ordered(
        MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
        &mut runtime,
        ExecutionServices::new(
            &mut |destination, _, _, _, _, _| {
                destinations.push(destination.path().to_owned());
                Ok::<_, DeliveryAttemptError<&str>>(())
            },
            &mut NoTrace,
        ),
    )
    .unwrap();

    assert_eq!(destinations, ["selected-selected"]);
    assert_eq!(runtime.get_bytes("SIDE"), Some(&b"selected"[..]));
}

#[test]
fn destination_command_substitution_uses_complete_message_and_runtime_values() {
    let config = config::parse("MAILDIR=/mail\nBOX=archive\n:0\nmbox:`choose`-$BOX\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    assert_eq!(
        plan.requirements(),
        InputRequirements {
            needs_headers: true,
            needs_body_contents: true,
            needs_end_of_message: true,
        }
    );
    let raw = b"Subject: test\n\nbody";
    let mut runtime = RuntimeVariables::default();
    let mut delivered = None;

    plan.execute_ordered(
        MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
        &mut runtime,
        ExecutionServices::new(
            &mut |destination, _, _, _, _, _| {
                delivered = Some(destination.path().to_owned());
                Ok::<_, DeliveryAttemptError<&str>>(())
            },
            &mut NoTrace,
        )
        .with_capture(&mut |command, input, _, options, limit, _, _| {
            assert_eq!(command, "choose");
            assert_eq!(input, raw);
            assert_eq!(options, None);
            assert_eq!(limit, crate::config::DEFAULT_LINEBUF);
            Ok::<_, DeliveryAttemptError<&str>>(CapturedCommand::new(b"selected\n\n".to_vec()))
        }),
    )
    .unwrap();

    assert_eq!(delivered.as_deref(), Some("/mail/selected-archive"));
}

#[test]
fn destination_command_output_obeys_active_linebuf() {
    for length in [127, 128, 129] {
        let config = config::parse("MAILDIR=/mail\nLINEBUF=128\n:0\nmbox:`choose`\n")
            .unwrap()
            .expand(&[])
            .unwrap();
        let plan = ExecutionPlan::compile(&config, None);
        let mut runtime = RuntimeVariables::default();
        let mut delivered = false;
        let result = plan.execute_ordered(
            MappedMessageInput::new(b"X: y\n\nbody", 6, None),
            &mut runtime,
            ExecutionServices::new(
                &mut |_, _, _, _, _, _| {
                    delivered = true;
                    Ok::<_, DeliveryAttemptError<&str>>(())
                },
                &mut NoTrace,
            )
            .with_capture(&mut |_, _, _, _, limit, _, _| {
                assert_eq!(limit, 128);
                Ok::<_, DeliveryAttemptError<&str>>(CapturedCommand::new(vec![b'x'; length]))
            }),
        );

        if length <= 128 {
            assert!(result.is_ok(), "length {length}: {result:?}");
            assert!(delivered);
        } else {
            assert!(matches!(
                result,
                Err(OrderedExecutionError::Evaluation(
                    EvalError::VariableValueTooLarge { size: 129, .. }
                ))
            ));
            assert!(!delivered);
        }
    }
}

#[test]
fn destination_command_rejects_non_utf8_output_before_delivery() {
    let config = config::parse("MAILDIR=/mail\n:0\nmbox:`choose`\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let mut delivered = false;
    let result = plan.execute_ordered(
        MappedMessageInput::new(b"X: y\n\nbody", 6, None),
        &mut RuntimeVariables::default(),
        ExecutionServices::new(
            &mut |_, _, _, _, _, _| {
                delivered = true;
                Ok::<_, DeliveryAttemptError<&str>>(())
            },
            &mut NoTrace,
        )
        .with_capture(&mut |_, _, _, _, _, _, _| {
            Ok::<_, DeliveryAttemptError<&str>>(CapturedCommand::new(b"bad-\xff-path".to_vec()))
        }),
    );

    assert_eq!(
        result,
        Err(OrderedExecutionError::Evaluation(
            EvalError::DestinationCommandOutputIsNotUtf8 { line: 3 }
        ))
    );
    assert!(!delivered);
}

#[test]
fn failed_backquoted_fragment_does_not_publish_a_partial_value() {
    let config =
        config::parse("VALUE=old\nVALUE=pre`first`middle`second`post\n:0\nmaildir:selected\n")
            .unwrap()
            .expand(&[])
            .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"Subject: test\n\nbody";
    let mut runtime = RuntimeVariables::default();

    let result = plan.execute_ordered(
        MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
        &mut runtime,
        ExecutionServices::new(
            &mut |_, _, _, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(()),
            &mut NoTrace,
        )
        .with_capture(&mut |command, _, _, _, _, _, _| {
            if command == "first" {
                Ok(CapturedCommand::new(b"first-output".to_vec()))
            } else {
                Err(DeliveryAttemptError::Recoverable("second failed"))
            }
        }),
    );

    assert_eq!(
        result,
        Err(OrderedExecutionError::Delivery("second failed"))
    );
    assert_eq!(runtime.get_bytes("VALUE"), Some(&b"old"[..]));
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
        .execute_ordered(
            MappedMessageInput::new(original, b"X-State: old\n\n".len(), None),
            &mut runtime,
            ExecutionServices::new(
                &mut |destination, message, _, _, _, _| {
                    delivered.push((destination.path().to_owned(), message.to_vec()));
                    Ok::<_, DeliveryAttemptError<&str>>(())
                },
                &mut trace,
            )
            .with_external_action(&mut |action, options, _, input, _, _| {
                external_calls += 1;
                assert_eq!(action.command, "rewrite");
                assert_eq!(options.action_mode, crate::config::ActionMode::Filter);
                assert_eq!(input.selected(), original);
                Ok::<_, DeliveryAttemptError<&str>>(Some(Message::from_bytes(replacement.to_vec())))
            }),
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
        .execute_ordered(
            MappedMessageInput::new(
                raw,
                b"Subject: program\n condition\n\n".len(),
                Some(MatchingMessage::from_normalized_parts(
                    matching_header,
                    None,
                )),
            ),
            &mut runtime,
            ExecutionServices::new(
                &mut |destination, _, _, _, _, _| {
                    delivered.push(destination.path().to_owned());
                    Ok::<_, DeliveryAttemptError<&str>>(())
                },
                &mut trace,
            )
            .with_external_condition(&mut |command, input, _, _| {
                condition_calls += 1;
                assert_eq!(command, "test ! -e $LISTDIR");
                assert_eq!(input, b"Subject: program\n condition\n\n");
                Ok::<_, DeliveryAttemptError<&str>>(true)
            })
            .with_external_action(&mut |_, _, _, _, _, _| {
                panic!("recipe contains no pipe action");
            })
            .with_capture(&mut |_, _, _, _, _, _, _| {
                panic!("recipe contains no command capture");
            })
            .with_global_lock(&mut |_, _| Ok::<_, &str>(()))
            .with_local_lock(&mut |_, _| {
                Ok::<Box<dyn RecipeLockGuard>, DeliveryAttemptError<&str>>(Box::new(()))
            }),
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
        "MAILDIR=/mail\nLOCKMETHOD=flock\nLOCKSLEEP=3\nLOCKTIMEOUT=7\nUMASK=077\nLOCKNAME=block.lock\n:0 : $LOCKNAME\n{\n:0\nmaildir:selected\n}\n",
    )
    .unwrap()
    .expand(&[])
    .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"Subject: lock\n\nbody";
    let held = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = held.clone();
    let released = held.clone();
    let mut runtime = RuntimeVariables::default();
    let mut trace = NoTrace;

    let outcome = plan
        .execute_ordered(
            MappedMessageInput::new(raw, b"Subject: lock\n\n".len(), None),
            &mut runtime,
            ExecutionServices::new(
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
                &mut trace,
            )
            .with_external_condition(&mut |_, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(true))
            .with_external_action(&mut |_, _, _, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(None))
            .with_capture(&mut |_, _, _, _, _, _, _| {
                panic!("recipe contains no command capture");
            })
            .with_global_lock(&mut |_, _| Ok::<_, &str>(()))
            .with_local_lock(&mut |path, runtime| {
                assert_eq!(path, "/mail/block.lock");
                assert_eq!(runtime.get("LOCKMETHOD"), Some("flock"));
                assert_eq!(runtime.get("LOCKSLEEP"), Some("3"));
                assert_eq!(runtime.get("LOCKTIMEOUT"), Some("7"));
                assert_eq!(runtime.get("UMASK"), Some("077"));
                assert!(!held.replace(true));
                Ok::<Box<dyn RecipeLockGuard>, DeliveryAttemptError<&str>>(Box::new(Guard(
                    held.clone(),
                )))
            }),
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
        .execute_ordered(
            MappedMessageInput::new(original, b"Subject: original\n\n".len(), None),
            &mut runtime,
            ExecutionServices::new(
                &mut |destination, message, _, _, _, _| {
                    delivered.push((destination.path().to_owned(), message.to_vec()));
                    Ok::<_, DeliveryAttemptError<&str>>(())
                },
                &mut trace,
            )
            .with_external_action(&mut |_, _, _, input, _, _| {
                assert_eq!(input.selected(), original);
                Err(DeliveryAttemptError::Recoverable("filter failed"))
            }),
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
            .execute_ordered(
                MappedMessageInput::new(original, b"Subject: original\n\n".len(), None),
                &mut runtime,
                ExecutionServices::new(
                    &mut |_, _, _, _, _, _| Ok::<_, DeliveryAttemptError<&str>>(()),
                    &mut trace,
                )
                .with_external_action(&mut |_, _, _, input, _, _| {
                    assert_eq!(input.selected(), expected, "flags {flags}");
                    Ok::<_, DeliveryAttemptError<&str>>(Some(Message::from_bytes(
                        original.to_vec(),
                    )))
                }),
            )
            .unwrap();
        assert!(outcome.original_delivered());
    }
}
