// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::io::{BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

use crate::bounded_bytes::{BoundedBytes, BoundedBytesError};
use crate::config::{ActionInput, AssignmentTarget, Config, OutputEnding, Statement};
use crate::environment::{ProcessEnvironment, ShellPolicy};
use crate::external_filter::{ChildExit, FilterOutput, InputWrite};
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

struct ProgramIoOptions {
    output_ending: OutputEnding,
    timeout: Duration,
    stdout: Stdio,
    stderr: Stdio,
    append_lf: bool,
    body_input: bool,
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
    pub fn input_write(self) -> InputWrite {
        self.input_write
    }

    pub fn child_exit(self) -> ChildExit {
        self.child_exit
    }

    pub fn exit_code(self) -> Option<u8> {
        self.exit_code
    }
}

impl CaptureRun {
    pub fn input_write(&self) -> InputWrite {
        self.input_write
    }

    pub fn output(&self) -> Result<&[u8], &std::io::Error> {
        self.output.as_deref()
    }

    pub fn into_output(self) -> std::io::Result<Vec<u8>> {
        self.output
    }

    pub fn child_exit(&self) -> ChildExit {
        self.child_exit
    }

    pub fn exit_code(&self) -> Option<u8> {
        self.exit_code
    }
}

impl FilterRun {
    pub fn input_write(&self) -> InputWrite {
        self.input_write
    }

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

    pub fn child_exit(&self) -> ChildExit {
        self.child_exit
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

pub fn run_filter(
    policy: &ShellPolicy,
    environment: &ProcessEnvironment,
    command: &str,
    input: &[u8],
    options: FilterOptions,
    stderr: Stdio,
) -> Result<FilterRun, ExternalProcessError> {
    let invocation = policy
        .authorize(environment)
        .map_err(|error| process_error(error.to_string()))?;
    let mut command_builder = Command::new(invocation.path());
    let mut child = command_builder
        .arg(invocation.flags())
        .arg(command)
        .env_clear()
        .envs(environment.values())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|error| process_error(format!("cannot start external command: {error}")))?;
    drop(command_builder);
    let mut child_stdin = child
        .stdin
        .take()
        .expect("piped child stdin is available after spawn");
    let child_stdout = child
        .stdout
        .take()
        .expect("piped child stdout is available after spawn");

    // The command may produce output before it consumes all input. Pump stdin
    // on a scoped thread while this thread drains and validates stdout so
    // neither finite pipe buffer can make an otherwise progressing filter
    // wait forever for procmail-rs.
    let (input_write, output, status) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || {
            write_action_input(
                &mut child_stdin,
                input,
                options.output_ending,
                false,
                options.action_input == ActionInput::Body,
            )
        });
        let waiter = scope.spawn(move || wait_for_process_group(&mut child, options.timeout));
        // Body-only output has no header separator. Prefix a private separator
        // while parsing stdout so arbitrary body bytes are governed by body
        // limits instead of being mistaken for an unterminated header field.
        let output = if options.action_input == ActionInput::Body {
            let reader = std::io::Cursor::new(&b"\n"[..]).chain(child_stdout);
            Message::read_from(&mut BufReader::new(reader), options.limits)
        } else {
            Message::read_from(&mut BufReader::new(child_stdout), options.limits)
        };
        let input_write = writer.join();
        let status = waiter.join();
        (input_write, output, status)
    });

    let (status, timed_out) = status
        .map_err(|_| process_error("external command wait worker failed"))?
        .map_err(|error| process_error(format!("cannot wait for external command: {error}")))?;
    let input_write = match input_write {
        Ok(Ok(())) => InputWrite::Complete,
        Ok(Err(_)) => InputWrite::Failed,
        Err(_) => return Err(process_error("external command input worker failed")),
    };

    Ok(FilterRun {
        input_write,
        output,
        child_exit: if timed_out {
            ChildExit::TimedOut
        } else if status.success() {
            ChildExit::Success
        } else {
            ChildExit::Failure
        },
    })
}

