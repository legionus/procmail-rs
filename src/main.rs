// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![forbid(unsafe_code)]

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
compile_error!("procmail-rs currently supports only 64-bit Linux targets");

use std::env;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use procmail_rs::config::{
    self, Destination, MAX_COMMAND_LINE_VARIABLES, Statement, SuppliedVariable,
};
use procmail_rs::delivery::maildir::MaildirSink;
use procmail_rs::delivery::staging::StagingFile;
use procmail_rs::delivery::{PendingFanout, PendingSink};
use procmail_rs::eval::{DeliveryPlan, ExecutionPlan, HeaderEvaluation};
use procmail_rs::limits::{MAX_MESSAGE_SIZE, MAX_RC_SIZE, MessageLimits};
use procmail_rs::message::Message;
use procmail_rs::runtime::RuntimeVariables;
use procmail_rs::trace::{
    DeliveryStage, DestinationKind as TraceDestinationKind, FailureClass, NoTrace, TraceEvent,
    TraceSink,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Check,
    Filter,
}

struct Command {
    action: Action,
    config: PathBuf,
    supplied: Vec<SuppliedVariable>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("procmail-rs: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let command = parse_args()?;
    let path = &command.config;
    let source = read_config(path)?;
    let config = config::parse(&source)
        .map_err(|error| format!("{}:{error}", path.display()))?
        .expand_with(&command.supplied)
        .map_err(|error| format!("{}:{error}", path.display()))?;
    validate_destination_types(&config, path)?;
    let staging_directory = config.maildir().map(PathBuf::from);
    if let Some(maildir) = &staging_directory {
        validate_maildir_path(maildir)
            .map_err(|error| format!("{}: invalid MAILDIR: {error}", path.display()))?;
    }
    let limits = MessageLimits::from_config(&config)
        .map_err(|error| format!("{}:{error}", path.display()))?;
    let plan = ExecutionPlan::compile(&config);

    // A deferred decision needs a replayable private copy of stdin. Requiring
    // MAILDIR before reading headers prevents a configuration failure from
    // consuming part of a message that the caller may need to retry.
    if command.action == Action::Filter
        && plan.requirements().needs_end_of_message
        && staging_directory.is_none()
    {
        return Err(format!(
            "{}: MAILDIR is required when a recipe needs the body or final message size",
            path.display()
        ));
    }

    match command.action {
        Action::Check => Ok(()),
        Action::Filter => {
            let mut runtime = RuntimeVariables::default();
            let mut trace = NoTrace;
            let mut stdin = io::stdin().lock();
            let head = Message::read_headers(&mut stdin, limits)
                .map_err(|error| format!("cannot read message headers from stdin: {error}"))?;
            match plan.evaluate_headers_with_trace(&head, &mut runtime, &mut trace) {
                HeaderEvaluation::Decided(delivery) => {
                    deliver_decided(head, &mut stdin, &delivery, &mut runtime, &mut trace)
                }
                HeaderEvaluation::NeedsMessage(continuation) => {
                    let staging_directory = staging_directory.as_deref().ok_or_else(|| {
                        "internal error: deferred evaluation has no staging directory".to_owned()
                    })?;
                    deliver_staged(
                        head,
                        &mut stdin,
                        &plan,
                        continuation,
                        staging_directory,
                        &mut runtime,
                        &mut trace,
                    )
                }
                HeaderEvaluation::Error(error) => Err(format!("cannot evaluate message: {error}")),
            }
        }
    }
}

fn validate_destination_types(config: &config::Config, rc_path: &Path) -> Result<(), String> {
    // Recipe selection depends on hostile message data, so every reachable
    // backend must be known before stdin is touched. Validate the complete rc
    // file here instead of waiting until a selected recipe opens its sink.
    for statement in &config.statements {
        let Statement::Recipe(recipe) = statement else {
            continue;
        };
        let message = match &recipe.destination {
            Destination::Maildir(_) => continue,
            Destination::Mbox(_) => "mbox delivery is not implemented",
        };
        return Err(format!(
            "{}:line {}: {message}",
            rc_path.display(),
            recipe.action_line
        ));
    }
    Ok(())
}

fn deliver_decided(
    head: procmail_rs::message::MessageHead,
    reader: &mut impl io::BufRead,
    plan: &DeliveryPlan,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), String> {
    let sinks = open_sinks(plan.destinations(), runtime, trace)?;
    let pending = PendingFanout::new(sinks).map_err(|error| error.to_string())?;
    let (validated, _) = pending
        .stream(head, reader)
        .map_err(|error| format!("cannot stream message from stdin: {error}"))?;
    commit_delivery(validated, plan.destinations(), runtime, trace)?;

    delivery_outcome(plan)
}

fn deliver_staged(
    head: procmail_rs::message::MessageHead,
    reader: &mut impl io::BufRead,
    execution: &ExecutionPlan,
    continuation: procmail_rs::eval::Continuation,
    staging_directory: &Path,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), String> {
    let early_count = continuation.pending_destinations().len();
    let early_sinks = if execution.requires_ordered_delivery() {
        Vec::new()
    } else {
        open_sinks(continuation.pending_destinations(), runtime, trace)?
    };
    let pending = PendingFanout::new(early_sinks).map_err(|error| error.to_string())?;
    let mut staging = StagingFile::create(staging_directory)
        .map_err(|error| format!("cannot create private staging file: {error}"))?;
    let header_len = head.len();

    // Early copies and staging receive identical bytes in one pass over stdin.
    // Neither side is published yet, so any failure drops both private outputs
    // before the caller can observe a partial message.
    let (validated, _) = pending
        .stage(head, reader, &mut staging)
        .map_err(|error| format!("cannot stage message from stdin: {error}"))?;
    let staged = staging
        .map(MAX_MESSAGE_SIZE, header_len)
        .map_err(|error| format!("cannot map staged message: {error}"))?;

    let plan = execution
        .resume_mapped_with_trace(
            continuation,
            staged.as_bytes(),
            staged.header_len(),
            runtime,
            trace,
        )
        .map_err(|error| format!("cannot evaluate message: {error}"))?;
    if execution.requires_ordered_delivery() {
        deliver_ordered(&plan, staged.as_bytes(), runtime, trace)?;
        return delivery_outcome(&plan);
    }
    let late_destinations = plan.destinations().get(early_count..).ok_or_else(|| {
        "internal error: deferred delivery discarded an early copy destination".to_owned()
    })?;
    let late_sinks = open_sinks(late_destinations, runtime, trace)?;
    let late = PendingFanout::new(late_sinks).map_err(|error| error.to_string())?;
    let validated = validated
        .append_bytes(late, staged.as_bytes())
        .map_err(|error| error.to_string())?;
    commit_delivery(validated, plan.destinations(), runtime, trace)?;

    delivery_outcome(&plan)
}

fn deliver_ordered(
    plan: &DeliveryPlan,
    message: &[u8],
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), String> {
    // A later path may depend on the exact file published by an earlier copy.
    // Publish one complete staged copy at a time and update runtime values
    // before resolving the next expression.
    for destination in plan.destinations() {
        let mut sinks = open_sinks(std::slice::from_ref(destination), runtime, trace)?;
        let mut sink = sinks
            .pop()
            .ok_or_else(|| "internal error: destination produced no sink".to_owned())?;
        sink.write_all(message)
            .map_err(|error| format!("cannot write staged delivery: {error}"))?;
        let published = sink.commit().map_err(|error| {
            record_delivery(
                destination,
                DeliveryStage::Failed(FailureClass::Transient),
                trace,
            );
            format!("cannot publish Maildir delivery: {error}")
        })?;
        record_delivery(destination, DeliveryStage::Published, trace);
        runtime.record_delivery_with_trace(&published, trace)?;
    }
    Ok(())
}

fn commit_delivery(
    validated: procmail_rs::delivery::ValidatedFanout,
    destinations: &[Destination],
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), String> {
    // Each sink reports the path it actually made visible. Update LASTFOLDER
    // from that report, including the last successful sink in a partial
    // fan-out, instead of guessing from the requested destination directory.
    match validated.commit() {
        Ok(report) => {
            for destination in destinations.iter().take(report.published().len()) {
                record_delivery(destination, DeliveryStage::Published, trace);
            }
            runtime.record_commit_with_trace(&report, trace)
        }
        Err(error) => {
            for destination in destinations.iter().take(error.published().len()) {
                record_delivery(destination, DeliveryStage::Published, trace);
            }
            if let Some(destination) = destinations.get(error.published().len()) {
                record_delivery(
                    destination,
                    DeliveryStage::Failed(FailureClass::Transient),
                    trace,
                );
            }
            runtime.record_partial_commit_with_trace(&error, trace)?;
            Err(format!("cannot publish Maildir delivery: {error}"))
        }
    }
}

fn open_sinks(
    destinations: &[Destination],
    runtime: &RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<Vec<Box<dyn PendingSink>>, String> {
    let mut sinks: Vec<Box<dyn PendingSink>> = Vec::with_capacity(destinations.len());
    for unresolved in destinations {
        record_delivery(unresolved, DeliveryStage::Preparing, trace);
        let destination = unresolved
            .resolve_with(|name| runtime.get(name).map(str::to_owned))
            .map_err(|error| {
                record_delivery(
                    unresolved,
                    DeliveryStage::Failed(FailureClass::Permanent),
                    trace,
                );
                error.to_string()
            })?;
        match &destination {
            Destination::Maildir(expression) => {
                let path = Path::new(expression.source());
                let sink = MaildirSink::create(path).map_err(|error| {
                    record_delivery(
                        unresolved,
                        DeliveryStage::Failed(FailureClass::Transient),
                        trace,
                    );
                    format!("cannot open Maildir {}: {error}", path.display())
                })?;
                sinks.push(Box::new(sink));
            }
            Destination::Mbox(expression) => {
                record_delivery(
                    unresolved,
                    DeliveryStage::Failed(FailureClass::Permanent),
                    trace,
                );
                return Err(format!(
                    "mbox delivery is not implemented yet: {}",
                    expression.source()
                ));
            }
        }
    }
    Ok(sinks)
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

fn delivery_outcome(plan: &DeliveryPlan) -> Result<(), String> {
    if plan.original_delivered() {
        Ok(())
    } else {
        Err(format!(
            "original message was not delivered (published {} copy destination(s))",
            plan.copies()
        ))
    }
}

fn read_config(path: &Path) -> Result<String, String> {
    let file =
        File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut source = Vec::with_capacity(MAX_RC_SIZE.min(64 * 1024));
    file.take((MAX_RC_SIZE + 1) as u64)
        .read_to_end(&mut source)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if source.len() > MAX_RC_SIZE {
        return Err(format!(
            "cannot read {}: rc file exceeds the hard limit of {MAX_RC_SIZE} bytes",
            path.display()
        ));
    }
    String::from_utf8(source)
        .map_err(|_| format!("cannot read {}: rc file is not valid UTF-8", path.display()))
}

fn parse_args() -> Result<Command, String> {
    let mut args = env::args_os().skip(1);
    let action = args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .ok_or_else(usage)?;
    let action = match action.as_str() {
        "check" => Action::Check,
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
    "usage: procmail-rs <check|filter> --config PATH [--set NAME=VALUE]...".into()
}
