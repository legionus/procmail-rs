// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

type DeliveryExecutor<'a, E, T> = dyn FnMut(
        &Destination,
        &[u8],
        OutputEnding,
        Option<&str>,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<(), DeliveryAttemptError<E>>
    + Send
    + 'a;
type ExternalActionExecutor<'a, E, T> = dyn FnMut(
        &PipeAction,
        RecipeOptions,
        Option<&str>,
        ExternalActionInput<'_>,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<Option<Message>, DeliveryAttemptError<E>>
    + Send
    + 'a;
type ExternalConditionExecutor<'a, E, T> = dyn FnMut(&str, &[u8], &mut RuntimeVariables, &mut T) -> Result<bool, DeliveryAttemptError<E>>
    + Send
    + 'a;
type CommandCaptureExecutor<'a, E, T> = dyn FnMut(
        &str,
        &[u8],
        OutputEnding,
        Option<RecipeOptions>,
        usize,
        &mut RuntimeVariables,
        &mut T,
    ) -> Result<CapturedCommand, DeliveryAttemptError<E>>
    + Send
    + 'a;
type GlobalLockExecutor<'a, E> =
    dyn FnMut(&str, &mut RuntimeVariables) -> Result<(), E> + Send + 'a;
type LocalLockExecutor<'a, E> = dyn FnMut(&str, &mut RuntimeVariables) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<E>>
    + Send
    + 'a;
type CompletionExecutor<'a, E, T> =
    dyn FnMut(FinalMessage<'_>, &mut RuntimeVariables, &mut T, CompletionState<'_, E>) + Send + 'a;

pub struct ExecutionServices<'a, E, T> {
    delivery: &'a mut DeliveryExecutor<'a, E, T>,
    trace: &'a mut T,
    external: Option<&'a mut ExternalActionExecutor<'a, E, T>>,
    capture: Option<&'a mut CommandCaptureExecutor<'a, E, T>>,
    external_condition: Option<&'a mut ExternalConditionExecutor<'a, E, T>>,
    global_lock: Option<&'a mut GlobalLockExecutor<'a, E>>,
    local_lock: Option<&'a mut LocalLockExecutor<'a, E>>,
    completion: Option<&'a mut CompletionExecutor<'a, E, T>>,
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
            + Send
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
            + Send
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
            + Send
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
            + Send
            + 'a,
    {
        self.capture = Some(executor);
        self
    }

    pub fn with_global_lock<G>(mut self, executor: &'a mut G) -> Self
    where
        G: FnMut(&str, &mut RuntimeVariables) -> Result<(), E> + Send + 'a,
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
            + Send
            + 'a,
    {
        self.local_lock = Some(executor);
        self
    }

    pub fn with_completion<F>(mut self, executor: &'a mut F) -> Self
    where
        F: FnMut(FinalMessage<'_>, &mut RuntimeVariables, &mut T, CompletionState<'_, E>)
            + Send
            + 'a,
    {
        self.completion = Some(executor);
        self
    }
}

impl<E, T: TraceSink + Send> OrderedExecutionHost for ExecutionServices<'_, E, T> {
    type Error = E;
    type Trace = T;

    fn trace(&mut self) -> &mut Self::Trace {
        self.trace
    }

    fn deliver(
        &mut self,
        destination: &Destination,
        message: &[u8],
        output_ending: OutputEnding,
        lock: Option<&str>,
        runtime: &mut RuntimeVariables,
    ) -> Result<(), DeliveryAttemptError<Self::Error>> {
        (self.delivery)(
            destination,
            message,
            output_ending,
            lock,
            runtime,
            self.trace,
        )
    }

    fn external_action(
        &mut self,
        action: &PipeAction,
        options: RecipeOptions,
        lock: Option<&str>,
        input: ExternalActionInput<'_>,
        runtime: &mut RuntimeVariables,
    ) -> Result<Option<Message>, DeliveryAttemptError<Self::Error>> {
        self.external.as_deref_mut().unwrap()(action, options, lock, input, runtime, self.trace)
    }

    fn capture(
        &mut self,
        command: &str,
        input: &[u8],
        output_ending: OutputEnding,
        options: Option<RecipeOptions>,
        limit: usize,
        runtime: &mut RuntimeVariables,
    ) -> Result<CapturedCommand, DeliveryAttemptError<Self::Error>> {
        self.capture.as_deref_mut().unwrap()(
            command,
            input,
            output_ending,
            options,
            limit,
            runtime,
            self.trace,
        )
    }

    fn external_condition(
        &mut self,
        command: &str,
        input: &[u8],
        runtime: &mut RuntimeVariables,
    ) -> Result<bool, DeliveryAttemptError<Self::Error>> {
        self.external_condition.as_deref_mut().unwrap()(command, input, runtime, self.trace)
    }

    fn replace_global_lock(
        &mut self,
        path: &str,
        runtime: &mut RuntimeVariables,
    ) -> Result<(), Self::Error> {
        self.global_lock.as_deref_mut().unwrap()(path, runtime)
    }

    fn acquire_local_lock(
        &mut self,
        path: &str,
        runtime: &mut RuntimeVariables,
    ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<Self::Error>> {
        self.local_lock.as_deref_mut().unwrap()(path, runtime)
    }

    fn complete(
        &mut self,
        message: FinalMessage<'_>,
        runtime: &mut RuntimeVariables,
        state: CompletionState<'_, Self::Error>,
    ) {
        if let Some(completion) = self.completion.as_deref_mut() {
            completion(message, runtime, self.trace, state);
        }
    }
}
