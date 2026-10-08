// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

pub(super) fn render_human_event(output: &mut impl fmt::Write, event: &TraceEvent) -> fmt::Result {
    match event {
        TraceEvent::RcFile {
            line,
            statement,
            stage,
            target,
        } => {
            write!(
                output,
                "procmail-rs: {} at line {line}: {}",
                statement.name(),
                stage.name()
            )?;

            if let Some(target) = target {
                write!(
                    output,
                    " \"{}\"{}",
                    EscapedBytes::new(target.as_bytes()),
                    if target.was_truncated() {
                        " [truncated]"
                    } else {
                        ""
                    }
                )?;
            }

            Ok(())
        }
        TraceEvent::SessionStarted { pid, timestamp } => {
            write!(output, "procmail-rs: [{pid}] {timestamp}")
        }
        TraceEvent::VariableAssigned {
            line, name, value, ..
        } => {
            match line {
                Some(line) => write!(
                    output,
                    "procmail-rs: Assigning at line {line} \"{}",
                    name.as_str()
                )?,
                None => write!(output, "procmail-rs: Assigning \"{}", name.as_str())?,
            }
            match value {
                Some(value) => write!(
                    output,
                    "={}\"{}",
                    EscapedBytes::new(value.as_bytes()),
                    if value.was_truncated() {
                        " (truncated)"
                    } else {
                        ""
                    }
                ),
                None => output.write_str("\" (value hidden)"),
            }
        }
        TraceEvent::VariableUnset { line, name, .. } => match line {
            Some(line) => write!(
                output,
                "procmail-rs: Unsetting at line {line} \"{}\"",
                name.as_str()
            ),
            None => write!(output, "procmail-rs: Unsetting \"{}\"", name.as_str()),
        },
        TraceEvent::LastFolderUpdated => output.write_str("procmail-rs: Updated LASTFOLDER"),
        TraceEvent::ConditionEvaluated {
            recipe_line,
            condition_line,
            condition_index,
            kind,
            negated,
            matched,
            expression,
        } => {
            write!(
                output,
                "procmail-rs: {} on line {condition_line}",
                if *matched { "Match" } else { "No match" }
            )?;
            if let Some(expression) = expression {
                write!(
                    output,
                    " on \"{}\"",
                    EscapedBytes::new(expression.as_bytes())
                )?;
            } else {
                write!(
                    output,
                    " (condition {} of recipe at line {recipe_line}: {}{})",
                    condition_index + 1,
                    human_condition_kind(*kind),
                    if *negated { ", negated" } else { "" }
                )?;
            }
            Ok(())
        }
        TraceEvent::RecipeEvaluated { line, decision } => write!(
            output,
            "procmail-rs: Recipe at line {line}: {}",
            match decision {
                RecipeDecision::Selected => "selected",
                RecipeDecision::Deferred => "waiting for more message data",
                RecipeDecision::Skipped => "skipped",
            }
        ),
        TraceEvent::Delivery {
            recipe_line,
            destination,
            stage,
            path,
        } => match stage {
            DeliveryStage::DryRun => {
                write!(
                    output,
                    "procmail-rs: Would deliver to {}",
                    human_destination_kind(*destination)
                )?;
                if let Some(path) = path {
                    write!(output, " \"{}\"", EscapedBytes::new(path.as_bytes()))?;
                }
                write!(output, " (recipe at line {recipe_line})")
            }
            DeliveryStage::Preparing => write!(
                output,
                "procmail-rs: Recipe at line {recipe_line}: preparing {} delivery",
                human_destination_kind(*destination)
            ),
            DeliveryStage::Published => {
                write!(
                    output,
                    "procmail-rs: Delivered to {}",
                    human_destination_kind(*destination)
                )?;
                if let Some(path) = path {
                    write!(output, " \"{}\"", EscapedBytes::new(path.as_bytes()))?;
                }
                write!(output, " (recipe at line {recipe_line})")
            }
            DeliveryStage::Failure(failure) => {
                write!(
                    output,
                    "procmail-rs: Recipe at line {recipe_line}: {} delivery failed while {}",
                    human_destination_kind(*destination),
                    failure.operation.description()
                )?;
                if let Some(path) = path {
                    write!(output, " for \"{}\"", EscapedBytes::new(path.as_bytes()))?;
                }
                write!(
                    output,
                    ": {} ({}",
                    failure.kind,
                    delivery_failure_class_name(failure.class)
                )?;
                if failure.published {
                    output.write_str("; message already published")?;
                }
                output.write_char(')')
            }
            DeliveryStage::Failed(class) => write!(
                output,
                "procmail-rs: Recipe at line {recipe_line}: {} delivery failed ({})",
                human_destination_kind(*destination),
                failure_class_name(*class)
            ),
        },
        TraceEvent::ExternalCommand { recipe_line, stage } => write!(
            output,
            "procmail-rs: Recipe at line {recipe_line}: external command {}",
            match stage {
                ExternalCommandStage::Starting => "started",
                ExternalCommandStage::Succeeded => "succeeded",
                ExternalCommandStage::Failed(_) => "failed",
            }
        ),
        TraceEvent::ExternalCommandExecuting { line, command } => {
            write!(output, "procmail-rs: Executing at line {line}")?;
            if let Some(command) = command {
                write!(output, " \"{}\"", EscapedBytes::new(command.as_bytes()))?;
            } else {
                output.write_str(" external command")?;
            }
            Ok(())
        }
        TraceEvent::ExternalFilterReplaced { recipe_line, bytes } => write!(
            output,
            "procmail-rs: Filter at line {recipe_line} replaced message: {bytes} bytes"
        ),
        TraceEvent::HeaderOperation {
            line,
            kind,
            name,
            argument,
            extraction_mode,
        } => {
            write!(
                output,
                "procmail-rs: {} header \"{}\"",
                human_header_operation(*kind),
                name.as_str()
            )?;
            if let Some(argument) = argument {
                match kind {
                    HeaderOperationKind::Rename => write!(output, " to \"{}\"", argument.as_str())?,
                    HeaderOperationKind::Extract => {
                        write!(output, " into \"{}\"", argument.as_str())?
                    }
                    _ => {}
                }
            }
            if let Some(mode) = extraction_mode {
                write!(output, " ({})", header_extraction_mode_name(*mode))?;
            }
            write!(output, " at line {line}")
        }
        TraceEvent::Log { value, .. } => {
            output.write_str(std::str::from_utf8(value.as_bytes()).map_err(|_| fmt::Error)?)?;
            if value.was_truncated() {
                output.write_str("...[truncated]")?;
            }
            Ok(())
        }
        TraceEvent::DeliveryAbstract {
            recipe_line,
            destination,
            path,
        } => {
            write!(
                output,
                "procmail-rs: Abstract: delivered to {}",
                human_destination_kind(*destination)
            )?;
            if let Some(path) = path {
                write!(output, " \"{}\"", EscapedBytes::new(path.as_bytes()))?;
            }
            write!(output, " (recipe at line {recipe_line})")
        }
    }
}

