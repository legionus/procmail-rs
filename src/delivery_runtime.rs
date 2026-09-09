// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use procmail_rs::config::{self, Destination, DestinationKind, OutputEnding, RecipeOptions};
use procmail_rs::delivery::discard::DiscardSink;
use procmail_rs::delivery::local_lock::LocalLock;
use procmail_rs::delivery::maildir::{Durability, MaildirSink};
use procmail_rs::delivery::mbox::MboxFile;
use procmail_rs::delivery::staging::StagingFile;
use procmail_rs::delivery::{DeliveryFailureClass, PendingFanout, PendingSink};
use procmail_rs::eval::{
    CapturedCommand, CompletionState, DeliveryAttemptError, DeliveryPlan, ExecutionPlan,
    ExternalActionInput, FinalMessage, MappedMessageInput, MatchingMessage, OrderedExecutionError,
    OrderedExecutionHost, PlannedDelivery, RecipeLockGuard,
};
use procmail_rs::limits::{MAX_MESSAGE_SIZE, MessageLimits};
use procmail_rs::runtime::{PublicationResult, RuntimeSettings, RuntimeVariables};
use procmail_rs::trace::{
    DeliveryStage, DestinationKind as TraceDestinationKind, FailureClass, TraceEvent, TraceSink,
};

use crate::command_runner::CommandRunner;
use crate::{ExitStatus, OperationalError};

pub(super) struct DeliveryRuntime {
    staging_directory: Option<PathBuf>,
    durability: Durability,
    limits: MessageLimits,
    uid: u32,
    global_lock: Option<LocalLock>,
    publications: PublicationTracker,
}

struct OrderedDeliveryHost<'a, T> {
    command_runner: CommandRunner,
    durability: Durability,
    uid: u32,
    global_lock: &'a mut Option<LocalLock>,
    suspended_global_locks: Vec<Option<LocalLock>>,
    trace: &'a mut T,
}

impl<T: TraceSink> OrderedExecutionHost for OrderedDeliveryHost<'_, T> {
    type Error = OperationalError;
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
        check_signal().map_err(DeliveryAttemptError::Fatal)?;
        let _local_lock = acquire_recipe_lock(lock, Some(destination), runtime, self.uid)
            .map_err(classify_execution_error)?;
        let result = if destination.supports_fanout_delivery() {
            deliver_one_sink(destination, message, self.durability, runtime, self.trace)
        } else {
            deliver_file_destination(
                destination,
                message,
                output_ending,
                self.durability,
                runtime,
                self.trace,
            )
        };
        check_signal().map_err(DeliveryAttemptError::Fatal)?;
        result.map_err(|error| {
            if error.can_handle {
                DeliveryAttemptError::Recoverable(error.error)
            } else {
                DeliveryAttemptError::Fatal(error.error)
            }
        })
    }

    fn external_action(
        &mut self,
        action: &procmail_rs::config::PipeAction,
        options: RecipeOptions,
        lock: Option<&str>,
        input: ExternalActionInput<'_>,
        runtime: &mut RuntimeVariables,
    ) -> Result<Option<procmail_rs::message::Message>, DeliveryAttemptError<Self::Error>> {
        check_signal().map_err(DeliveryAttemptError::Fatal)?;
        let _local_lock =
            acquire_recipe_lock(lock, None, runtime, self.uid).map_err(classify_execution_error)?;
        let result = self
            .command_runner
            .action(action.command.as_str(), options, input, runtime);
        check_signal().map_err(DeliveryAttemptError::Fatal)?;
        result
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
        check_signal().map_err(DeliveryAttemptError::Fatal)?;
        let result =
            self.command_runner
                .capture(command, input, output_ending, options, limit, runtime);
        check_signal().map_err(DeliveryAttemptError::Fatal)?;
        result
    }

    fn external_condition(
        &mut self,
        command: &str,
        input: &[u8],
        runtime: &mut RuntimeVariables,
    ) -> Result<bool, DeliveryAttemptError<Self::Error>> {
        check_signal().map_err(DeliveryAttemptError::Fatal)?;
        let result = self.command_runner.condition(command, input, runtime);
        check_signal().map_err(DeliveryAttemptError::Fatal)?;
        result
    }

    fn replace_global_lock(
        &mut self,
        path: &str,
        runtime: &mut RuntimeVariables,
    ) -> Result<(), Self::Error> {
        check_signal()?;
        // Replacing LOCKFILE first releases the preceding global lock. Clear
        // its visible value on failure so later statements cannot treat an
        // unheld path as an active lock.
        *self.global_lock = None;
        if path.is_empty() {
            return Ok(());
        }
        match acquire_configured_lock(path, runtime, self.uid) {
            Ok(lock) => {
                *self.global_lock = Some(lock);
                Ok(())
            }
            Err(error) => {
                runtime.set("LOCKFILE".to_owned(), String::new());
                Err(error)
            }
        }
    }

    fn acquire_local_lock(
        &mut self,
        path: &str,
        runtime: &mut RuntimeVariables,
    ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<Self::Error>> {
        check_signal().map_err(DeliveryAttemptError::Fatal)?;
        acquire_configured_lock(path, runtime, self.uid)
            .map(|lock| Box::new(lock) as Box<dyn RecipeLockGuard>)
            .map_err(classify_execution_error)
    }

    fn enter_copy_branch(&mut self) {
        // A procmail copy branch does not own the parent's tracked locks. Keep
        // the parent lock alive off to the side while branch-local LOCKFILE
        // assignments operate on a separate slot, including in nested copies.
        self.suspended_global_locks
            .push(std::mem::take(self.global_lock));
    }

    fn leave_copy_branch(&mut self) {
        *self.global_lock = None;
        *self.global_lock = self.suspended_global_locks.pop().unwrap_or_default();
    }

    fn complete(
        &mut self,
        message: FinalMessage<'_>,
        runtime: &mut RuntimeVariables,
        state: CompletionState<'_, Self::Error>,
    ) {
        if procmail_rs::signal_state::received().is_some() {
            return;
        }
        self.command_runner
            .trap(message.as_bytes(), runtime, completion_exit_status(state));
    }
}

