// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

fn outcome(input_write: InputWrite, child_exit: ChildExit) -> CommandOutcome {
    CommandOutcome::new(input_write, child_exit)
}

#[test]
fn condition_uses_only_successful_child_status() {
    assert!(
        outcome(InputWrite::Failed, ChildExit::Success)
            .decide(CommandOutcomePolicy::Condition)
            .accepted()
    );
    for child_exit in [
        ChildExit::ExitFailure,
        ChildExit::Signaled,
        ChildExit::TimedOut,
    ] {
        assert!(
            !outcome(InputWrite::Complete, child_exit)
                .decide(CommandOutcomePolicy::Condition)
                .accepted()
        );
    }
}

#[test]
fn expansion_capture_rejects_only_timeout() {
    for child_exit in [
        ChildExit::Success,
        ChildExit::ExitFailure,
        ChildExit::Signaled,
    ] {
        assert!(
            outcome(InputWrite::Failed, child_exit)
                .decide(CommandOutcomePolicy::ExpansionCapture)
                .accepted()
        );
    }
    assert!({
        let decision = outcome(InputWrite::Complete, ChildExit::TimedOut)
            .decide(CommandOutcomePolicy::ExpansionCapture);
        !decision.accepted() && decision.report_child_failure()
    });
}

#[test]
fn recipe_capture_rejects_timeout_even_when_status_is_ignored() {
    let decision = outcome(InputWrite::Complete, ChildExit::TimedOut).decide(
        CommandOutcomePolicy::RecipeCapture {
            child_status: ChildStatusMode::Ignore,
            write_errors: WriteErrorMode::Ignore,
        },
    );

    assert!(!decision.accepted());
    assert!(decision.report_child_failure());
}

#[test]
fn wait_modes_reject_failed_status_but_only_lowercase_reports_it() {
    for (mode, report) in [
        (ChildStatusMode::Wait, true),
        (ChildStatusMode::WaitQuietly, false),
    ] {
        let decision =
            outcome(InputWrite::Complete, ChildExit::Signaled).decide(CommandOutcomePolicy::Pipe {
                child_status: mode,
                write_errors: WriteErrorMode::Fail,
            });
        assert!(!decision.accepted());
        assert_eq!(decision.report_child_failure(), report);
    }
}

#[test]
fn ignored_status_can_accept_complete_filter_output() {
    let decision = outcome(InputWrite::Complete, ChildExit::ExitFailure).decide(
        CommandOutcomePolicy::Filter {
            child_status: ChildStatusMode::Ignore,
            write_errors: WriteErrorMode::Fail,
            output: FilterOutput::CompleteAndValid,
        },
    );

    assert!(decision.accepted());
    assert!(decision.replace_message());
    assert!(!decision.report_child_failure());
}

#[test]
fn invalid_filter_output_never_replaces_the_message() {
    let decision =
        outcome(InputWrite::Complete, ChildExit::Success).decide(CommandOutcomePolicy::Filter {
            child_status: ChildStatusMode::Ignore,
            write_errors: WriteErrorMode::Ignore,
            output: FilterOutput::Failed,
        });

    assert!(!decision.accepted());
    assert!(!decision.replace_message());
}

#[test]
fn ignore_write_error_controls_only_the_stdin_failure() {
    for (write_errors, accepted) in [
        (WriteErrorMode::Fail, false),
        (WriteErrorMode::Ignore, true),
    ] {
        let decision =
            outcome(InputWrite::Failed, ChildExit::Success).decide(CommandOutcomePolicy::Pipe {
                child_status: ChildStatusMode::Wait,
                write_errors,
            });
        assert_eq!(decision.accepted(), accepted);
    }
}

#[test]
fn trap_reports_success_without_applying_recipe_flags() {
    assert!(
        outcome(InputWrite::Complete, ChildExit::Success)
            .decide(CommandOutcomePolicy::Trap)
            .accepted()
    );
    assert!(
        !outcome(InputWrite::Complete, ChildExit::ExitFailure)
            .decide(CommandOutcomePolicy::Trap)
            .accepted()
    );
}