fn human_header_operation(kind: HeaderOperationKind) -> &'static str {
    match kind {
        HeaderOperationKind::Remove => "Removing",
        HeaderOperationKind::Set => "Setting",
        HeaderOperationKind::Add => "Adding",
        HeaderOperationKind::Prepend => "Prepending",
        HeaderOperationKind::Rename => "Renaming",
        HeaderOperationKind::Extract => "Extracting",
    }
}

fn human_condition_kind(kind: ConditionKind) -> &'static str {
    match kind {
        ConditionKind::ShellExpanded => "expanded condition",
        ConditionKind::HeaderRegex => "header regular expression",
        ConditionKind::BodyRegex => "body regular expression",
        ConditionKind::MessageRegex => "message regular expression",
        ConditionKind::VariableRegex => "variable regular expression",
        ConditionKind::Address => "address regular expression",
        ConditionKind::Identifier => "identifier regular expression",
        ConditionKind::Program => "external program",
        ConditionKind::SmallerThan => "message size is smaller than",
        ConditionKind::LargerThan => "message size is larger than",
    }
}

fn human_destination_kind(kind: DestinationKind) -> &'static str {
    match kind {
        DestinationKind::Maildir => "Maildir",
        DestinationKind::Mbox => "mbox",
        DestinationKind::File => "file",
        DestinationKind::Discard => "/dev/null",
    }
}
