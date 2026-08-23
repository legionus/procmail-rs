// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

fn variable_event(name: &str) -> TraceEvent {
    TraceEvent::VariableAssigned {
        line: Some(7),
        name: TraceName::new(name).unwrap(),
        source: VariableSource::RcFile,
        value: None,
    }
}

#[test]
fn variable_event_contains_a_name_but_no_value_slot() {
    let event = variable_event("MAILBOX");

    let rendered = format!("{event:?}");
    assert!(rendered.contains("MAILBOX"));
    assert!(!rendered.contains("secret-value"));
}

#[test]
fn bounds_and_validates_variable_names() {
    assert!(TraceName::new("NAME_1").is_ok());
    assert!(TraceName::new("1NAME").is_err());
    assert!(TraceName::new(&"N".repeat(MAX_ASSIGNMENT_NAME_LEN + 1)).is_err());
}

#[test]
fn models_each_required_execution_decision() {
    let events = [
        TraceEvent::LastFolderUpdated,
        TraceEvent::ConditionEvaluated {
            recipe_line: 2,
            condition_line: 3,
            condition_index: 0,
            kind: ConditionKind::HeaderRegex,
            negated: false,
            matched: false,
        },
        TraceEvent::RecipeEvaluated {
            line: 2,
            decision: RecipeDecision::Skipped,
        },
        TraceEvent::Delivery {
            recipe_line: 5,
            destination: DestinationKind::Maildir,
            stage: DeliveryStage::Failed(FailureClass::Transient),
        },
        TraceEvent::ExternalCommand {
            recipe_line: 9,
            stage: ExternalCommandStage::Starting,
        },
    ];

    assert_eq!(events.len(), 5);
}

#[test]
fn memory_trace_preserves_order_and_stops_at_its_limit() {
    let mut trace = MemoryTrace::default();
    for line in 0..=MAX_MEMORY_TRACE_EVENTS {
        trace.record(TraceEvent::RecipeEvaluated {
            line,
            decision: RecipeDecision::Skipped,
        });
    }

    assert_eq!(trace.events().len(), MAX_MEMORY_TRACE_EVENTS);
    assert_eq!(
        trace.events().first(),
        Some(&TraceEvent::RecipeEvaluated {
            line: 0,
            decision: RecipeDecision::Skipped,
        })
    );
    assert_eq!(
        trace.events().last(),
        Some(&TraceEvent::RecipeEvaluated {
            line: MAX_MEMORY_TRACE_EVENTS - 1,
            decision: RecipeDecision::Skipped,
        })
    );
    assert!(trace.was_truncated());
}

#[test]
fn escapes_record_delimiters_control_bytes_and_non_utf8_input() {
    let hostile = b"line\nnext\r\t'\"\\\0\x1f\x7f\x80\xff";
    assert_eq!(
        EscapedBytes::new(hostile).to_string(),
        "line\\nnext\\r\\t\\'\\\"\\\\\\x00\\x1f\\x7f\\x80\\xff"
    );
    assert!(!EscapedBytes::new(hostile).to_string().contains('\n'));
}

#[test]
fn leaves_only_safe_printable_ascii_unescaped() {
    let printable = (b' '..=b'~')
        .filter(|byte| !matches!(byte, b'\\' | b'\'' | b'"'))
        .collect::<Vec<_>>();
    assert_eq!(
        EscapedBytes::new(&printable).to_string().as_bytes(),
        printable
    );
}

#[test]
fn bounded_writer_emits_complete_records_with_accounted_sizes() {
    let mut trace = BoundedTraceWriter::new(Vec::new());
    trace.record(variable_event("MAILBOX"));
    trace.record(TraceEvent::LastFolderUpdated);

    assert_eq!(trace.event_count(), 2);
    assert_eq!(trace.stop_reason(), None);
    assert_eq!(trace.byte_count(), trace.writer.len());
    let output = String::from_utf8(trace.into_inner()).unwrap();
    assert_eq!(output.lines().count(), 2);
    assert!(output.contains("name=\"MAILBOX\""));
}

#[test]
fn renders_stable_event_fixture_with_source_lines() {
    let events = [
        variable_event("BOX"),
        TraceEvent::ConditionEvaluated {
            recipe_line: 10,
            condition_line: 11,
            condition_index: 0,
            kind: ConditionKind::BodyRegex,
            negated: true,
            matched: false,
        },
        TraceEvent::RecipeEvaluated {
            line: 10,
            decision: RecipeDecision::Deferred,
        },
        TraceEvent::Delivery {
            recipe_line: 12,
            destination: DestinationKind::Maildir,
            stage: DeliveryStage::Failed(FailureClass::Transient),
        },
        TraceEvent::LastFolderUpdated,
        TraceEvent::ExternalCommand {
            recipe_line: 20,
            stage: ExternalCommandStage::Succeeded,
        },
    ];
    let mut trace = BoundedTraceWriter::new(Vec::new());
    for event in events {
        trace.record(event);
    }

    let rendered = String::from_utf8(trace.into_inner()).unwrap();
    assert_eq!(
        rendered,
        concat!(
            "event=variable-assigned line=7 name=\"BOX\" source=rc-file\n",
            "event=condition recipe_line=10 condition_line=11 condition_index=0 kind=body-regex negated=true matched=false\n",
            "event=recipe line=10 decision=deferred\n",
            "event=delivery recipe_line=12 destination=maildir stage=failed failure_class=transient\n",
            "event=last-folder-updated\n",
            "event=external-command recipe_line=20 stage=succeeded\n",
        )
    );
}

