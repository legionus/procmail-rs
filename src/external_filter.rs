// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use crate::config::{ChildStatusMode, WriteErrorMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExit {
    Success,
    Failure,
    TimedOut,
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
        report_child_failure: child_exit != ChildExit::Success
            && status_mode == ChildStatusMode::Wait,
    }
}

#[cfg(test)]
mod tests;
