// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::io::{BufReader, Read, Write};
use std::process::{Command, Stdio};

use crate::config::{ActionInput, OutputEnding};
use crate::environment::{ProcessEnvironment, ShellPolicy};
use crate::external_filter::{ChildExit, FilterOutput, InputWrite};
use crate::limits::MessageLimits;
use crate::message::{Message, MessageReadError};

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
}

// These settings jointly describe how one filter invocation consumes and
// validates bytes. Keeping them together makes it harder to reuse a message
// limit with the wrong selected area or output-ending policy at a call site.
#[derive(Debug, Clone, Copy)]
pub struct FilterOptions {
    output_ending: OutputEnding,
    action_input: ActionInput,
    limits: MessageLimits,
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
        }
    }
}

impl ProgramRun {
    pub fn input_write(self) -> InputWrite {
        self.input_write
    }

    pub fn child_exit(self) -> ChildExit {
        self.child_exit
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
    let mut child = Command::new(invocation.path())
        .arg(invocation.flags())
        .arg(command)
        .env_clear()
        .envs(environment.values())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
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
        let writer =
            scope.spawn(move || write_action_input(&mut child_stdin, input, options.output_ending));
        // Body-only output has no header separator. Prefix a private separator
        // while parsing stdout so arbitrary body bytes are governed by body
        // limits instead of being mistaken for an unterminated header field.
        let output = if options.action_input == ActionInput::Body {
            let reader = std::io::Cursor::new(&b"\n"[..]).chain(child_stdout);
            Message::read_from(&mut BufReader::new(reader), options.limits)
        } else {
            Message::read_from(&mut BufReader::new(child_stdout), options.limits)
        };
        let status = child.wait();
        let input_write = writer.join();
        (input_write, output, status)
    });

    let status = status
        .map_err(|error| process_error(format!("cannot wait for external command: {error}")))?;
    let input_write = match input_write {
        Ok(Ok(())) => InputWrite::Complete,
        Ok(Err(_)) => InputWrite::Failed,
        Err(_) => return Err(process_error("external command input worker failed")),
    };

    Ok(FilterRun {
        input_write,
        output,
        child_exit: if status.success() {
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
    let invocation = policy
        .authorize(environment)
        .map_err(|error| process_error(error.to_string()))?;
    let mut child = Command::new(invocation.path())
        .arg(invocation.flags())
        .arg(command)
        .env_clear()
        .envs(environment.values())
        .stdin(Stdio::piped())
        // Original procmail closes stdout for a regular pipe delivery. The
        // safe standard process API cannot request a closed descriptor, so
        // discard output without buffering it or exposing it to our caller.
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|error| process_error(format!("cannot start external command: {error}")))?;
    let mut child_stdin = child
        .stdin
        .take()
        .expect("piped child stdin is available after spawn");

    // Close the input pipe before waiting even when the command stops reading
    // early. The direct child is then always reaped, while the caller's `i`
    // policy decides whether a write error changes the recipe result.
    let input_write = match write_action_input(&mut child_stdin, input, output_ending) {
        Ok(()) => InputWrite::Complete,
        Err(_) => InputWrite::Failed,
    };
    drop(child_stdin);
    let status = child
        .wait()
        .map_err(|error| process_error(format!("cannot wait for external command: {error}")))?;

    Ok(ProgramRun {
        input_write,
        child_exit: if status.success() {
            ChildExit::Success
        } else {
            ChildExit::Failure
        },
    })
}

fn write_action_input(
    writer: &mut impl Write,
    input: &[u8],
    output_ending: OutputEnding,
) -> std::io::Result<()> {
    writer.write_all(input)?;
    if output_ending == OutputEnding::Normalize && !input.ends_with(b"\n") {
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
    }
}
