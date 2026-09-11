// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![forbid(unsafe_code)]

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use procmail_rs::config::{self, MAX_COMMAND_LINE_VARIABLES, SuppliedVariable};
use procmail_rs::configuration;
use procmail_rs::delivery::DeliveryFailureClass;
use procmail_rs::eval::{
    ActionKindExplanation, ConditionKindExplanation, ExecutionPlan, HeaderEvaluation,
    OrderedExecutionError, PlanExplanation,
};
use procmail_rs::hostname::current_hostname;
use procmail_rs::message::Message;
use procmail_rs::rc_file::{LoadedRcFile, RcFileLoader};
use procmail_rs::runtime::RuntimeVariables;
use procmail_rs::signal_state::{self, InterruptibleReader, ReceivedSignal};
use procmail_rs::trace::{
    BoundedTraceWriter, NoTrace, TraceDetail, TraceEvent, TraceFormat, TraceSink,
};
use procmail_rs::user_identity::UserIdentity;

mod command_log;
mod command_runner;
mod delivery_runtime;

use command_runner::CommandRunner;
use delivery_runtime::{DeliveryRuntime, validate_maildir_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Check,
    Explain,
    Filter,
}

struct Command {
    action: Action,
    dry_run: bool,
    trace_format: TraceFormat,
    trace_detail: Option<TraceDetail>,
    config: Option<PathBuf>,
    supplied: Vec<SuppliedVariable>,
}

enum Invocation {
    Run(Command),
    Help,
    Version,
}

const HELP: &str = "procmail-rs - bounded procmail-compatible mail filtering\n\n\
usage: procmail-rs <check|explain|filter> [--dry-run] [--format FORMAT] [--detail DETAIL]\n\
       [--config PATH] [--set NAME=VALUE]...\n\
       procmail-rs --help\n\
       procmail-rs --version\n\n\
commands:\n\
  check    validate the statically reachable configuration without reading stdin\n\
  explain  describe the bounded execution plan without reading stdin\n\
  filter   read one message from stdin and deliver it to explicit destinations\n\n\
options:\n\
  --dry-run         evaluate filter without publishing delivery destinations\n\
  --format FORMAT   trace format: text (default) or json\n\
  --detail DETAIL   override LOGDETAIL: metadata or values\n\
  --config PATH     override the automatically selected root rc file\n\
  --set NAME=VALUE  provide one policy-checked external value (maximum 256)\n\
  -h, --help        print this help text\n\
  -V, --version     print the program version\n";

enum FilterTrace {
    Disabled(NoTrace),
    Enabled(BoundedTraceWriter<Box<dyn Write + Send>>),
}

impl TraceSink for FilterTrace {
    fn detail(&self) -> procmail_rs::trace::TraceDetail {
        match self {
            Self::Disabled(trace) => trace.detail(),
            Self::Enabled(trace) => trace.detail(),
        }
    }

    fn record(&mut self, event: TraceEvent) {
        match self {
            Self::Disabled(trace) => trace.record(event),
            Self::Enabled(trace) => trace.record(event),
        }
    }
}

impl FilterTrace {
    fn stop_reason(&self) -> Option<procmail_rs::trace::TraceStopReason> {
        match self {
            Self::Disabled(_) => None,
            Self::Enabled(trace) => trace.stop_reason(),
        }
    }
}

#[derive(Debug)]
enum OperationalError {
    Configuration(String),
    Input(String),
    TemporaryDelivery(String),
    PermanentDestination(String),
    Undelivered(String),
    Internal(String),
    Signaled(ReceivedSignal),
}

// Use the established sysexits values when they describe the action a caller
// should take. Keep unmatched delivery separate because none of those names
// accurately describes a valid message for which no final recipe was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ExitStatus {
    Success = 0,
    Input = 65,
    PermanentDestination = 73,
    TemporaryDelivery = 75,
    Configuration = 78,
    Undelivered = 79,
    Internal = 70,
}

impl std::fmt::Display for OperationalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Configuration(message)
            | Self::Input(message)
            | Self::TemporaryDelivery(message)
            | Self::PermanentDestination(message)
            | Self::Undelivered(message)
            | Self::Internal(message) => message,
            Self::Signaled(signal) => return write!(formatter, "interrupted by {}", signal.name()),
        };
        formatter.write_str(message)
    }
}