#[derive(Default)]
struct PublicationTracker {
    published: usize,
    original_delivered: bool,
}

impl PublicationTracker {
    fn record(
        &mut self,
        published: usize,
        original_delivered: bool,
    ) -> Result<(), OperationalError> {
        self.published = self.published.checked_add(published).ok_or_else(|| {
            OperationalError::Internal("published destination count overflows".to_owned())
        })?;
        self.original_delivered |= original_delivered;
        Ok(())
    }

    fn record_outcome(
        &mut self,
        outcome: procmail_rs::eval::DeliveryOutcome,
    ) -> Result<(), OperationalError> {
        self.record(outcome.published(), outcome.original_delivered())
    }

    fn finish(&mut self) -> Result<(), OperationalError> {
        // Consume the state even when the original was not delivered. This
        // keeps accidental reuse of DeliveryRuntime from carrying publication
        // counts into another message while retaining the copy count in the
        // diagnostic produced for this one.
        let completed = std::mem::take(self);
        if completed.original_delivered {
            Ok(())
        } else {
            Err(OperationalError::Undelivered(format!(
                "original message was not delivered (published {} copy destination(s))",
                completed.published
            )))
        }
    }
}

impl DeliveryRuntime {
    pub(super) fn new(
        staging_directory: Option<PathBuf>,
        durability: Durability,
        limits: MessageLimits,
        uid: u32,
    ) -> Self {
        Self {
            staging_directory,
            durability,
            limits,
            uid,
            global_lock: None,
            publications: PublicationTracker::default(),
        }
    }

