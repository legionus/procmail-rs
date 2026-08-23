// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

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