impl OperationalError {
    fn delivery(class: DeliveryFailureClass, message: String) -> Self {
        match class {
            DeliveryFailureClass::Retryable => Self::TemporaryDelivery(message),
            DeliveryFailureClass::Permanent => Self::PermanentDestination(message),
            DeliveryFailureClass::Internal => Self::Internal(message),
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Self::Configuration(_) => ExitStatus::Configuration as u8,
            Self::Input(_) => ExitStatus::Input as u8,
            Self::TemporaryDelivery(_) => ExitStatus::TemporaryDelivery as u8,
            Self::PermanentDestination(_) => ExitStatus::PermanentDestination as u8,
            Self::Undelivered(_) => ExitStatus::Undelivered as u8,
            Self::Internal(_) => ExitStatus::Internal as u8,
            Self::Signaled(signal) => signal.exit_code(),
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(status) => ExitCode::from(status),
        Err(error) => {
            eprintln!("procmail-rs: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn run() -> Result<u8, OperationalError> {
    let invocation = parse_args().map_err(OperationalError::Configuration)?;
    let command = match invocation {
        Invocation::Run(command) => command,
        Invocation::Help => {
            print_stdout(HELP)?;
            return Ok(ExitStatus::Success as u8);
        }
        Invocation::Version => {
            print_stdout(concat!("procmail-rs ", env!("CARGO_PKG_VERSION"), "\n"))?;
            return Ok(ExitStatus::Success as u8);
        }
    };
    let identity = UserIdentity::current().map_err(|error| {
        OperationalError::Configuration(format!("cannot determine current user identity: {error}"))
    })?;
    let hostname = current_hostname().map_err(|error| {
        OperationalError::Configuration(format!("cannot determine current hostname: {error}"))
    })?;
    let mut supplied = vec![
        SuppliedVariable::from_environment("HOME", identity.home().to_owned())
            .map_err(|error| OperationalError::Configuration(error.to_string()))?,
        SuppliedVariable::from_environment("LOGNAME", identity.logname().to_owned())
            .map_err(|error| OperationalError::Configuration(error.to_string()))?,
        SuppliedVariable::from_system_hostname(hostname.clone())
            .map_err(|error| OperationalError::Configuration(error.to_string()))?,
        SuppliedVariable::from_program_version()
            .map_err(|error| OperationalError::Configuration(error.to_string()))?,
    ];
    supplied.extend(command.supplied.iter().cloned());
    let (path, mut rc_loader, root_rc) =
        load_root_config(command.config.as_deref(), identity.home())?;
    let config = config::parse(root_rc.source())
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?
        .expand(&supplied)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;
    rc_loader
        .account_root_config(&config)
        .map_err(|error| OperationalError::Configuration(error.to_string()))?;
    let staging_directory = config.maildir().map(PathBuf::from);
    if let Some(maildir) = &staging_directory {
        validate_maildir_path(maildir).map_err(|error| {
            OperationalError::Configuration(format!("{}: invalid MAILDIR: {error}", path.display()))
        })?;
    }
    let settings = configuration::validate(&config)
        .map_err(|error| OperationalError::Configuration(format!("{}:{error}", path.display())))?;
    let limits = settings.message_limits;
    let durability = settings.durability;
    let trace_detail = command.trace_detail.unwrap_or(settings.trace.detail());

    config.for_each_compatibility_warning(|line, flag| {
        eprintln!(
            "procmail-rs: warning: {}:{line}: recipe flag '{flag}' has no effect on a block",
            path.display()
        );
    });

    // Check resolvable runtime files before building the lazy execution tree.
    // Message-derived paths remain unavailable without stdin, so report them
    // as bounded warnings instead of pretending they were validated.
    if command.action == Action::Check {
        let warnings = rc_loader
            .check_resolvable_files(&config)
            .map_err(|error| OperationalError::Configuration(error.to_string()))?;
        for warning in warnings {
            eprintln!("procmail-rs: warning: {warning}");
        }
        if ExecutionPlan::compile(&config, None).has_external_commands() {
            eprintln!(
                "procmail-rs: warning: configuration contains external shell actions; no command was executed"
            );
        }
        return Ok(ExitStatus::Success as u8);
    }

    let plan = ExecutionPlan::compile(&config, Some(rc_loader));

    // A deferred decision needs a replayable private copy of stdin. Requiring
    // MAILDIR before reading headers prevents a configuration failure from
    // consuming part of a message that the caller may need to retry.
    if command.action == Action::Filter
        && plan.requirements().needs_end_of_message
        && staging_directory.is_none()
    {
        return Err(OperationalError::Configuration(format!(
            "{}: MAILDIR is required when a recipe needs the body or final message size",
            path.display()
        )));
    }
    if command.action == Action::Filter {
        signal_state::install().map_err(|error| {
            OperationalError::Internal(format!("cannot install signal handlers: {error}"))
        })?;
    }

    // Runtime rc diagnostics belong to the completed attempt, including an
    // attempt that later fails delivery. Run the action inside a closure so
    // every `?` returns here first and the bounded diagnostic queue is always
    // drained before this function returns to main.
    let mut requested_status = None;
    let result = (|| match command.action {
        Action::Check => unreachable!(),
        Action::Explain => {
            let mut stdout = io::stdout().lock();
            write_plan_explanation(&plan.explain(), &mut stdout).map_err(|error| {
                OperationalError::Internal(format!("cannot write plan explanation: {error}"))
            })
        }
        Action::Filter => {
            let mut runtime = RuntimeVariables::default();
            runtime.set_system_hostname(hostname);
            let mut delivery_runtime = DeliveryRuntime::new(
                staging_directory,
                durability,
                limits,
                identity.uid(),
                command.dry_run,
            );
            let mut trace = create_filter_trace(
                command.dry_run,
                command.trace_format,
                trace_detail,
                &settings.trace,
            );
            procmail_rs::trace::record_session_start(&mut trace);
            let stdin = io::stdin().lock();
            let mut stdin = InterruptibleReader::new(stdin);
            let mut head = Message::read_headers(&mut stdin, limits).map_err(|error| {
                OperationalError::Input(format!("cannot read message headers from stdin: {error}"))
            })?;
            let mut command_runner = CommandRunner::new(limits);
            let header_evaluation = plan.evaluate_headers_editing_with_capture_trace(
                &mut head,
                &mut runtime,
                &mut trace,
                &mut |command, input, output_ending, recipe_options, limit, runtime, _| {
                    command_runner.capture(
                        command,
                        input,
                        output_ending,
                        recipe_options,
                        limit,
                        runtime,
                    )
                },
            );
            let delivery_result = match header_evaluation {
                Ok(evaluation) => match evaluation {
                    HeaderEvaluation::Decided(delivery) => delivery_runtime.deliver_decided(
                        head,
                        &mut stdin,
                        &delivery,
                        &mut runtime,
                        &mut trace,
                    ),
                    HeaderEvaluation::NeedsMessage(continuation) => delivery_runtime
                        .deliver_staged(
                            head,
                            &mut stdin,
                            &plan,
                            continuation,
                            &mut runtime,
                            &mut trace,
                        ),
                    HeaderEvaluation::Error(error) => Err(OperationalError::PermanentDestination(
                        format!("cannot evaluate message: {error}"),
                    )),
                },
                Err(OrderedExecutionError::Evaluation(error)) => {
                    Err(OperationalError::PermanentDestination(format!(
                        "cannot evaluate message: {error}"
                    )))
                }
                Err(OrderedExecutionError::Delivery(error)) => Err(error),
            };
            if let Some(reason) = trace.stop_reason() {
                eprintln!("procmail-rs: warning: trace stopped: {reason:?}");
            }

            // EXITCODE is resolved after recipe processing because a failure
            // handler may assign it using values produced while filtering.
            // A valid value deliberately replaces a delivery error, matching
            // procmail's final-status override behavior.
            requested_status = parse_requested_exit_code(&runtime)?;
            if requested_status.is_some() {
                Ok(())
            } else {
                delivery_result
            }
        }
    })();
    // A signal may interrupt any fallible operation inside the closure before
    // normal filtering reaches its completion path. Resolve the recorded
    // signal here so an early `?`, including an interrupted stdin read, cannot
    // turn deliberate termination into an unrelated operational error.
    let result = match signal_state::received() {
        Some(signal) if command.action == Action::Filter => Err(OperationalError::Signaled(signal)),
        _ => result,
    };
    for diagnostic in plan.take_rc_diagnostics() {
        eprintln!("procmail-rs: {diagnostic}");
    }
    result?;
    Ok(requested_status.unwrap_or(ExitStatus::Success as u8))
}

fn create_filter_trace(
    dry_run: bool,
    format: TraceFormat,
    detail: TraceDetail,
    config: &procmail_rs::trace::TraceConfig,
) -> FilterTrace {
    if !dry_run && !config.enabled() {
        return FilterTrace::Disabled(NoTrace);
    }
    if dry_run || config.logfile().is_none() {
        return FilterTrace::Enabled(BoundedTraceWriter::formatted(
            Box::new(io::stderr()),
            detail,
            format,
        ));
    }
    let path = config.logfile().unwrap_or_default();
    let nofollow = match i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()) {
        Ok(flags) => flags,
        Err(_) => {
            eprintln!("procmail-rs: cannot enable trace: O_NOFOLLOW does not fit platform flags");
            return FilterTrace::Disabled(NoTrace);
        }
    };

    // Open the configured trace once before message processing. Refusing
    // symlinks and non-regular files keeps diagnostics from being redirected
    // to an unexpected object, while any failure remains advisory and disables
    // only tracing for this message.
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(nofollow)
        .open(path)
        .and_then(|file| {
            if file.metadata()?.file_type().is_file() {
                Ok(file)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "LOGFILE is not a regular file",
                ))
            }
        });
    match file {
        Ok(file) => FilterTrace::Enabled(BoundedTraceWriter::formatted(
            Box::new(file),
            detail,
            format,
        )),
        Err(error) => {
            eprintln!("procmail-rs: cannot open LOGFILE for trace: {error}");
            FilterTrace::Disabled(NoTrace)
        }
    }
}

fn parse_requested_exit_code(runtime: &RuntimeVariables) -> Result<Option<u8>, OperationalError> {
    let Some(value) = runtime.get("EXITCODE") else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(OperationalError::PermanentDestination(
            "EXITCODE must be an unsigned decimal value from 0 through 255".to_owned(),
        ));
    }
    value.parse::<u8>().map(Some).map_err(|_| {
        OperationalError::PermanentDestination(
            "EXITCODE must be an unsigned decimal value from 0 through 255".to_owned(),
        )
    })
}