pub fn run_program(
    policy: &ShellPolicy,
    environment: &ProcessEnvironment,
    command: &str,
    input: &[u8],
    options: ProgramOptions,
    stderr: Stdio,
) -> Result<ProgramRun, ExternalProcessError> {
    run_program_with_timeout(policy, environment, command, input, options, stderr)
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

pub fn run_capture_with_timeout(
    policy: &ShellPolicy,
    environment: &ProcessEnvironment,
    command: &str,
    input: &[u8],
    options: CaptureOptions,
    stderr: Stdio,
) -> Result<CaptureRun, ExternalProcessError> {
    let invocation = policy
        .authorize(environment)
        .map_err(|error| process_error(error.to_string()))?;
    let (mut output_reader, output_writer) = UnixStream::pair()
        .map_err(|error| process_error(format!("cannot create command output channel: {error}")))?;
    output_reader
        .set_read_timeout(Some(PROCESS_POLL_INTERVAL))
        .map_err(|error| process_error(format!("cannot bound command output wait: {error}")))?;
    let output_writer: OwnedFd = output_writer.into();
    let mut command_builder = Command::new(invocation.path());
    let mut child = command_builder
        .arg(invocation.flags())
        .arg(command)
        .env_clear()
        .envs(environment.values())
        .stdin(Stdio::piped())
        .stdout(Stdio::from(output_writer))
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|error| process_error(format!("cannot start external command: {error}")))?;
    drop(command_builder);
    let mut child_stdin = child
        .stdin
        .take()
        .expect("piped child stdin is available after spawn");
    // A command can block unless stdin and stdout progress independently. A
    // timed socket also lets this thread supervise descendants that retain
    // stdout after the direct shell exits; a plain blocking pipe read would
    // otherwise wait forever without reaching the process-group timeout.
    let (input_write, output, status, timed_out) = thread::scope(|scope| {
        let writer = scope.spawn(move || {
            write_action_input(
                &mut child_stdin,
                input,
                options.output_ending,
                false,
                options.action_input == ActionInput::Body,
            )
        });
        let started = Instant::now();
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
        let waited = if output_failed {
            terminate_process_group(&mut child).map(|status| (status, timed_out))
        } else {
            let remaining = options.timeout.saturating_sub(started.elapsed());
            wait_for_process_group(&mut child, remaining)
        };
        let input_write = writer.join();
        (input_write, output, waited, timed_out)
    });

    let input_write = match input_write {
        Ok(Ok(())) => InputWrite::Complete,
        Ok(Err(_)) => InputWrite::Failed,
        Err(_) => return Err(process_error("external command input worker failed")),
    };
    let (status, wait_timed_out) = status
        .map_err(|error| process_error(format!("cannot wait for external command: {error}")))?;
    let timed_out = timed_out || wait_timed_out;
    let exit_code = status.code().and_then(|code| u8::try_from(code).ok());

    Ok(CaptureRun {
        input_write,
        output,
        child_exit: if timed_out {
            ChildExit::TimedOut
        } else if status.success() {
            ChildExit::Success
        } else {
            ChildExit::Failure
        },
        exit_code,
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
    let invocation = policy
        .authorize(environment)
        .map_err(|error| process_error(error.to_string()))?;
    let mut command_builder = Command::new(invocation.path());
    let mut child = command_builder
        .arg(invocation.flags())
        .arg(command)
        .env_clear()
        .envs(environment.values())
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|error| process_error(format!("cannot start external command: {error}")))?;
    let mut child_stdin = child
        .stdin
        .take()
        .expect("piped child stdin is available after spawn");

    // Wait supervision must run while stdin is written. A command that never
    // reads can otherwise fill the pipe and prevent this thread from reaching
    // the timeout code that is supposed to terminate it.
    let (input_write, waited) = thread::scope(|scope| {
        let waiter = scope.spawn(move || wait_for_process_group(&mut child, timeout));
        let input_write = match write_action_input(
            &mut child_stdin,
            input,
            output_ending,
            append_lf,
            body_input,
        ) {
            Ok(()) => InputWrite::Complete,
            Err(_) => InputWrite::Failed,
        };
        drop(child_stdin);
        (input_write, waiter.join())
    });
    let (status, timed_out) =
        waited.map_err(|_| process_error("external command wait worker failed"))??;

    let exit_code = status.code().and_then(|code| u8::try_from(code).ok());
    Ok(ProgramRun {
        input_write,
        child_exit: if timed_out {
            ChildExit::TimedOut
        } else if status.success() {
            ChildExit::Success
        } else {
            ChildExit::Failure
        },
        exit_code,
    })
}

fn wait_for_process_group(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, bool), ExternalProcessError> {
    let started = Instant::now();
    loop {
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
mod tests;
