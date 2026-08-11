#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use procmail_rs::config;
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
    let source = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    config::parse(&source).map_err(|error| format!("{}:{error}", path.display()))?;

    match command {
        Command::Check { .. } => Ok(()),
        Command::Filter { .. } => {
            let mut raw = Vec::new();
            io::stdin()
                .read_to_end(&mut raw)
                .map_err(|error| format!("cannot read message from stdin: {error}"))?;
            let message = Message::from_bytes(raw);
            Err(format!(
                "delivery is not implemented yet (read {} message bytes)",
                message.len()
            ))
        }
    }
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
