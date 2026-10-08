// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::path::PathBuf;

use super::{
    OperationalError, PublicationAttempt, PublicationDestinations, PublicationTracker,
    apply_publication, derive_implicit_lockfile_path, report_stream_failure,
};
use procmail_rs::config::{self, Destination};
use procmail_rs::delivery::{DeliveryFailure, DeliveryOperation, PublishedDelivery};
use procmail_rs::runtime::{PublicationResult, RuntimeVariables};
use procmail_rs::trace::{
    DeliveryStage, DestinationKind as TraceDestinationKind, MemoryTrace, TraceEvent,
};

fn two_destination_plan() -> procmail_rs::eval::DeliveryPlan {
    use procmail_rs::eval::{
        CapturedCommand, DeliveryAttemptError, ExecutionPlan, HeaderEvaluation,
    };
    use procmail_rs::limits::MessageLimits;
    use procmail_rs::message::Message;

    let config = config::parse(":0c\nmaildir:/first\n:0\nmaildir:/second\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let execution = ExecutionPlan::compile(&config, None);
    let mut head =
        Message::read_headers(&mut std::io::Cursor::new(b"\n"), MessageLimits::default()).unwrap();
    match execution
        .evaluate_headers_editing_with_capture_trace(
            &mut head,
            &mut RuntimeVariables::default(),
            &mut MemoryTrace::default(),
            &mut |_, _, _, _, _, _, _| -> Result<CapturedCommand, DeliveryAttemptError<()>> {
                panic!("plain destinations must not execute commands")
            },
        )
        .unwrap()
    {
        HeaderEvaluation::Decided(plan) => plan,
        _ => panic!("plain destinations must not defer evaluation"),
    }
}

#[test]
fn sync_failure_is_traced_on_the_published_sink_not_the_next_sink() {
    let plan = two_destination_plan();
    let published = PublishedDelivery::new(PathBuf::from("/first/new/message"));
    let failure = DeliveryFailure::from_io(
        DeliveryOperation::SyncDirectory,
        &std::io::Error::from(std::io::ErrorKind::StorageFull),
        true,
    );
    let mut trace = MemoryTrace::default();
    let mut runtime = RuntimeVariables::default();
    let error = apply_publication(
        PublicationAttempt::failed(
            Some(PublicationResult::Delivery(&published)),
            failure,
            0,
            "sync error".to_owned(),
        ),
        PublicationDestinations::Plan(plan.deliveries()),
        &mut runtime,
        &mut trace,
    )
    .unwrap_err();
    assert!(!error.can_handle);
    assert_eq!(error.error.exit_code(), 75);
    assert_eq!(runtime.last_folder(), Some("/first/new/message"));
    assert!(
        matches!(trace.records().last().map(|record| &record.event), Some(TraceEvent::Delivery {
        recipe_line: 2, stage: DeliveryStage::Failure(actual), ..
    }) if *actual == failure)
    );
    assert!(
        !trace
            .records()
            .iter()
            .map(|record| &record.event)
            .any(|event| matches!(event, TraceEvent::Delivery { recipe_line: 4, .. }))
    );
}

struct WriteFailureSink;

impl std::io::Write for WriteFailureSink {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "private-error-sentinel",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl procmail_rs::delivery::PendingSink for WriteFailureSink {
    fn commit(
        self: Box<Self>,
    ) -> Result<PublishedDelivery, procmail_rs::delivery::SinkCommitError> {
        panic!("failed streaming must not commit")
    }

    fn abort(self: Box<Self>) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn streaming_write_failure_keeps_delivery_status_and_sink_location() {
    use procmail_rs::delivery::PendingFanout;
    use procmail_rs::limits::MessageLimits;
    use procmail_rs::message::Message;

    let plan = two_destination_plan();
    let mut reader =
        std::io::Cursor::new(b"Subject: private-header-sentinel\n\nprivate-body-sentinel");
    let head = Message::read_headers(&mut reader, MessageLimits::default()).unwrap();
    let pending = PendingFanout::new(vec![Box::new(WriteFailureSink)]).unwrap();
    let error = match pending.stream(head, &mut reader) {
        Err(error) => error,
        Ok(_) => panic!("failed streaming was accepted"),
    };
    let mut trace = MemoryTrace::default();
    let error = report_stream_failure(error, plan.deliveries(), &mut trace);
    assert_eq!(error.exit_code(), 75);
    assert!(matches!(error, OperationalError::Delivery { failure, .. }
        if failure.operation == DeliveryOperation::Write && !failure.published));
    assert!(matches!(
        trace
            .records()
            .iter()
            .map(|record| record.event.clone())
            .collect::<Vec<_>>()
            .as_slice(),
        [TraceEvent::Delivery {
            recipe_line: 2,
            stage: DeliveryStage::Failure(_),
            path: None,
            ..
        }]
    ));
    let rendered = format!("{:?}", trace.records());
    for secret in [
        "private-error-sentinel",
        "private-header-sentinel",
        "private-body-sentinel",
    ] {
        assert!(!rendered.contains(secret));
    }
}

#[test]
fn implicit_lockfile_path_enforces_the_complete_path_limit() {
    let at_limit = "x".repeat(config::MAX_PATH_EXPRESSION_LEN - 1);
    assert_eq!(
        derive_implicit_lockfile_path("d", &at_limit).unwrap().len(),
        config::MAX_PATH_EXPRESSION_LEN
    );

    let above_limit = "x".repeat(config::MAX_PATH_EXPRESSION_LEN);
    let error = derive_implicit_lockfile_path("d", &above_limit).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("implicit lockfile path exceeds the hard limit")
    );
    assert_eq!(
        derive_implicit_lockfile_path("mailbox", "").unwrap(),
        "mailbox"
    );
}

#[test]
fn publication_tracker_reports_copies_and_resets_after_completion() {
    let mut tracker = PublicationTracker::default();
    tracker.record(2, false).unwrap();
    let error = tracker.finish().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("published 2 copy destination(s)")
    );

    tracker.record(1, true).unwrap();
    assert!(tracker.finish().is_ok());
    assert_eq!(tracker.published, 0);
    assert!(!tracker.original_delivered);
}

#[test]
fn publication_tracker_rejects_count_overflow() {
    let mut tracker = PublicationTracker::default();
    tracker.record(usize::MAX, false).unwrap();
    let error = tracker.record(1, false).unwrap_err();
    assert!(error.to_string().contains("count overflows"));
}

#[test]
fn publication_effects_use_the_visible_backend_result() {
    let destination = Destination::Maildir("requested".into());
    let published = PublishedDelivery::new(PathBuf::from("visible/new/message"));
    let mut runtime = RuntimeVariables::default();
    let mut trace = MemoryTrace::default();

    let count = match apply_publication(
        PublicationAttempt::published(PublicationResult::Delivery(&published)),
        PublicationDestinations::One(&destination),
        &mut runtime,
        &mut trace,
    ) {
        Ok(count) => count,
        Err(_) => panic!("successful publication report was rejected"),
    };

    assert_eq!(count, 1);
    assert_eq!(runtime.last_folder(), Some("visible/new/message"));
    assert_eq!(
        trace
            .records()
            .iter()
            .map(|record| record.event.clone())
            .collect::<Vec<_>>()
            .as_slice(),
        [
            TraceEvent::Delivery {
                recipe_line: 0,
                destination: TraceDestinationKind::Maildir,
                stage: DeliveryStage::Published,
                path: None,
            },
            TraceEvent::LastFolderUpdated,
        ]
    );
}

#[test]
fn publication_effects_distinguish_failures_before_and_after_visibility() {
    let failure = DeliveryFailure::from_io(
        DeliveryOperation::Publish,
        &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        false,
    );
    let destination = Destination::Maildir("requested".into());
    let mut runtime = RuntimeVariables::default();
    let mut trace = MemoryTrace::default();
    let before = apply_publication(
        PublicationAttempt::failed(None, failure, 0, "before".to_owned()),
        PublicationDestinations::One(&destination),
        &mut runtime,
        &mut trace,
    )
    .unwrap_err();
    assert!(before.can_handle);
    assert_eq!(runtime.last_folder(), None);
    assert_eq!(
        trace
            .records()
            .iter()
            .map(|record| record.event.clone())
            .collect::<Vec<_>>()
            .as_slice(),
        [TraceEvent::Delivery {
            recipe_line: 0,
            destination: TraceDestinationKind::Maildir,
            stage: DeliveryStage::Failure(failure),
            path: None,
        }]
    );

    let published = PublishedDelivery::new(PathBuf::from("visible/new/message"));
    let failure = DeliveryFailure::from_io(
        DeliveryOperation::SyncDirectory,
        &std::io::Error::from(std::io::ErrorKind::StorageFull),
        true,
    );
    let mut after_trace = MemoryTrace::default();
    let after = apply_publication(
        PublicationAttempt::failed(
            Some(PublicationResult::Delivery(&published)),
            failure,
            0,
            "after".to_owned(),
        ),
        PublicationDestinations::One(&destination),
        &mut runtime,
        &mut after_trace,
    )
    .unwrap_err();
    assert!(!after.can_handle);
    assert_eq!(runtime.last_folder(), Some("visible/new/message"));
    assert!(
        matches!(after_trace.records().last().map(|record| &record.event), Some(TraceEvent::Delivery {
        stage: DeliveryStage::Failure(actual), ..
    }) if *actual == failure)
    );
}
