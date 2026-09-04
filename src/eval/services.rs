// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use super::*;

pub trait RecipeLockGuard {}

impl<T> RecipeLockGuard for T {}

pub(super) type DeliveryExecutor<'a, E, T> = dyn FnMut(
        &Destination,
        &[u8],
        OutputEnding,
        Option<&str>,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<(), DeliveryAttemptError<E>>
    + 'a;

pub(super) type ExternalActionExecutor<'a, E, T> = dyn FnMut(
        &PipeAction,
        RecipeOptions,
        Option<&str>,
        ExternalActionInput<'_>,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<Option<Message>, DeliveryAttemptError<E>>
    + 'a;

pub(super) type ExternalConditionExecutor<'a, E, T> = dyn FnMut(&str, &[u8], &mut RuntimeVariables, &mut T) -> Result<bool, DeliveryAttemptError<E>>
    + 'a;

pub(super) type CommandCaptureExecutor<'a, E, T> = dyn FnMut(
        &str,
        &[u8],
        OutputEnding,
        Option<RecipeOptions>,
        usize,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<CapturedCommand, DeliveryAttemptError<E>>
    + 'a;

pub(super) type GlobalLockExecutor<'a, E> =
    dyn FnMut(&str, &mut RuntimeVariables) -> Result<(), E> + 'a;

pub(super) type LocalLockExecutor<'a, E> = dyn FnMut(&str, &mut RuntimeVariables) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<E>>
    + 'a;

pub(super) type CompletionExecutor<'a, E, T> =
    dyn FnMut(FinalMessage<'_>, &mut RuntimeVariables, &mut T, CompletionState<'_, E>) + 'a;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionServicesError {
    missing: &'static str,
}

impl ExecutionServicesError {
    pub fn missing(&self) -> &'static str {
        self.missing
    }
}

impl fmt::Display for ExecutionServicesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "execution service '{}' is unavailable",
            self.missing
        )
    }
}

impl std::error::Error for ExecutionServicesError {}

pub struct ExecutionServices<'a, E, T> {
    pub(super) delivery: &'a mut DeliveryExecutor<'a, E, T>,
    pub(super) trace: &'a mut T,
    pub(super) external: Option<&'a mut ExternalActionExecutor<'a, E, T>>,
    pub(super) capture: Option<&'a mut CommandCaptureExecutor<'a, E, T>>,
    pub(super) external_condition: Option<&'a mut ExternalConditionExecutor<'a, E, T>>,
    pub(super) global_lock: Option<&'a mut GlobalLockExecutor<'a, E>>,
    pub(super) local_lock: Option<&'a mut LocalLockExecutor<'a, E>>,
    pub(super) completion: Option<&'a mut CompletionExecutor<'a, E, T>>,
}

impl<'a, E, T> ExecutionServices<'a, E, T> {
    pub fn new<D>(delivery: &'a mut D, trace: &'a mut T) -> Self
    where
        D: FnMut(
                &Destination,
                &[u8],
                OutputEnding,
                Option<&str>,
                &mut RuntimeVariables,
                &mut T,
            ) -> Result<(), DeliveryAttemptError<E>>
            + 'a,
    {
        Self {
            delivery,
            trace,
            external: None,
            capture: None,
            external_condition: None,
            global_lock: None,
            local_lock: None,
            completion: None,
        }
    }

    pub fn with_external_condition<C>(mut self, executor: &'a mut C) -> Self
    where
        C: FnMut(
                &str,
                &[u8],
                &mut RuntimeVariables,
                &mut T,
            ) -> Result<bool, DeliveryAttemptError<E>>
            + 'a,
    {
        self.external_condition = Some(executor);
        self
    }

    pub fn with_external_action<X>(mut self, executor: &'a mut X) -> Self
    where
        X: FnMut(
                &PipeAction,
                RecipeOptions,
                Option<&str>,
                ExternalActionInput<'_>,
                &mut RuntimeVariables,
                &mut T,
            ) -> Result<Option<Message>, DeliveryAttemptError<E>>
            + 'a,
    {
        self.external = Some(executor);
        self
    }

    pub fn with_capture<K>(mut self, executor: &'a mut K) -> Self
    where
        K: FnMut(
                &str,
                &[u8],
                OutputEnding,
                Option<RecipeOptions>,
                usize,
                &mut RuntimeVariables,
                &mut T,
            ) -> Result<CapturedCommand, DeliveryAttemptError<E>>
            + 'a,
    {
        self.capture = Some(executor);
        self
    }

    pub fn with_global_lock<G>(mut self, executor: &'a mut G) -> Self
    where
        G: FnMut(&str, &mut RuntimeVariables) -> Result<(), E> + 'a,
    {
        self.global_lock = Some(executor);
        self
    }

    pub fn with_local_lock<L>(mut self, executor: &'a mut L) -> Self
    where
        L: FnMut(
                &str,
                &mut RuntimeVariables,
            ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<E>>
            + 'a,
    {
        self.local_lock = Some(executor);
        self
    }

    pub fn with_completion<F>(mut self, executor: &'a mut F) -> Self
    where
        F: FnMut(FinalMessage<'_>, &mut RuntimeVariables, &mut T, CompletionState<'_, E>) + 'a,
    {
        self.completion = Some(executor);
        self
    }

    pub fn require_complete(self) -> Result<Self, ExecutionServicesError> {
        // The production filtering path must not discover a missing executor
        // partway through ordered evaluation, after commands or deliveries
        // may already have changed external state. Check every optional
        // service while the caller is still assembling the operation.
        for (name, present) in [
            ("external condition", self.external_condition.is_some()),
            ("external action", self.external.is_some()),
            ("command capture", self.capture.is_some()),
            ("global lock", self.global_lock.is_some()),
            ("local lock", self.local_lock.is_some()),
            ("completion", self.completion.is_some()),
        ] {
            if !present {
                return Err(ExecutionServicesError { missing: name });
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests;