    pub(super) fn deliver_decided(
        &mut self,
        head: procmail_rs::message::MessageHead,
        reader: &mut impl io::BufRead,
        plan: &DeliveryPlan,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> Result<(), OperationalError> {
        check_signal()?;
        let sinks = open_sinks(plan.deliveries(), self.durability, runtime, trace)?;
        let pending = PendingFanout::new(sinks)
            .map_err(|error| OperationalError::Internal(error.to_string()))?;
        let (validated, _) = pending.stream(head, reader).map_err(|error| {
            OperationalError::Input(format!("cannot stream message from stdin: {error}"))
        })?;
        check_signal()?;
        let published = commit_delivery(validated, plan.deliveries(), runtime, trace)?;
        self.publications
            .record(published, plan.original_delivered())?;
        self.publications.finish()
    }

    pub(super) fn deliver_staged<T: TraceSink>(
        &mut self,
        mut head: procmail_rs::message::MessageHead,
        reader: &mut impl io::BufRead,
        execution: &ExecutionPlan,
        continuation: procmail_rs::eval::Continuation,
        runtime: &mut RuntimeVariables,
        trace: &mut T,
    ) -> Result<(), OperationalError> {
        check_signal()?;
        let runtime_staging = RuntimeSettings::new(runtime).maildir().map(PathBuf::from);
        let staging_directory = runtime_staging
            .as_deref()
            .or(self.staging_directory.as_deref())
            .ok_or_else(|| {
                OperationalError::Internal(
                    "internal error: deferred evaluation has no staging directory".to_owned(),
                )
            })?;
        let early_count = continuation.pending_deliveries().len();
        let early_sinks = if execution.requires_ordered_delivery() {
            Vec::new()
        } else {
            open_sinks(
                continuation.pending_deliveries(),
                self.durability,
                runtime,
                trace,
            )?
        };
        let pending = PendingFanout::new(early_sinks)
            .map_err(|error| OperationalError::Internal(error.to_string()))?;
        let mut staging = StagingFile::create(staging_directory).map_err(|error| {
            OperationalError::TemporaryDelivery(format!(
                "cannot create private staging file: {error}"
            ))
        })?;
        let header_len = head.len();
        let matching_header = head.take_matching_header();

        // Early copies and staging receive identical bytes in one pass over stdin.
        // Neither side is published yet, so any failure drops both private outputs
        // before the caller can observe a partial message.
        let (validated, _) = pending.stage(head, reader, &mut staging).map_err(|error| {
            OperationalError::Input(format!("cannot stage message from stdin: {error}"))
        })?;
        check_signal()?;
        let staged = staging.map(MAX_MESSAGE_SIZE, header_len).map_err(|error| {
            OperationalError::Internal(format!("cannot map staged message: {error}"))
        })?;
        let matching_staged = if execution.needs_message_contents() {
            matching_header
                .as_deref()
                .map(|header| stage_matching_message(staging_directory, header, &staged))
                .transpose()?
        } else {
            None
        };
        let matching_raw = matching_staged.as_ref().map(|message| message.as_bytes());
        let matching = matching_header
            .as_deref()
            .map(|header| MatchingMessage::from_normalized_parts(header, matching_raw));

        if execution.requires_ordered_delivery() {
            check_signal()?;
            let host = OrderedDeliveryHost {
                command_runner: CommandRunner::new(self.limits),
                durability: self.durability,
                uid: self.uid,
                global_lock: &mut self.global_lock,
                suspended_global_locks: Vec::new(),
                trace,
            };
            let outcome = execution
                .execute_ordered(
                    MappedMessageInput::new(staged.as_bytes(), staged.header_len(), matching),
                    runtime,
                    host,
                )
                .map_err(|error| match error {
                    OrderedExecutionError::Evaluation(error) => {
                        OperationalError::PermanentDestination(format!(
                            "cannot evaluate message: {error}"
                        ))
                    }
                    OrderedExecutionError::Delivery(error) => error,
                })?;
            self.publications.record_outcome(outcome)?;
            return self.publications.finish();
        }
        let plan = execution
            .resume_with_trace(
                continuation,
                MappedMessageInput::new(staged.as_bytes(), staged.header_len(), matching),
                runtime,
                trace,
            )
            .map_err(|error| {
                OperationalError::PermanentDestination(format!("cannot evaluate message: {error}"))
            })?;
        check_signal()?;
        let late_deliveries = plan.deliveries().get(early_count..).ok_or_else(|| {
            OperationalError::Internal(
                "internal error: deferred delivery discarded an early copy destination".to_owned(),
            )
        })?;
        let late_sinks = open_sinks(late_deliveries, self.durability, runtime, trace)?;
        let late = PendingFanout::new(late_sinks)
            .map_err(|error| OperationalError::Internal(error.to_string()))?;
        let validated = validated
            .append_bytes(late, staged.as_bytes())
            .map_err(|error| OperationalError::delivery(error.class(), error.to_string()))?;
        check_signal()?;
        let published = commit_delivery(validated, plan.deliveries(), runtime, trace)?;
        self.publications
            .record(published, plan.original_delivered())?;
        self.publications.finish()
    }
}

fn completion_exit_status(state: CompletionState<'_, OperationalError>) -> u8 {
    match state {
        CompletionState::Completed(outcome) if outcome.original_delivered() => {
            ExitStatus::Success as u8
        }
        CompletionState::Completed(_) => ExitStatus::Undelivered as u8,
        CompletionState::Failed(OrderedExecutionError::Evaluation(_)) => {
            ExitStatus::PermanentDestination as u8
        }
        CompletionState::Failed(OrderedExecutionError::Delivery(error)) => error.exit_code(),
    }
}

fn check_signal() -> Result<(), OperationalError> {
    match procmail_rs::signal_state::received() {
        Some(signal) => Err(OperationalError::Signaled(signal)),
        None => Ok(()),
    }
}

fn classify_execution_error(error: OperationalError) -> DeliveryAttemptError<OperationalError> {
    if matches!(error, OperationalError::Signaled(_)) {
        DeliveryAttemptError::Fatal(error)
    } else {
        DeliveryAttemptError::Recoverable(error)
    }
}

fn acquire_recipe_lock(
    lock: Option<&str>,
    destination: Option<&Destination>,
    runtime: &RuntimeVariables,
    uid: u32,
) -> Result<Option<LocalLock>, OperationalError> {
    let Some(lock) = lock else {
        return Ok(None);
    };
    let path = if lock.is_empty() {
        let destination = destination.ok_or_else(|| {
            OperationalError::PermanentDestination(
                "an implicit local lockfile requires a filesystem destination".to_owned(),
            )
        })?;
        let destination = destination
            .resolve_with(|name| runtime.get(name).map(str::to_owned))
            .map_err(|error| OperationalError::PermanentDestination(error.to_string()))?;
        let extension = runtime.get("LOCKEXT").unwrap_or(config::DEFAULT_LOCK_EXT);
        derive_implicit_lockfile_path(destination.path(), extension)?
    } else {
        lock.to_owned()
    };
    acquire_configured_lock(&path, runtime, uid).map(Some)
}

fn derive_implicit_lockfile_path(
    destination: &str,
    extension: &str,
) -> Result<String, OperationalError> {
    config::validate_lock_ext(extension).map_err(OperationalError::PermanentDestination)?;

    // Check the complete byte length before reserving or appending the
    // user-controlled suffix. This keeps a large LOCKEXT from causing a
    // transient over-limit allocation and preserves the path ceiling at the
    // filesystem boundary even if an internal caller bypassed rc validation.
    let derived_len = destination
        .len()
        .checked_add(extension.len())
        .ok_or_else(|| {
            OperationalError::PermanentDestination(
                "implicit lockfile path length overflows".to_owned(),
            )
        })?;
    if derived_len > config::MAX_PATH_EXPRESSION_LEN {
        return Err(OperationalError::PermanentDestination(format!(
            "implicit lockfile path exceeds the hard limit of {} bytes",
            config::MAX_PATH_EXPRESSION_LEN
        )));
    }
    let mut path = String::new();
    path.try_reserve_exact(derived_len).map_err(|_| {
        OperationalError::Internal("cannot allocate implicit lockfile path".to_owned())
    })?;
    path.push_str(destination);
    path.push_str(extension);
    Ok(path)
}

fn acquire_configured_lock(
    path: &str,
    runtime: &RuntimeVariables,
    uid: u32,
) -> Result<LocalLock, OperationalError> {
    let settings = RuntimeSettings::new(runtime);
    let method = settings
        .lock_method()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))?;
    let timeout = settings
        .lock_timeout()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))?;
    let retry = settings
        .lock_sleep()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))?;
    let mask = settings
        .umask()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))?;
    LocalLock::acquire(Path::new(path), method, uid, timeout, retry, mask).map_err(|error| {
        if let Some(signal) = procmail_rs::signal_state::received() {
            return OperationalError::Signaled(signal);
        }
        OperationalError::delivery(
            DeliveryFailureClass::from_io_error(&error),
            format!("cannot acquire local lockfile: {error}"),
        )
    })
}

