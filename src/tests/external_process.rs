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

    assert_eq!(run.outcome().input_write(), InputWrite::Complete);
    assert_eq!(run.outcome().child_exit(), ChildExit::Success);
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

    assert_eq!(run.outcome().input_write(), InputWrite::Complete);
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

    assert_eq!(run.outcome().child_exit(), ChildExit::Success);
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

    assert_eq!(run.outcome().child_exit(), ChildExit::ExitFailure);
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
fn normalizes_only_the_bytes_sent_to_the_filter() {
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
fn body_action_input_uses_procmail_double_lf_ending() {
    for (input, ending, expected) in [
        (&b"body"[..], OutputEnding::Normalize, &b"body\n"[..]),
        (&b"body\n"[..], OutputEnding::Normalize, &b"body\n\n"[..]),
        (&b"body\n\n"[..], OutputEnding::Normalize, &b"body\n\n"[..]),
        (&b"body\n"[..], OutputEnding::Preserve, &b"body\n"[..]),
    ] {
        let mut output = Vec::new();
        write_action_input(&mut output, input, ending, false, true).unwrap();
        assert_eq!(output, expected);
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
    let run = run_program_with_timeout(
        &policy,
        &environment,
        "cat >/dev/null; printf 'discarded output'",
        b"Subject: test\n\nbody",
        ProgramOptions::new(OutputEnding::Preserve, ActionInput::Message),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(run.outcome().input_write(), InputWrite::Complete);
    assert_eq!(run.outcome().child_exit(), ChildExit::Success);
}

#[test]
fn regular_program_reports_failed_exit_without_parsing_output() {
    let (environment, policy) = enabled_shell(&RuntimeVariables::default());
    let run = run_program_with_timeout(
        &policy,
        &environment,
        "printf 'not a message'; exit 19",
        b"",
        ProgramOptions::new(OutputEnding::Preserve, ActionInput::Message),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(run.outcome().child_exit(), ChildExit::ExitFailure);
    assert_eq!(run.exit_code(), Some(19));
}

#[test]
fn background_program_returns_after_input_and_is_reaped_later() {
    let (environment, policy) = enabled_shell(&RuntimeVariables::default());
    let marker = temporary_path("background-finished");
    let command = format!("cat >/dev/null; sleep 1; : > {}", marker.display());
    let started = Instant::now();
    let run = run_program_in_background(
        &policy,
        &environment,
        &command,
        b"complete input",
        ProgramOptions::new(OutputEnding::Preserve, ActionInput::Message)
            .with_timeout(Duration::from_secs(3)),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(run.input_write(), InputWrite::Complete);
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(!marker.exists());
    let completed = run.wait().unwrap();
    assert_eq!(completed.outcome().child_exit(), ChildExit::Success);
    assert!(marker.exists());
    fs::remove_file(marker).unwrap();
}

#[test]
fn background_program_keeps_stderr_and_timeout_supervision_active() {
    let (environment, policy) = enabled_shell(&RuntimeVariables::default());
    let path = temporary_path("background-stderr");
    let file = File::create(&path).unwrap();
    let run = run_program_in_background(
        &policy,
        &environment,
        "printf diagnostic >&2; sleep 30",
        b"",
        ProgramOptions::new(OutputEnding::Preserve, ActionInput::Message)
            .with_timeout(Duration::from_millis(50)),
        Stdio::from(file),
    )
    .unwrap();

    let completed = run.wait().unwrap();
    assert_eq!(completed.outcome().child_exit(), ChildExit::TimedOut);
    assert_eq!(fs::read(&path).unwrap(), b"diagnostic");
    fs::remove_file(path).unwrap();
}

#[test]
fn regular_program_distinguishes_signal_termination_from_exit_failure() {
    let (environment, policy) = enabled_shell(&RuntimeVariables::default());
    let run = run_program_with_timeout(
        &policy,
        &environment,
        "kill -TERM $$",
        b"",
        ProgramOptions::new(OutputEnding::Preserve, ActionInput::Message),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(run.outcome().child_exit(), ChildExit::Signaled);
    assert_eq!(run.exit_code(), None);
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
        ProgramOptions::new(OutputEnding::Preserve, ActionInput::Message)
            .with_timeout(Duration::from_millis(50)),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(run.outcome().child_exit(), ChildExit::TimedOut);
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

    assert_eq!(run.outcome().child_exit(), ChildExit::TimedOut);
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
        ProgramOptions::new(OutputEnding::Preserve, ActionInput::Message)
            .with_timeout(Duration::from_millis(50)),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(run.outcome().input_write(), InputWrite::Failed);
    assert_eq!(run.outcome().child_exit(), ChildExit::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn capture_pumps_binary_input_and_output_concurrently() {
    let (environment, policy) = enabled_shell(&RuntimeVariables::default());
    let input: Vec<u8> = (0..=255).cycle().take(1024 * 1024).collect();
    let run = run_capture_with_timeout(
        &policy,
        &environment,
        "cat",
        &input,
        CaptureOptions::new(OutputEnding::Preserve, input.len()),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(run.outcome().input_write(), InputWrite::Complete);
    assert_eq!(run.outcome().child_exit(), ChildExit::Success);
    assert_eq!(run.exit_code(), Some(0));
    assert_eq!(run.output().unwrap(), input);
}

#[test]
fn body_capture_receives_procmail_double_lf_ending() {
    let (environment, policy) = enabled_shell(&RuntimeVariables::default());
    let run = run_capture_with_timeout(
        &policy,
        &environment,
        "cat",
        b"body\n",
        CaptureOptions::new(OutputEnding::Normalize, 16).with_action_input(ActionInput::Body),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(run.output().unwrap(), b"body\n\n");
}

#[test]
fn capture_exports_binary_runtime_values_to_the_shell() {
    let mut runtime = RuntimeVariables::default();
    runtime.set_bytes("BINARY", vec![b'a', 0xff, b'z']);
    let (environment, policy) = enabled_shell(&runtime);
    let run = run_capture_with_timeout(
        &policy,
        &environment,
        "printf %s \"$BINARY\"",
        b"",
        CaptureOptions::new(OutputEnding::Preserve, 3),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(run.output().unwrap(), b"a\xffz");
}

#[test]
fn capture_enforces_its_output_limit_at_the_boundary() {
    let (environment, policy) = enabled_shell(&RuntimeVariables::default());
    let limit = 1024usize;
    for length in [limit - 1, limit, limit + 1] {
        let input = vec![b'x'; length];
        let run = run_capture_with_timeout(
            &policy,
            &environment,
            "cat",
            &input,
            CaptureOptions::new(OutputEnding::Preserve, limit),
            Stdio::null(),
        )
        .unwrap();

        if length <= limit {
            assert_eq!(run.output().unwrap().len(), length);
        } else {
            assert_eq!(
                run.output().unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
        }
    }
}

#[test]
fn capture_streams_stderr_and_obeys_timeout() {
    let (environment, policy) = enabled_shell(&RuntimeVariables::default());
    let path = temporary_path("capture-stderr");
    let file = File::create(&path).unwrap();
    let run = run_capture_with_timeout(
        &policy,
        &environment,
        "printf diagnostic >&2; sleep 30",
        b"",
        CaptureOptions::new(OutputEnding::Preserve, 16).with_timeout(Duration::from_millis(50)),
        Stdio::from(file),
    )
    .unwrap();

    assert_eq!(run.outcome().child_exit(), ChildExit::TimedOut);
    assert_eq!(fs::read(&path).unwrap(), b"diagnostic");
    fs::remove_file(path).unwrap();
}

#[test]
fn capture_times_out_when_a_background_descendant_keeps_stdout_open() {
    let (environment, policy) = enabled_shell(&RuntimeVariables::default());
    let started = Instant::now();
    let run = run_capture_with_timeout(
        &policy,
        &environment,
        "sleep 30 &",
        b"",
        CaptureOptions::new(OutputEnding::Preserve, 16).with_timeout(Duration::from_millis(50)),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(run.outcome().child_exit(), ChildExit::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn capture_stops_infinite_output_at_the_byte_limit() {
    let (environment, policy) = enabled_shell(&RuntimeVariables::default());
    let started = Instant::now();
    let run = run_capture_with_timeout(
        &policy,
        &environment,
        "while :; do printf 0123456789abcdef; done",
        b"",
        CaptureOptions::new(OutputEnding::Preserve, 1024),
        Stdio::null(),
    )
    .unwrap();

    assert_eq!(
        run.output().unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
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
        .expand(&[])
        .unwrap();
    assert_eq!(
        process_timeout_from_config(&config).unwrap(),
        Duration::from_secs(2)
    );
}
