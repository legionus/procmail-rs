// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn wait_modes_reject_failed_status_but_only_lowercase_reports_it() {
    for (mode, report) in [
        (ChildStatusMode::Wait, true),
        (ChildStatusMode::WaitQuietly, false),
    ] {
        let decision = decide_filter(
            mode,
            WriteErrorMode::Fail,
            InputWrite::Complete,
            FilterOutput::CompleteAndValid,
            ChildExit::Failure,
        );
        assert!(!decision.succeeded());
        assert!(!decision.replace_message());
        assert_eq!(decision.report_child_failure(), report);
    }
}

#[test]
fn ignored_status_can_accept_complete_output() {
    let decision = decide_filter(
        ChildStatusMode::Ignore,
        WriteErrorMode::Fail,
        InputWrite::Complete,
        FilterOutput::CompleteAndValid,
        ChildExit::Failure,
    );

    assert!(decision.succeeded());
    assert!(decision.replace_message());
    assert!(!decision.report_child_failure());
}

#[test]
fn invalid_or_incomplete_output_never_replaces_the_message() {
    for status_mode in [
        ChildStatusMode::Ignore,
        ChildStatusMode::Wait,
        ChildStatusMode::WaitQuietly,
    ] {
        let decision = decide_filter(
            status_mode,
            WriteErrorMode::Ignore,
            InputWrite::Complete,
            FilterOutput::Failed,
            ChildExit::Success,
        );
        assert!(!decision.succeeded());
        assert!(!decision.replace_message());
    }
}

#[test]
fn ignore_write_error_controls_only_the_stdin_failure() {
    let failed = decide_program(
        ChildStatusMode::Wait,
        WriteErrorMode::Fail,
        InputWrite::Failed,
        ChildExit::Success,
    );
    assert!(!failed.succeeded());

    let ignored = decide_program(
        ChildStatusMode::Wait,
        WriteErrorMode::Ignore,
        InputWrite::Failed,
        ChildExit::Success,
    );
    assert!(ignored.succeeded());
    assert!(!ignored.replace_message());
}

#[test]
fn timeout_uses_the_existing_child_status_modes() {
    for (mode, succeeded, report) in [
        (ChildStatusMode::Ignore, true, false),
        (ChildStatusMode::Wait, false, true),
        (ChildStatusMode::WaitQuietly, false, false),
    ] {
        let decision = decide_program(
            mode,
            WriteErrorMode::Ignore,
            InputWrite::Failed,
            ChildExit::TimedOut,
        );
        assert_eq!(decision.succeeded(), succeeded);
        assert_eq!(decision.report_child_failure(), report);
    }
}
