// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

pub(super) fn render_json_event(output: &mut impl fmt::Write, event: &TraceEvent) -> fmt::Result {
    match event {
        TraceEvent::RcFile {
            line,
            statement,
            stage,
            target,
        } => {
            write!(
                output,
                "{{\"event\":\"rc-file\",\"line\":{line},\"statement\":\"{}\",\"stage\":\"{}\"",
                statement.name(),
                stage.name()
            )?;

            if let Some(target) = target {
                output.write_str(",\"target\":")?;
                render_json_string(output, target.as_bytes())?;
                write!(output, ",\"target_truncated\":{}", target.was_truncated())?;
            }

            output.write_char('}')
        }
        TraceEvent::SessionStarted { pid, timestamp } => {
            write!(
                output,
                "{{\"event\":\"session-start\",\"pid\":{pid},\"timestamp\":"
            )?;
            render_json_string(output, timestamp.as_bytes())?;
            output.write_char('}')
        }
        TraceEvent::VariableAssigned {
            line,
            name,
            source,
            value,
        } => {
            write!(
                output,
                "{{\"event\":\"variable-assigned\",\"line\":{},\"name\":",
                line.unwrap_or(0),
            )?;
            render_json_string(output, name.as_str().as_bytes())?;
            write!(output, ",\"source\":\"{}\"", variable_source_name(*source))?;
            if let Some(value) = value {
                output.write_str(",\"value\":")?;
                render_json_string(output, value.as_bytes())?;
                write!(output, ",\"value_truncated\":{}", value.was_truncated())?;
            }
            output.write_char('}')
        }
        TraceEvent::VariableUnset { line, name, source } => {
            write!(
                output,
                "{{\"event\":\"variable-unset\",\"line\":{},\"name\":",
                line.unwrap_or(0),
            )?;
            render_json_string(output, name.as_str().as_bytes())?;
            write!(
                output,
                ",\"source\":\"{}\"}}",
                variable_source_name(*source)
            )
        }
        TraceEvent::LastFolderUpdated => output.write_str("{\"event\":\"last-folder-updated\"}"),
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
                "{{\"event\":\"condition\",\"recipe_line\":{recipe_line},\"condition_line\":{condition_line},\"condition_index\":{condition_index},\"kind\":\"{}\",\"negated\":{negated},\"matched\":{matched}",
                condition_kind_name(*kind)
            )?;
            if let Some(expression) = expression {
                output.write_str(",\"expression\":")?;
                render_json_string(output, expression.as_bytes())?;
                write!(
                    output,
                    ",\"expression_truncated\":{}",
                    expression.was_truncated()
                )?;
            }
            output.write_char('}')
        }
        TraceEvent::RecipeEvaluated { line, decision } => {
            write!(
                output,
                "{{\"event\":\"recipe\",\"line\":{line},\"decision\":\"{}\"}}",
                recipe_decision_name(*decision)
            )
        }
        TraceEvent::Delivery {
            recipe_line,
            destination,
            stage,
            path,
        } => {
            write!(
                output,
                "{{\"event\":\"delivery\",\"recipe_line\":{recipe_line},\"destination\":\"{}\",\"stage\":\"",
                destination_kind_name(*destination)
            )?;
            if let DeliveryStage::Failure(failure) = stage {
                write!(
                    output,
                    "failed\",\"failure_class\":\"{}\",\"reason\":\"{}\",\"operation\":\"{}\",\"published\":{}",
                    delivery_failure_class_name(failure.class),
                    failure.kind,
                    failure.operation.name(),
                    failure.published
                )?;
            } else {
                render_delivery_stage(output, *stage)?;
                output.write_char('"')?;
            }
            if let Some(path) = path {
                output.write_str(",\"path\":")?;
                render_json_string(output, path.as_bytes())?;
                write!(output, ",\"path_truncated\":{}}}", path.was_truncated())
            } else {
                output.write_char('}')
            }
        }
        TraceEvent::ExternalCommand { recipe_line, stage } => {
            write!(
                output,
                "{{\"event\":\"external-command\",\"recipe_line\":{recipe_line},\"stage\":\""
            )?;
            render_external_stage(output, *stage).and_then(|()| output.write_str("\"}"))
        }
        TraceEvent::ExternalCommandExecuting { line, command } => {
            write!(
                output,
                "{{\"event\":\"external-command-executing\",\"line\":{line}"
            )?;
            if let Some(command) = command {
                output.write_str(",\"command\":")?;
                render_json_string(output, command.as_bytes())?;
                write!(output, ",\"command_truncated\":{}", command.was_truncated())?;
            }
            output.write_char('}')
        }
        TraceEvent::ExternalFilterReplaced { recipe_line, bytes } => write!(
            output,
            "{{\"event\":\"external-filter-replaced\",\"recipe_line\":{recipe_line},\"bytes\":{bytes}}}"
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
                "{{\"event\":\"header-operation\",\"line\":{line},\"operation\":\"{}\",\"name\":",
                header_operation_kind_name(*kind)
            )?;
            render_json_string(output, name.as_str().as_bytes())?;
            if let Some(argument) = argument {
                output.write_str(",\"argument\":")?;
                render_json_string(output, argument.as_str().as_bytes())?;
            }
            if let Some(mode) = extraction_mode {
                write!(
                    output,
                    ",\"extraction_mode\":\"{}\"",
                    header_extraction_mode_name(*mode)
                )?;
            }
            output.write_char('}')
        }
        TraceEvent::Log { line, value } => {
            write!(output, "{{\"event\":\"log\",\"line\":{line},\"value\":")?;
            render_json_string(output, value.as_bytes())?;
            write!(output, ",\"value_truncated\":{}}}", value.was_truncated())
        }
        TraceEvent::DeliveryAbstract {
            recipe_line,
            destination,
            path,
        } => {
            write!(
                output,
                "{{\"event\":\"delivery-abstract\",\"recipe_line\":{recipe_line},\"destination\":\"{}\"",
                destination_kind_name(*destination)
            )?;
            if let Some(path) = path {
                output.write_str(",\"path\":")?;
                render_json_string(output, path.as_bytes())?;
                write!(output, ",\"path_truncated\":{}", path.was_truncated())?;
            }
            output.write_char('}')
        }
    }
}