fn stage_matching_message(
    directory: &Path,
    matching_header: &[u8],
    staged: &procmail_rs::delivery::staging::StagedMessage,
) -> Result<procmail_rs::delivery::staging::StagedMessage, OperationalError> {
    let mut matching = StagingFile::create(directory).map_err(|error| {
        OperationalError::TemporaryDelivery(format!(
            "cannot create private regex staging file: {error}"
        ))
    })?;

    // A single mapped range is required because an HB expression may begin
    // in a normalized continued header and finish in the body. Writing the
    // already bounded pieces to private staging avoids a second message-sized
    // heap allocation, while the original mapping remains the delivery data.
    matching.write_all(matching_header).map_err(|error| {
        OperationalError::TemporaryDelivery(format!(
            "cannot write normalized headers to regex staging file: {error}"
        ))
    })?;
    matching
        .write_all(&staged.as_bytes()[staged.header_len()..])
        .map_err(|error| {
            OperationalError::TemporaryDelivery(format!(
                "cannot write message body to regex staging file: {error}"
            ))
        })?;
    matching
        .map(MAX_MESSAGE_SIZE, matching_header.len())
        .map_err(|error| {
            OperationalError::Internal(format!(
                "cannot map normalized message for regex matching: {error}"
            ))
        })
}

