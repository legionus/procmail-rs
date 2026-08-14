// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![forbid(unsafe_code)]

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
compile_error!("procmail-rs currently supports only 64-bit Linux targets");

use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use procmail_rs::config::{self, Destination, MAX_COMMAND_LINE_VARIABLES, SuppliedVariable};
use procmail_rs::delivery::maildir::{Durability, MaildirSink};
use procmail_rs::delivery::mbox::MboxFile;
use procmail_rs::delivery::staging::StagingFile;
use procmail_rs::delivery::{DeliveryFailureClass, PendingFanout, PendingSink};
use procmail_rs::eval::{
    ConditionKindExplanation, DeliveryAttemptError, DeliveryPlan, DestinationKind, ExecutionPlan,
    HeaderEvaluation, MatchingMessage, OrderedExecutionError, PlanExplanation, PlannedDelivery,
};
use procmail_rs::limits::{MAX_MESSAGE_SIZE, MessageLimits};
use procmail_rs::message::Message;
use procmail_rs::rc_file::RcFileLoader;
use procmail_rs::runtime::RuntimeVariables;
use procmail_rs::trace::{
    DeliveryStage, DestinationKind as TraceDestinationKind, FailureClass, NoTrace, TraceConfig,
    TraceEvent, TraceSink,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Check,
    Explain,
    Filter,
}

struct Command {
    action: Action,
    config: PathBuf,
    supplied: Vec<SuppliedVariable>,
}

#[derive(Clone, Copy)]
struct StagingOptions<'a> {
    directory: &'a Path,
    durability: Durability,
}

#[derive(Debug)]
enum OperationalError {
    Configuration(String),
    Input(String),
    TemporaryDelivery(String),
    PermanentDestination(String),
    Undelivered(String),
    Internal(String),
}

// Use the established sysexits values when they describe the action a caller
// should take. Keep unmatched delivery separate because none of those names
// accurately describes a valid message for which no final recipe was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ExitStatus {
    Success = 0,
    Input = 65,
    PermanentDestination = 73,
    TemporaryDelivery = 75,
    Configuration = 78,
    Undelivered = 79,
    Internal = 70,
}

impl std::fmt::Display for OperationalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Configuration(message)
            | Self::Input(message)
            | Self::TemporaryDelivery(message)
            | Self::PermanentDestination(message)
            | Self::Undelivered(message)
            | Self::Internal(message) => message,
        };
        formatter.write_str(message)
    }
}

impl OperationalError {
    fn delivery(class: DeliveryFailureClass, message: String) -> Self {
        match class {
            DeliveryFailureClass::Retryable => Self::TemporaryDelivery(message),
            DeliveryFailureClass::Permanent => Self::PermanentDestination(message),
            DeliveryFailureClass::Internal => Self::Internal(message),
        }
    }