fn write_plan_explanation(
    explanation: &PlanExplanation,
    writer: &mut impl Write,
) -> io::Result<()> {
    let requirements = explanation.requirements();
    writeln!(
        writer,
        "input headers={} body={} end={}",
        yes_no(requirements.needs_headers),
        yes_no(requirements.needs_body_contents),
        yes_no(requirements.needs_end_of_message)
    )?;
    writeln!(
        writer,
        "ordered-delivery={}",
        yes_no(explanation.requires_ordered_delivery())
    )?;

    // The explanation deliberately describes only control-flow shape. Do not
    // print regex text, assignment values, or destination paths because they
    // can contain credentials or other private configuration data.
    for recipe in explanation.recipes() {
        let action = match recipe.action() {
            ActionKindExplanation::Maildir => "maildir",
            ActionKindExplanation::Mbox => "mbox",
            ActionKindExplanation::File => "file",
            ActionKindExplanation::Discard => "discard",
            ActionKindExplanation::ExternalProgram => "external-program",
            ActionKindExplanation::Headers => "headers",
        };
        writeln!(
            writer,
            "recipe line={} copy={} assignments={} action={} deferred={}",
            recipe.line(),
            yes_no(recipe.is_copy()),
            recipe.assignment_count(),
            action,
            yes_no(recipe.defers_destination())
        )?;
        if let Some(operations) = recipe.header_operations() {
            writeln!(
                writer,
                "  header-operations remove={} set={} add={} prepend={} rename={} extract={}",
                operations.remove_count(),
                operations.set_count(),
                operations.add_count(),
                operations.prepend_count(),
                operations.rename_count(),
                operations.extract_count()
            )?;
        }
        for condition in recipe.conditions() {
            let kind = match condition.kind() {
                ConditionKindExplanation::ShellExpanded => "shell-expanded",
                ConditionKindExplanation::HeaderRegex => "header-regex",
                ConditionKindExplanation::BodyRegex => "body-regex",
                ConditionKindExplanation::MessageRegex => "message-regex",
                ConditionKindExplanation::VariableRegex => "variable-regex",
                ConditionKindExplanation::Program => "program",
                ConditionKindExplanation::SmallerThan => "smaller-than",
                ConditionKindExplanation::LargerThan => "larger-than",
            };
            writeln!(
                writer,
                "  condition kind={} negated={}",
                kind,
                yes_no(condition.is_negated())
            )?;
        }
    }
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn print_stdout(value: &str) -> Result<(), OperationalError> {
    io::stdout()
        .lock()
        .write_all(value.as_bytes())
        .map_err(|error| OperationalError::Internal(format!("cannot write stdout: {error}")))
}

fn parse_args() -> Result<Invocation, String> {
    let mut args = env::args_os().skip(1);
    let action = args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .ok_or_else(usage)?;
    let action = match action.as_str() {
        "-h" | "--help" => {
            return if args.next().is_none() {
                Ok(Invocation::Help)
            } else {
                Err(usage())
            };
        }
        "-V" | "--version" => {
            return if args.next().is_none() {
                Ok(Invocation::Version)
            } else {
                Err(usage())
            };
        }
        "check" => Action::Check,
        "explain" => Action::Explain,
        "filter" => Action::Filter,
        _ => return Err(usage()),
    };
    let mut config = None;
    let mut supplied = Vec::new();
    let mut dry_run = false;
    let mut trace_format = TraceFormat::Text;
    let mut trace_detail = None;

    // Parse every option before opening the rc file or stdin. This keeps bad
    // or excessive caller-controlled assignments from affecting filtering or
    // consuming any part of a message.
    while let Some(option) = args.next() {
        match option.to_str() {
            Some("-h" | "--help") => return Ok(Invocation::Help),
            Some("-V" | "--version") => return Ok(Invocation::Version),
            Some("--config") => {
                if config.is_some() {
                    return Err("--config may only be specified once".into());
                }
                config = Some(PathBuf::from(args.next().ok_or_else(usage)?));
            }
            Some("--dry-run") => {
                if action != Action::Filter {
                    return Err("--dry-run may only be used with filter".into());
                }
                if dry_run {
                    return Err("--dry-run may only be specified once".into());
                }
                dry_run = true;
            }
            Some("--format") => {
                if action != Action::Filter {
                    return Err("--format may only be used with filter".into());
                }
                trace_format = parse_trace_format(args.next().ok_or_else(usage)?)?;
            }
            Some("--detail") => {
                if action != Action::Filter {
                    return Err("--detail may only be used with filter".into());
                }
                trace_detail = Some(parse_trace_detail(args.next().ok_or_else(usage)?)?);
            }
            Some(option) if option.starts_with("--format=") => {
                if action != Action::Filter {
                    return Err("--format may only be used with filter".into());
                }
                trace_format = parse_trace_format(option["--format=".len()..].into())?;
            }
            Some(option) if option.starts_with("--detail=") => {
                if action != Action::Filter {
                    return Err("--detail may only be used with filter".into());
                }
                trace_detail = Some(parse_trace_detail(option["--detail=".len()..].into())?);
            }
            Some("--set") => {
                if supplied.len() == MAX_COMMAND_LINE_VARIABLES {
                    return Err(format!(
                        "too many --set values; hard limit is {MAX_COMMAND_LINE_VARIABLES}"
                    ));
                }
                let value = args
                    .next()
                    .ok_or_else(usage)?
                    .into_string()
                    .map_err(|_| "--set value is not valid UTF-8".to_owned())?;
                supplied.push(SuppliedVariable::parse(value).map_err(|error| error.to_string())?);
            }
            _ => return Err(usage()),
        }
    }

    Ok(Invocation::Run(Command {
        action,
        dry_run,
        trace_format,
        trace_detail,
        config,
        supplied,
    }))
}

fn parse_trace_format(value: std::ffi::OsString) -> Result<TraceFormat, String> {
    match value.to_str() {
        Some("text") => Ok(TraceFormat::Text),
        Some("json") => Ok(TraceFormat::Json),
        _ => Err("--format expects 'text' or 'json'".into()),
    }
}

fn parse_trace_detail(value: std::ffi::OsString) -> Result<TraceDetail, String> {
    match value.to_str() {
        Some("metadata") => Ok(TraceDetail::Metadata),
        Some("values") => Ok(TraceDetail::Values),
        _ => Err("--detail expects 'metadata' or 'values'".into()),
    }
}

fn usage() -> String {
    "usage: procmail-rs <check|explain|filter> [--dry-run] [--format FORMAT] [--detail DETAIL] [--config PATH] [--set NAME=VALUE]...".into()
}

fn load_root_config(
    explicit: Option<&Path>,
    home: &str,
) -> Result<(PathBuf, RcFileLoader, LoadedRcFile), OperationalError> {
    if let Some(path) = explicit {
        let loaded = RcFileLoader::for_root(path)
            .map_err(|error| OperationalError::Configuration(error.to_string()))?;
        return Ok((path.to_owned(), loaded.0, loaded.1));
    }

    let home = Path::new(home);
    let candidates = [
        home.join(".config/procmail-rs/config"),
        home.join(".procmailrc"),
    ];
    for path in &candidates {
        match RcFileLoader::for_root(path) {
            Ok((loader, root)) => return Ok((path.clone(), loader, root)),
            Err(error) if error.is_not_found() => continue,
            Err(error) => return Err(OperationalError::Configuration(error.to_string())),
        }
    }

    Err(OperationalError::Configuration(format!(
        "no rc file found; tried {} and {} (use --config PATH to select one)",
        candidates[0].display(),
        candidates[1].display()
    )))
}

#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;