struct OrderedStepError {
    error: OperationalError,
    can_handle: bool,
}

enum PublicationDestinations<'a> {
    One(&'a Destination),
    Plan(&'a [PlannedDelivery]),
}

impl PublicationDestinations<'_> {
    fn get(&self, index: usize) -> Option<&Destination> {
        match self {
            Self::One(destination) => (index == 0).then_some(*destination),
            Self::Plan(deliveries) => deliveries.get(index).map(PlannedDelivery::destination),
        }
    }
}

struct PublicationFailure {
    class: DeliveryFailureClass,
    error: OperationalError,
}

struct PublicationAttempt<'a> {
    published: Option<PublicationResult<'a>>,
    failure: Option<PublicationFailure>,
}

impl<'a> PublicationAttempt<'a> {
    fn published(result: PublicationResult<'a>) -> Self {
        Self {
            published: Some(result),
            failure: None,
        }
    }

    fn failed(
        published: Option<PublicationResult<'a>>,
        class: DeliveryFailureClass,
        error: OperationalError,
    ) -> Self {
        Self {
            published,
            failure: Some(PublicationFailure { class, error }),
        }
    }
}

fn apply_publication(
    attempt: PublicationAttempt<'_>,
    destinations: PublicationDestinations<'_>,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<usize, OrderedStepError> {
    let published = attempt.published.map_or(0, PublicationResult::len);

    // Only the backend can tell whether a destination became visible. Apply
    // every externally observable consequence from that report so trace,
    // LASTFOLDER, and the returned count cannot disagree after a partial
    // fanout or a durability failure following publication.
    for index in 0..published {
        let destination = destinations.get(index).ok_or_else(|| {
            OrderedStepError::after_publication(OperationalError::Internal(
                "published destination has no matching delivery plan entry".to_owned(),
            ))
        })?;
        record_delivery(destination, DeliveryStage::Published, trace);
    }
    if let Some(result) = attempt.published {
        runtime
            .record_publication(result, trace)
            .map_err(OperationalError::Internal)
            .map_err(OrderedStepError::after_publication)?;
    }

    if let Some(failure) = attempt.failure {
        if let Some(destination) = destinations.get(published) {
            record_delivery(
                destination,
                DeliveryStage::Failed(trace_failure_class(failure.class)),
                trace,
            );
        }
        return Err(if published == 0 {
            OrderedStepError::before_publication(failure.error)
        } else {
            OrderedStepError::after_publication(failure.error)
        });
    }
    Ok(published)
}

impl OrderedStepError {
    fn before_publication(error: OperationalError) -> Self {
        Self {
            error,
            can_handle: true,
        }
    }

    fn after_publication(error: OperationalError) -> Self {
        Self {
            error,
            can_handle: false,
        }
    }
}

