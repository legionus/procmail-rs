// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;
use std::path::Path;

#[test]
fn source_path_is_absent_from_metadata_records_and_rendered_bytes() {
    let source = SourceLocation::for_file(Path::new("/private-rc-sentinel\n\".rc"), 7).unwrap();
    let record = variable_event("VALUE").at(&source, TraceDetail::Metadata);
    assert_eq!(record.location.line(), 7);
    assert_eq!(record.location.file(), None);
    assert!(!format!("{record:?}").contains("private-rc-sentinel"));

    for format in [TraceFormat::Text, TraceFormat::Json] {
        let mut writer = BoundedTraceWriter::formatted(Vec::new(), TraceDetail::Metadata, format);
        // A metadata sink also discards a source supplied by a values caller.
        writer.record(variable_event("VALUE").at(&source, TraceDetail::Values));
        let rendered = String::from_utf8(writer.into_inner()).unwrap();
        assert!(!rendered.contains("private-rc-sentinel"));
        assert!(!rendered.contains("rc_file"));
        assert_eq!(rendered.lines().count(), 1);
    }
}

#[test]
fn source_path_uses_existing_byte_escaping_in_both_renderers() {
    let source = SourceLocation::for_file(Path::new("/private\n\"\\rc"), 7).unwrap();

    for (format, expected) in [
        (TraceFormat::Text, "[rc \"/private\\n\\\"\\\\rc\":7]"),
        (TraceFormat::Json, "\"rc_file\":\"/private\\n\\\"\\\\rc\""),
    ] {
        let mut writer = BoundedTraceWriter::formatted(Vec::new(), TraceDetail::Values, format);
        writer.record_at(&source, variable_event("VALUE"));
        let rendered = String::from_utf8(writer.into_inner()).unwrap();
        assert!(rendered.contains(expected), "{rendered}");
        assert_eq!(rendered.lines().count(), 1);
    }
}

#[test]
fn source_prefix_limit_and_utf8_boundary_have_explicit_truncation() {
    for length in [
        MAX_TRACE_VALUE_SIZE - 1,
        MAX_TRACE_VALUE_SIZE,
        MAX_TRACE_VALUE_SIZE + 1,
    ] {
        let path = "x".repeat(length);
        let source = SourceLocation::for_file(Path::new(&path), 7).unwrap();
        let record = variable_event("VALUE").at(&source, TraceDetail::Values);
        assert_eq!(
            record.location.file().unwrap().len(),
            length.min(MAX_TRACE_VALUE_SIZE)
        );
        assert_eq!(
            record.location.is_truncated(),
            length > MAX_TRACE_VALUE_SIZE
        );

        for format in [TraceFormat::Text, TraceFormat::Json] {
            let mut writer = BoundedTraceWriter::formatted(Vec::new(), TraceDetail::Values, format);
            writer.record(record.clone());
            assert!(writer.stop_reason().is_none());
            let rendered = String::from_utf8(writer.into_inner()).unwrap();
            let marker = if format == TraceFormat::Json {
                "\"rc_file_truncated\":true"
            } else {
                "[truncated]"
            };
            assert_eq!(rendered.contains(marker), length > MAX_TRACE_VALUE_SIZE);
        }
    }

    let path = format!("{}é", "x".repeat(MAX_TRACE_VALUE_SIZE - 1));
    let source = SourceLocation::for_file(Path::new(&path), 7).unwrap();
    let record = variable_event("VALUE").at(&source, TraceDetail::Values);
    assert_eq!(
        record.location.file().unwrap().len(),
        MAX_TRACE_VALUE_SIZE - 1
    );
    assert!(record.location.is_truncated());
}

#[test]
fn final_delivery_abstract_keeps_the_published_recipe_source() {
    let source = SourceLocation::for_file(Path::new("included.rc"), 7).unwrap();
    let mut writer = BoundedTraceWriter::runtime_formatted(
        Vec::new(),
        TraceDetail::Values,
        TraceFormat::Json,
        false,
        LogAbstractMode::Yes,
    );
    writer.record_at(
        &source,
        TraceEvent::Delivery {
            recipe_line: 7,
            destination: DestinationKind::Maildir,
            stage: DeliveryStage::Published,
            path: None,
        },
    );
    writer.finish();
    let rendered = String::from_utf8(writer.into_inner()).unwrap();
    assert_eq!(rendered.lines().count(), 1);
    assert!(rendered.contains("\"event\":\"delivery-abstract\""));
    assert!(rendered.contains("\"rc_file\":\"included.rc\""));
}

#[test]
fn source_annotation_does_not_modify_literal_text_log_output() {
    let source = SourceLocation::for_file(Path::new("included.rc"), 7).unwrap();
    let mut writer =
        BoundedTraceWriter::formatted(Vec::new(), TraceDetail::Values, TraceFormat::Text);
    writer.record_at(
        &source,
        TraceEvent::Log {
            line: 7,
            value: TraceValue::new(b"literal"),
        },
    );
    assert_eq!(writer.into_inner(), b"literal");
}

#[test]
fn source_annotation_cannot_bypass_the_complete_record_limit() {
    let source =
        SourceLocation::for_file(Path::new(&"\n".repeat(MAX_TRACE_VALUE_SIZE)), 7).unwrap();

    for format in [TraceFormat::Text, TraceFormat::Json] {
        let mut writer = BoundedTraceWriter::formatted(Vec::new(), TraceDetail::Values, format);
        writer.record_at(
            &source,
            TraceEvent::VariableAssigned {
                line: Some(7),
                name: TraceName::new("VALUE").unwrap(),
                source: VariableSource::RcFile,
                value: Some(TraceValue::new(&vec![0xff; MAX_TRACE_VALUE_SIZE])),
            },
        );
        assert_eq!(writer.stop_reason(), Some(TraceStopReason::EventSizeLimit));
        assert!(writer.into_inner().is_empty());
    }
}
