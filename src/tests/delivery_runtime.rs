// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::path::PathBuf;

use super::{
    OperationalError, PublicationAttempt, PublicationDestinations, PublicationTracker,
    apply_publication, derive_implicit_lockfile_path,
};
use procmail_rs::config::{self, Destination};
use procmail_rs::delivery::{DeliveryFailureClass, PublishedDelivery};
use procmail_rs::runtime::{PublicationResult, RuntimeVariables};
use procmail_rs::trace::{
    DeliveryStage, DestinationKind as TraceDestinationKind, FailureClass, MemoryTrace, TraceEvent,
};

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
        trace.events(),
        [
            TraceEvent::Delivery {
                recipe_line: 0,
                destination: TraceDestinationKind::Maildir,
                stage: DeliveryStage::Published,
            },
            TraceEvent::LastFolderUpdated,
        ]
    );
}

#[test]
fn publication_effects_distinguish_failures_before_and_after_visibility() {
    let destination = Destination::Maildir("requested".into());
    let mut runtime = RuntimeVariables::default();
    let mut trace = MemoryTrace::default();
    let before = apply_publication(
        PublicationAttempt::failed(
            None,
            DeliveryFailureClass::Permanent,
            OperationalError::PermanentDestination("before".to_owned()),
        ),
        PublicationDestinations::One(&destination),
        &mut runtime,
        &mut trace,
    )
    .unwrap_err();
    assert!(before.can_handle);
    assert_eq!(runtime.last_folder(), None);
    assert_eq!(
        trace.events(),
        [TraceEvent::Delivery {
            recipe_line: 0,
            destination: TraceDestinationKind::Maildir,
            stage: DeliveryStage::Failed(FailureClass::Permanent),
        }]
    );

    let published = PublishedDelivery::new(PathBuf::from("visible/new/message"));
    let after = apply_publication(
        PublicationAttempt::failed(
            Some(PublicationResult::Delivery(&published)),
            DeliveryFailureClass::Retryable,
            OperationalError::TemporaryDelivery("after".to_owned()),
        ),
        PublicationDestinations::One(&destination),
        &mut runtime,
        &mut MemoryTrace::default(),
    )
    .unwrap_err();
    assert!(!after.can_handle);
    assert_eq!(runtime.last_folder(), Some("visible/new/message"));
}
