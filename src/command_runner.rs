// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io::{self, Write};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use procmail_rs::config::{ActionInput, ActionMode, OutputEnding, RecipeOptions, WriteErrorMode};
use procmail_rs::environment::{ProcessEnvironment, ShellPolicy};
use procmail_rs::eval::{
    CapturedCommand, DeliveryAttemptError, ExternalActionInput, RecipeLockGuard,
};
use procmail_rs::external_command::{
    ChildExit, CommandDecision, CommandOutcome, CommandOutcomePolicy, FilterOutput,
};
use procmail_rs::external_process::{
    BackgroundProgramRun, CaptureOptions, CaptureRun, FilterOptions, FilterRun, ProgramOptions,
    ProgramRun, run_capture_with_timeout, run_filter, run_program_in_background,
    run_program_with_timeout, run_trap_with_timeout,
};
use procmail_rs::limits::MessageLimits;
use procmail_rs::message::Message;
use procmail_rs::runtime::{RuntimeSettings, RuntimeVariables};

use super::{ExitStatus, OperationalError};
use crate::command_log::{CommandLog, DiagnosticWriteError, TrapOutputError};

pub const MAX_BACKGROUND_COMMANDS: usize = 128;

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

struct ProcessedCommand<T> {
    run: T,
    decision: CommandDecision,
    outcome: CommandOutcome,
}

struct AcceptedCommand<T>(T);

impl<T> ProcessedCommand<T> {
    fn require(
        self,
        accepted: impl FnOnce(CommandDecision) -> bool,
        failure: impl FnOnce(CommandOutcome) -> String,
    ) -> Result<AcceptedCommand<T>, DeliveryAttemptError<OperationalError>> {
        if accepted(self.decision) {
            Ok(AcceptedCommand(self.run))
        } else {
            Err(recoverable_error(failure(self.outcome)))
        }
    }
}

impl<T> AcceptedCommand<T> {
    fn into_inner(self) -> T {
        self.0
    }
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

struct BackgroundCommand {
    run: BackgroundProgramRun,
    _lock: Option<Box<dyn RecipeLockGuard>>,
}

#[derive(Default)]
struct BackgroundCommandBudget {
    started: AtomicUsize,
}

impl BackgroundCommandBudget {
    fn reserve(&self) -> Result<(), String> {
        self.started
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |started| {
                (started < MAX_BACKGROUND_COMMANDS).then_some(started + 1)
            })
            .map(|_| ())
            .map_err(|_| {
                format!(
                    "background external commands exceed the hard limit of {MAX_BACKGROUND_COMMANDS} per message"
                )
            })
    }
}

pub struct CommandRunner {
    limits: MessageLimits,
    background: Vec<BackgroundCommand>,
    background_budget: Arc<BackgroundCommandBudget>,
}

impl CommandRunner {
    pub fn new(limits: MessageLimits) -> Self {
        Self {
            limits,
            background: Vec::new(),
            background_budget: Arc::new(BackgroundCommandBudget::default()),
        }
    }

    pub fn fork(&self) -> Self {
        Self {
            limits: self.limits,
            background: Vec::new(),
            background_budget: Arc::clone(&self.background_budget),
        }
    }

    pub fn condition(
        &mut self,
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
        Ok(run
            .outcome()
            .decide(CommandOutcomePolicy::Condition)
            .accepted())
    }

    pub fn capture(
        &mut self,
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
        let policy = recipe_options.map_or(CommandOutcomePolicy::ExpansionCapture, |options| {
            CommandOutcomePolicy::RecipeCapture {
                child_status: options.child_status,
                write_errors: options.write_errors,
            }
        });
        let outcome = run.outcome();
        let run = process_command(run, outcome, policy, runtime)?
            .require(CommandDecision::accepted, |outcome| {
                if outcome.child_exit() == ChildExit::TimedOut {
                    "command assignment exceeded TIMEOUT".to_owned()
                } else {
                    "command capture did not complete successfully".to_owned()
                }
            })?
            .into_inner();
        let output = run.into_output().map_err(|error| {
            recoverable_error(format!("command returned invalid captured output: {error}"))
        })?;
        Ok(CapturedCommand::new(output))
    }

