// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#[test]
fn static_shell_condition_escapes_variable_text_and_stays_header_only() {
    let config = config::parse("NEEDLE=a.b\n:0\n* $^Subject: $\\NEEDLE$\nmaildir:selected\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    assert_eq!(
        plan.requirements(),
        InputRequirements {
            needs_headers: true,
            ..InputRequirements::default()
        }
    );

    for (subject, selected) in [("a.b", true), ("axb", false)] {
        let raw = format!("Subject: {subject}\n\nbody");
        let mut head = head(raw.as_bytes());
        let result = plan.evaluate_headers_editing_with_trace(
            &mut head,
            &mut RuntimeVariables::default(),
            &mut NoTrace,
        );
        match (result, selected) {
            (HeaderEvaluation::Decided(delivery), true) => {
                assert_eq!(
                    destinations(&delivery),
                    [Destination::Maildir("selected".into())]
                );
            }
            (HeaderEvaluation::Decided(delivery), false) => {
                assert!(delivery.deliveries().is_empty());
            }
            (other, _) => panic!("unexpected evaluation for {subject:?}: {other:?}"),
        }
    }
}

#[test]
fn structured_header_conditions_match_normalized_values_and_set_captures() {
    let source = ":0\n* address From,To,Cc ?? ^([^@]+)@example\\.org$\n* identifier List-Id ?? ^project\\.example$\nmaildir:selected\n";
    let config = config::parse(source).unwrap().expand(&[]).unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    assert!(plan.requirements().needs_headers);
    assert!(!plan.requirements().needs_body_contents);
    let mut message = head(
        b"From: \"decoy@invalid.example\" <Local(Comment)@Example.ORG>\r\n\
List-Id: Discussion <Project.Example>\r\n\r\nbody",
    );
    let mut runtime = RuntimeVariables::default();
    let result =
        plan.evaluate_headers_editing_with_trace(&mut message, &mut runtime, &mut NoTrace);
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected headers-only decision");
    };
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("selected".into())]
    );
    assert_eq!(runtime.get("MATCH1"), Some("Local"));
}

#[test]
fn address_aliases_match_delivery_and_resent_headers_with_captures() {
    let source = "\
:0\n\
* address %sender ?? ^forwarder@example\\.org$\n\
* address %delivered ?? ^user\\+cron@example\\.org$\n\
* address %recipient ?? ^user\\+alert([a-z]+)@example\\.org$\n\
maildir:selected\n";
    let config = config::parse(source).unwrap().expand(&[]).unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    assert!(plan.requirements().needs_headers);
    assert!(!plan.requirements().needs_body_contents);
    let mut message = head(
        b"Resent-From: \"Mail Forwarder\" <forwarder@Example.ORG>\r\n\
Delivered-To: user+cron@Example.ORG\r\n\
X-Original-To: \"Sub Address\" <user+alerts@Example.ORG>\r\n\
To: other-team@example.com\r\n\r\nbody",
    );
    let mut runtime = RuntimeVariables::default();
    let result =
        plan.evaluate_headers_editing_with_trace(&mut message, &mut runtime, &mut NoTrace);
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected headers-only decision");
    };
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("selected".into())]
    );
    assert_eq!(runtime.get("MATCH1"), Some("s"));
}

#[test]
fn keyword_conditions_observe_header_edits_captures_and_case_flags() {
    let source = "\
:0\n\
headers {\n\
    keywords add Work, Project:Alpha\n\
}\n\
:0 D\n\
* ! keyword Keywords ?? ^work$\n\
* keyword all ?? ^Project:\\/([A-Za-z]+)$\n\
maildir:selected\n";
    let config = config::parse(source).unwrap().expand(&[]).unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    assert!(plan.requirements().needs_headers);
    assert!(!plan.requirements().needs_body_contents);
    let mut message = head(b"X-Label: Initial\r\n\r\nbody");
    let mut runtime = RuntimeVariables::default();
    let mut trace = BoundedTraceWriter::new(Vec::new());
    let result = plan.evaluate_headers_editing_with_trace(&mut message, &mut runtime, &mut trace);
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected headers-only decision");
    };
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("selected".into())]
    );
    assert_eq!(runtime.get("MATCH"), Some("Alpha"));
    assert_eq!(runtime.get("MATCH1"), Some("Alpha"));
    assert_eq!(
        message.as_bytes(),
        b"X-Label: Initial, Work, Project:Alpha\r\n\
Keywords: Initial, Work, Project:Alpha\r\n\
X-Keywords: Initial, Work, Project:Alpha\r\n\r\n"
    );
    let rendered = String::from_utf8(trace.into_inner()).unwrap();
    assert!(rendered.contains("\"kind\":\"keyword\""), "{rendered}");
    for private in ["Initial", "Work", "Project:Alpha", "Alpha"] {
        assert!(!rendered.contains(private), "leaked {private:?}");
    }
}

