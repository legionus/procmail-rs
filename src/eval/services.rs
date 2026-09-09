// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

pub trait RecipeLockGuard {}

impl<T> RecipeLockGuard for T {}

pub trait OrderedExecutionHost {
    type Error;
    type Trace: TraceSink;

    fn trace(&mut self) -> &mut Self::Trace;
    fn deliver(
        &mut self,
        destination: &Destination,
        message: &[u8],
        output_ending: OutputEnding,
        lock: Option<&str>,
        runtime: &mut RuntimeVariables,
    ) -> Result<(), DeliveryAttemptError<Self::Error>>;
    fn external_action(
        &mut self,
        action: &PipeAction,
        options: RecipeOptions,
        lock: Option<&str>,
        input: ExternalActionInput<'_>,
        runtime: &mut RuntimeVariables,
    ) -> Result<Option<Message>, DeliveryAttemptError<Self::Error>>;
    fn capture(
        &mut self,
        command: &str,
        input: &[u8],
        output_ending: OutputEnding,
        options: Option<RecipeOptions>,
        limit: usize,
        runtime: &mut RuntimeVariables,
    ) -> Result<CapturedCommand, DeliveryAttemptError<Self::Error>>;
    fn external_condition(
        &mut self,
        command: &str,
        input: &[u8],
        runtime: &mut RuntimeVariables,
    ) -> Result<bool, DeliveryAttemptError<Self::Error>>;
    fn replace_global_lock(
        &mut self,
        path: &str,
        runtime: &mut RuntimeVariables,
    ) -> Result<(), Self::Error>;
    fn acquire_local_lock(
        &mut self,
        path: &str,
        runtime: &mut RuntimeVariables,
    ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<Self::Error>>;
    fn enter_copy_branch(&mut self) {}
    fn leave_copy_branch(&mut self) {}
    fn complete(
        &mut self,
        message: FinalMessage<'_>,
        runtime: &mut RuntimeVariables,
        state: CompletionState<'_, Self::Error>,
    );
}
