// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::io::Cursor;
use std::time::Instant;

use procmail_rs::config::{
    ActionInput, ActionMode, Destination, OutputEnding, PipeAction, RecipeOptions, SuppliedVariable,
};
use procmail_rs::eval::{
    CapturedCommand, CompletionState, DeliveryAttemptError, ExecutionPlan, ExternalActionInput,
    FinalMessage, MappedMessageInput, OrderedExecutionHost, PreparedMatchingMessage,
    RecipeLockGuard,
};
use procmail_rs::limits::MessageLimits;
use procmail_rs::message::Message;
use procmail_rs::rc_file::RcFileLoader;
use procmail_rs::runtime::RuntimeVariables;
use procmail_rs::trace::NoTrace;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes =
        b"From: sender@example.org\nTo: reader@example.org\nSubject: ordinary\n folded subject\n\n"
            .to_vec();
    bytes.resize(1024 * 1024, b'x');
    let limits = MessageLimits::default();
    let message = Message::read_from(&mut Cursor::new(&bytes), limits).unwrap();
    println!("benchmark,round,iterations,elapsed_ns");

    for round in 1..=5 {
        let start = Instant::now();

        for _ in 0..100 {
            let output = message.clone();
            black_box(
                Message::from_filter_output(
                    message.header(),
                    message.body(),
                    black_box(output),
                    ActionInput::Message,
                    limits,
                )
                .unwrap(),
            );
        }

        println!("full_filter_1m,{round},100,{}", start.elapsed().as_nanos());
        let start = Instant::now();

        for _ in 0..100 {
            black_box(PreparedMatchingMessage::new(black_box(&message), true));
        }

        println!(
            "matching_full_1m,{round},100,{}",
            start.elapsed().as_nanos()
        );
        let start = Instant::now();

        for _ in 0..100 {
            black_box(PreparedMatchingMessage::new(black_box(&message), false));
        }

        println!(
            "matching_headers_1m,{round},100,{}",
            start.elapsed().as_nanos()
        );
    }
    bench_rc(&message)?;
    Ok(())
}

fn bench_rc(message: &Message) -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args_os().nth(1) else {
        return Ok(());
    };
    let (loader, loaded) = RcFileLoader::for_root(std::path::Path::new(&path))?;
    let supplied = [
        SuppliedVariable::from_environment("HOME", "/benchmark".to_owned())?,
        SuppliedVariable::from_environment("LOGNAME", "benchmark".to_owned())?,
    ];
    let config = loaded.parse()?.expand(&supplied)?;
    let plan = ExecutionPlan::compile(&config, Some(loader));
    let matching = PreparedMatchingMessage::new(message, true);

    // Use the real recipes with deterministic command results and no delivery
    // writes. This isolates evaluator copying from subprocess and disk costs.
    for round in 1..=5 {
        let start = Instant::now();

        for _ in 0..50 {
            let mut runtime = RuntimeVariables::default();
            let input = MappedMessageInput::new(
                message.as_bytes(),
                message.header().len(),
                Some(matching.views(message)),
            );
            black_box(
                plan.execute_ordered(input, &mut runtime, BenchmarkHost::default())
                    .map_err(|error| format!("{error:?}"))?,
            );
        }

        println!("rc_replay_1m,{round},50,{}", start.elapsed().as_nanos());
    }
    Ok(())
}

#[derive(Default)]
struct BenchmarkHost {
    trace: NoTrace,
}

impl OrderedExecutionHost for BenchmarkHost {
    type Error = String;
    type Trace = NoTrace;

    fn trace(&mut self) -> &mut NoTrace {
        &mut self.trace
    }

    fn deliver(
        &mut self,
        destination: &Destination,
        message: &[u8],
        _: OutputEnding,
        _: Option<&str>,
        runtime: &mut RuntimeVariables,
    ) -> Result<(), DeliveryAttemptError<String>> {
        black_box(message);
        runtime.set("LASTFOLDER", destination.path());
        Ok(())
    }

    fn external_action(
        &mut self,
        _: &PipeAction,
        options: RecipeOptions,
        _: Option<&str>,
        input: ExternalActionInput<'_>,
        _: &mut RuntimeVariables,
    ) -> Result<Option<Message>, DeliveryAttemptError<String>> {
        if options.action_mode != ActionMode::Filter {
            return Ok(None);
        }

        let bytes = if options.action_input == ActionInput::Body {
            [&b"\n"[..], input.selected()].concat()
        } else {
            input.selected().to_vec()
        };
        let limits = MessageLimits::default();
        let output = Message::read_from(&mut Cursor::new(bytes), limits)
            .map_err(|error| DeliveryAttemptError::Fatal(error.to_string()))?;
        Message::from_filter_output(
            input.header(),
            input.body(),
            output,
            options.action_input,
            limits,
        )
        .map(Some)
        .map_err(|error| DeliveryAttemptError::Fatal(error.to_string()))
    }

    fn capture(
        &mut self,
        _: &str,
        _: &[u8],
        _: OutputEnding,
        _: Option<RecipeOptions>,
        _: usize,
        _: &mut RuntimeVariables,
    ) -> Result<CapturedCommand, DeliveryAttemptError<String>> {
        Ok(CapturedCommand::new(b"mock".to_vec()))
    }

    fn external_condition(
        &mut self,
        _: &str,
        _: &[u8],
        _: &mut RuntimeVariables,
    ) -> Result<bool, DeliveryAttemptError<String>> {
        Ok(false)
    }

    fn replace_global_lock(&mut self, _: &str, _: &mut RuntimeVariables) -> Result<(), String> {
        Ok(())
    }

    fn acquire_local_lock(
        &mut self,
        _: &str,
        _: &mut RuntimeVariables,
    ) -> Result<Box<dyn RecipeLockGuard>, DeliveryAttemptError<String>> {
        Ok(Box::new(()))
    }

    fn fork_copy_branch(&mut self) -> Option<Self> {
        Some(Self::default())
    }

    fn complete(
        &mut self,
        message: FinalMessage<'_>,
        _: &mut RuntimeVariables,
        _: CompletionState<'_, String>,
    ) {
        black_box(message.as_bytes());
    }
}