#[test]
fn thread_identifier_conditions_match_root_and_replies_with_captures() {
    let source =
        ":0\n* identifier %thread ?? ^root\\.([0-9]+)@example\\.org$\nmaildir:selected\n";
    let config = config::parse(source).unwrap().expand(&[]).unwrap();
    let plan = ExecutionPlan::compile(&config, None);

    // 1. Root message
    let mut root_msg = head(b"Message-ID: <root.123@example.org>\nSubject: Thread root\n\nBody");
    let mut runtime = RuntimeVariables::default();
    let result =
        plan.evaluate_headers_editing_with_trace(&mut root_msg, &mut runtime, &mut NoTrace);
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected decision on root message");
    };
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("selected".into())]
    );
    assert_eq!(runtime.get("MATCH1"), Some("123"));

    // 2. Direct reply (matches In-Reply-To)
    let mut reply_msg = head(
        b"Message-ID: <reply.456@example.org>\nIn-Reply-To: <root.123@example.org>\nReferences: <root.123@example.org>\n\nBody",
    );
    let mut runtime = RuntimeVariables::default();
    let result =
        plan.evaluate_headers_editing_with_trace(&mut reply_msg, &mut runtime, &mut NoTrace);
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected decision on direct reply");
    };
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("selected".into())]
    );
    assert_eq!(runtime.get("MATCH1"), Some("123"));

    // 3. Sub-reply (matches References)
    let mut subreply_msg = head(
        b"Message-ID: <subreply.789@example.org>\nIn-Reply-To: <reply.456@example.org>\nReferences: <root.123@example.org> <reply.456@example.org>\n\nBody",
    );
    let mut runtime = RuntimeVariables::default();
    let result =
        plan.evaluate_headers_editing_with_trace(&mut subreply_msg, &mut runtime, &mut NoTrace);
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected decision on sub-reply");
    };
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("selected".into())]
    );
    assert_eq!(runtime.get("MATCH1"), Some("123"));

    // 4. Unrelated message does not match
    let mut unrelated_msg = head(b"Message-ID: <unrelated.999@example.org>\n\nBody");
    let mut runtime = RuntimeVariables::default();
    let result =
        plan.evaluate_headers_editing_with_trace(&mut unrelated_msg, &mut runtime, &mut NoTrace);
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected decision on unrelated message");
    };
    assert!(delivery.deliveries().is_empty());
}

#[test]
fn multipart_conditions_match_single_and_multipart_messages_with_captures() {
    let source = "\
:0
* multipart %mimetype ?? ^text/calendar$
* multipart %filename ?? ^([a-z]+)\\.ics$
maildir:calendar
";
    let config = config::parse(source).unwrap().expand(&[]).unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    assert!(plan.requirements().needs_headers);
    assert!(plan.requirements().needs_body_contents);

    let raw_single = b"Content-Type: text/calendar; name=\"meeting.ics\"\r\n\
Content-Disposition: inline; filename=\"meeting.ics\"\r\n\r\n\
BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n";
    let msg_single = Message::from_bytes(raw_single.to_vec());
    let mut runtime = RuntimeVariables::default();
    let delivery = plan
        .evaluate_full_with_runtime(&msg_single, &mut runtime)
        .unwrap();
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("calendar".into())]
    );
    assert_eq!(runtime.get("MATCH1"), Some("meeting"));

    // Multipart with block matching
    let source_block = "\
:0
* multipart {
  %mimetype ?? ^text/x-patch$
  %disposition ?? ^attachment$
  %filename ?? ^([a-z]+)\\.patch$
}
maildir:patches
";
    let config_block = config::parse(source_block).unwrap().expand(&[]).unwrap();
    let plan_block = ExecutionPlan::compile(&config_block, None);

    let raw_multi = b"Content-Type: multipart/mixed; boundary=\"frontier\"\r\n\r\n\
--frontier\r\n\
Content-Type: text/plain\r\n\
Content-Disposition: inline\r\n\r\n\
Here is the patch.\r\n\
--frontier\r\n\
Content-Type: text/x-patch\r\n\
Content-Disposition: attachment; filename=\"bugfix.patch\"\r\n\r\n\
--- a/file\r\n+++ b/file\r\n\
--frontier--\r\n";
    let msg_multi = Message::from_bytes(raw_multi.to_vec());
    let mut runtime_block = RuntimeVariables::default();
    let delivery_block = plan_block
        .evaluate_full_with_runtime(&msg_multi, &mut runtime_block)
        .unwrap();
    assert_eq!(
        destinations(&delivery_block),
        [Destination::Maildir("patches".into())]
    );
    assert_eq!(runtime_block.get("MATCH1"), Some("bugfix"));
}

