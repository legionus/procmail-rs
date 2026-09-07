// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use crate::config::{ChildStatusMode, WriteErrorMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExit {
    Success,
    ExitFailure,
    Signaled,
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
pub enum CommandOutcomePolicy {
    Condition,
    ExpansionCapture,
    RecipeCapture {
        child_status: ChildStatusMode,
        write_errors: WriteErrorMode,
    },
    Pipe {
        child_status: ChildStatusMode,
        write_errors: WriteErrorMode,
    },
    Filter {
        child_status: ChildStatusMode,
        write_errors: WriteErrorMode,
        output: FilterOutput,
    },
    Trap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandOutcome {
    input_write: InputWrite,
    child_exit: ChildExit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDecision {
    accepted: bool,
    replace_message: bool,
    report_child_failure: bool,
}

impl CommandOutcome {
    pub fn new(input_write: InputWrite, child_exit: ChildExit) -> Self {
        Self {
            input_write,
            child_exit,
        }
    }

    pub fn child_exit(self) -> ChildExit {
        self.child_exit
    }

    pub fn input_write(self) -> InputWrite {
        self.input_write
    }

    pub fn decide(self, policy: CommandOutcomePolicy) -> CommandDecision {
        // Each command form deliberately selects its own policy here. This
        // keeps the shared status rules consistent without making condition,
        // capture, pipe, filter, and TRAP callers share their distinct output
        // validation or side effects.
        match policy {
            CommandOutcomePolicy::Condition => {
                CommandDecision::new(self.child_exit == ChildExit::Success, false, false)
            }
            CommandOutcomePolicy::ExpansionCapture => CommandDecision::new(
                self.child_exit != ChildExit::TimedOut,
                false,
                self.child_exit == ChildExit::TimedOut,
            ),
            CommandOutcomePolicy::RecipeCapture {
                child_status,
                write_errors,
            } => {
                let mut decision = self.recipe_decision(child_status, write_errors);
                if self.child_exit == ChildExit::TimedOut {
                    decision.accepted = false;
                    decision.report_child_failure = true;
                }
                decision
            }
            CommandOutcomePolicy::Pipe {
                child_status,
                write_errors,
            } => self.recipe_decision(child_status, write_errors),
            CommandOutcomePolicy::Filter {
                child_status,
                write_errors,
                output,
            } => {
                let mut decision = self.recipe_decision(child_status, write_errors);
                decision.accepted &= output == FilterOutput::CompleteAndValid;
                decision.replace_message = decision.accepted;
                decision
            }
            CommandOutcomePolicy::Trap => {
                CommandDecision::new(self.child_exit == ChildExit::Success, false, false)
            }
        }
    }

    fn recipe_decision(
        self,
        child_status: ChildStatusMode,
        write_errors: WriteErrorMode,
    ) -> CommandDecision {
        let write_succeeded =
            self.input_write == InputWrite::Complete || write_errors == WriteErrorMode::Ignore;
        let status_succeeded =
            self.child_exit == ChildExit::Success || child_status == ChildStatusMode::Ignore;

        CommandDecision::new(
            write_succeeded && status_succeeded,
            false,
            self.child_exit != ChildExit::Success && child_status == ChildStatusMode::Wait,
        )
    }
}

impl CommandDecision {
    fn new(accepted: bool, replace_message: bool, report_child_failure: bool) -> Self {
        Self {
            accepted,
            replace_message,
            report_child_failure,
        }
    }

    pub fn accepted(self) -> bool {
        self.accepted
    }

    pub fn replace_message(self) -> bool {
        self.replace_message
    }

    pub fn report_child_failure(self) -> bool {
        self.report_child_failure
    }
}

#[cfg(test)]
#[path = "tests/external_command.rs"]
mod tests;
