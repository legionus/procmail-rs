// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use procmail_rs::config::{self, Destination, OutputEnding, RecipeOptions};
use procmail_rs::delivery::discard::DiscardSink;
use procmail_rs::delivery::local_lock::LocalLock;
use procmail_rs::delivery::maildir::{Durability, MaildirSink};
use procmail_rs::delivery::mbox::MboxFile;
use procmail_rs::delivery::staging::StagingFile;
use procmail_rs::delivery::{DeliveryFailureClass, PendingFanout, PendingSink};
use procmail_rs::eval::{
    CompletionState, DeliveryAttemptError, DeliveryPlan, ExecutionPlan, ExecutionServices,
    ExternalActionInput, FinalMessage, MappedMessageInput, MatchingMessage, OrderedExecutionError,
    PlannedDelivery, RecipeLockGuard,
};
use procmail_rs::limits::{MAX_MESSAGE_SIZE, MessageLimits};
use procmail_rs::runtime::{RuntimeSettings, RuntimeVariables};
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
        let sinks = open_sinks(plan.deliveries(), self.durability, runtime, trace)?;
        let pending = PendingFanout::new(sinks)
            .map_err(|error| OperationalError::Internal(error.to_string()))?;
        let (validated, _) = pending.stream(head, reader).map_err(|error| {
            OperationalError::Input(format!("cannot stream message from stdin: {error}"))
        })?;
        commit_delivery(validated, plan.deliveries(), runtime, trace)?;

        delivery_outcome(plan)
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
            .map(|header| MatchingMessage::new(header, matching_raw));

        if execution.requires_ordered_delivery() {
            let command_runner = CommandRunner::new(self.limits);
            let durability = self.durability;
            let uid = self.uid;
            let global_lock = &mut self.global_lock;
            let mut delivery = |destination: &Destination,
                                message: &[u8],
                                output_ending: OutputEnding,
                                lock: Option<&str>,
                                runtime: &mut RuntimeVariables,
                                trace: &mut T| {
                let _local_lock = acquire_recipe_lock(lock, Some(destination), runtime, uid)
                    .map_err(DeliveryAttemptError::Recoverable)?;
                let result = if matches!(
                    destination,
                    Destination::Mbox(_) | Destination::File(_) | Destination::Discard(_)
                ) {
                    deliver_file_destination(
                        destination,
                        message,
                        output_ending,
                        durability,
                        runtime,
                        trace,
                    )
                } else {
                    deliver_one_maildir(destination, message, durability, runtime, trace)
                };
                result.map_err(|error| {
                    if error.can_handle {
                        DeliveryAttemptError::Recoverable(error.error)
                    } else {
                        DeliveryAttemptError::Fatal(error.error)
                    }
                })
            };
            let mut condition =
                |command: &str, input: &[u8], runtime: &mut RuntimeVariables, _: &mut T| {
                    command_runner.condition(command, input, runtime)
                };
            let mut action = |action: &procmail_rs::config::PipeAction,
                              recipe_options: RecipeOptions,
                              lock: Option<&str>,
                              input: ExternalActionInput<'_>,
                              runtime: &mut RuntimeVariables,
                              _: &mut T| {
                let _local_lock = acquire_recipe_lock(lock, None, runtime, uid)
                    .map_err(DeliveryAttemptError::Recoverable)?;
                command_runner.action(action.command.as_str(), recipe_options, input, runtime)
            };
            let mut capture = |command: &str,
                               input: &[u8],
                               output_ending: OutputEnding,
                               recipe_options: Option<RecipeOptions>,
                               limit: usize,
                               runtime: &mut RuntimeVariables,
                               _: &mut T| {
                command_runner.capture(
                    command,
                    input,
                    output_ending,
                    recipe_options,
                    limit,
                    runtime,
                )
            };
            let mut global_lock_service = |path: &str, runtime: &mut RuntimeVariables| {
                // Replacing LOCKFILE first releases the preceding global lock. If
                // replacement fails, clear its visible value so later statements
                // cannot mistake an unheld path for an active semaphore.
                *global_lock = None;
                if path.is_empty() {
                    return Ok(());
                }
                match acquire_configured_lock(path, runtime, uid) {
                    Ok(lock) => {
                        *global_lock = Some(lock);
                        Ok(())
                    }
                    Err(error) => {
                        runtime.set("LOCKFILE".to_owned(), String::new());
                        Err(error)
                    }
                }
            };
            let mut local_lock_service = |path: &str, runtime: &mut RuntimeVariables| {
                acquire_configured_lock(path, runtime, uid)
                    .map(|lock| Box::new(lock) as Box<dyn RecipeLockGuard>)
                    .map_err(DeliveryAttemptError::Recoverable)
            };
            let mut completion =
                |message: FinalMessage<'_>,
                 runtime: &mut RuntimeVariables,
                 _: &mut T,
                 state: CompletionState<'_, OperationalError>| {
                    command_runner.trap(message.as_bytes(), runtime, completion_exit_status(state));
                };
            let services = ExecutionServices::new(&mut delivery, trace)
                .with_external_condition(&mut condition)
                .with_external_action(&mut action)
                .with_capture(&mut capture)
                .with_global_lock(&mut global_lock_service)
                .with_local_lock(&mut local_lock_service)
                .with_completion(&mut completion)
                .require_complete()
                .map_err(|error| OperationalError::Internal(error.to_string()))?;
            let outcome = execution
                .execute_mapped_ordered_with_services(
                    MappedMessageInput::new(staged.as_bytes(), staged.header_len(), matching),
                    runtime,
                    services,
                )
                .map_err(|error| match error {
                    OrderedExecutionError::Evaluation(error) => {
                        OperationalError::PermanentDestination(format!(
                            "cannot evaluate message: {error}"
                        ))
                    }
                    OrderedExecutionError::Delivery(error) => error,
                })?;
            return delivery_outcome_counts(outcome.original_delivered(), outcome.published());
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
        commit_delivery(validated, plan.deliveries(), runtime, trace)?;

        delivery_outcome(&plan)
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
        CompletionState::Failed(OrderedExecutionError::Delivery(error)) => {
            error.exit_status() as u8
        }
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
    let mask = settings
        .umask()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))?;
    LocalLock::acquire_with_mask(Path::new(path), method, uid, timeout, mask).map_err(|error| {
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

fn deliver_one_maildir(
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
            if let Some(published) = error.published() {
                record_delivery(destination, DeliveryStage::Published, trace);
                runtime
                    .record_delivery_with_trace(published, trace)
                    .map_err(OperationalError::Internal)
                    .map_err(OrderedStepError::after_publication)?;
            }
            record_delivery(
                destination,
                DeliveryStage::Failed(FailureClass::Transient),
                trace,
            );
            let failure = OperationalError::delivery(
                error.class(),
                format!("cannot publish Maildir delivery: {error}"),
            );
            return Err(if error.published().is_some() {
                OrderedStepError::after_publication(failure)
            } else {
                OrderedStepError::before_publication(failure)
            });
        }
    };
    record_delivery(destination, DeliveryStage::Published, trace);
    runtime
        .record_delivery_with_trace(&published, trace)
        .map_err(OperationalError::Internal)
        .map_err(OrderedStepError::after_publication)?;
    Ok(())
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
    if let Destination::Discard(expression) = &destination {
        record_delivery(&destination, DeliveryStage::Published, trace);
        runtime
            .record_delivery_with_trace(
                &procmail_rs::delivery::PublishedDelivery::new(PathBuf::from(expression.source())),
                trace,
            )
            .map_err(OperationalError::Internal)
            .map_err(OrderedStepError::after_publication)?;
        return Ok(());
    }
    let Destination::Mbox(expression) = &destination else {
        return Err(OrderedStepError::before_publication(
            OperationalError::Internal(
                "internal error: file delivery resolved to another destination type".to_owned(),
            ),
        ));
    };
    let path = Path::new(expression.source());
    let settings = RuntimeSettings::new(runtime);
    let lock_timeout = settings
        .lock_timeout()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))
        .map_err(OrderedStepError::before_publication)?;
    let mask = settings
        .umask()
        .map_err(|error| OperationalError::PermanentDestination(error.to_string()))
        .map_err(OrderedStepError::before_publication)?;
    let locked = MboxFile::open_with_mask(path, mask)
        .and_then(|mbox| mbox.lock_with_timeout(lock_timeout))
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
        Ok(published) => {
            record_delivery(&destination, DeliveryStage::Published, trace);
            runtime
                .record_delivery_with_trace(&published, trace)
                .map_err(OperationalError::Internal)
                .map_err(OrderedStepError::after_publication)
        }
        Err(error) => {
            let class = error.class();
            if error.published() {
                record_delivery(&destination, DeliveryStage::Published, trace);
                runtime
                    .record_delivery_with_trace(
                        &procmail_rs::delivery::PublishedDelivery::new(path.to_owned()),
                        trace,
                    )
                    .map_err(OperationalError::Internal)
                    .map_err(OrderedStepError::after_publication)?;
            } else {
                record_delivery(
                    &destination,
                    DeliveryStage::Failed(trace_failure_class(class)),
                    trace,
                );
            }
            let failure = OperationalError::delivery(
                class,
                format!("cannot deliver to mbox {}: {error}", path.display()),
            );
            Err(if error.published() {
                OrderedStepError::after_publication(failure)
            } else {
                OrderedStepError::before_publication(failure)
            })
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
) -> Result<(), OperationalError> {
    // Each sink reports the path it actually made visible. Update LASTFOLDER
    // from that report, including the last successful sink in a partial
    // fan-out, instead of guessing from the requested destination directory.
    match validated.commit() {
        Ok(report) => {
            for delivery in deliveries.iter().take(report.published().len()) {
                record_delivery(delivery.destination(), DeliveryStage::Published, trace);
            }
            runtime
                .record_commit_with_trace(&report, trace)
                .map_err(OperationalError::Internal)
        }
        Err(error) => {
            for delivery in deliveries.iter().take(error.published().len()) {
                record_delivery(delivery.destination(), DeliveryStage::Published, trace);
            }
            if let Some(delivery) = deliveries.get(error.published().len()) {
                record_delivery(
                    delivery.destination(),
                    DeliveryStage::Failed(FailureClass::Transient),
                    trace,
                );
            }
            runtime
                .record_partial_commit_with_trace(&error, trace)
                .map_err(OperationalError::Internal)?;
            Err(OperationalError::delivery(
                error.class(),
                format!("cannot publish Maildir delivery: {error}"),
            ))
        }
    }
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
    match &destination {
        Destination::Maildir(expression) => {
            let path = Path::new(expression.source());
            let sink = MaildirSink::create_with_durability_and_mask(path, durability, mask)
                .map_err(|error| {
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
        Destination::Mbox(expression) => {
            record_delivery(
                unresolved,
                DeliveryStage::Failed(FailureClass::Permanent),
                trace,
            );
            Err(OperationalError::Internal(format!(
                "internal error: mbox destination reached streaming delivery: {}",
                expression.source()
            )))
        }
        Destination::Discard(_) => Ok(Box::new(DiscardSink::null())),
        Destination::File(expression) => {
            record_delivery(
                unresolved,
                DeliveryStage::Failed(FailureClass::Permanent),
                trace,
            );
            Err(OperationalError::Internal(format!(
                "internal error: ordered destination reached streaming delivery: {}",
                expression.source()
            )))
        }
    }
}

fn record_delivery(destination: &Destination, stage: DeliveryStage, trace: &mut impl TraceSink) {
    let (line, destination) = match destination {
        Destination::Maildir(expression) => (expression.line(), TraceDestinationKind::Maildir),
        Destination::Mbox(expression) => (expression.line(), TraceDestinationKind::Mbox),
        Destination::File(expression) => (expression.line(), TraceDestinationKind::File),
        Destination::Discard(expression) => (expression.line(), TraceDestinationKind::Discard),
    };
    trace.record(TraceEvent::Delivery {
        recipe_line: line,
        destination,
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

fn delivery_outcome(plan: &DeliveryPlan) -> Result<(), OperationalError> {
    delivery_outcome_counts(plan.original_delivered(), plan.deliveries().len())
}

fn delivery_outcome_counts(
    original_delivered: bool,
    published: usize,
) -> Result<(), OperationalError> {
    if original_delivered {
        Ok(())
    } else {
        Err(OperationalError::Undelivered(format!(
            "original message was not delivered (published {} copy destination(s))",
            published
        )))
    }
}

#[cfg(test)]
#[path = "delivery_runtime_tests.rs"]
mod tests;
