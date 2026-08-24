// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![forbid(unsafe_code)]

#[cfg(not(all(
    target_os = "linux",
    any(target_pointer_width = "32", target_pointer_width = "64")
)))]
compile_error!("procmail-rs currently supports only 32-bit and 64-bit Linux targets");

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use procmail_rs::config::{
    self, ActionMode, Destination, MAX_COMMAND_LINE_VARIABLES, SuppliedVariable, parse_umask,
};
use procmail_rs::delivery::local_lock::{
    LocalLock, LockMethod, lock_timeout_from_config, parse_lock_timeout,
};
use procmail_rs::delivery::maildir::{Durability, MaildirSink};
use procmail_rs::delivery::mbox::MboxFile;
use procmail_rs::delivery::staging::StagingFile;
use procmail_rs::delivery::{DeliveryFailureClass, PendingFanout, PendingSink};
use procmail_rs::environment::{ProcessEnvironment, ShellPolicy};
use procmail_rs::eval::{
    CompletionState, ConditionKindExplanation, DeliveryAttemptError, DeliveryPlan, DestinationKind,
    ExecutionPlan, ExternalActionInput, FinalMessage, HeaderEvaluation, MappedMessageInput,
    MatchingMessage, OrderedExecutionError, PlanExplanation, PlannedDelivery, RecipeLockGuard,
};
use procmail_rs::external_filter::{ChildExit, FilterOutput, decide_filter, decide_program};
use procmail_rs::external_process::{
    FilterOptions, parse_process_timeout, process_timeout_from_config, run_filter,
    run_program_with_timeout, run_trap_with_timeout,
};
use procmail_rs::hostname::current_hostname;
use procmail_rs::limits::{MAX_MESSAGE_SIZE, MessageLimits};
use procmail_rs::message::Message;
use procmail_rs::rc_file::RcFileLoader;
use procmail_rs::runtime::RuntimeVariables;
use procmail_rs::trace::{
    DeliveryStage, DestinationKind as TraceDestinationKind, FailureClass, NoTrace, TraceConfig,
    TraceEvent, TraceSink,
};
use procmail_rs::user_identity::UserIdentity;

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
    limits: MessageLimits,
    uid: u32,
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
        Ok(status) => ExitCode::from(status),
        Err(error) => {
            eprintln!("procmail-rs: {error}");
            ExitCode::from(error.exit_status() as u8)
        }
    }
}