    fn exit_status(&self) -> ExitStatus {
        match self {
            Self::Configuration(_) => ExitStatus::Configuration,
            Self::Input(_) => ExitStatus::Input,
            Self::TemporaryDelivery(_) => ExitStatus::TemporaryDelivery,
            Self::PermanentDestination(_) => ExitStatus::PermanentDestination,
            Self::Undelivered(_) => ExitStatus::Undelivered,
            Self::Internal(_) => ExitStatus::Internal,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::from(ExitStatus::Success as u8),
        Err(error) => {
            eprintln!("procmail-rs: {error}");
            ExitCode::from(error.exit_status() as u8)
        }
    }
}

fn run() -> Result<(), OperationalError> {
    let command = parse_args().map_err(OperationalError::Configuration)?;
    let path = &command.config;
    let (rc_loader, root_rc) = RcFileLoader::for_root(path)
        .map_err(|error| OperationalError::Configuration(error.to_string()))?;
    let config = config::parse(root_rc.source())
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?
        .expand_with(&command.supplied)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;
    let staging_directory = config.maildir().map(PathBuf::from);
    if let Some(maildir) = &staging_directory {
        validate_maildir_path(maildir).map_err(|error| {
            OperationalError::Configuration(format!("{}: invalid MAILDIR: {error}", path.display()))
        })?;
    }
    let limits = MessageLimits::from_config(&config)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;
    let durability = Durability::from_config(&config)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;
    let plan = ExecutionPlan::compile_with_loader(&config, rc_loader);
    let _trace_config = TraceConfig::from_config(&config)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;

    // A deferred decision needs a replayable private copy of stdin. Requiring
    // MAILDIR before reading headers prevents a configuration failure from
    // consuming part of a message that the caller may need to retry.
    if command.action == Action::Filter
        && plan.requirements().needs_end_of_message
        && staging_directory.is_none()
    {
        return Err(OperationalError::Configuration(format!(
            "{}: MAILDIR is required when a recipe needs the body or final message size",
            path.display()
        )));
    }

    match command.action {
        Action::Check => Ok(()),
        Action::Explain => {
            let mut stdout = io::stdout().lock();
            write_plan_explanation(&plan.explain(), &mut stdout).map_err(|error| {
                OperationalError::Internal(format!("cannot write plan explanation: {error}"))
            })
        }
        Action::Filter => {
            let mut runtime = RuntimeVariables::default();
            let mut trace = NoTrace;
            let mut stdin = io::stdin().lock();
            let head = Message::read_headers(&mut stdin, limits).map_err(|error| {
                OperationalError::Input(format!("cannot read message headers from stdin: {error}"))
            })?;
            match plan.evaluate_headers_with_trace(&head, &mut runtime, &mut trace) {
                HeaderEvaluation::Decided(delivery) => deliver_decided(
                    head,
                    &mut stdin,
                    &delivery,
                    durability,
                    &mut runtime,
                    &mut trace,
                ),
                HeaderEvaluation::NeedsMessage(continuation) => {
                    let runtime_staging = runtime.get("MAILDIR").map(PathBuf::from);
                    let staging_directory = runtime_staging
                        .as_deref()
                        .or(staging_directory.as_deref())
                        .ok_or_else(|| {
                            OperationalError::Internal(
                                "internal error: deferred evaluation has no staging directory"
                                    .to_owned(),
                            )
                        })?;
                    deliver_staged(
                        head,
                        &mut stdin,
                        &plan,
                        continuation,
                        StagingOptions {
                            directory: staging_directory,
                            durability,
                        },
                        &mut runtime,
                        &mut trace,
                    )
                }
                HeaderEvaluation::Error(error) => Err(OperationalError::PermanentDestination(
                    format!("cannot evaluate message: {error}"),
                )),
            }
        }
    }
}

fn write_plan_explanation(
    explanation: &PlanExplanation,
    writer: &mut impl Write,
) -> io::Result<()> {
    let requirements = explanation.requirements();
    writeln!(
        writer,
        "input headers={} body={} end={}",
        yes_no(requirements.needs_headers),
        yes_no(requirements.needs_body_contents),
        yes_no(requirements.needs_end_of_message)
    )?;
    writeln!(
        writer,
        "ordered-delivery={}",
        yes_no(explanation.requires_ordered_delivery())
    )?;

    // The explanation deliberately describes only control-flow shape. Do not
    // print regex text, assignment values, or destination paths because they
    // can contain credentials or other private configuration data.
    for recipe in explanation.recipes() {
        let destination = match recipe.destination() {
            DestinationKind::Maildir => "maildir",
            DestinationKind::Mbox => "mbox",
        };
        writeln!(
            writer,
            "recipe line={} copy={} assignments={} destination={} deferred={}",
            recipe.line(),
            yes_no(recipe.is_copy()),
            recipe.assignment_count(),
            destination,
            yes_no(recipe.defers_destination())
        )?;
        for condition in recipe.conditions() {
            let kind = match condition.kind() {
                ConditionKindExplanation::HeaderRegex => "header-regex",
                ConditionKindExplanation::BodyRegex => "body-regex",
                ConditionKindExplanation::MessageRegex => "message-regex",
                ConditionKindExplanation::VariableRegex => "variable-regex",
                ConditionKindExplanation::SmallerThan => "smaller-than",
                ConditionKindExplanation::LargerThan => "larger-than",
            };
            writeln!(
                writer,
                "  condition kind={} negated={}",
                kind,
                yes_no(condition.is_negated())
            )?;
        }
    }
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn deliver_decided(
    head: procmail_rs::message::MessageHead,
    reader: &mut impl io::BufRead,
    plan: &DeliveryPlan,
    durability: Durability,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), OperationalError> {
    let sinks = open_sinks(plan.deliveries(), durability, runtime, trace)?;
    let pending =
        PendingFanout::new(sinks).map_err(|error| OperationalError::Internal(error.to_string()))?;
    let (validated, _) = pending.stream(head, reader).map_err(|error| {
        OperationalError::Input(format!("cannot stream message from stdin: {error}"))
    })?;
    commit_delivery(validated, plan.deliveries(), runtime, trace)?;

    delivery_outcome(plan)
}

fn deliver_staged(
    mut head: procmail_rs::message::MessageHead,
    reader: &mut impl io::BufRead,
    execution: &ExecutionPlan,
    continuation: procmail_rs::eval::Continuation,
    options: StagingOptions<'_>,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), OperationalError> {
    let early_count = continuation.pending_deliveries().len();
    let early_sinks = if execution.requires_ordered_delivery() {
        Vec::new()
    } else {
        open_sinks(
            continuation.pending_deliveries(),
            options.durability,
            runtime,
            trace,
        )?
    };
    let pending = PendingFanout::new(early_sinks)
        .map_err(|error| OperationalError::Internal(error.to_string()))?;
    let mut staging = StagingFile::create(options.directory).map_err(|error| {
        OperationalError::TemporaryDelivery(format!("cannot create private staging file: {error}"))
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
            .map(|header| stage_matching_message(options.directory, header, &staged))
            .transpose()?
    } else {
        None
    };
    let matching_raw = matching_staged.as_ref().map(|message| message.as_bytes());
    let matching = matching_header
        .as_deref()
        .map(|header| MatchingMessage::new(header, matching_raw));

    if execution.requires_ordered_delivery() {
        let outcome = execution
            .execute_mapped_ordered_with_matching_trace(
                staged.as_bytes(),
                staged.header_len(),
                matching,
                runtime,
                trace,
                &mut |destination, message, runtime, trace| {
                    let result = if matches!(destination, Destination::Mbox(_)) {
                        deliver_mbox(destination, message, options.durability, runtime, trace)
                    } else {
                        deliver_one_maildir(
                            destination,
                            message,
                            options.durability,
                            runtime,
                            trace,
                        )
                    };
                    result.map_err(|error| {
                        if error.can_handle {
                            DeliveryAttemptError::Recoverable(error.error)
                        } else {
                            DeliveryAttemptError::Fatal(error.error)
                        }
                    })
                },
            )
            .map_err(|error| match error {
                OrderedExecutionError::Evaluation(error) => OperationalError::PermanentDestination(
                    format!("cannot evaluate message: {error}"),
                ),
                OrderedExecutionError::Delivery(error) => error,
            })?;
        return delivery_outcome_counts(outcome.original_delivered(), outcome.published());
    }
    let plan = execution
        .resume_mapped_with_matching_trace(
            continuation,
            staged.as_bytes(),
            staged.header_len(),
            matching,
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
    let late_sinks = open_sinks(late_deliveries, options.durability, runtime, trace)?;
    let late = PendingFanout::new(late_sinks)
        .map_err(|error| OperationalError::Internal(error.to_string()))?;
    let validated = validated
        .append_bytes(late, staged.as_bytes())
        .map_err(|error| OperationalError::delivery(error.class(), error.to_string()))?;
    commit_delivery(validated, plan.deliveries(), runtime, trace)?;

    delivery_outcome(&plan)
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
    let mut sinks = vec![
        open_sink(destination, durability, runtime, trace)
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

fn deliver_mbox(
    unresolved: &Destination,
    message: &[u8],
    durability: Durability,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), OrderedStepError> {
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
        })
        .map_err(OrderedStepError::before_publication)?;
    let Destination::Mbox(expression) = destination else {
        return Err(OrderedStepError::before_publication(
            OperationalError::Internal(
                "internal error: mbox delivery resolved to another destination type".to_owned(),
            ),
        ));
    };
    let path = Path::new(expression.source());
    let locked = MboxFile::open(path)
        .and_then(MboxFile::lock)
        .map_err(|error| {
            let class = DeliveryFailureClass::from_io_error(&error);
            record_delivery(
                unresolved,
                DeliveryStage::Failed(trace_failure_class(class)),
                trace,
            );
            OperationalError::delivery(
                class,
                format!("cannot open or lock mbox {}: {error}", path.display()),
            )
        })
        .map_err(OrderedStepError::before_publication)?;
    match locked.append(message, durability) {
        Ok(published) => {
            record_delivery(unresolved, DeliveryStage::Published, trace);
            runtime
                .record_delivery_with_trace(&published, trace)
                .map_err(OperationalError::Internal)
                .map_err(OrderedStepError::after_publication)
        }
        Err(error) => {
            let class = error.class();
            if error.published() {
                record_delivery(unresolved, DeliveryStage::Published, trace);
                runtime
                    .record_delivery_with_trace(
                        &procmail_rs::delivery::PublishedDelivery::new(path.to_owned()),
                        trace,
                    )
                    .map_err(OperationalError::Internal)
                    .map_err(OrderedStepError::after_publication)?;
            } else {
                record_delivery(
                    unresolved,
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
            runtime,
            trace,
        )?);
    }
    Ok(sinks)
}

fn open_sink(
    unresolved: &Destination,
    durability: Durability,
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
            let sink = MaildirSink::create_with_durability(path, durability).map_err(|error| {
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
    }
}

fn record_delivery(destination: &Destination, stage: DeliveryStage, trace: &mut impl TraceSink) {
    let (line, destination) = match destination {
        Destination::Maildir(expression) => (expression.line(), TraceDestinationKind::Maildir),
        Destination::Mbox(expression) => (expression.line(), TraceDestinationKind::Mbox),
    };
    trace.record(TraceEvent::Delivery {
        recipe_line: line,
        destination,
        stage,
    });
}

fn validate_maildir_path(path: &Path) -> Result<(), String> {
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

fn parse_args() -> Result<Command, String> {
    let mut args = env::args_os().skip(1);
    let action = args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .ok_or_else(usage)?;
    let action = match action.as_str() {
        "check" => Action::Check,
        "explain" => Action::Explain,
        "filter" => Action::Filter,
        _ => return Err(usage()),
    };
    let mut config = None;
    let mut supplied = Vec::new();

    // Parse every option before opening the rc file or stdin. This keeps bad
    // or excessive caller-controlled assignments from affecting filtering or
    // consuming any part of a message.
    while let Some(option) = args.next() {
        match option.to_str() {
            Some("--config") => {
                if config.is_some() {
                    return Err("--config may only be specified once".into());
                }
                config = Some(PathBuf::from(args.next().ok_or_else(usage)?));
            }
            Some("--set") => {
                if supplied.len() == MAX_COMMAND_LINE_VARIABLES {
                    return Err(format!(
                        "too many --set values; hard limit is {MAX_COMMAND_LINE_VARIABLES}"
                    ));
                }
                let value = args
                    .next()
                    .ok_or_else(usage)?
                    .into_string()
                    .map_err(|_| "--set value is not valid UTF-8".to_owned())?;
                supplied.push(SuppliedVariable::parse(value).map_err(|error| error.to_string())?);
            }
            _ => return Err(usage()),
        }
    }

    Ok(Command {
        action,
        config: config.ok_or_else(usage)?,
        supplied,
    })
}

fn usage() -> String {
    "usage: procmail-rs <check|explain|filter> --config PATH [--set NAME=VALUE]...".into()
}

#[cfg(test)]
mod tests {
    use super::{ExitStatus, OperationalError};

    #[test]
    fn operational_errors_have_distinct_stable_exit_statuses() {
        let cases = [
            (
                OperationalError::Configuration(String::new()),
                ExitStatus::Configuration,
                78,
            ),
            (
                OperationalError::Input(String::new()),
                ExitStatus::Input,
                65,
            ),
            (
                OperationalError::TemporaryDelivery(String::new()),
                ExitStatus::TemporaryDelivery,
                75,
            ),
            (
                OperationalError::PermanentDestination(String::new()),
                ExitStatus::PermanentDestination,
                73,
            ),
            (
                OperationalError::Undelivered(String::new()),
                ExitStatus::Undelivered,
                79,
            ),
            (
                OperationalError::Internal(String::new()),
                ExitStatus::Internal,
                70,
            ),
        ];

        for (error, status, value) in cases {
            assert_eq!(error.exit_status(), status);
            assert_eq!(status as u8, value);
        }
        assert_eq!(ExitStatus::Success as u8, 0);
    }
}
