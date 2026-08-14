// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use crate::config::{ChildStatusMode, WriteErrorMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExit {
    Success,
    Failure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputWrite {
    Complete,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOutput {
    CompleteAndValid,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalActionDecision {
    succeeded: bool,
    replace_message: bool,
    report_child_failure: bool,
}

impl ExternalActionDecision {
    pub fn succeeded(self) -> bool {
        self.succeeded
    }

    pub fn replace_message(self) -> bool {
        self.replace_message
    }

    pub fn report_child_failure(self) -> bool {
        self.report_child_failure
    }
}

pub fn decide_filter(
    status_mode: ChildStatusMode,
    write_mode: WriteErrorMode,
    input_write: InputWrite,
    output: FilterOutput,
    child_exit: ChildExit,
) -> ExternalActionDecision {
    decide(
        status_mode,
        write_mode,
        input_write,
        Some(output),
        child_exit,
    )
}

pub fn decide_program(
    status_mode: ChildStatusMode,
    write_mode: WriteErrorMode,
    input_write: InputWrite,
    child_exit: ChildExit,
) -> ExternalActionDecision {
    decide(status_mode, write_mode, input_write, None, child_exit)
}

fn decide(
    status_mode: ChildStatusMode,
    write_mode: WriteErrorMode,
    input_write: InputWrite,
    output: Option<FilterOutput>,
    child_exit: ChildExit,
) -> ExternalActionDecision {
    let write_succeeded =
        input_write == InputWrite::Complete || write_mode == WriteErrorMode::Ignore;
    let status_succeeded =
        child_exit == ChildExit::Success || status_mode == ChildStatusMode::Ignore;
    let output_succeeded = output != Some(FilterOutput::Failed);
    let succeeded = write_succeeded && status_succeeded && output_succeeded;

    ExternalActionDecision {
        succeeded,
        replace_message: succeeded && output == Some(FilterOutput::CompleteAndValid),
        report_child_failure: child_exit == ChildExit::Failure
            && status_mode == ChildStatusMode::Wait,
    }
}

#[cfg(test)]
mod tests {
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
}