fn deliver_one_sink(
    destination: &Destination,
    message: &[u8],
    durability: Durability,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), OrderedStepError> {
    let mask = RuntimeSettings::new(runtime)
        .umask()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))
        .map_err(OrderedStepError::before_publication)?;
    let mut sinks = vec![
        open_sink(destination, durability, mask, runtime, trace)
            .map_err(OrderedStepError::before_publication)?,
    ];
    let mut sink = sinks
        .pop()
        .ok_or_else(|| {
            OperationalError::Internal("internal error: destination produced no sink".to_owned())
        })
        .map_err(OrderedStepError::before_publication)?;
    sink.write_all(message)
        .map_err(|error| {
            OperationalError::delivery(
                DeliveryFailureClass::from_io_error(&error),
                format!("cannot write staged delivery: {error}"),
            )
        })
        .map_err(OrderedStepError::before_publication)?;
    let published = match sink.commit() {
        Ok(published) => published,
        Err(error) => {
            let failure = OperationalError::delivery(
                error.class(),
                format!("cannot publish Maildir delivery: {error}"),
            );
            return apply_publication(
                PublicationAttempt::failed(
                    error.published().map(PublicationResult::Delivery),
                    error.class(),
                    failure,
                ),
                PublicationDestinations::One(destination),
                runtime,
                trace,
            )
            .map(|_| ());
        }
    };
    apply_publication(
        PublicationAttempt::published(PublicationResult::Delivery(&published)),
        PublicationDestinations::One(destination),
        runtime,
        trace,
    )
    .map(|_| ())
}

fn deliver_file_destination(
    unresolved: &Destination,
    message: &[u8],
    output_ending: procmail_rs::config::OutputEnding,
    durability: Durability,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), OrderedStepError> {
    let destination = unresolved
        .resolve_with(|name| runtime.get(name).map(str::to_owned))
        .map_err(|error| {
            record_delivery(
                unresolved,
                DeliveryStage::Failed(FailureClass::Permanent),
                trace,
            );
            OperationalError::PermanentDestination(error.to_string())
        })
        .map_err(OrderedStepError::before_publication)?;
    record_delivery(&destination, DeliveryStage::Preparing, trace);

    // Treat the resolved null device as a semantic discard instead of opening
    // a hostile filesystem object. The complete message has already passed
    // input validation, while avoiding device writes also keeps mbox locking,
    // rollback, and durability assumptions limited to regular files.
    if destination.kind() == DestinationKind::Discard {
        let published =
            procmail_rs::delivery::PublishedDelivery::new(PathBuf::from(destination.path()));
        return apply_publication(
            PublicationAttempt::published(PublicationResult::Delivery(&published)),
            PublicationDestinations::One(&destination),
            runtime,
            trace,
        )
        .map(|_| ());
    }
    if destination.kind() != DestinationKind::Mbox {
        return Err(OrderedStepError::before_publication(
            OperationalError::Internal(
                "internal error: file delivery resolved to another destination type".to_owned(),
            ),
        ));
    }
    let path = Path::new(destination.path());
    let settings = RuntimeSettings::new(runtime);
    let lock_timeout = settings
        .lock_timeout()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))
        .map_err(OrderedStepError::before_publication)?;
    let lock_sleep = settings
        .lock_sleep()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))
        .map_err(OrderedStepError::before_publication)?;
    let mask = settings
        .umask()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))
        .map_err(OrderedStepError::before_publication)?;
    let locked = MboxFile::open(path, mask)
        .and_then(|mbox| mbox.lock(lock_timeout, lock_sleep))
        .map_err(|error| {
            let class = DeliveryFailureClass::from_io_error(&error);
            record_delivery(
                &destination,
                DeliveryStage::Failed(trace_failure_class(class)),
                trace,
            );
            OperationalError::delivery(
                class,
                format!("cannot open or lock mbox {}: {error}", path.display()),
            )
        })
        .map_err(OrderedStepError::before_publication)?;
    match locked.append(message, output_ending, durability) {
        Ok(published) => apply_publication(
            PublicationAttempt::published(PublicationResult::Delivery(&published)),
            PublicationDestinations::One(&destination),
            runtime,
            trace,
        )
        .map(|_| ()),
        Err(error) => {
            let class = error.class();
            let failure = OperationalError::delivery(
                class,
                format!("cannot deliver to mbox {}: {error}", path.display()),
            );
            let published = error
                .published()
                .then(|| procmail_rs::delivery::PublishedDelivery::new(path.to_owned()));
            apply_publication(
                PublicationAttempt::failed(
                    published.as_ref().map(PublicationResult::Delivery),
                    class,
                    failure,
                ),
                PublicationDestinations::One(&destination),
                runtime,
                trace,
            )
            .map(|_| ())
        }
    }
}

