// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

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
fn metadata_trace_excludes_capture_command_output_and_message_values() {
    let config = config::parse(":0 h\nCAPTURED=| command-secret\n:0\nmaildir:selected\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let mut head = head(b"X-Secret: header-secret\n\nbody-secret");
    let mut runtime = RuntimeVariables::default();
    let mut trace = BoundedTraceWriter::new(Vec::new());

    let result = plan.evaluate_headers_editing_with_capture_trace(
        &mut head,
        &mut runtime,
        &mut trace,
        &mut |_, _, _, _, _, _, _| {
            Ok::<_, DeliveryAttemptError<&str>>(CapturedCommand::new(b"output-secret".to_vec()))
        },
    );
    assert!(matches!(result, Ok(HeaderEvaluation::Decided(_))));

    let rendered = String::from_utf8(trace.into_inner()).unwrap();
    for private in [
        "command-secret",
        "output-secret",
        "header-secret",
        "body-secret",
    ] {
        assert!(!rendered.contains(private), "leaked {private:?}");
    }
    assert!(rendered.contains("\"name\":\"CAPTURED\""));
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
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
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
                expression: None,
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
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
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
    assert!(rendered.contains("\"name\":\"TOKEN\""));
    assert!(rendered.contains("\"event\":\"condition\""));
    assert!(rendered.contains("\"event\":\"recipe\""));
}

#[test]
fn rendered_trace_identifies_header_operations_without_values() {
    let config = config::parse(
        ":0\nheaders {\n add X-Private-Edited-Name private-edited-value\n}\n:0\nmaildir:selected\n",
    )
    .unwrap()
    .expand(&[])
    .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let mut head = head(b"Subject: test\n\nbody");
    let mut runtime = RuntimeVariables::default();
    let mut trace = BoundedTraceWriter::new(Vec::new());

    let result = plan.evaluate_headers_editing_with_trace(&mut head, &mut runtime, &mut trace);
    assert!(matches!(result, HeaderEvaluation::Decided(_)));

    let rendered = String::from_utf8(trace.into_inner()).unwrap();
    assert!(rendered.contains("\"event\":\"header-operation\""));
    assert!(rendered.contains("X-Private-Edited-Name"));
    assert!(!rendered.contains("private-edited-value"));
    assert!(rendered.contains("\"event\":\"recipe\""));
}

#[test]
fn variable_values_require_an_explicit_high_detail_sink() {
    let config = config::parse("TOKEN=secret-value\n:0\nmaildir:inbox\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let plan = ExecutionPlan::compile(&config, None);
    let mut runtime = RuntimeVariables::default();
    let mut trace = BoundedTraceWriter::with_detail(Vec::new(), crate::trace::TraceDetail::Values);

    let result =
        plan.evaluate_headers_with_trace(&head(b"Subject: test\n\nbody"), &mut runtime, &mut trace);
    assert!(matches!(result, HeaderEvaluation::Decided(_)));

    let rendered = String::from_utf8(trace.into_inner()).unwrap();
    assert!(rendered.contains("\"value\":\"secret-value\""));
}

#[test]
fn explains_plan_shape_without_private_configuration_values() {
    let config = config::parse(
            "PRIVATE_TOKEN=do-not-print\n:0 HBc\n* ! private-pattern\nmaildir:${LASTFOLDER:-private-path}\n",
        )
        .unwrap()
        .expand(&[])
        .unwrap();
    let explanation = ExecutionPlan::compile(&config, None).explain();

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
        ":0\nheaders {\n remove X-Secret-Remove\n set X-Secret-Set secret-set-value\n add X-Secret-Add secret-add-value\n add X-Other-Add other-add-value\n prepend X-Secret-Prepend secret-prepend-value\n rename X-Secret-Old to X-Secret-New\n extract raw X-Secret-Value into PRIVATE_VALUE\n}\n",
    )
    .unwrap()
    .expand(&[])
    .unwrap();
    let explanation = ExecutionPlan::compile(&config, None).explain();
    let [recipe] = explanation.recipes() else {
        panic!("expected one recipe");
    };

    assert_eq!(recipe.action(), ActionKindExplanation::Headers);
    let operations = recipe.header_operations().unwrap();
    assert_eq!(operations.remove_count(), 1);
    assert_eq!(operations.set_count(), 1);
    assert_eq!(operations.add_count(), 2);
    assert_eq!(operations.prepend_count(), 1);
    assert_eq!(operations.rename_count(), 1);
    assert_eq!(operations.extract_count(), 1);
    let rendered = format!("{explanation:?}");
    for private in [
        "X-Secret-Remove",
        "X-Secret-Set",
        "X-Secret-Add",
        "X-Other-Add",
        "X-Secret-Prepend",
        "X-Secret-Old",
        "X-Secret-New",
        "X-Secret-Value",
        "PRIVATE_VALUE",
        "secret-set-value",
        "secret-add-value",
        "other-add-value",
        "secret-prepend-value",
    ] {
        assert!(!rendered.contains(private), "leaked {private:?}");
    }
}

#[test]
fn explains_static_null_destination_as_discard() {
    let config = config::parse(":0\n/dev/null\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let explanation = ExecutionPlan::compile(&config, None).explain();
    let [recipe] = explanation.recipes() else {
        panic!("expected one recipe");
    };

    assert_eq!(recipe.action(), ActionKindExplanation::Discard);
    assert!(!recipe.defers_destination());
}
