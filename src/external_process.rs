// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::io::{BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

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

struct ProgramIoOptions {
    output_ending: OutputEnding,
    timeout: Duration,
    stdout: Stdio,
    stderr: Stdio,
    append_lf: bool,
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
            write_action_input(&mut child_stdin, input, options.output_ending, false)
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
    output_ending: OutputEnding,
    stderr: Stdio,
) -> Result<ProgramRun, ExternalProcessError> {
    run_program_with_timeout(
        policy,
        environment,
        command,
        input,
        output_ending,
        DEFAULT_PROCESS_TIMEOUT,
        stderr,
    )
}

pub fn run_program_with_timeout(
    policy: &ShellPolicy,
    environment: &ProcessEnvironment,
    command: &str,
    input: &[u8],
    output_ending: OutputEnding,
    timeout: Duration,
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
            output_ending,
            timeout,
            stdout: Stdio::null(),
            stderr,
            append_lf: false,
        },
    )
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
        let input_write =
            match write_action_input(&mut child_stdin, input, output_ending, append_lf) {
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
    let group = i32::try_from(child.id())
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| process_error("external command returned an invalid process id"))?;
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

    // The shell starts a fresh group so its descendants receive termination
    // together. Keep the direct child unreaped during the grace interval: its
    // PID continues to anchor the group number and cannot be reused before
    // the final signal is sent.
    match kill_process_group(group, Signal::TERM) {
        Ok(()) => {}
        Err(rustix::io::Errno::SRCH) => {
            let status = child.wait().map_err(|error| {
                process_error(format!("cannot reap timed-out command: {error}"))
            })?;
            return Ok((status, true));
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
    Ok((status, true))
}

fn write_action_input(
    writer: &mut impl Write,
    input: &[u8],
    output_ending: OutputEnding,
    append_lf: bool,
) -> std::io::Result<()> {
    writer.write_all(input)?;
    if append_lf || output_ending == OutputEnding::Normalize && !input.ends_with(b"\n") {
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