    pub fn action(
        &mut self,
        command: &str,
        options: RecipeOptions,
        input: ExternalActionInput<'_>,
        lock: Option<Box<dyn RecipeLockGuard>>,
        runtime: &mut RuntimeVariables,
    ) -> Result<Option<Message>, DeliveryAttemptError<OperationalError>> {
        if command.is_empty() {
            return write_stdout(input.selected(), options, runtime);
        }
        if options.action_mode == ActionMode::Deliver
            && options.child_status == procmail_rs::config::ChildStatusMode::Ignore
        {
            return self.start_unwaited_action(command, options, input.selected(), lock, runtime);
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
                let outcome = run.outcome();
                process_command(
                    run,
                    outcome,
                    CommandOutcomePolicy::Pipe {
                        child_status: options.child_status,
                        write_errors: options.write_errors,
                    },
                    runtime,
                )?
                .require(CommandDecision::accepted, |_| {
                    "external program did not complete successfully".to_owned()
                })?;
                runtime.set("LASTFOLDER", command);
                Ok(None)
            }
            CommandRun::Filter(run) => finish_filter(run, input, options, self.limits, runtime),
            CommandRun::Capture(_) => Err(internal_runner_error(
                "pipe action returned captured command output",
            )),
        }
    }

    fn start_unwaited_action(
        &mut self,
        command: &str,
        options: RecipeOptions,
        input: &[u8],
        lock: Option<Box<dyn RecipeLockGuard>>,
        runtime: &mut RuntimeVariables,
    ) -> Result<Option<Message>, DeliveryAttemptError<OperationalError>> {
        self.background_budget
            .reserve()
            .map_err(recoverable_error)?;
        self.background.try_reserve(1).map_err(|_| {
            recoverable_error("cannot reserve background external command supervision")
        })?;
        let prepared = prepare(runtime)?;
        let run = run_program_in_background(
            &prepared.shell_policy,
            &prepared.environment,
            command,
            input,
            ProgramOptions::new(options.output_ending, options.action_input)
                .with_timeout(prepared.timeout),
            prepared.stderr,
        )
        .map_err(process_error)?;
        let outcome = CommandOutcome::new(run.input_write(), ChildExit::Success);

        // Register the waiter before interpreting a write failure. Even when
        // the recipe handles that failure through `i` or `e`, the shell still
        // belongs to this message and must be timed out and reaped.
        self.background.push(BackgroundCommand { run, _lock: lock });
        if !outcome
            .decide(CommandOutcomePolicy::Pipe {
                child_status: options.child_status,
                write_errors: options.write_errors,
            })
            .accepted()
        {
            return Err(recoverable_error(
                "cannot write complete message to external program",
            ));
        }
        runtime.set("LASTFOLDER", command);
        Ok(None)
    }

    pub fn finish_background(&mut self) -> Result<(), OperationalError> {
        for command in self.background.drain(..) {
            command.run.wait().map_err(|error| {
                OperationalError::Internal(format!(
                    "cannot supervise background external command: {error}"
                ))
            })?;
        }
        Ok(())
    }

    pub fn trap(&mut self, message: &[u8], runtime: &mut RuntimeVariables, provisional_status: u8) {
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
                let outcome = run.outcome();
                let decision = outcome.decide(CommandOutcomePolicy::Trap);
                if outcome.child_exit() == ChildExit::TimedOut {
                    report_trap_diagnostic(runtime, "TRAP exceeded TIMEOUT");
                }
                if let Some(code) = run.exit_code().filter(|code| *code != 0) {
                    runtime.set("EXITCODE", code.to_string());
                } else if !decision.accepted() {
                    runtime.set(
                        "EXITCODE",
                        (ExitStatus::TemporaryDelivery as u8).to_string(),
                    );
                }
            }
            Ok(run) => {
                if run.outcome().child_exit() == ChildExit::TimedOut {
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
        &mut self,
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
        &mut self,
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
    let outcome = run.outcome();
    let output_state = run.output_state();
    let processed = process_command(
        run,
        outcome,
        CommandOutcomePolicy::Filter {
            child_status: options.child_status,
            write_errors: options.write_errors,
            output: output_state,
        },
        runtime,
    )?;
    if output_state == FilterOutput::Failed {
        return match processed.run.into_output() {
            Err(error) => Err(recoverable_error(format!(
                "external filter returned an invalid message: {error}"
            ))),
            Ok(_) => Err(internal_runner_error(
                "failed filter output did not retain its validation error",
            )),
        };
    }
    let run = processed
        .require(CommandDecision::replace_message, |_| {
            "external filter did not complete successfully".to_owned()
        })?
        .into_inner();
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

fn process_command<T>(
    run: T,
    outcome: CommandOutcome,
    policy: CommandOutcomePolicy,
    runtime: &RuntimeVariables,
) -> Result<ProcessedCommand<T>, DeliveryAttemptError<OperationalError>> {
    let decision = outcome.decide(policy);

    // Emit the policy-selected diagnostic before returning the child result.
    // Keeping this step next to policy evaluation prevents capture, pipe, and
    // filter callers from drifting apart while leaving their output decoding
    // and user-facing failure text under form-specific control.
    if decision.report_child_failure() {
        report_child_failure(runtime, outcome.child_exit()).map_err(log_error)?;
    }
    Ok(ProcessedCommand {
        run,
        decision,
        outcome,
    })
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
    let diagnostic: &[u8] = match child_exit {
        ChildExit::Success => return Ok(()),
        ChildExit::ExitFailure => b"procmail-rs: external command exited unsuccessfully\n",
        ChildExit::Signaled => b"procmail-rs: external command terminated by a signal\n",
        ChildExit::TimedOut => b"procmail-rs: external command exceeded TIMEOUT\n",
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

#[cfg(test)]
#[path = "tests/command_runner.rs"]
mod tests;

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