#[test]
fn multipart_block_prevents_false_positives_across_different_parts() {
    let source = "\
:0
* multipart {
  %mimetype ?? ^text/x-patch$
  %filename ?? \\.patch$
}
maildir:exact_patches

:0
* multipart %mimetype ?? ^text/x-patch$
* multipart %filename ?? \\.patch$
maildir:loose_patches
";
    let config = config::parse(source).unwrap().expand(&[]).unwrap();
    let plan = ExecutionPlan::compile(&config, None);

    // Message where part 1 has text/x-patch (no filename)
    // and part 2 has application/octet-stream with filename="payload.patch"
    let raw = b"Content-Type: multipart/mixed; boundary=\"frontier\"\r\n\r\n\
--frontier\r\n\
Content-Type: text/x-patch\r\n\
Content-Disposition: inline\r\n\r\n\
diff inline\r\n\
--frontier\r\n\
Content-Type: application/octet-stream\r\n\
Content-Disposition: attachment; filename=\"payload.patch\"\r\n\r\n\
binary data\r\n\
--frontier--\r\n";
    let msg = Message::from_bytes(raw.to_vec());
    let mut runtime = RuntimeVariables::default();
    let delivery = plan.evaluate_full_with_runtime(&msg, &mut runtime).unwrap();
    // exact_patches must NOT match because no single part matches both!
    // loose_patches DOES match because each condition checks parts independently!
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("loose_patches".into())]
    );
}

#[test]
fn runtime_shell_condition_reparses_match_as_a_size_test() {
    let config =
        config::parse(":0 c\n* ^X-Condition: \\/(.*)$\nmbox:first\n:0\n* $$MATCH\nmbox:second\n")
            .unwrap()
            .expand(&[])
            .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"X-Condition: < 128\n\nbody";
    let mut runtime = RuntimeVariables::default();
    let mut paths = Vec::new();

    plan.execute_ordered(
        MappedMessageInput::new(raw, b"X-Condition: < 128\n\n".len(), None),
        &mut runtime,
        ExecutionServices::new(
            &mut |destination, _, _, _, _, _| {
                paths.push(destination.path().to_owned());
                Ok::<_, DeliveryAttemptError<&str>>(())
            },
            &mut NoTrace,
        ),
    )
    .unwrap();

    assert_eq!(paths, ["first", "second"]);
}

#[test]
fn runtime_shell_condition_can_reparse_to_a_program_test() {
    let config =
        config::parse(":0 c\n* ^X-Condition: \\/(.*)$\nmbox:first\n:0\n* $$MATCH\nmbox:second\n")
            .unwrap()
            .expand(&[])
            .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"X-Condition: ? selected-command\n\nbody";
    let header_len = b"X-Condition: ? selected-command\n\n".len();
    let mut runtime = RuntimeVariables::default();
    let mut paths = Vec::new();
    let mut commands = Vec::new();

    plan.execute_ordered(
        MappedMessageInput::new(raw, header_len, None),
        &mut runtime,
        ExecutionServices::new(
            &mut |destination, _, _, _, _, _| {
                paths.push(destination.path().to_owned());
                Ok::<_, DeliveryAttemptError<&str>>(())
            },
            &mut NoTrace,
        )
        .with_external_condition(&mut |command, input, _, _| {
            commands.push(command.to_owned());
            assert_eq!(input, &raw[..header_len]);
            Ok::<_, DeliveryAttemptError<&str>>(true)
        })
        .with_external_action(&mut |_, _, _, _, _, _| panic!("recipe contains no pipe action"))
        .with_capture(&mut |_, _, _, _, _, _, _| panic!("recipe contains no command capture"))
        .with_global_lock(&mut |_, _| Ok::<_, &str>(()))
        .with_local_lock(&mut |_, _| {
            Ok::<Box<dyn RecipeLockGuard>, DeliveryAttemptError<&str>>(Box::new(()))
        }),
    )
    .unwrap();

    assert_eq!(commands, ["selected-command"]);
    assert_eq!(paths, ["first", "second"]);
}