#[test]
fn bounded_writer_stops_before_exceeding_total_byte_limit() {
    let mut trace = BoundedTraceWriter::new(Vec::new());
    let event = variable_event(&"N".repeat(MAX_ASSIGNMENT_NAME_LEN));
    while trace.stop_reason().is_none() {
        trace.record(event.clone());
    }

    assert_eq!(trace.stop_reason(), Some(TraceStopReason::ByteLimit));
    assert!(trace.byte_count() <= MAX_TRACE_BYTES);
    assert_eq!(trace.byte_count(), trace.writer.len());
}

#[test]
fn bounded_writer_stops_at_event_count_limit() {
    let mut trace = BoundedTraceWriter::new(io::sink());
    for _ in 0..=MAX_TRACE_EVENTS {
        trace.record(TraceEvent::LastFolderUpdated);
    }

    assert_eq!(trace.event_count(), MAX_TRACE_EVENTS);
    assert_eq!(trace.stop_reason(), Some(TraceStopReason::EventLimit));
}

#[test]
fn bounded_text_rejects_a_record_before_partial_growth() {
    let mut output = BoundedText::new(4);
    assert!(output.write_str("1234").is_ok());
    assert!(output.write_str("5").is_err());
    assert_eq!(output.as_bytes(), b"1234");
}

#[test]
fn reads_procmail_style_trace_controls_in_statement_order() {
    let config = crate::config::parse(
            "MAILDIR=/mail\nVERBOSE=off\nLOGFILE=logs/first\nLOGDETAIL=metadata\nVERBOSE=YesPlease\nLOGFILE=$MAILDIR/log\nLOGDETAIL=values\n",
        )
        .unwrap()
        .expand()
        .unwrap();

    let settings = TraceConfig::from_config(&config).unwrap();
    assert!(settings.enabled());
    assert!(settings.verbose());
    assert_eq!(settings.logfile(), Some("/mail/log"));
    assert_eq!(settings.detail(), TraceDetail::Values);
}

#[test]
fn tracing_is_disabled_without_an_explicit_verbose_assignment() {
    let empty = crate::config::parse("").unwrap().expand().unwrap();
    let logfile_only = crate::config::parse("LOGFILE=/mail/filter.log\n")
        .unwrap()
        .expand()
        .unwrap();

    for config in [&empty, &logfile_only] {
        let settings = TraceConfig::from_config(config).unwrap();
        assert!(!settings.verbose());
        assert!(!settings.enabled());
    }
    assert_eq!(
        TraceConfig::from_config(&logfile_only).unwrap().logfile(),
        Some("/mail/filter.log")
    );
    assert_eq!(
        TraceConfig::from_config(&logfile_only)
            .unwrap()
            .failure_policy(),
        LogFailurePolicy::Advisory
    );
}

#[test]
fn accepts_documented_procmail_boolean_prefixes() {
    for value in ["1", "9anything", "on", "yes", "true", "enable"] {
        assert_eq!(parse_procmail_boolean(value), Some(true), "{value:?}");
    }
    for value in ["0", "0anything", "off", "no", "false", "disable"] {
        assert_eq!(parse_procmail_boolean(value), Some(false), "{value:?}");
    }
    for value in ["", "maybe", "-1"] {
        assert_eq!(parse_procmail_boolean(value), None, "{value:?}");
    }
}

#[test]
fn rejects_invalid_verbose_value_with_its_source_line() {
    let config = crate::config::parse("VERBOSE=maybe\n:0\nmaildir:inbox\n")
        .unwrap()
        .expand()
        .unwrap();

    let error = TraceConfig::from_config(&config).unwrap_err();
    assert_eq!(error.line, 1);
    assert_eq!(error.name, "VERBOSE");
}

#[test]
fn rejects_unknown_log_detail_with_its_source_line() {
    let config = crate::config::parse("LOGDETAIL=everything\n:0\nmaildir:inbox\n")
        .unwrap()
        .expand()
        .unwrap();

    let error = TraceConfig::from_config(&config).unwrap_err();
    assert_eq!(error.line, 1);
    assert_eq!(error.name, "LOGDETAIL");
}

#[test]
fn high_detail_writer_escapes_and_truncates_variable_values() {
    let mut trace = BoundedTraceWriter::with_detail(Vec::new(), TraceDetail::Values);
    let value = [b"secret\n".as_slice(), &vec![b'x'; MAX_TRACE_VALUE_SIZE]].concat();
    trace.record(TraceEvent::VariableAssigned {
        line: Some(1),
        name: TraceName::new("TOKEN").unwrap(),
        source: VariableSource::RcFile,
        value: Some(TraceValue::new(&value)),
    });

    let rendered = String::from_utf8(trace.into_inner()).unwrap();
    assert!(rendered.contains("value=\"secret\\n"));
    assert!(rendered.contains("value_truncated=true"));
    assert_eq!(rendered.lines().count(), 1);
}
