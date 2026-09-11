// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::io::{BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

use crate::bounded_bytes::{BoundedBytes, BoundedBytesError};
use crate::config::{ActionInput, AssignmentTarget, Config, OutputEnding, Statement};
use crate::environment::{ProcessEnvironment, ShellPolicy};
use crate::external_command::{ChildExit, CommandOutcome, FilterOutput, InputWrite};
use crate::limits::MessageLimits;
use crate::message::{Message, MessageReadError};

pub const DEFAULT_PROCESS_TIMEOUT: Duration = Duration::from_secs(960);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const TERMINATION_GRACE: Duration = Duration::from_millis(250);

pub fn process_timeout_from_config(config: &Config) -> Result<Duration, String> {
    let mut timeout = DEFAULT_PROCESS_TIMEOUT;
    for statement in &config.statements {
        let Statement::Assignment(assignment) = statement else {
            continue;
        };
        if assignment.target != AssignmentTarget::ProcessTimeout {
            continue;
        }
        timeout = parse_process_timeout(&assignment.value)
            .map_err(|error| format!("line {}: {error}", assignment.line))?;
    }
    Ok(timeout)
}

pub fn parse_process_timeout(value: &str) -> Result<Duration, String> {
    crate::config::parse_process_timeout_seconds(value).map(Duration::from_secs)
}

#[derive(Debug)]
pub struct FilterRun {
    input_write: InputWrite,
    output: Result<Message, MessageReadError>,
    child_exit: ChildExit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramRun {
    input_write: InputWrite,
    child_exit: ChildExit,
    exit_code: Option<u8>,
}

#[derive(Debug)]
pub struct CaptureRun {
    input_write: InputWrite,
    output: std::io::Result<Vec<u8>>,
    child_exit: ChildExit,
    exit_code: Option<u8>,
}

#[derive(Debug)]
pub struct BackgroundProgramRun {
    input_write: InputWrite,
    waiter: thread::JoinHandle<Result<(ExitStatus, bool), ExternalProcessError>>,
}

struct ProgramIoOptions {
    output_ending: OutputEnding,
    timeout: Duration,
    stdout: Stdio,
    stderr: Stdio,
    append_lf: bool,
    body_input: bool,
}

#[derive(Clone, Copy)]
struct ProcessInput<'a> {
    bytes: &'a [u8],
    output_ending: OutputEnding,
    append_lf: bool,
    body_input: bool,
}

struct ChildLifecycle {
    child: Child,
    stdin: ChildStdin,
    timeout: Duration,
}

#[derive(Clone, Copy)]
struct CompletedChild {
    input_write: InputWrite,
    child_exit: ChildExit,
    exit_code: Option<u8>,
}

struct OutputConsumption<T> {
    value: T,
    failed: bool,
    timed_out: bool,
}