fn trace_failure_class(class: DeliveryFailureClass) -> FailureClass {
    match class {
        DeliveryFailureClass::Retryable => FailureClass::Transient,
        DeliveryFailureClass::Permanent => FailureClass::Permanent,
        DeliveryFailureClass::Internal => FailureClass::Internal,
    }
}

fn commit_delivery(
    validated: procmail_rs::delivery::ValidatedFanout,
    deliveries: &[PlannedDelivery],
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<usize, OperationalError> {
    check_signal()?;
    let result = match validated.commit() {
        Ok(report) => apply_publication(
            PublicationAttempt::published(PublicationResult::Fanout(&report)),
            PublicationDestinations::Plan(deliveries),
            runtime,
            trace,
        ),
        Err(error) => {
            let failure = OperationalError::delivery(
                error.class(),
                format!("cannot publish Maildir delivery: {error}"),
            );
            apply_publication(
                PublicationAttempt::failed(
                    (!error.published().is_empty())
                        .then_some(PublicationResult::PartialFanout(&error)),
                    error.class(),
                    failure,
                ),
                PublicationDestinations::Plan(deliveries),
                runtime,
                trace,
            )
        }
    };
    result.map_err(|error| error.error)
}

fn open_sinks(
    deliveries: &[PlannedDelivery],
    durability: Durability,
    runtime: &RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<Vec<Box<dyn PendingSink>>, OperationalError> {
    let mut sinks: Vec<Box<dyn PendingSink>> = Vec::with_capacity(deliveries.len());
    for delivery in deliveries {
        sinks.push(open_sink(
            delivery.destination(),
            durability,
            delivery.umask(),
            runtime,
            trace,
        )?);
    }
    Ok(sinks)
}

fn open_sink(
    unresolved: &Destination,
    durability: Durability,
    mask: u32,
    runtime: &RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<Box<dyn PendingSink>, OperationalError> {
    record_delivery(unresolved, DeliveryStage::Preparing, trace);
    let destination = unresolved
        .resolve_with(|name| runtime.get(name).map(str::to_owned))
        .map_err(|error| {
            record_delivery(
                unresolved,
                DeliveryStage::Failed(FailureClass::Permanent),
                trace,
            );
            OperationalError::PermanentDestination(error.to_string())
        })?;
    match destination.kind() {
        DestinationKind::Maildir => {
            let path = Path::new(destination.path());
            let sink = MaildirSink::create(path, durability, mask).map_err(|error| {
                record_delivery(
                    unresolved,
                    DeliveryStage::Failed(FailureClass::Transient),
                    trace,
                );
                OperationalError::delivery(
                    DeliveryFailureClass::from_io_error(&error),
                    format!("cannot open Maildir {}: {error}", path.display()),
                )
            })?;
            Ok(Box::new(sink))
        }
        DestinationKind::Mbox => {
            record_delivery(
                unresolved,
                DeliveryStage::Failed(FailureClass::Permanent),
                trace,
            );
            Err(OperationalError::Internal(format!(
                "internal error: mbox destination reached streaming delivery: {}",
                destination.path()
            )))
        }
        DestinationKind::Discard => Ok(Box::new(DiscardSink::null())),
        DestinationKind::File => {
            record_delivery(
                unresolved,
                DeliveryStage::Failed(FailureClass::Permanent),
                trace,
            );
            Err(OperationalError::Internal(format!(
                "internal error: ordered destination reached streaming delivery: {}",
                destination.path()
            )))
        }
    }
}

fn record_delivery(destination: &Destination, stage: DeliveryStage, trace: &mut impl TraceSink) {
    let destination_kind = match destination.kind() {
        DestinationKind::Maildir => TraceDestinationKind::Maildir,
        DestinationKind::Mbox => TraceDestinationKind::Mbox,
        DestinationKind::File => TraceDestinationKind::File,
        DestinationKind::Discard => TraceDestinationKind::Discard,
    };
    trace.record(TraceEvent::Delivery {
        recipe_line: destination.line(),
        destination: destination_kind,
        stage,
    });
}

pub(super) fn validate_maildir_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("path is empty".into());
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("path must not contain '..'".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/delivery_runtime.rs"]
mod tests;
