// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io::{self, Write};
use std::process::Stdio;

use procmail_rs::config::{ActionInput, ActionMode, OutputEnding, RecipeOptions, WriteErrorMode};
use procmail_rs::environment::{ProcessEnvironment, ShellPolicy};
use procmail_rs::eval::{CapturedCommand, DeliveryAttemptError, ExternalActionInput};
use procmail_rs::external_filter::{ChildExit, FilterOutput, decide_filter, decide_program};
use procmail_rs::external_process::{
    CaptureOptions, CaptureRun, FilterOptions, FilterRun, ProgramOptions, ProgramRun,
    run_capture_with_timeout, run_filter, run_program_with_timeout, run_trap_with_timeout,
};
use procmail_rs::limits::MessageLimits;
use procmail_rs::message::Message;
use procmail_rs::runtime::{RuntimeSettings, RuntimeVariables};

use super::{ExitStatus, OperationalError};
use crate::command_log::{CommandLog, DiagnosticWriteError, TrapOutputError};

#[derive(Debug, Clone, Copy)]
enum InputSelection {
    Message,
    Recipe(ActionInput),
}

impl InputSelection {
    fn action_input(self) -> ActionInput {
        match self {
            Self::Message => ActionInput::Message,
            Self::Recipe(input) => input,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum OutputHandling {
    Discard,
    Capture { limit: usize },
    Filter { limits: MessageLimits },
}

#[derive(Debug, Clone, Copy)]
enum ChildStatusPolicy {
    Condition,
    Capture,
    Recipe,
}

#[derive(Debug, Clone, Copy)]
struct CommandRequest<'a> {
    command: &'a str,
    input: &'a [u8],
    input_selection: InputSelection,
    output_ending: OutputEnding,
    output: OutputHandling,
    child_status: ChildStatusPolicy,
}

enum CommandRun {
    Program(ProgramRun),
    Capture(CaptureRun),
    Filter(FilterRun),
}

struct PreparedCommand {
    environment: ProcessEnvironment,
    shell_policy: ShellPolicy,
    timeout: std::time::Duration,
    stderr: Stdio,
}

struct PreparedEnvironment {
    environment: ProcessEnvironment,
    shell_policy: ShellPolicy,
    timeout: std::time::Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct CommandRunner {
    limits: MessageLimits,
}

impl CommandRunner {
    pub fn new(limits: MessageLimits) -> Self {
        Self { limits }
    }

    pub fn condition(
        self,
        command: &str,
        input: &[u8],
        runtime: &mut RuntimeVariables,
    ) -> Result<bool, DeliveryAttemptError<OperationalError>> {
        let request = CommandRequest {
            command,
            input,
            input_selection: InputSelection::Message,
            output_ending: OutputEnding::Preserve,
            output: OutputHandling::Discard,
            child_status: ChildStatusPolicy::Condition,
        };
        let CommandRun::Program(run) = self.run(request, runtime)? else {
            return Err(internal_runner_error(
                "condition returned non-program output",
            ));
        };
        Ok(run.child_exit() == ChildExit::Success)
    }

    pub fn capture(
        self,
        command: &str,
        input: &[u8],
        output_ending: OutputEnding,
        recipe_options: Option<RecipeOptions>,
        limit: usize,
        runtime: &mut RuntimeVariables,
    ) -> Result<CapturedCommand, DeliveryAttemptError<OperationalError>> {
        let request = CommandRequest {
            command,
            input,
            input_selection: InputSelection::Recipe(
                recipe_options.map_or(ActionInput::Message, |options| options.action_input),
            ),
            output_ending,
            output: OutputHandling::Capture { limit },
            child_status: ChildStatusPolicy::Capture,
        };
        let CommandRun::Capture(run) = self.run(request, runtime)? else {
            return Err(internal_runner_error("capture returned non-capture output"));
        };
        let input_write = run.input_write();
        let child_exit = run.child_exit();

        if child_exit == ChildExit::TimedOut {
            report_child_failure(runtime, child_exit).map_err(log_error)?;
            return Err(recoverable_error("command assignment exceeded TIMEOUT"));
        }
        if let Some(options) = recipe_options {
            let decision = decide_program(
                options.child_status,
                options.write_errors,
                input_write,
                child_exit,
            );
            if decision.report_child_failure() {
                report_child_failure(runtime, child_exit).map_err(log_error)?;
            }
            if !decision.succeeded() {
                return Err(recoverable_error(
                    "command capture did not complete successfully",
                ));
            }
        }
        let output = run.into_output().map_err(|error| {
            recoverable_error(format!("command returned invalid captured output: {error}"))
        })?;
        Ok(CapturedCommand::new(output, input_write, child_exit))
    }

    pub fn action(
        self,
        command: &str,
        options: RecipeOptions,
        input: ExternalActionInput<'_>,
        runtime: &mut RuntimeVariables,
    ) -> Result<Option<Message>, DeliveryAttemptError<OperationalError>> {
        if command.is_empty() {
            return write_stdout(input.selected(), options, runtime);
        }
        let output = if options.action_mode == ActionMode::Deliver {
            OutputHandling::Discard
        } else {
            OutputHandling::Filter {
                limits: self.limits,
            }
        };
        let request = CommandRequest {
            command,
            input: input.selected(),
            input_selection: InputSelection::Recipe(options.action_input),
            output_ending: options.output_ending,
            output,
            child_status: ChildStatusPolicy::Recipe,
        };
        match self.run(request, runtime)? {
            CommandRun::Program(run) => {
                let decision = decide_program(
                    options.child_status,
                    options.write_errors,
                    run.input_write(),
                    run.child_exit(),
                );
                if decision.report_child_failure() {
                    report_child_failure(runtime, run.child_exit()).map_err(log_error)?;
                }
                if !decision.succeeded() {
                    return Err(recoverable_error(
                        "external program did not complete successfully",
                    ));
                }
                runtime.set("LASTFOLDER", command);
                Ok(None)
            }
            CommandRun::Filter(run) => finish_filter(run, input, options, self.limits, runtime),
            CommandRun::Capture(_) => Err(internal_runner_error(
                "pipe action returned captured command output",
            )),
        }
    }

    pub fn trap(self, message: &[u8], runtime: &mut RuntimeVariables, provisional_status: u8) {
        let Some(command) = runtime.get("TRAP").filter(|command| !command.is_empty()) else {
            return;
        };
        let command = command.to_owned();
        let exitcode_was_absent = runtime.get("EXITCODE").is_none();
        let exitcode_was_empty = runtime.get("EXITCODE") == Some("");
        if exitcode_was_absent {
            runtime.set("EXITCODE", provisional_status.to_string());
        }

        let result = self.run_trap(&command, message, runtime);
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

    fn run(
        self,
        request: CommandRequest<'_>,
        runtime: &RuntimeVariables,
    ) -> Result<CommandRun, DeliveryAttemptError<OperationalError>> {
        let prepared = prepare(runtime)?;

        // Reject mismatched policies before spawning. Output validation and
        // child status are deliberately paired because applying capture rules
        // to a filter, or recipe flags to a condition, changes error handling.
        if !matches!(
            (request.output, request.child_status),
            (OutputHandling::Discard, ChildStatusPolicy::Condition)
                | (OutputHandling::Capture { .. }, ChildStatusPolicy::Capture)
                | (OutputHandling::Discard, ChildStatusPolicy::Recipe)
                | (OutputHandling::Filter { .. }, ChildStatusPolicy::Recipe)
        ) {
            return Err(internal_runner_error(
                "command output and child-status policies do not match",
            ));
        }
        let action_input = request.input_selection.action_input();
        match request.output {
            OutputHandling::Discard => run_program_with_timeout(
                &prepared.shell_policy,
                &prepared.environment,
                request.command,
                request.input,
                ProgramOptions::new(request.output_ending, action_input)
                    .with_timeout(prepared.timeout),
                prepared.stderr,
            )
            .map(CommandRun::Program)
            .map_err(process_error),
            OutputHandling::Capture { limit } => run_capture_with_timeout(
                &prepared.shell_policy,
                &prepared.environment,
                request.command,
                request.input,
                CaptureOptions::new(request.output_ending, limit)
                    .with_timeout(prepared.timeout)
                    .with_action_input(action_input),
                prepared.stderr,
            )
            .map(CommandRun::Capture)
            .map_err(process_error),
            OutputHandling::Filter { limits } => run_filter(
                &prepared.shell_policy,
                &prepared.environment,
                request.command,
                request.input,
                FilterOptions::new(request.output_ending, action_input, limits)
                    .with_timeout(prepared.timeout),
                prepared.stderr,
            )
            .map(CommandRun::Filter)
            .map_err(process_error),
        }
    }

    fn run_trap(
        self,
        command: &str,
        message: &[u8],
        runtime: &RuntimeVariables,
    ) -> Result<ProgramRun, String> {
        let prepared = prepare_environment(runtime, "TRAP")?;
        let (stdout, stderr) = trap_output(runtime);
        run_trap_with_timeout(
            &prepared.shell_policy,
            &prepared.environment,
            command,
            message,
            prepared.timeout,
            stdout,
            stderr,
        )
        .map_err(|error| error.to_string())
    }
}

fn prepare(
    runtime: &RuntimeVariables,
) -> Result<PreparedCommand, DeliveryAttemptError<OperationalError>> {
    let prepared = prepare_environment(runtime, "external command").map_err(recoverable_error)?;
    let stderr = CommandLog::new(runtime)
        .stderr()
        .map_err(|error| recoverable_error(format!("cannot open external command log: {error}")))?;
    Ok(PreparedCommand {
        environment: prepared.environment,
        shell_policy: prepared.shell_policy,
        timeout: prepared.timeout,
        stderr,
    })
}

fn prepare_environment(
    runtime: &RuntimeVariables,
    purpose: &str,
) -> Result<PreparedEnvironment, String> {
    let timeout = RuntimeSettings::new(runtime)
        .process_timeout()
        .map_err(|error| error.to_string())?;
    let environment = ProcessEnvironment::from_runtime(runtime)
        .map_err(|error| format!("cannot build {purpose} environment: {error}"))?;
    let configured_shell = environment
        .get("SHELL")
        .ok_or_else(|| format!("bounded {purpose} environment does not contain SHELL"))?;
    let shell_policy = ShellPolicy::approve(configured_shell).map_err(|error| error.to_string())?;
    Ok(PreparedEnvironment {
        environment,
        shell_policy,
        timeout,
    })
}

fn finish_filter(
    run: FilterRun,
    input: ExternalActionInput<'_>,
    options: RecipeOptions,
    limits: MessageLimits,
    runtime: &RuntimeVariables,
) -> Result<Option<Message>, DeliveryAttemptError<OperationalError>> {
    let decision = decide_filter(
        options.child_status,
        options.write_errors,
        run.input_write(),
        run.output_state(),
        run.child_exit(),
    );
    if decision.report_child_failure() {
        report_child_failure(runtime, run.child_exit()).map_err(log_error)?;
    }
    if run.output_state() == FilterOutput::Failed {
        return match run.into_output() {
            Err(error) => Err(recoverable_error(format!(
                "external filter returned an invalid message: {error}"
            ))),
            Ok(_) => Err(internal_runner_error(
                "failed filter output did not retain its validation error",
            )),
        };
    }
    if !decision.succeeded() {
        return Err(recoverable_error(
            "external filter did not complete successfully",
        ));
    }
    let output = run.into_output().map_err(|error| {
        internal_runner_error(&format!(
            "validated filter output retained an unexpected error: {error}"
        ))
    })?;
    let replacement = Message::from_filter_output(
        input.header(),
        input.body(),
        &output,
        options.action_input,
        limits,
    )
    .map_err(|error| {
        recoverable_error(format!(
            "external filter returned an invalid replacement message: {error}"
        ))
    })?;
    Ok(Some(replacement))
}

fn write_stdout(
    input: &[u8],
    options: RecipeOptions,
    runtime: &mut RuntimeVariables,
) -> Result<Option<Message>, DeliveryAttemptError<OperationalError>> {
    let mut stdout = io::stdout().lock();
    let result = stdout.write_all(input).and_then(|()| {
        if options.output_ending == OutputEnding::Normalize && !input.ends_with(b"\n") {
            stdout.write_all(b"\n")?;
        }
        stdout.flush()
    });
    if let Err(error) = result
        && options.write_errors == WriteErrorMode::Fail
    {
        return Err(recoverable_error(format!(
            "cannot write message to stdout: {error}"
        )));
    }
    runtime.set("LASTFOLDER", "|");
    Ok(None)
}

fn process_error(error: impl std::fmt::Display) -> DeliveryAttemptError<OperationalError> {
    recoverable_error(error.to_string())
}

fn internal_runner_error(message: &str) -> DeliveryAttemptError<OperationalError> {
    DeliveryAttemptError::Fatal(OperationalError::Internal(message.to_owned()))
}

fn recoverable_error(message: impl Into<String>) -> DeliveryAttemptError<OperationalError> {
    DeliveryAttemptError::Recoverable(OperationalError::TemporaryDelivery(message.into()))
}

fn log_error(error: io::Error) -> DeliveryAttemptError<OperationalError> {
    recoverable_error(format!(
        "cannot write external command failure diagnostic: {error}"
    ))
}

fn report_child_failure(runtime: &RuntimeVariables, child_exit: ChildExit) -> io::Result<()> {
    let diagnostic = if child_exit == ChildExit::TimedOut {
        b"procmail-rs: external command exceeded TIMEOUT\n".as_slice()
    } else {
        b"procmail-rs: external command exited unsuccessfully\n".as_slice()
    };
    CommandLog::new(runtime)
        .write_diagnostic(diagnostic)
        .map_err(DiagnosticWriteError::into_io_error)
}

fn report_trap_diagnostic(runtime: &RuntimeVariables, message: &str) {
    let record = format!("procmail-rs: {message}\n");
    match CommandLog::new(runtime).write_diagnostic(record.as_bytes()) {
        Ok(()) => {}
        Err(DiagnosticWriteError::Open(error)) => {
            eprintln!("procmail-rs: cannot write TRAP diagnostic to LOGFILE: {error}");
        }
        Err(DiagnosticWriteError::Write(error)) => {
            eprintln!("procmail-rs: cannot write TRAP diagnostic: {error}");
        }
    }
}

fn trap_output(runtime: &RuntimeVariables) -> (Stdio, Stdio) {
    match CommandLog::new(runtime).trap_output() {
        Ok(output) => return output,
        Err(TrapOutputError::Open(error)) => {
            eprintln!("procmail-rs: cannot open LOGFILE for TRAP output: {error}");
        }
        Err(TrapOutputError::Duplicate(error)) => {
            eprintln!("procmail-rs: cannot duplicate LOGFILE for TRAP output: {error}");
        }
    }
    CommandLog::inherited_trap_output()
}