#[test]
fn capture_action_requirements_follow_h_and_b_flags() {
    assert_eq!(
        compile(":0 h\nVALUE=| extract\n").requirements(),
        InputRequirements {
            needs_headers: true,
            needs_body_contents: false,
            needs_end_of_message: false,
        }
    );

    for source in [":0 b\nVALUE=| extract\n", ":0\nVALUE=| extract\n"] {
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
fn ordered_capture_uses_selected_area_strips_one_lf_and_continues() {
    let config = config::parse(":0 hW\nVALUE=| capture\n:0\nmaildir:selected\n").unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let raw = b"Subject: test\n\nbody";
    let mut runtime = RuntimeVariables::default();
    let mut trace = NoTrace;
    let mut delivered = false;

    let outcome = plan
        .execute_ordered(
            MappedMessageInput::new(raw, b"Subject: test\n\n".len(), None),
            &mut runtime,
            ExecutionServices::new(
                &mut |_, _, _, _, _, _| {
                    delivered = true;
                    Ok::<_, DeliveryAttemptError<&str>>(())
                },
                &mut trace,
            )
            .with_capture(&mut |command, input, _, options, _, _, _| {
                assert_eq!(command, "capture");
                assert_eq!(input, b"Subject: test\n\n");
                assert_eq!(
                    options.unwrap().child_status,
                    crate::config::ChildStatusMode::WaitQuietly
                );
                Ok::<_, DeliveryAttemptError<&str>>(CapturedCommand::new(b"value\n\n\n".to_vec()))
            }),
        )
        .unwrap();

    assert!(delivered);
    assert!(outcome.original_delivered());
    assert_eq!(runtime.get_bytes("VALUE"), Some(&b"value\n\n"[..]));
}

#[test]
fn header_capture_runs_without_body_staging_and_updates_later_paths() {
    let config = config::parse(":0 h\nVALUE=| capture\nNEXT=$VALUE\n:0\nmaildir:selected\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let mut head = head(b"Subject: test\n\nbody-not-read");
    let mut runtime = RuntimeVariables::default();
    let mut called = false;

    assert!(!plan.requirements().needs_end_of_message);
    let result = plan
        .evaluate_headers_editing_with_capture_trace(
            &mut head,
            &mut runtime,
            &mut NoTrace,
            &mut |command, input, _, _, _, _, _| {
                called = true;
                assert_eq!(command, "capture");
                assert_eq!(input, b"Subject: test\n\n");
                Ok::<_, DeliveryAttemptError<&str>>(CapturedCommand::new(b"selected\n".to_vec()))
            },
        )
        .unwrap();
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected a header-only decision");
    };

    assert!(called);
    assert_eq!(runtime.get_bytes("VALUE"), Some(&b"selected"[..]));
    assert_eq!(runtime.get_bytes("NEXT"), Some(&b"selected"[..]));
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("selected".into())]
    );
}

#[test]
fn failed_header_capture_preserves_value_and_selects_error_handler() {
    let config = config::parse("VALUE=old\n:0 hW\nVALUE=| capture\n:0 e\nmaildir:recovered\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let mut head = head(b"Subject: test\n\nbody-not-read");
    let mut runtime = RuntimeVariables::default();

    let result = plan
        .evaluate_headers_editing_with_capture_trace(
            &mut head,
            &mut runtime,
            &mut NoTrace,
            &mut |_, _, _, _, _, _, _| {
                Err::<CapturedCommand, _>(DeliveryAttemptError::Recoverable("failed"))
            },
        )
        .unwrap();
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected the error handler to recover the capture failure");
    };

    assert_eq!(runtime.get_bytes("VALUE"), Some(&b"old"[..]));
    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("recovered".into())]
    );
}