fn run() -> Result<u8, OperationalError> {
    let command = parse_args().map_err(OperationalError::Configuration)?;
    let identity = UserIdentity::current().map_err(|error| {
        OperationalError::Configuration(format!("cannot determine current user identity: {error}"))
    })?;
    let hostname = current_hostname().map_err(|error| {
        OperationalError::Configuration(format!("cannot determine current hostname: {error}"))
    })?;
    let mut supplied = vec![
        SuppliedVariable::from_environment("HOME", identity.home().to_owned())
            .map_err(|error| OperationalError::Configuration(error.to_string()))?,
        SuppliedVariable::from_environment("LOGNAME", identity.logname().to_owned())
            .map_err(|error| OperationalError::Configuration(error.to_string()))?,
        SuppliedVariable::from_system_hostname(hostname.clone())
            .map_err(|error| OperationalError::Configuration(error.to_string()))?,
        SuppliedVariable::from_program_version()
            .map_err(|error| OperationalError::Configuration(error.to_string()))?,
    ];
    supplied.extend(command.supplied.iter().cloned());
    let path = &command.config;
    let (mut rc_loader, root_rc) = RcFileLoader::for_root(path)
        .map_err(|error| OperationalError::Configuration(error.to_string()))?;
    let config = config::parse(root_rc.source())
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?
        .expand_with(&supplied)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;
    rc_loader
        .account_root_config(&config)
        .map_err(|error| OperationalError::Configuration(error.to_string()))?;
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
    let _lock_method = LockMethod::from_config(&config)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;
    let _lock_timeout = lock_timeout_from_config(&config)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;
    let _process_timeout = process_timeout_from_config(&config)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;
    let _umask = config::umask_from_config(&config)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;
    let _trace_config = TraceConfig::from_config(&config)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;

    config.for_each_compatibility_warning(|line, flag| {
        eprintln!(
            "procmail-rs: warning: {}:{line}: recipe flag '{flag}' has no effect on a block",
            path.display()
        );
    });

    // Check resolvable runtime files before building the lazy execution tree.
    // Message-derived paths remain unavailable without stdin, so report them
    // as bounded warnings instead of pretending they were validated.
    if command.action == Action::Check {
        let warnings = rc_loader
            .check_resolvable_files(&config)
            .map_err(|error| OperationalError::Configuration(error.to_string()))?;
        for warning in warnings {
            eprintln!("procmail-rs: warning: {warning}");
        }
        if config.has_external_commands() {
            eprintln!(
                "procmail-rs: warning: configuration contains external shell actions; no command was executed"
            );
        }
        return Ok(ExitStatus::Success as u8);
    }

    let plan = ExecutionPlan::compile_with_loader(&config, rc_loader);

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

    // Runtime rc diagnostics belong to the completed attempt, including an
    // attempt that later fails delivery. Run the action inside a closure so
    // every `?` returns here first and the bounded diagnostic queue is always
    // drained before this function returns to main.
    let mut requested_status = None;
    let result = (|| match command.action {
        Action::Check => unreachable!(),
        Action::Explain => {
            let mut stdout = io::stdout().lock();
            write_plan_explanation(&plan.explain(), &mut stdout).map_err(|error| {
                OperationalError::Internal(format!("cannot write plan explanation: {error}"))
            })
        }
        Action::Filter => {
            let mut runtime = RuntimeVariables::default();
            runtime.set_system_hostname(hostname);
            let mut trace = NoTrace;
            let mut stdin = io::stdin().lock();
            let head = Message::read_headers(&mut stdin, limits).map_err(|error| {
                OperationalError::Input(format!("cannot read message headers from stdin: {error}"))
            })?;
            let delivery_result =
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
                                limits,
                                uid: identity.uid(),
                            },
                            &mut runtime,
                            &mut trace,
                        )
                    }
                    HeaderEvaluation::Error(error) => Err(OperationalError::PermanentDestination(
                        format!("cannot evaluate message: {error}"),
                    )),
                };

            // EXITCODE is resolved after recipe processing because a failure
            // handler may assign it using values produced while filtering.
            // A valid value deliberately replaces a delivery error, matching
            // procmail's final-status override behavior.
            requested_status = parse_requested_exit_code(&runtime)?;
            if requested_status.is_some() {
                Ok(())
            } else {
                delivery_result
            }
        }
    })();
    for diagnostic in plan.take_rc_diagnostics() {
        eprintln!("procmail-rs: {diagnostic}");
    }
    result?;
    Ok(requested_status.unwrap_or(ExitStatus::Success as u8))
}