pub(super) fn render_json_string(output: &mut impl fmt::Write, value: &[u8]) -> fmt::Result {
    output.write_char('"')?;
    for byte in value {
        match byte {
            b'"' => output.write_str("\\\"")?,
            b'\\' => output.write_str("\\\\")?,
            b'\n' => output.write_str("\\n")?,
            b'\r' => output.write_str("\\r")?,
            b'\t' => output.write_str("\\t")?,
            b' '..=b'~' => output.write_char(char::from(*byte))?,
            _ => write!(output, "\\u00{byte:02x}")?,
        }
    }
    output.write_char('"')
}

fn header_operation_kind_name(kind: HeaderOperationKind) -> &'static str {
    match kind {
        HeaderOperationKind::Remove => "remove",
        HeaderOperationKind::Set => "set",
        HeaderOperationKind::Add => "add",
        HeaderOperationKind::Prepend => "prepend",
        HeaderOperationKind::Rename => "rename",
        HeaderOperationKind::Extract => "extract",
    }
}

fn condition_kind_name(kind: ConditionKind) -> &'static str {
    match kind {
        ConditionKind::ShellExpanded => "shell-expanded",
        ConditionKind::HeaderRegex => "header-regex",
        ConditionKind::BodyRegex => "body-regex",
        ConditionKind::MessageRegex => "message-regex",
        ConditionKind::VariableRegex => "variable-regex",
        ConditionKind::Address => "address",
        ConditionKind::Identifier => "identifier",
        ConditionKind::Program => "program",
        ConditionKind::SmallerThan => "smaller-than",
        ConditionKind::LargerThan => "larger-than",
    }
}

fn variable_source_name(source: VariableSource) -> &'static str {
    match source {
        VariableSource::RcFile => "rc-file",
        VariableSource::CommandLine => "command-line",
        VariableSource::Environment => "environment",
        VariableSource::System => "system",
        VariableSource::Runtime => "runtime",
    }
}

fn recipe_decision_name(decision: RecipeDecision) -> &'static str {
    match decision {
        RecipeDecision::Selected => "selected",
        RecipeDecision::Deferred => "deferred",
        RecipeDecision::Skipped => "skipped",
    }
}

fn destination_kind_name(kind: DestinationKind) -> &'static str {
    match kind {
        DestinationKind::Maildir => "maildir",
        DestinationKind::Mbox => "mbox",
        DestinationKind::File => "file",
        DestinationKind::Discard => "discard",
    }
}

fn render_delivery_stage(output: &mut impl fmt::Write, stage: DeliveryStage) -> fmt::Result {
    match stage {
        DeliveryStage::Preparing => output.write_str("preparing"),
        DeliveryStage::DryRun => output.write_str("dry-run"),
        DeliveryStage::Published => output.write_str("published"),
        DeliveryStage::Failure(failure) => write!(
            output,
            "failed failure_class={} reason={} operation={} published={}",
            delivery_failure_class_name(failure.class),
            failure.kind,
            failure.operation.name(),
            failure.published
        ),
        DeliveryStage::Failed(class) => {
            write!(output, "failed failure_class={}", failure_class_name(class))
        }
    }
}

fn render_external_stage(output: &mut impl fmt::Write, stage: ExternalCommandStage) -> fmt::Result {
    match stage {
        ExternalCommandStage::Starting => output.write_str("starting"),
        ExternalCommandStage::Succeeded => output.write_str("succeeded"),
        ExternalCommandStage::Failed(class) => {
            write!(output, "failed failure_class={}", failure_class_name(class))
        }
    }
}