#[test]
fn successful_header_capture_selects_success_handler() {
    let config = config::parse(":0 hW\nVALUE=| capture\n:0 a\nmaildir:succeeded\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let mut head = head(b"Subject: test\n\nbody-not-read");
    let mut runtime = RuntimeVariables::default();

    let result = plan
        .evaluate_headers_editing_with_capture_trace(
            &mut head,
            &mut runtime,
            &mut NoTrace,
            &mut |_, _, _, _, _, _, _| {
                Ok::<_, DeliveryAttemptError<&str>>(CapturedCommand::new(b"value".to_vec()))
            },
        )
        .unwrap();
    let HeaderEvaluation::Decided(delivery) = result else {
        panic!("expected the success handler to finish header evaluation");
    };

    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("succeeded".into())]
    );
}

#[test]
fn header_capture_validates_output_limit_before_replacing_value() {
    let config =
        config::parse("VALUE=old\nLINEBUF=128\n:0 h\nVALUE=| capture\n:0\nmaildir:selected\n")
            .unwrap()
            .expand(&[])
            .unwrap();
    let plan = ExecutionPlan::compile(&config, None);

    for length in [127, 128, 129] {
        let mut head = head(b"Subject: test\n\nbody-not-read");
        let mut runtime = RuntimeVariables::default();
        let result = plan.evaluate_headers_editing_with_capture_trace(
            &mut head,
            &mut runtime,
            &mut NoTrace,
            &mut |_, _, _, _, limit, _, _| {
                assert_eq!(limit, 128);
                Ok::<_, DeliveryAttemptError<&str>>(CapturedCommand::new(vec![b'x'; length]))
            },
        );

        if length <= 128 {
            assert!(matches!(result, Ok(HeaderEvaluation::Decided(_))));
            assert_eq!(runtime.get_bytes("VALUE"), Some(&vec![b'x'; length][..]));
        } else {
            assert!(matches!(
                result,
                Err(OrderedExecutionError::Evaluation(
                    EvalError::VariableValueTooLarge { size: 129, .. }
                ))
            ));
            assert_eq!(runtime.get_bytes("VALUE"), Some(&b"old"[..]));
        }
    }
}

#[test]
fn nested_assignment_uses_runtime_capture_before_delivery() {
    let config = config::parse(
        ":0\n* ^Subject: \\/(.*)$\n{\nBOX=${MATCH1:-fallback}\n:0\nmaildir:$BOX\n}\n",
    )
    .unwrap()
    .expand(&[])
    .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
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
    assert_eq!(runtime.get("MATCH"), Some("?\n"));
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

    assert_eq!(runtime.get("MATCH"), Some("beta\n"));
    assert_eq!(runtime.get("MATCH1"), Some("alpha"));
    assert_eq!(runtime.get("MATCH2"), Some("beta"));
    let resolved = delivery.deliveries()[0]
        .destination()
        .resolve_with(|name| runtime.get(name).map(str::to_owned))
        .unwrap();
    assert_eq!(resolved, Destination::Maildir("alpha-beta\n-beta".into()));
}

#[test]
fn successful_branch_without_a_marker_preserves_match() {
    // Debian-patched procmail 3.23pre leaves MATCH unchanged when the selected
    // alternative never reaches the marker in another alternative.
    let plan = compile(":0\n* ^Subject: (foo|bar\\/baz)qux$\nmaildir:matched\n");
    let mut runtime = RuntimeVariables::default();
    runtime.set("MATCH", "before");

    let HeaderEvaluation::Decided(delivery) =
        plan.evaluate_headers_with_runtime(&head(b"Subject: fooqux\n\nbody"), &mut runtime)
    else {
        panic!("expected a header decision");
    };

    assert_eq!(
        destinations(&delivery),
        [Destination::Maildir("matched".into())]
    );
    assert_eq!(runtime.get("MATCH"), Some("before"));
}

