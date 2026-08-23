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
mod tests {
    use std::fs::{self, File};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::environment::DEFAULT_SHELL;
    use crate::runtime::RuntimeVariables;

    fn enabled_shell(runtime: &RuntimeVariables) -> (ProcessEnvironment, ShellPolicy) {
        (
            ProcessEnvironment::from_runtime(runtime).unwrap(),
            ShellPolicy::approve(DEFAULT_SHELL).unwrap(),
        )
    }

    fn temporary_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "procmail-rs-external-{}-{unique}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn runs_with_only_the_bounded_runtime_environment() {
        let mut runtime = RuntimeVariables::default();
        runtime.set("TOKEN", "expected");
        let (environment, policy) = enabled_shell(&runtime);

        let run = run_filter(
            &policy,
            &environment,
            "cat >/dev/null; printf 'X-Token: %s\\n\\nbody' \"$TOKEN\"",
            b"ignored\n\n",
            FilterOptions::new(
                OutputEnding::Preserve,
                ActionInput::Message,
                MessageLimits::default(),
            ),
            Stdio::null(),
        )
        .unwrap();

        assert_eq!(run.input_write(), InputWrite::Complete);
        assert_eq!(run.child_exit(), ChildExit::Success);
        assert_eq!(
            run.output().unwrap().as_bytes(),
            b"X-Token: expected\n\nbody"
        );
    }

    #[test]
    fn pumps_large_input_and_output_concurrently() {
        let (environment, policy) = enabled_shell(&RuntimeVariables::default());
        let mut input = b"Subject: test\n\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', 1024 * 1024));

        let run = run_filter(
            &policy,
            &environment,
            "cat",
            &input,
            FilterOptions::new(
                OutputEnding::Preserve,
                ActionInput::Message,
                MessageLimits::default(),
            ),
            Stdio::null(),
        )
        .unwrap();

        assert_eq!(run.input_write(), InputWrite::Complete);
        assert_eq!(run.output().unwrap().as_bytes(), input);
    }

    #[test]
    fn streams_command_stderr_to_the_supplied_descriptor() {
        let (environment, policy) = enabled_shell(&RuntimeVariables::default());
        let path = temporary_path("stderr");
        let file = File::create(&path).unwrap();

        let run = run_filter(
            &policy,
            &environment,
            "printf 'Subject: ok\\n\\n'; printf 'filter diagnostic' >&2",
            b"",
            FilterOptions::new(
                OutputEnding::Preserve,
                ActionInput::Message,
                MessageLimits::default(),
            ),
            Stdio::from(file),
        )
        .unwrap();

        assert_eq!(run.child_exit(), ChildExit::Success);
        assert_eq!(fs::read(&path).unwrap(), b"filter diagnostic");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reports_status_and_keeps_complete_output_separate() {
        let (environment, policy) = enabled_shell(&RuntimeVariables::default());
        let run = run_filter(
            &policy,
            &environment,
            "printf 'Subject: failed\\n\\noutput'; exit 23",
            b"",
            FilterOptions::new(
                OutputEnding::Preserve,
                ActionInput::Message,
                MessageLimits::default(),
            ),
            Stdio::null(),
        )
        .unwrap();

        assert_eq!(run.child_exit(), ChildExit::Failure);
        assert_eq!(run.output_state(), FilterOutput::CompleteAndValid);
    }

    #[test]
    fn applies_message_limits_to_filter_output() {
        let (environment, policy) = enabled_shell(&RuntimeVariables::default());
        let limits = MessageLimits {
            message_size: 10,
            headers_size: 10,
            body_size: 3,
            header_line_size: 10,
            header_field_size: 10,
        };
        let run = run_filter(
            &policy,
            &environment,
            "printf '\\n1234'",
            b"",
            FilterOptions::new(OutputEnding::Preserve, ActionInput::Message, limits),
            Stdio::null(),
        )
        .unwrap();

        assert_eq!(run.output_state(), FilterOutput::Failed);
        assert!(
            run.output()
                .unwrap_err()
                .to_string()
                .contains("LIMIT_MSG_BODY")
        );
    }

    #[test]
    fn normalizes_only_the_bytes_sent_to_the_action() {
        let (environment, policy) = enabled_shell(&RuntimeVariables::default());
        for (input, ending, expected) in [
            (
                &b"Subject: x\n\nbody"[..],
                OutputEnding::Normalize,
                &b"Subject: x\n\nbody\n"[..],
            ),
            (
                &b"Subject: x\n\nbody\n\n"[..],
                OutputEnding::Normalize,
                &b"Subject: x\n\nbody\n\n"[..],
            ),
            (
                &b"Subject: x\n\nbody\n"[..],
                OutputEnding::Normalize,
                &b"Subject: x\n\nbody\n"[..],
            ),
            (
                &b"Subject: x\n\nbody"[..],
                OutputEnding::Preserve,
                &b"Subject: x\n\nbody"[..],
            ),
        ] {
            let run = run_filter(
                &policy,
                &environment,
                "cat",
                input,
                FilterOptions::new(ending, ActionInput::Message, MessageLimits::default()),
                Stdio::null(),
            )
            .unwrap();
            assert_eq!(run.output().unwrap().as_bytes(), expected);
        }
    }

    #[test]
    fn rejects_execution_before_spawning_when_policy_is_disabled() {
        let environment = ProcessEnvironment::from_runtime(&RuntimeVariables::default()).unwrap();
        let error = run_filter(
            &ShellPolicy::disabled(),
            &environment,
            "exit 0",
            b"",
            FilterOptions::new(
                OutputEnding::Preserve,
                ActionInput::Message,
                MessageLimits::default(),
            ),
            Stdio::null(),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "shell execution is disabled by operator policy"
        );
    }

    #[test]
    fn regular_program_discards_stdout_and_reports_completion() {
        let (environment, policy) = enabled_shell(&RuntimeVariables::default());
        let run = run_program(
            &policy,
            &environment,
            "cat >/dev/null; printf 'discarded output'",
            b"Subject: test\n\nbody",
            OutputEnding::Preserve,
            Stdio::null(),
        )
        .unwrap();

        assert_eq!(run.input_write(), InputWrite::Complete);
        assert_eq!(run.child_exit(), ChildExit::Success);
    }

    #[test]
    fn regular_program_reports_failed_exit_without_parsing_output() {
        let (environment, policy) = enabled_shell(&RuntimeVariables::default());
        let run = run_program(
            &policy,
            &environment,
            "printf 'not a message'; exit 19",
            b"",
            OutputEnding::Preserve,
            Stdio::null(),
        )
        .unwrap();

        assert_eq!(run.child_exit(), ChildExit::Failure);
        assert_eq!(run.exit_code(), Some(19));
    }

    #[test]
    fn timeout_terminates_a_program_and_its_process_group() {
        let (environment, policy) = enabled_shell(&RuntimeVariables::default());
        let started = Instant::now();
        let run = run_program_with_timeout(
            &policy,
            &environment,
            "trap '' TERM; (trap '' TERM; sleep 30) & wait",
            b"",
            OutputEnding::Preserve,
            Duration::from_millis(50),
            Stdio::null(),
        )
        .unwrap();

        assert_eq!(run.child_exit(), ChildExit::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn timeout_interrupts_filter_output_waiting() {
        let (environment, policy) = enabled_shell(&RuntimeVariables::default());
        let run = run_filter(
            &policy,
            &environment,
            "sleep 30",
            b"Subject: input\n\nbody",
            FilterOptions::new(
                OutputEnding::Preserve,
                ActionInput::Message,
                MessageLimits::default(),
            )
            .with_timeout(Duration::from_millis(50)),
            Stdio::null(),
        )
        .unwrap();

        assert_eq!(run.child_exit(), ChildExit::TimedOut);
    }

    #[test]
    fn timeout_interrupts_a_blocked_program_input_write() {
        let (environment, policy) = enabled_shell(&RuntimeVariables::default());
        let input = vec![b'x'; 1024 * 1024];
        let started = Instant::now();
        let run = run_program_with_timeout(
            &policy,
            &environment,
            "sleep 30",
            &input,
            OutputEnding::Preserve,
            Duration::from_millis(50),
            Stdio::null(),
        )
        .unwrap();

        assert_eq!(run.input_write(), InputWrite::Failed);
        assert_eq!(run.child_exit(), ChildExit::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn parses_timeout_at_boundaries_and_in_statement_order() {
        assert_eq!(parse_process_timeout("1").unwrap(), Duration::from_secs(1));
        assert_eq!(
            parse_process_timeout(&crate::config::MAX_PROCESS_TIMEOUT_SECONDS.to_string()).unwrap(),
            Duration::from_secs(crate::config::MAX_PROCESS_TIMEOUT_SECONDS)
        );
        for value in ["", "0", "1s", "86401", "18446744073709551616"] {
            assert!(parse_process_timeout(value).is_err(), "accepted {value:?}");
        }

        let config = crate::config::parse("TIMEOUT=1\nTIMEOUT=2\n")
            .unwrap()
            .expand()
            .unwrap();
        assert_eq!(
            process_timeout_from_config(&config).unwrap(),
            Duration::from_secs(2)
        );
    }
}