fn parse_requested_exit_code(runtime: &RuntimeVariables) -> Result<Option<u8>, OperationalError> {
    let Some(value) = runtime.get("EXITCODE") else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(OperationalError::PermanentDestination(
            "EXITCODE must be an unsigned decimal value from 0 through 255".to_owned(),
        ));
    }
    value.parse::<u8>().map(Some).map_err(|_| {
        OperationalError::PermanentDestination(
            "EXITCODE must be an unsigned decimal value from 0 through 255".to_owned(),
        )
    })
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
            DestinationKind::ExternalProgram => "external-program",
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
                ConditionKindExplanation::Program => "program",
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
    staging_options: StagingOptions<'_>,
    runtime: &mut RuntimeVariables,
    trace: &mut impl TraceSink,
) -> Result<(), OperationalError> {
    let early_count = continuation.pending_deliveries().len();
    let early_sinks = if execution.requires_ordered_delivery() {
        Vec::new()
    } else {
        open_sinks(
            continuation.pending_deliveries(),
            staging_options.durability,
            runtime,
            trace,
        )?
    };
    let pending = PendingFanout::new(early_sinks)
        .map_err(|error| OperationalError::Internal(error.to_string()))?;
    let mut staging = StagingFile::create(staging_options.directory).map_err(|error| {
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
            .map(|header| stage_matching_message(staging_options.directory, header, &staged))
            .transpose()?
    } else {
        None
    };
    let matching_raw = matching_staged.as_ref().map(|message| message.as_bytes());
    let matching = matching_header
        .as_deref()
        .map(|header| MatchingMessage::new(header, matching_raw));

    if execution.requires_ordered_delivery() {
        let mut global_lock = None;
        let outcome = execution
            .execute_mapped_ordered_with_processes_and_completion_trace(
                MappedMessageInput::new(staged.as_bytes(), staged.header_len(), matching),
                runtime,
                trace,
                &mut |destination, message, output_ending, lock, runtime, trace| {
                    let _local_lock =
                        acquire_recipe_lock(lock, Some(destination), runtime, staging_options.uid)
                            .map_err(DeliveryAttemptError::Recoverable)?;
                    let result = if matches!(destination, Destination::Mbox(_)) {
                        deliver_mbox(
                            destination,
                            message,
                            output_ending,
                            staging_options.durability,
                            runtime,
                            trace,
                        )
                    } else {
                        deliver_one_maildir(
                            destination,
                            message,
                            staging_options.durability,
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
                (
                    &mut |command, input, runtime, _| {
                        execute_external_condition(command, input, runtime)
                    },
                    &mut |action, recipe_options, lock, input, runtime, _| {
                        let _local_lock =
                            acquire_recipe_lock(lock, None, runtime, staging_options.uid)
                                .map_err(DeliveryAttemptError::Recoverable)?;
                        execute_external_action(
                            staging_options.limits,
                            action.command.as_str(),
                            recipe_options,
                            input,
                            runtime,
                        )
                    },
                    &mut |path, runtime| {
                        // Replacing LOCKFILE first releases the preceding
                        // global lock. If replacement fails, clear the visible
                        // value so later statements cannot mistake an unheld
                        // path for an active semaphore.
                        global_lock = None;
                        if path.is_empty() {
                            return Ok(());
                        }
                        match acquire_configured_lock(path, runtime, staging_options.uid) {
                            Ok(lock) => {
                                global_lock = Some(lock);
                                Ok(())
                            }
                            Err(error) => {
                                runtime.set("LOCKFILE".to_owned(), String::new());
                                Err(error)
                            }
                        }
                    },
                    &mut |path, runtime| {
                        acquire_configured_lock(path, runtime, staging_options.uid)
                            .map(|lock| Box::new(lock) as Box<dyn RecipeLockGuard>)
                            .map_err(DeliveryAttemptError::Recoverable)
                    },
                ),
                &mut |message: FinalMessage<'_>, runtime, _, state| {
                    execute_trap(message.as_bytes(), runtime, completion_exit_status(state));
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
    let late_sinks = open_sinks(late_deliveries, staging_options.durability, runtime, trace)?;
    let late = PendingFanout::new(late_sinks)
        .map_err(|error| OperationalError::Internal(error.to_string()))?;
    let validated = validated
        .append_bytes(late, staged.as_bytes())
        .map_err(|error| OperationalError::delivery(error.class(), error.to_string()))?;
    commit_delivery(validated, plan.deliveries(), runtime, trace)?;

    delivery_outcome(&plan)
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

fn execute_trap(message: &[u8], runtime: &mut RuntimeVariables, provisional_status: u8) {
    let Some(command) = runtime.get("TRAP").filter(|command| !command.is_empty()) else {
        return;
    };
    let command = command.to_owned();
    let exitcode_was_absent = runtime.get("EXITCODE").is_none();
    let exitcode_was_empty = runtime.get("EXITCODE") == Some("");
    if exitcode_was_absent {
        runtime.set("EXITCODE", provisional_status.to_string());
    }

    let result = (|| {
        let timeout = parse_process_timeout(runtime.get("TIMEOUT").unwrap_or("960"))?;
        let environment = ProcessEnvironment::from_runtime(runtime)
            .map_err(|error| format!("cannot build TRAP environment: {error}"))?;
        let configured_shell = environment
            .get("SHELL")
            .ok_or_else(|| "bounded TRAP environment has no SHELL".to_owned())?;
        let shell_policy =
            ShellPolicy::approve(configured_shell).map_err(|error| error.to_string())?;
        let (stdout, stderr) = trap_output(runtime);
        run_trap_with_timeout(
            &shell_policy,
            &environment,
            &command,
            message,
            timeout,
            stdout,
            stderr,
        )
        .map_err(|error| error.to_string())
    })();

    match result {
        Ok(run) if exitcode_was_empty => {
            if run.child_exit() == ChildExit::TimedOut {
                report_trap_diagnostic(runtime, "TRAP exceeded TIMEOUT");
            }
            if let Some(code) = run.exit_code().filter(|code| *code != 0) {
                runtime.set("EXITCODE", code.to_string());
            } else if run.child_exit() != ChildExit::Success {
                runtime.set(
                    "EXITCODE",
                    (ExitStatus::TemporaryDelivery as u8).to_string(),
                );
            }
        }
        Ok(run) => {
            if run.child_exit() == ChildExit::TimedOut {
                report_trap_diagnostic(runtime, "TRAP exceeded TIMEOUT");
            }
        }
        Err(error) => {
            report_trap_diagnostic(runtime, &format!("TRAP failed: {error}"));
            if exitcode_was_empty {
                runtime.set(
                    "EXITCODE",
                    (ExitStatus::TemporaryDelivery as u8).to_string(),
                );
            }
        }
    }
}

fn report_trap_diagnostic(runtime: &RuntimeVariables, message: &str) {
    let record = format!("procmail-rs: {message}\n");
    let result = match open_external_log(runtime) {
        Ok(Some(mut file)) => file.write_all(record.as_bytes()),
        Ok(None) => io::stderr().lock().write_all(record.as_bytes()),
        Err(error) => {
            eprintln!("procmail-rs: cannot write TRAP diagnostic to LOGFILE: {error}");
            return;
        }
    };
    if let Err(error) = result {
        eprintln!("procmail-rs: cannot write TRAP diagnostic: {error}");
    }
}

fn trap_output(runtime: &RuntimeVariables) -> (Stdio, Stdio) {
    match open_external_log(runtime) {
        Ok(Some(file)) => match file.try_clone() {
            Ok(stdout) => return (Stdio::from(stdout), Stdio::from(file)),
            Err(error) => {
                eprintln!("procmail-rs: cannot duplicate LOGFILE for TRAP output: {error}");
            }
        },
        Ok(None) => {}
        Err(error) => {
            eprintln!("procmail-rs: cannot open LOGFILE for TRAP output: {error}");
        }
    }

    // TRAP combines stdout with stderr in original procmail. Duplicate the
    // inherited descriptor instead of routing stdout to the caller's normal
    // output, where command text could corrupt a protocol-facing response.
    (Stdio::from(io::stderr()), Stdio::from(io::stderr()))
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
    let method = LockMethod::parse(runtime.get("LOCKMETHOD").unwrap_or("flock"))
        .map_err(OperationalError::PermanentDestination)?;
    let timeout = parse_lock_timeout(runtime.get("LOCKTIMEOUT").unwrap_or("1024"))
        .map_err(OperationalError::PermanentDestination)?;
    let mask = active_umask(runtime)?;
    LocalLock::acquire_with_mask(Path::new(path), method, uid, timeout, mask).map_err(|error| {
        OperationalError::delivery(
            DeliveryFailureClass::from_io_error(&error),
            format!("cannot acquire local lockfile: {error}"),
        )
    })
}

fn active_umask(runtime: &RuntimeVariables) -> Result<u32, OperationalError> {
    parse_umask(runtime.get("UMASK").unwrap_or("077"))
        .map_err(OperationalError::PermanentDestination)
}

fn execute_external_condition(
    command: &str,
    input: &[u8],
    runtime: &mut RuntimeVariables,
) -> Result<bool, DeliveryAttemptError<OperationalError>> {
    let timeout = parse_process_timeout(runtime.get("TIMEOUT").unwrap_or("960"))
        .map_err(recoverable_external_error)?;
    let environment = ProcessEnvironment::from_runtime(runtime).map_err(|error| {
        recoverable_external_error(format!(
            "cannot build external condition environment: {error}"
        ))
    })?;
    let configured_shell = environment
        .get("SHELL")
        .expect("bounded process environment always contains SHELL");
    let shell_policy = ShellPolicy::approve(configured_shell)
        .map_err(|error| recoverable_external_error(error.to_string()))?;
    let stderr = external_stderr(runtime).map_err(|error| {
        recoverable_external_error(format!("cannot open external command log: {error}"))
    })?;

    // Program conditions always wait for the child and decide solely from its
    // exit status. A child that closes stdin early must still be usable for
    // commands such as test(1), which do not consume the message at all.
    let run = run_program_with_timeout(
        &shell_policy,
        &environment,
        command,
        input,
        procmail_rs::config::OutputEnding::Preserve,
        timeout,
        stderr,
    )
    .map_err(|error| recoverable_external_error(error.to_string()))?;
    Ok(run.child_exit() == ChildExit::Success)
}

fn execute_external_action(
    limits: MessageLimits,
    command: &str,
    options: procmail_rs::config::RecipeOptions,
    input: ExternalActionInput<'_>,
    runtime: &mut RuntimeVariables,
) -> Result<Option<Message>, DeliveryAttemptError<OperationalError>> {
    let timeout = parse_process_timeout(runtime.get("TIMEOUT").unwrap_or("960"))
        .map_err(recoverable_external_error)?;
    let environment = ProcessEnvironment::from_runtime(runtime).map_err(|error| {
        recoverable_external_error(format!(
            "cannot build external command environment: {error}"
        ))
    })?;
    let configured_shell = environment
        .get("SHELL")
        .expect("bounded process environment always contains SHELL");
    let shell_policy = ShellPolicy::approve(configured_shell)
        .map_err(|error| recoverable_external_error(error.to_string()))?;
    let stderr = external_stderr(runtime).map_err(|error| {
        recoverable_external_error(format!("cannot open external command log: {error}"))
    })?;
    if options.action_mode == ActionMode::Deliver {
        let run = run_program_with_timeout(
            &shell_policy,
            &environment,
            command,
            input.selected(),
            options.output_ending,
            timeout,
            stderr,
        )
        .map_err(|error| recoverable_external_error(error.to_string()))?;
        let decision = decide_program(
            options.child_status,
            options.write_errors,
            run.input_write(),
            run.child_exit(),
        );
        if decision.report_child_failure() {
            report_external_child_failure(runtime, run.child_exit()).map_err(|error| {
                recoverable_external_error(format!(
                    "cannot write external command failure diagnostic: {error}"
                ))
            })?;
        }
        return if decision.succeeded() {
            runtime.set("LASTFOLDER", command);
            Ok(None)
        } else {
            Err(recoverable_external_error(
                "external program did not complete successfully",
            ))
        };
    }
    let run = run_filter(
        &shell_policy,
        &environment,
        command,
        input.selected(),
        FilterOptions::new(options.output_ending, options.action_input, limits)
            .with_timeout(timeout),
        stderr,
    )
    .map_err(|error| recoverable_external_error(error.to_string()))?;
    let decision = decide_filter(
        options.child_status,
        options.write_errors,
        run.input_write(),
        run.output_state(),
        run.child_exit(),
    );
    if decision.report_child_failure() {
        report_external_child_failure(runtime, run.child_exit()).map_err(|error| {
            recoverable_external_error(format!(
                "cannot write external command failure diagnostic: {error}"
            ))
        })?;
    }

    // Read and validate stdout before consulting the status decision. This
    // keeps the detailed bounded-input error available while the previous
    // message remains owned by the evaluator for a following error recipe.
    if run.output_state() == FilterOutput::Failed {
        let error = run
            .into_output()
            .expect_err("failed filter output retains its validation error");
        return Err(recoverable_external_error(format!(
            "external filter returned an invalid message: {error}"
        )));
    }
    if !decision.succeeded() {
        return Err(recoverable_external_error(
            "external filter did not complete successfully",
        ));
    }
    let output = run
        .into_output()
        .expect("successful filter output was validated");
    let replacement = Message::from_filter_output(
        input.header(),
        input.body(),
        &output,
        options.action_input,
        limits,
    )
    .map_err(|error| {
        recoverable_external_error(format!(
            "external filter returned an invalid replacement message: {error}"
        ))
    })?;
    Ok(Some(replacement))
}

fn recoverable_external_error(
    message: impl Into<String>,
) -> DeliveryAttemptError<OperationalError> {
    DeliveryAttemptError::Recoverable(OperationalError::TemporaryDelivery(message.into()))
}

fn external_stderr(runtime: &RuntimeVariables) -> io::Result<Stdio> {
    Ok(match open_external_log(runtime)? {
        Some(file) => Stdio::from(file),
        None => Stdio::inherit(),
    })
}

fn report_external_child_failure(
    runtime: &RuntimeVariables,
    child_exit: ChildExit,
) -> io::Result<()> {
    let diagnostic = if child_exit == ChildExit::TimedOut {
        b"procmail-rs: external command exceeded TIMEOUT\n".as_slice()
    } else {
        b"procmail-rs: external command exited unsuccessfully\n".as_slice()
    };
    match open_external_log(runtime)? {
        Some(mut file) => file.write_all(diagnostic),
        None => io::stderr().lock().write_all(diagnostic),
    }
}

fn open_external_log(runtime: &RuntimeVariables) -> io::Result<Option<File>> {
    let Some(path) = runtime.get("LOGFILE").filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let mask = parse_umask(runtime.get("UMASK").unwrap_or("077"))
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600 & !mask)
        .custom_flags(
            i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits())
                .expect("Linux O_NOFOLLOW fits in the std custom-flags type"),
        )
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "LOGFILE is not a regular file",
        ));
    }
    Ok(Some(file))
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
    let mask = active_umask(runtime).map_err(OrderedStepError::before_publication)?;
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

fn deliver_mbox(
    unresolved: &Destination,
    message: &[u8],
    output_ending: procmail_rs::config::OutputEnding,
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
    let lock_timeout = parse_lock_timeout(runtime.get("LOCKTIMEOUT").unwrap_or("1024"))
        .map_err(OperationalError::PermanentDestination)
        .map_err(OrderedStepError::before_publication)?;
    let mask = active_umask(runtime).map_err(OrderedStepError::before_publication)?;
    let locked = MboxFile::open_with_mask(path, mask)
        .and_then(|mbox| mbox.lock_with_timeout(lock_timeout))
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
    match locked.append(message, output_ending, durability) {
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
        let mask = parse_umask(delivery.umask()).map_err(OperationalError::PermanentDestination)?;
        sinks.push(open_sink(
            delivery.destination(),
            durability,
            mask,
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
#[path = "main_tests.rs"]
mod tests;
