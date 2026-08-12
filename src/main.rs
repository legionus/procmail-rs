#![forbid(unsafe_code)]

use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use procmail_rs::config;
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

    match command {
        Command::Check { .. } => Ok(()),
        Command::Filter { .. } => {
            let message = Message::read_from(&mut io::stdin().lock(), limits)
                .map_err(|error| format!("cannot read message from stdin: {error}"))?;
            Err(format!(
                "delivery is not implemented yet (read {} message bytes)",
                message.len()
            ))
        }
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