#[test]
fn ambiguous_match_selection_follows_the_documented_rust_priority() {
    // The corresponding original-procmail results are recorded separately in
    // tests/fixtures/regex_match_selection/reference-behavior.md. Keeping the
    // deliberate differences visible prevents an engine change from being
    // mistaken for a harmless regex refactor.
    for (pattern, subject, expected) in [
        (r"\/(a|aa)", "aa", "a"),
        (r"\/(aa|a)", "aa", "aa"),
        (r"\/a*", "aaa", "aaa"),
        (r"(a|aa)\/b*", "aabbb", ""),
        (r"(aa|a)\/b*", "aabbb", "bbb"),
        (r"(a|aa\/a)", "aaa", "before"),
        (r"(aa\/a|a)", "aaa", "a"),
    ] {
        let plan = compile(&format!(
            ":0\n* ^Subject: {pattern}\nmaildir:matched\n"
        ));
        let mut runtime = RuntimeVariables::default();
        runtime.set("MATCH", "before");
        let message = format!("Subject: {subject}\n\nbody");

        let HeaderEvaluation::Decided(delivery) =
            plan.evaluate_headers_with_runtime(&head(message.as_bytes()), &mut runtime)
        else {
            panic!("expected a header decision for {pattern:?}");
        };

        assert_eq!(
            destinations(&delivery),
            [Destination::Maildir("matched".into())],
            "{pattern}"
        );
        assert_eq!(runtime.get("MATCH"), Some(expected), "{pattern}");
    }
}

#[test]
fn reserved_mail_address_macros_follow_the_reference_matrix() {
    // These cases replace permissive reference scripts with direct decisions.
    // In particular, original procmail matches numeric and plus-tag suffixes
    // after ^TOmatchme; the macro requires a boundary before the word, not
    // after it.
    for (pattern, headers, expected) in [
        ("^TOmatchme", "To: matchme@example.com\n", true),
        ("^TOmatchme", "To: prefixmatchme@example.com\n", false),
        ("^TOmatchme", "To: matchme2@example.com\n", true),
        ("^TOmatchme", "To: matchme+tag@example.com\n", true),
        ("^TOmatchme", "To: match-me@example.com\n", false),
        ("^TOmatchme", "To: match.me@example.com\n", false),
        ("^TOmatchme", "To: \"matchme\"@example.com\n", true),
        (
            "^TOmatchme",
            "To: \"Doe (matchme)\" <other@example.com>\n",
            true,
        ),
        (
            "^TO_matchme@example\\.com",
            "To: first@example.com, matchme@example.com\n",
            true,
        ),
        (
            "^TO_matchme@example\\.com",
            "To: first@example.com,\n\tmatchme@example.com\n",
            true,
        ),
        (
            "^TO_matchme@example\\.com",
            "Cc: matchme@example.com\n",
            true,
        ),
        (
            "^TO_matchme@example\\.com",
            "Bcc: matchme@example.com\n",
            true,
        ),
        (
            "^TO_matchme@example\\.com",
            "Original-To: matchme@example.com\n",
            true,
        ),
        (
            "^TO_matchme@example\\.com",
            "Original-Cc: matchme@example.com\n",
            true,
        ),
        (
            "^TO_matchme@example\\.com",
            "Resent-To: matchme@example.com\n",
            true,
        ),
        (
            "^TO_matchme@example\\.com",
            "Resent-Cc: matchme@example.com\n",
            true,
        ),
        (
            "^TO_matchme@example\\.com",
            "X-To: matchme@example.com\n",
            false,
        ),
        (
            "^FROM_DAEMON",
            "From: MAILER-DAEMON@example.com\n",
            true,
        ),
        ("^FROM_DAEMON", "From: user@example.com\n", false),
        (
            "^FROM_MAILER",
            "From: postmaster@sendmail.example\n",
            true,
        ),
        (
            "^FROM_MAILER",
            "From: regularuser@example.org\n",
            false,
        ),
    ] {
        let plan = compile(&format!(":0\n* {pattern}\nmaildir:matched\n"));
        let message = format!("{headers}Subject: macro probe\n\nbody");
        let HeaderEvaluation::Decided(delivery) = plan.evaluate_headers(&head(message.as_bytes()))
        else {
            panic!("expected a header decision for {pattern:?} and {headers:?}");
        };
        assert_eq!(
            !delivery.deliveries().is_empty(),
            expected,
            "{pattern:?} against {headers:?}"
        );
    }
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
fn failed_header_match_defers_to_reachable_body_recipe() {
    let plan = compile(":0\n* ^Subject: wanted$\nmaildir:wanted\n\n:0 B\n* needle\nmaildir:body\n");
    let result = plan.evaluate_headers(&head(b"Subject: other\n\nbody"));

    let HeaderEvaluation::NeedsMessage(continuation) = result else {
        panic!("expected deferred evaluation");
    };
    assert!(continuation.requirements().needs_body_contents);
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