// These settings jointly describe how one filter invocation consumes and
// validates bytes. Keeping them together makes it harder to reuse a message
// limit with the wrong selected area or output-ending policy at a call site.
#[derive(Debug, Clone, Copy)]
pub struct FilterOptions {
    output_ending: OutputEnding,
    action_input: ActionInput,
    limits: MessageLimits,
    timeout: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureOptions {
    output_ending: OutputEnding,
    timeout: Duration,
    output_limit: usize,
    action_input: ActionInput,
}

#[derive(Debug, Clone, Copy)]
pub struct ProgramOptions {
    output_ending: OutputEnding,
    action_input: ActionInput,
    timeout: Duration,
}

impl ProgramOptions {
    pub fn new(output_ending: OutputEnding, action_input: ActionInput) -> Self {
        Self {
            output_ending,
            action_input,
            timeout: DEFAULT_PROCESS_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl CaptureOptions {
    pub fn new(output_ending: OutputEnding, output_limit: usize) -> Self {
        Self {
            output_ending,
            timeout: DEFAULT_PROCESS_TIMEOUT,
            output_limit,
            action_input: ActionInput::Message,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_action_input(mut self, action_input: ActionInput) -> Self {
        self.action_input = action_input;
        self
    }
}

impl FilterOptions {
    pub fn new(
        output_ending: OutputEnding,
        action_input: ActionInput,
        limits: MessageLimits,
    ) -> Self {
        Self {
            output_ending,
            action_input,
            limits,
            timeout: DEFAULT_PROCESS_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl ProgramRun {
    pub fn outcome(self) -> CommandOutcome {
        CommandOutcome::new(self.input_write, self.child_exit)
    }

    pub fn exit_code(self) -> Option<u8> {
        self.exit_code
    }
}

impl BackgroundProgramRun {
    pub fn input_write(&self) -> InputWrite {
        self.input_write
    }

    pub fn wait(self) -> Result<ProgramRun, ExternalProcessError> {
        let (status, timed_out) = self
            .waiter
            .join()
            .map_err(|_| process_error("background command wait worker failed"))??;
        let completed = complete_child(self.input_write, status, timed_out);
        Ok(ProgramRun {
            input_write: completed.input_write,
            child_exit: completed.child_exit,
            exit_code: completed.exit_code,
        })
    }
}

impl CaptureRun {
    pub fn output(&self) -> Result<&[u8], &std::io::Error> {
        self.output.as_deref()
    }

    pub fn into_output(self) -> std::io::Result<Vec<u8>> {
        self.output
    }

    pub fn outcome(&self) -> CommandOutcome {
        CommandOutcome::new(self.input_write, self.child_exit)
    }

    pub fn exit_code(&self) -> Option<u8> {
        self.exit_code
    }
}

impl FilterRun {
    pub fn output_state(&self) -> FilterOutput {
        match self.output {
            Ok(_) => FilterOutput::CompleteAndValid,
            Err(_) => FilterOutput::Failed,
        }
    }

    pub fn output(&self) -> Result<&Message, &MessageReadError> {
        self.output.as_ref()
    }

    pub fn into_output(self) -> Result<Message, MessageReadError> {
        self.output
    }

    pub fn outcome(&self) -> CommandOutcome {
        CommandOutcome::new(self.input_write, self.child_exit)
    }
}

fn classify_child_exit(status: std::process::ExitStatus, timed_out: bool) -> ChildExit {
    if timed_out {
        ChildExit::TimedOut
    } else if status.success() {
        ChildExit::Success
    } else if status.signal().is_some() {
        ChildExit::Signaled
    } else {
        ChildExit::ExitFailure
    }
}

#[derive(Debug)]
pub struct ExternalProcessError {
    message: String,
}

impl fmt::Display for ExternalProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExternalProcessError {}

impl ChildLifecycle {
    fn spawn(
        policy: &ShellPolicy,
        environment: &ProcessEnvironment,
        command: &str,
        timeout: Duration,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self, ExternalProcessError> {
        crate::signal_state::check_io().map_err(|error| process_error(error.to_string()))?;
        let invocation = policy
            .authorize(environment)
            .map_err(|error| process_error(error.to_string()))?;
        let mut child = Command::new(invocation.path());
        child
            .arg(invocation.flags())
            .arg(command)
            .env_clear()
            .envs(environment.values())
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0);

        // Original procmail makes MAILDIR the current directory for commands.
        // Set it on the child instead of changing this process directory: copy
        // branches and unwaited recipes may spawn concurrently, so a global
        // directory change could make one branch execute in another branch's
        // MAILDIR.
        if let Some(maildir) = environment.get("MAILDIR").filter(|path| !path.is_empty()) {
            child.current_dir(maildir);
        }
        let mut child = child
            .spawn()
            .map_err(|error| process_error(format!("cannot start external command: {error}")))?;
        let stdin = child.stdin.take().ok_or_else(|| {
            process_error("external command did not provide its requested stdin pipe")
        })?;
        Ok(Self {
            child,
            stdin,
            timeout,
        })
    }

    fn take_stdout(&mut self) -> Result<ChildStdout, ExternalProcessError> {
        self.child.stdout.take().ok_or_else(|| {
            process_error("external command did not provide its requested stdout pipe")
        })
    }

    fn run_with_piped_output<T>(
        mut self,
        input: ProcessInput<'_>,
        consume: impl FnOnce(ChildStdout) -> T,
    ) -> Result<(CompletedChild, T), ExternalProcessError> {
        let stdout = self.take_stdout()?;

        // The child may write before consuming all input. Keep input, output,
        // and timeout supervision active together so finite pipe capacity
        // cannot prevent the timeout path from reaching the process group.
        let (input_write, output, waited) = thread::scope(|scope| {
            let writer = scope.spawn(move || write_process_input(self.stdin, input));
            let waiter = scope.spawn(move || wait_for_process_group(&mut self.child, self.timeout));
            let output = consume(stdout);
            (writer.join(), output, waiter.join())
        });
        let input_write = joined_input(input_write)?;
        let (status, timed_out) =
            waited.map_err(|_| process_error("external command wait worker failed"))??;
        Ok((complete_child(input_write, status, timed_out), output))
    }

    fn run_with_timed_output<T>(
        mut self,
        input: ProcessInput<'_>,
        consume: impl FnOnce(Instant) -> OutputConsumption<T>,
    ) -> Result<(CompletedChild, T), ExternalProcessError> {
        let (input_write, consumed, waited) = thread::scope(|scope| {
            let writer = scope.spawn(move || write_process_input(self.stdin, input));
            let started = Instant::now();
            let consumed = consume(started);
            let waited = if consumed.failed {
                terminate_process_group(&mut self.child).map(|status| (status, consumed.timed_out))
            } else {
                let remaining = self.timeout.saturating_sub(started.elapsed());
                wait_for_process_group(&mut self.child, remaining)
            };
            (writer.join(), consumed, waited)
        });
        let input_write = joined_input(input_write)?;
        let (status, wait_timed_out) = waited
            .map_err(|error| process_error(format!("cannot wait for external command: {error}")))?;
        let completed = complete_child(input_write, status, consumed.timed_out || wait_timed_out);
        Ok((completed, consumed.value))
    }

    fn run_without_output(
        mut self,
        input: ProcessInput<'_>,
    ) -> Result<CompletedChild, ExternalProcessError> {
        // Supervision runs while stdin is written because an uncooperative
        // command can stop reading before the pipe is drained.
        let (input_write, waited) = thread::scope(|scope| {
            let waiter = scope.spawn(move || wait_for_process_group(&mut self.child, self.timeout));
            let input_write = write_process_input(self.stdin, input);
            (input_write, waiter.join())
        });
        let input_write = input_write
            .map(|()| InputWrite::Complete)
            .unwrap_or(InputWrite::Failed);
        let (status, timed_out) =
            waited.map_err(|_| process_error("external command wait worker failed"))??;
        Ok(complete_child(input_write, status, timed_out))
    }
}

fn write_process_input(mut stdin: ChildStdin, input: ProcessInput<'_>) -> std::io::Result<()> {
    write_action_input(
        &mut stdin,
        input.bytes,
        input.output_ending,
        input.append_lf,
        input.body_input,
    )
}

fn joined_input(
    result: thread::Result<std::io::Result<()>>,
) -> Result<InputWrite, ExternalProcessError> {
    match result {
        Ok(Ok(())) => Ok(InputWrite::Complete),
        Ok(Err(_)) => Ok(InputWrite::Failed),
        Err(_) => Err(process_error("external command input worker failed")),
    }
}

fn complete_child(input_write: InputWrite, status: ExitStatus, timed_out: bool) -> CompletedChild {
    CompletedChild {
        input_write,
        child_exit: classify_child_exit(status, timed_out),
        exit_code: status.code().and_then(|code| u8::try_from(code).ok()),
    }
}

pub fn run_filter(
    policy: &ShellPolicy,
    environment: &ProcessEnvironment,
    command: &str,
    input: &[u8],
    options: FilterOptions,
    stderr: Stdio,
) -> Result<FilterRun, ExternalProcessError> {
    let lifecycle = ChildLifecycle::spawn(
        policy,
        environment,
        command,
        options.timeout,
        Stdio::piped(),
        stderr,
    )?;
    let process_input = ProcessInput {
        bytes: input,
        output_ending: options.output_ending,
        append_lf: false,
        body_input: options.action_input == ActionInput::Body,
    };
    let (completed, output) = lifecycle.run_with_piped_output(process_input, |child_stdout| {
        // Body-only output has no header separator. Prefix a private separator
        // while parsing stdout so arbitrary body bytes are governed by body
        // limits instead of being mistaken for an unterminated header field.
        if options.action_input == ActionInput::Body {
            let reader = std::io::Cursor::new(&b"\n"[..]).chain(child_stdout);
            Message::read_from(&mut BufReader::new(reader), options.limits)
        } else {
            Message::read_from(&mut BufReader::new(child_stdout), options.limits)
        }
    })?;

    Ok(FilterRun {
        input_write: completed.input_write,
        output,
        child_exit: completed.child_exit,
    })
}

pub fn run_program_with_timeout(
    policy: &ShellPolicy,
    environment: &ProcessEnvironment,
    command: &str,
    input: &[u8],
    options: ProgramOptions,
    stderr: Stdio,
) -> Result<ProgramRun, ExternalProcessError> {
    // Original procmail closes stdout for a regular pipe delivery. The safe
    // standard process API cannot request a closed descriptor, so discard it
    // here while the shared runner remains able to route TRAP output.
    run_program_with_streams(
        policy,
        environment,
        command,
        input,
        ProgramIoOptions {
            output_ending: options.output_ending,
            timeout: options.timeout,
            stdout: Stdio::null(),
            stderr,
            append_lf: false,
            body_input: options.action_input == ActionInput::Body,
        },
    )
}

pub fn run_program_in_background(
    policy: &ShellPolicy,
    environment: &ProcessEnvironment,
    command: &str,
    input: &[u8],
    options: ProgramOptions,
    stderr: Stdio,
) -> Result<BackgroundProgramRun, ExternalProcessError> {
    let (child_sender, child_receiver) = std::sync::mpsc::sync_channel(1);
    let waiter = thread::Builder::new()
        .name("procmail-rs-command-wait".to_owned())
        .spawn(move || {
            let (mut child, timeout) = child_receiver
                .recv()
                .map_err(|_| process_error("background command wait worker received no child"))?;
            wait_for_process_group(&mut child, timeout)
        })
        .map_err(|error| process_error(format!("cannot start command wait worker: {error}")))?;
    let lifecycle = match ChildLifecycle::spawn(
        policy,
        environment,
        command,
        options.timeout,
        Stdio::null(),
        stderr,
    ) {
        Ok(lifecycle) => lifecycle,
        Err(error) => {
            drop(child_sender);
            let _ = waiter.join();
            return Err(error);
        }
    };
    let ChildLifecycle {
        child,
        stdin,
        timeout,
    } = lifecycle;

    // The caller must know whether the complete selected input reached the
    // pipe before it continues recipe evaluation. Supervise the child in a
    // detached worker at the same time so a full pipe or a child that never
    // exits still reaches TIMEOUT and unblocks this write.
    if let Err(error) = child_sender.send((child, timeout)) {
        let (mut child, _) = error.0;
        let cleanup = terminate_process_group(&mut child);
        let _ = waiter.join();
        return Err(match cleanup {
            Ok(_) => process_error("command wait worker stopped before receiving its child"),
            Err(cleanup) => process_error(format!(
                "command wait worker stopped and child cleanup failed: {cleanup}"
            )),
        });
    }
    let input_write = write_process_input(
        stdin,
        ProcessInput {
            bytes: input,
            output_ending: options.output_ending,
            append_lf: false,
            body_input: options.action_input == ActionInput::Body,
        },
    )
    .map(|()| InputWrite::Complete)
    .unwrap_or(InputWrite::Failed);
    Ok(BackgroundProgramRun {
        input_write,
        waiter,
    })
}

pub fn run_capture_with_timeout(
    policy: &ShellPolicy,
    environment: &ProcessEnvironment,
    command: &str,
    input: &[u8],
    options: CaptureOptions,
    stderr: Stdio,
) -> Result<CaptureRun, ExternalProcessError> {
    let (mut output_reader, output_writer) = UnixStream::pair()
        .map_err(|error| process_error(format!("cannot create command output channel: {error}")))?;
    output_reader
        .set_read_timeout(Some(PROCESS_POLL_INTERVAL))
        .map_err(|error| process_error(format!("cannot bound command output wait: {error}")))?;
    let output_writer: OwnedFd = output_writer.into();
    let lifecycle = ChildLifecycle::spawn(
        policy,
        environment,
        command,
        options.timeout,
        Stdio::from(output_writer),
        stderr,
    )?;
    let process_input = ProcessInput {
        bytes: input,
        output_ending: options.output_ending,
        append_lf: false,
        body_input: options.action_input == ActionInput::Body,
    };

    // The timed socket keeps output consumption interruptible even when a
    // descendant retains stdout after the direct shell exits. Parsing and
    // byte limits remain properties of the capture consumer.
    let (completed, output) = lifecycle.run_with_timed_output(process_input, |started| {
        let output = read_bounded_output_until(
            &mut output_reader,
            options.output_limit,
            options.timeout,
            started,
        );
        drop(output_reader);
        let output_failed = output.is_err();
        let timed_out = output
            .as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::TimedOut);
        OutputConsumption {
            value: output,
            failed: output_failed,
            timed_out,
        }
    })?;

    Ok(CaptureRun {
        input_write: completed.input_write,
        output,
        child_exit: completed.child_exit,
        exit_code: completed.exit_code,
    })
}

fn read_bounded_output_until(
    reader: &mut impl Read,
    limit: usize,
    timeout: Duration,
    started: Instant,
) -> std::io::Result<Vec<u8>> {
    let mut output = BoundedBytes::with_capacity(limit, 64 * 1024);
    let mut buffer = [0u8; 8192];
    loop {
        crate::signal_state::check_io()?;
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if started.elapsed() >= timeout {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "captured command output exceeded TIMEOUT",
                    ));
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        if read == 0 {
            return Ok(output.into_vec());
        }
        let remaining = output.remaining().map_err(|_| {
            std::io::Error::other("captured command output size accounting overflowed")
        })?;
        if read > remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("captured command output exceeds the hard limit of {limit} bytes"),
            ));
        }
        output
            .try_extend(&buffer[..read])
            .map_err(|error| match error {
                BoundedBytesError::LengthOverflow => {
                    std::io::Error::other("captured command output size accounting overflowed")
                }
                BoundedBytesError::LimitExceeded { .. } => std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("captured command output exceeds the hard limit of {limit} bytes"),
                ),
            })?;
    }
}

pub fn run_trap_with_timeout(
    policy: &ShellPolicy,
    environment: &ProcessEnvironment,
    command: &str,
    input: &[u8],
    timeout: Duration,
    stdout: Stdio,
    stderr: Stdio,
) -> Result<ProgramRun, ExternalProcessError> {
    run_program_with_streams(
        policy,
        environment,
        command,
        input,
        ProgramIoOptions {
            output_ending: OutputEnding::Normalize,
            timeout,
            stdout,
            stderr,
            append_lf: true,
            body_input: false,
        },
    )
}

fn run_program_with_streams(
    policy: &ShellPolicy,
    environment: &ProcessEnvironment,
    command: &str,
    input: &[u8],
    options: ProgramIoOptions,
) -> Result<ProgramRun, ExternalProcessError> {
    let ProgramIoOptions {
        output_ending,
        timeout,
        stdout,
        stderr,
        append_lf,
        body_input,
    } = options;
    let lifecycle = ChildLifecycle::spawn(policy, environment, command, timeout, stdout, stderr)?;
    let completed = lifecycle.run_without_output(ProcessInput {
        bytes: input,
        output_ending,
        append_lf,
        body_input,
    })?;
    Ok(ProgramRun {
        input_write: completed.input_write,
        child_exit: completed.child_exit,
        exit_code: completed.exit_code,
    })
}

fn wait_for_process_group(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, bool), ExternalProcessError> {
    let started = Instant::now();
    loop {
        if crate::signal_state::received().is_some() {
            return terminate_process_group(child).map(|status| (status, false));
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| process_error(format!("cannot wait for external command: {error}")))?
        {
            return Ok((status, false));
        }
        let elapsed = started.elapsed();
        let Some(remaining) = timeout.checked_sub(elapsed) else {
            break;
        };
        if remaining.is_zero() {
            break;
        }
        thread::sleep(PROCESS_POLL_INTERVAL.min(remaining));
    }

    terminate_process_group(child).map(|status| (status, true))
}

fn terminate_process_group(
    child: &mut std::process::Child,
) -> Result<std::process::ExitStatus, ExternalProcessError> {
    let group = i32::try_from(child.id())
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| process_error("external command returned an invalid process id"))?;

    // Keep the direct child unreaped during the grace interval. Its PID then
    // anchors the process-group number, so termination cannot target a later
    // unrelated group after rapid PID reuse.
    match kill_process_group(group, Signal::TERM) {
        Ok(()) => {}
        Err(rustix::io::Errno::SRCH) => {
            let status = child.wait().map_err(|error| {
                process_error(format!("cannot reap timed-out command: {error}"))
            })?;
            return Ok(status);
        }
        Err(error) => {
            return Err(process_error(format!(
                "cannot terminate timed-out process group: {error}"
            )));
        }
    }
    thread::sleep(TERMINATION_GRACE);
    match kill_process_group(group, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(error) => {
            return Err(process_error(format!(
                "cannot kill timed-out process group: {error}"
            )));
        }
    }
    let status = child
        .wait()
        .map_err(|error| process_error(format!("cannot reap timed-out command: {error}")))?;
    Ok(status)
}

fn write_action_input(
    writer: &mut impl Write,
    input: &[u8],
    output_ending: OutputEnding,
    append_lf: bool,
    body_input: bool,
) -> std::io::Result<()> {
    writer.write_all(input)?;
    let needs_normalized_lf = if body_input {
        !input.ends_with(b"\n\n")
    } else {
        !input.ends_with(b"\n")
    };
    if append_lf || output_ending == OutputEnding::Normalize && needs_normalized_lf {
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn process_error(message: impl Into<String>) -> ExternalProcessError {
    ExternalProcessError {
        message: message.into(),
    }
}

#[cfg(test)]
#[path = "tests/external_process.rs"]
mod tests;
