#![forbid(unsafe_code)]

use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use procmail_rs::config::{self, Destination};
use procmail_rs::delivery::maildir::MaildirSink;
use procmail_rs::delivery::{PendingFanout, PendingSink};
use procmail_rs::eval::{DeliveryPlan, ExecutionPlan, HeaderEvaluation};
use procmail_rs::limits::{MAX_RC_SIZE, MessageLimits};
use procmail_rs::message::Message;

enum Command {
    Check { config: PathBuf },
    Filter { config: PathBuf },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("procmail-rs: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let command = parse_args()?;
    let path = match &command {
        Command::Check { config } | Command::Filter { config } => config,
    };
    let source = read_config(path)?;
    let config = config::parse(&source).map_err(|error| format!("{}:{error}", path.display()))?;
    let limits = MessageLimits::from_config(&config)
        .map_err(|error| format!("{}:{error}", path.display()))?;
    let plan = ExecutionPlan::compile(&config)
        .map_err(|error| format!("cannot compile {}: {error}", path.display()))?;

    match command {
        Command::Check { .. } => Ok(()),
        Command::Filter { .. } => {
            let mut stdin = io::stdin().lock();
            let head = Message::read_headers(&mut stdin, limits)
                .map_err(|error| format!("cannot read message headers from stdin: {error}"))?;
            let delivery = match plan.evaluate_headers(&head) {
                HeaderEvaluation::Decided(delivery) => {
                    return deliver_decided(head, &mut stdin, &delivery);
                }
                HeaderEvaluation::NeedsMessage(continuation)
                    if continuation.requirements().needs_body_contents =>
                {
                    let message = head
                        .read_body(&mut stdin)
                        .map_err(|error| format!("cannot read message body from stdin: {error}"))?;
                    plan.resume_buffered(continuation, &message)
                        .map_err(|error| format!("cannot evaluate message: {error}"))?
                }
                HeaderEvaluation::NeedsMessage(continuation) => {
                    let message = head
                        .stream_to(&mut stdin, &mut io::sink())
                        .map_err(|error| format!("cannot stream message from stdin: {error}"))?;
                    plan.resume_streamed(continuation, &message)
                        .map_err(|error| format!("cannot evaluate message: {error}"))?
                }
            };
            Err(format!(
                "delivery is not implemented yet (selected {} destination(s))",
                delivery.destinations().len()
            ))
        }
    }
}

fn deliver_decided(
    head: procmail_rs::message::MessageHead,
    reader: &mut impl io::BufRead,
    plan: &DeliveryPlan,
) -> Result<(), String> {
    let mut sinks: Vec<Box<dyn PendingSink>> = Vec::with_capacity(plan.destinations().len());
    for destination in plan.destinations() {
        match destination {
            Destination::Maildir(path) => {
                let sink = MaildirSink::create(Path::new(path))
                    .map_err(|error| format!("cannot open Maildir {path}: {error}"))?;
                sinks.push(Box::new(sink));
            }
            Destination::Mbox(path) => {
                return Err(format!("mbox delivery is not implemented yet: {path}"));
            }
            Destination::Auto(path) => {
                return Err(format!(
                    "automatic destination detection is not implemented yet: {path}"
                ));
            }
        }
    }

    let pending = PendingFanout::new(sinks).map_err(|error| error.to_string())?;
    let validated = pending
        .stream(head, reader)
        .map_err(|error| format!("cannot stream message from stdin: {error}"))?;
    validated
        .commit()
        .map_err(|error| format!("cannot publish Maildir delivery: {error}"))?;

    if plan.original_delivered() {
        Ok(())
    } else {
        Err(format!(
            "original message was not delivered (published {} copy destination(s))",
            plan.copies()
        ))
    }
}

fn read_config(path: &Path) -> Result<String, String> {
    let file =
        File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut source = Vec::with_capacity(MAX_RC_SIZE.min(64 * 1024));
    file.take((MAX_RC_SIZE + 1) as u64)
        .read_to_end(&mut source)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if source.len() > MAX_RC_SIZE {
        return Err(format!(
            "cannot read {}: rc file exceeds the hard limit of {MAX_RC_SIZE} bytes",
            path.display()
        ));
    }
    String::from_utf8(source)
        .map_err(|_| format!("cannot read {}: rc file is not valid UTF-8", path.display()))
}

fn parse_args() -> Result<Command, String> {
    let mut args = env::args_os().skip(1);
    let command = args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .ok_or_else(usage)?;
    let option = args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .ok_or_else(usage)?;
    if option != "--config" {
        return Err(usage());
    }
    let config = args.next().map(PathBuf::from).ok_or_else(usage)?;
    if args.next().is_some() {
        return Err(usage());
    }

    match command.as_str() {
        "check" => Ok(Command::Check { config }),
        "filter" => Ok(Command::Filter { config }),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage: procmail-rs <check|filter> --config PATH".into()
}
