// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use super::{
    Assignment, AssignmentTarget, Config, Destination, ExpansionExpression, ExpansionPart,
    HeaderAction, HeaderOperation, HeaderValue, MAX_ASSIGNMENT_VALUE_LEN, MAX_EXPANSION_DEPTH,
    MAX_PATH_EXPRESSION_LEN, PathExpression, RcFileExpression, Recipe, RecipeAction, Statement,
    SuppliedVariable, VariablePolicy, VariableSource, assignment_value_limit, variable_policy,
};

#[derive(Debug, Clone)]
struct ExpandedValue {
    text: String,
    depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpansionError {
    pub line: usize,
    pub message: String,
}

impl ExpansionError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for ExpansionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(formatter, "command line: {}", self.message)
        } else {
            write!(formatter, "line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for ExpansionError {}

impl Assignment {
    pub(crate) fn resolve_with(
        &self,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<String, ExpansionError> {
        let value = match self.expansion.as_ref() {
            Some(expression) => {
                evaluate_with_linebuf(
                    expression,
                    self.line,
                    assignment_value_limit(self.target),
                    &mut lookup,
                )?
                .text
            }
            None => self.value.clone(),
        };
        match self.target {
            AssignmentTarget::LockMethod => super::validate_lock_method(&value),
            AssignmentTarget::LockTimeout => super::parse_lock_timeout_seconds(&value).map(drop),
            AssignmentTarget::ProcessTimeout => {
                super::parse_process_timeout_seconds(&value).map(drop)
            }
            AssignmentTarget::Umask => super::parse_umask(&value).map(drop),
            AssignmentTarget::Trap => super::validate_trap_command(&value),
            AssignmentTarget::LockExt => super::validate_lock_ext(&value),
            AssignmentTarget::LogAbstract => super::validate_log_abstract(&value),
            _ => Ok(()),
        }
        .map_err(|message| ExpansionError::new(self.line, message))?;
        if !matches!(
            self.target,
            AssignmentTarget::Maildir | AssignmentTarget::LockFile
        ) {
            return Ok(value);
        }
        let base = lookup("MAILDIR");
        let value = resolve_relative_path(&value, base.as_deref(), self.line)?;
        if self.target == AssignmentTarget::Maildir {
            validate_filesystem_path(&value, self.line, "MAILDIR", true)?;
        } else if !value.is_empty() {
            validate_filesystem_path(&value, self.line, "LOCKFILE", false)?;
        }
        Ok(value)
    }
}

impl RcFileExpression {
    pub(crate) fn resolve_with(
        &self,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<String, ExpansionError> {
        let parsed;
        let expression = if let Some(expression) = self.expansion.as_ref() {
            expression
        } else {
            parsed = parse_expression(&self.value, self.line)?;
            &parsed
        };
        let value =
            evaluate_with_linebuf(expression, self.line, MAX_PATH_EXPRESSION_LEN, &mut lookup)?
                .text;
        if value.is_empty() {
            return Ok(value);
        }

        // procmail treats MAILDIR as its current directory. Resolve against
        // its value at the moment the statement executes; when it is unset,
        // leave the path relative so the loader uses the process directory.
        let base = lookup("MAILDIR");
        let value = resolve_relative_path(&value, base.as_deref(), self.line)?;
        validate_filesystem_path(&value, self.line, "rc file", false)?;
        Ok(value)
    }
}

impl HeaderValue {
    pub(crate) fn resolve_with(
        &self,
        line: usize,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<String, ExpansionError> {
        let parsed;
        let expression = if let Some(expression) = self.expansion.as_ref() {
            expression
        } else {
            parsed = parse_expression(&self.source, line)?;
            &parsed
        };
        let value =
            evaluate_with_linebuf(expression, line, MAX_ASSIGNMENT_VALUE_LEN, &mut lookup)?.text;
        validate_header_value(&value, line)?;
        Ok(value)
    }
}

impl HeaderAction {
    pub(crate) fn resolve_with(
        &self,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ExpansionError> {
        let mut resolved = self.clone();
        for operation in &mut resolved.operations {
            let (line, value) = match operation {
                HeaderOperation::Remove { .. } => continue,
                HeaderOperation::Set { line, value, .. }
                | HeaderOperation::Add { line, value, .. }
                | HeaderOperation::Prepend { line, value, .. } => (*line, value),
            };
            value.source = value.resolve_with(line, &mut lookup)?;
            value.expansion = None;
        }
        Ok(resolved)
    }
}

impl Destination {
    pub fn bind_with(
        &self,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ExpansionError> {
        let expression = match self {
            Self::Maildir(expression) | Self::Mbox(expression) => expression,
        };
        let parsed;
        let compiled = if let Some(compiled) = expression.expansion.as_ref() {
            compiled
        } else {
            parsed = parse_expression(&expression.source, expression.line)?;
            &parsed
        };
        let expansion = bind_static_expression(compiled, expression.line, &mut lookup, 0)?;
        let bound = PathExpression {
            source: expression.source.clone(),
            base: expression.base.clone(),
            line: expression.line,
            runtime_dependent: expression_has_runtime(&expansion),
            runtime_base: expression.runtime_base,
            expansion: Some(expansion),
        };
        Ok(match self {
            Self::Maildir(_) => Self::Maildir(bound),
            Self::Mbox(_) => Self::Mbox(bound),
        })
    }

    pub fn resolve_with(
        &self,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ExpansionError> {
        let (expression, description, allows_trailing_slash) = match self {
            Self::Maildir(expression) => (expression, "Maildir destination", true),
            Self::Mbox(expression) => (expression, "mbox destination", false),
        };
        let parsed;
        let compiled = if let Some(compiled) = expression.expansion.as_ref() {
            compiled
        } else {
            parsed = parse_expression(&expression.source, expression.line)?;
            &parsed
        };
        let source = evaluate_with_linebuf(
            compiled,
            expression.line,
            MAX_PATH_EXPRESSION_LEN,
            &mut lookup,
        )?
        .text;
        let runtime_base = expression.runtime_base.then(|| lookup("MAILDIR")).flatten();
        let base = runtime_base.as_deref().or(expression.base.as_deref());
        let path = resolve_relative_path(&source, base, expression.line)?;
        validate_filesystem_path(&path, expression.line, description, allows_trailing_slash)?;
        let resolved = PathExpression {
            source: path,
            base: None,
            line: expression.line,
            runtime_dependent: false,
            runtime_base: false,
            expansion: None,
        };
        Ok(match self {
            Self::Maildir(_) => Self::Maildir(resolved),
            Self::Mbox(_) => Self::Mbox(resolved),
        })
    }

    pub fn path(&self) -> &str {
        match self {
            Self::Maildir(expression) | Self::Mbox(expression) => expression.source(),
        }
    }

    pub fn needs_runtime_variables(&self) -> bool {
        match self {
            Self::Maildir(expression) | Self::Mbox(expression) => expression.runtime_dependent,
        }
    }
}

impl PathExpression {
    pub(crate) fn resolve_with(
        &self,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<String, ExpansionError> {
        let parsed;
        let compiled = if let Some(compiled) = self.expansion.as_ref() {
            compiled
        } else {
            parsed = parse_expression(&self.source, self.line)?;
            &parsed
        };
        let source =
            evaluate_with_linebuf(compiled, self.line, MAX_PATH_EXPRESSION_LEN, &mut lookup)?.text;
        if source.is_empty() {
            return Ok(source);
        }
        let runtime_base = self.runtime_base.then(|| lookup("MAILDIR")).flatten();
        let base = runtime_base.as_deref().or(self.base.as_deref());
        let path = resolve_relative_path(&source, base, self.line)?;
        validate_filesystem_path(&path, self.line, "lockfile", false)?;
        Ok(path)
    }
}

pub(super) fn expand(
    config: Config,
    supplied: &[SuppliedVariable],
) -> Result<Config, ExpansionError> {
    let mut variables = BTreeMap::<String, ExpandedValue>::new();
    let mut initial_variables = Vec::with_capacity(supplied.len());
    for variable in supplied {
        let value = if matches!(
            variable.source(),
            VariableSource::Environment | VariableSource::System
        ) {
            ExpandedValue {
                text: variable.value().to_owned(),
                depth: 0,
            }
        } else {
            expand_text(variable.value(), 0, MAX_ASSIGNMENT_VALUE_LEN, &variables)?
        };
        initial_variables.push((
            variable.name().to_owned(),
            value.text.clone(),
            variable.source(),
        ));
        variables.insert(variable.name().to_owned(), value);
    }
    expand_config(config, variables, initial_variables, None)
}

pub(super) fn expand_with_runtime_values<'a>(
    config: Config,
    values: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<Config, ExpansionError> {
    let values = values.collect::<Vec<_>>();
    let maildir = values
        .iter()
        .rev()
        .find_map(|(name, value)| (*name == "MAILDIR").then(|| (*value).to_owned()));
    let variables = values
        .into_iter()
        .map(|(name, value)| {
            (
                name.to_owned(),
                ExpandedValue {
                    text: value.to_owned(),
                    depth: 0,
                },
            )
        })
        .collect();
    expand_config(config, variables, Vec::new(), maildir)
}

pub(super) fn prepare_for_check<'a>(
    mut config: Config,
    values: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<Config, ExpansionError> {
    let values = values.collect::<Vec<_>>();
    let maildir = values
        .iter()
        .rev()
        .find_map(|(name, value)| (*name == "MAILDIR").then_some(*value));
    let known = values
        .into_iter()
        .map(|(name, value)| {
            (
                name.to_owned(),
                ExpandedValue {
                    text: value.to_owned(),
                    depth: 0,
                },
            )
        })
        .collect();
    let mut dynamic = BTreeSet::new();

    // A check has no message values, but it still needs to reject undefined
    // ordinary variables and malformed path expressions throughout a loaded
    // file. Prepare every statement for later symbolic evaluation instead of
    // demanding MATCH or LASTFOLDER before stdin exists.
    prepare_runtime_statements(&mut config.statements, &known, &mut dynamic, maildir)?;
    Ok(config)
}

fn expand_config(
    mut config: Config,
    mut variables: BTreeMap<String, ExpandedValue>,
    initial_variables: Vec<(String, String, VariableSource)>,
    mut maildir: Option<String>,
) -> Result<Config, ExpansionError> {
    let mut linebuf = config.initial_linebuf;
    config.initial_variables = initial_variables;
    variables
        .entry("LINEBUF".to_owned())
        .or_insert(ExpandedValue {
            text: linebuf.to_string(),
            depth: 0,
        });
    variables
        .entry("LOCKEXT".to_owned())
        .or_insert(ExpandedValue {
            text: super::DEFAULT_LOCK_EXT.to_owned(),
            depth: 0,
        });

    for statement in &mut config.statements {
        match statement {
            Statement::Assignment(assignment) => {
                let hard_limit = assignment_value_limit(assignment.target);
                let limit = hard_limit.min(linebuf);
                let expanded =
                    expand_text(&assignment.value, assignment.line, limit, &variables)
                        .map_err(|error| relabel_linebuf_error(error, linebuf, hard_limit))?;
                assignment.value = expanded.text;
                if assignment.target == AssignmentTarget::Trap {
                    super::validate_trap_command(&assignment.value)
                        .map_err(|message| ExpansionError::new(assignment.line, message))?;
                }
                if assignment.target == AssignmentTarget::LockExt {
                    super::validate_lock_ext(&assignment.value)
                        .map_err(|message| ExpansionError::new(assignment.line, message))?;
                }
                if assignment.target == AssignmentTarget::LogAbstract {
                    super::validate_log_abstract(&assignment.value)
                        .map_err(|message| ExpansionError::new(assignment.line, message))?;
                }
                if assignment.target == AssignmentTarget::LineBuf {
                    linebuf = parse_linebuf(&assignment.value, assignment.line)?;
                }
                if assignment.target == AssignmentTarget::Maildir {
                    assignment.value = resolve_relative_path(
                        &assignment.value,
                        maildir.as_deref(),
                        assignment.line,
                    )?;
                    validate_filesystem_path(&assignment.value, assignment.line, "MAILDIR", true)?;
                    maildir = Some(assignment.value.clone());
                } else if matches!(
                    assignment.target,
                    AssignmentTarget::LogFile | AssignmentTarget::LockFile
                ) && !assignment.value.is_empty()
                {
                    assignment.value = resolve_relative_path(
                        &assignment.value,
                        maildir.as_deref(),
                        assignment.line,
                    )?;
                    let description = if assignment.target == AssignmentTarget::LogFile {
                        "LOGFILE"
                    } else {
                        "LOCKFILE"
                    };
                    validate_filesystem_path(
                        &assignment.value,
                        assignment.line,
                        description,
                        false,
                    )?;
                }
                variables.insert(
                    assignment.name.clone(),
                    ExpandedValue {
                        text: assignment.value.clone(),
                        depth: expanded.depth,
                    },
                );
            }
            Statement::Recipe(recipe) => {
                expand_recipe(recipe, &variables, maildir.as_deref())?;
            }
            Statement::Include(expression) | Statement::Switch(expression) => {
                let parsed = parse_expression(&expression.value, expression.line)?;
                validate_runtime_references(
                    &parsed,
                    expression.line,
                    &variables,
                    &BTreeSet::new(),
                )?;
                expression.expansion = Some(parsed);
            }
        }
    }

    Ok(config)
}

fn active_linebuf(lookup: &mut impl FnMut(&str) -> Option<String>) -> usize {
    lookup("LINEBUF")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(super::DEFAULT_LINEBUF)
}

fn evaluate_with_linebuf(
    expression: &ExpansionExpression,
    line: usize,
    hard_limit: usize,
    lookup: &mut impl FnMut(&str) -> Option<String>,
) -> Result<ExpandedValue, ExpansionError> {
    let linebuf = active_linebuf(lookup);
    let limit = linebuf.min(hard_limit);
    evaluate_expression(expression, line, limit, lookup, 0)
        .map_err(|error| relabel_linebuf_error(error, linebuf, hard_limit))
}

fn relabel_linebuf_error(
    mut error: ExpansionError,
    linebuf: usize,
    hard_limit: usize,
) -> ExpansionError {
    if linebuf < hard_limit
        && error.message == format!("expanded value exceeds the hard limit of {linebuf} bytes")
    {
        error.message =
            format!("expanded value exceeds the active LINEBUF limit of {linebuf} bytes");
    }
    error
}

fn parse_linebuf(value: &str, line: usize) -> Result<usize, ExpansionError> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| ExpansionError::new(line, "LINEBUF must be an unsigned decimal integer"))?;
    if !(super::MIN_LINEBUF..=super::MAX_LINEBUF).contains(&parsed) {
        return Err(ExpansionError::new(
            line,
            format!(
                "LINEBUF must be from {} through {} bytes",
                super::MIN_LINEBUF,
                super::MAX_LINEBUF
            ),
        ));
    }
    Ok(parsed)
}

fn expand_recipe(
    recipe: &mut Recipe,
    variables: &BTreeMap<String, ExpandedValue>,
    maildir: Option<&str>,
) -> Result<(), ExpansionError> {
    if let Some(expression) = &mut recipe.lock {
        prepare_lock_expression(
            expression,
            recipe.line,
            variables,
            &BTreeSet::new(),
            maildir,
        )?;
        if expression.source.is_empty() && matches!(recipe.action, RecipeAction::Pipe(_)) {
            return Err(ExpansionError::new(
                recipe.line,
                "an implicit local lockfile requires a filesystem destination",
            ));
        }
    }

    match &mut recipe.action {
        RecipeAction::Deliver(destination) => {
            let (expression, description, allows_trailing_slash) = match destination {
                Destination::Maildir(expression) => (expression, "Maildir destination", true),
                Destination::Mbox(expression) => (expression, "mbox destination", false),
            };
            let parsed = parse_expression(&expression.source, recipe.action_line)?;
            validate_path_references(&parsed, recipe.action_line, variables)?;
            expression.base = maildir.map(str::to_owned);
            expression.line = recipe.action_line;
            let has_runtime_reference = expression_needs_runtime(&parsed, variables);
            expression.runtime_dependent = has_runtime_reference;
            expression.expansion = Some(parsed);
            let expression_line = expression.line;
            if !has_runtime_reference {
                let resolved = destination
                    .resolve_with(|name| variables.get(name).map(|value| value.text.clone()))?;
                validate_filesystem_path(
                    resolved.path(),
                    expression_line,
                    description,
                    allows_trailing_slash,
                )?;
            }
        }
        RecipeAction::Pipe(_) => {}
        RecipeAction::Headers(action) => {
            prepare_header_action(action, variables, &BTreeSet::new())?;
        }
        RecipeAction::Block(statements) => {
            prepare_runtime_statements(statements, variables, &mut BTreeSet::new(), maildir)?;
        }
    }
    Ok(())
}

fn prepare_runtime_statements(
    statements: &mut [Statement],
    known: &BTreeMap<String, ExpandedValue>,
    dynamic: &mut BTreeSet<String>,
    maildir: Option<&str>,
) -> Result<(), ExpansionError> {
    for statement in statements {
        match statement {
            Statement::Assignment(assignment) => {
                if !matches!(
                    assignment.target,
                    AssignmentTarget::User
                        | AssignmentTarget::Maildir
                        | AssignmentTarget::Shell
                        | AssignmentTarget::ShellFlags
                        | AssignmentTarget::Path
                        | AssignmentTarget::ExitCode
                        | AssignmentTarget::Host
                        | AssignmentTarget::LockMethod
                        | AssignmentTarget::LockFile
                        | AssignmentTarget::LockExt
                        | AssignmentTarget::LockTimeout
                        | AssignmentTarget::LineBuf
                        | AssignmentTarget::ProcessTimeout
                        | AssignmentTarget::Umask
                        | AssignmentTarget::Trap
                        | AssignmentTarget::LogAbstract
                ) {
                    return Err(ExpansionError::new(
                        assignment.line,
                        format!(
                            "variable {} cannot be assigned conditionally yet",
                            assignment.name
                        ),
                    ));
                }
                let expression = parse_expression(&assignment.value, assignment.line)?;
                validate_runtime_references(&expression, assignment.line, known, dynamic)?;
                if assignment.target == AssignmentTarget::ProcessTimeout
                    && !expression_needs_runtime(&expression, known)
                    && !expression_references_any(&expression, dynamic)
                {
                    let value = evaluate_config_expression(
                        &expression,
                        assignment.line,
                        assignment_value_limit(assignment.target),
                        known,
                        0,
                    )?;
                    super::parse_process_timeout_seconds(&value.text)
                        .map_err(|message| ExpansionError::new(assignment.line, message))?;
                }
                if assignment.target == AssignmentTarget::Umask
                    && !expression_needs_runtime(&expression, known)
                    && !expression_references_any(&expression, dynamic)
                {
                    let value = evaluate_config_expression(
                        &expression,
                        assignment.line,
                        assignment_value_limit(assignment.target),
                        known,
                        0,
                    )?;
                    super::parse_umask(&value.text)
                        .map_err(|message| ExpansionError::new(assignment.line, message))?;
                }
                if assignment.target == AssignmentTarget::LogAbstract
                    && !expression_needs_runtime(&expression, known)
                    && !expression_references_any(&expression, dynamic)
                {
                    let value = evaluate_config_expression(
                        &expression,
                        assignment.line,
                        assignment_value_limit(assignment.target),
                        known,
                        0,
                    )?;
                    super::validate_log_abstract(&value.text)
                        .map_err(|message| ExpansionError::new(assignment.line, message))?;
                }
                assignment.expansion = Some(expression);

                // A conditional assignment exists only if execution selects
                // this block. Keep its expression for that moment and mark
                // the name as runtime-produced for following statements in
                // the same selected sequence.
                dynamic.insert(assignment.name.clone());
            }
            Statement::Recipe(recipe) => {
                prepare_runtime_recipe(recipe, known, dynamic, maildir)?;
            }
            Statement::Include(expression) | Statement::Switch(expression) => {
                let parsed = parse_expression(&expression.value, expression.line)?;
                validate_runtime_references(&parsed, expression.line, known, dynamic)?;
                expression.expansion = Some(parsed);
            }
        }
    }
    Ok(())
}

fn prepare_runtime_recipe(
    recipe: &mut Recipe,
    known: &BTreeMap<String, ExpandedValue>,
    dynamic: &BTreeSet<String>,
    maildir: Option<&str>,
) -> Result<(), ExpansionError> {
    if let Some(expression) = &mut recipe.lock {
        prepare_lock_expression(expression, recipe.line, known, dynamic, maildir)?;
        if expression.source.is_empty() && matches!(recipe.action, RecipeAction::Pipe(_)) {
            return Err(ExpansionError::new(
                recipe.line,
                "an implicit local lockfile requires a filesystem destination",
            ));
        }
    }

    match &mut recipe.action {
        RecipeAction::Deliver(destination) => {
            let expression = match destination {
                Destination::Maildir(expression) | Destination::Mbox(expression) => expression,
            };
            let parsed = parse_expression(&expression.source, recipe.action_line)?;
            validate_runtime_references(&parsed, recipe.action_line, known, dynamic)?;
            expression.base = maildir.map(str::to_owned);
            expression.line = recipe.action_line;
            expression.runtime_dependent = true;
            expression.runtime_base = true;
            expression.expansion = Some(parsed);
        }
        RecipeAction::Pipe(_) => {}
        RecipeAction::Headers(action) => {
            prepare_header_action(action, known, dynamic)?;
        }
        RecipeAction::Block(children) => {
            let mut child_dynamic = dynamic.clone();
            prepare_runtime_statements(children, known, &mut child_dynamic, maildir)?;
        }
    }
    Ok(())
}

fn prepare_header_action(
    action: &mut HeaderAction,
    known: &BTreeMap<String, ExpandedValue>,
    dynamic: &BTreeSet<String>,
) -> Result<(), ExpansionError> {
    for operation in &mut action.operations {
        let (line, value) = match operation {
            HeaderOperation::Remove { .. } => continue,
            HeaderOperation::Set { line, value, .. }
            | HeaderOperation::Add { line, value, .. }
            | HeaderOperation::Prepend { line, value, .. } => (*line, value),
        };
        let expression = parse_expression(&value.source, line)?;
        validate_runtime_references(&expression, line, known, dynamic)?;
        let needs_runtime = expression_needs_runtime(&expression, known)
            || expression_references_any(&expression, dynamic);

        // Resolve expressions whose inputs are already fixed so malformed or
        // oversized generated values fail during configuration preparation.
        // Runtime-produced names remain structured for the selected recipe.
        if !needs_runtime {
            let expanded =
                evaluate_with_linebuf(&expression, line, MAX_ASSIGNMENT_VALUE_LEN, &mut |name| {
                    known.get(name).map(|item| item.text.clone())
                })?
                .text;
            validate_header_value(&expanded, line)?;
        }
        value.expansion = Some(expression);
    }
    Ok(())
}

fn validate_header_value(value: &str, line: usize) -> Result<(), ExpansionError> {
    if value.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n')) {
        return Err(ExpansionError::new(
            line,
            "expanded header value contains NUL, CR, or LF",
        ));
    }
    Ok(())
}

fn prepare_lock_expression(
    expression: &mut PathExpression,
    line: usize,
    known: &BTreeMap<String, ExpandedValue>,
    dynamic: &BTreeSet<String>,
    maildir: Option<&str>,
) -> Result<(), ExpansionError> {
    let parsed = parse_expression(&expression.source, line)?;
    validate_runtime_references(&parsed, line, known, dynamic)?;
    expression.base = maildir.map(str::to_owned);
    expression.line = line;
    expression.runtime_dependent =
        expression_needs_runtime(&parsed, known) || expression_references_any(&parsed, dynamic);
    expression.runtime_base = expression.runtime_dependent;
    expression.expansion = Some(parsed);
    if !expression.runtime_dependent && !expression.source.is_empty() {
        expression.resolve_with(|name| known.get(name).map(|value| value.text.clone()))?;
    }
    Ok(())
}

fn validate_runtime_references(
    expression: &ExpansionExpression,
    line: usize,
    known: &BTreeMap<String, ExpandedValue>,
    dynamic: &BTreeSet<String>,
) -> Result<(), ExpansionError> {
    for part in &expression.parts {
        let ExpansionPart::Variable { name, default } = part else {
            continue;
        };
        if known.contains_key(name)
            || dynamic.contains(name)
            || variable_policy(name) == VariablePolicy::RuntimeOnly
        {
            continue;
        }
        if let Some(default) = default {
            validate_runtime_references(default, line, known, dynamic)?;
        } else {
            return Err(ExpansionError::new(
                line,
                format!("variable {name} is not defined"),
            ));
        }
    }
    Ok(())
}

fn validate_filesystem_path(
    path: &str,
    line: usize,
    description: &str,
    allows_trailing_slash: bool,
) -> Result<(), ExpansionError> {
    if path.is_empty() {
        return Err(ExpansionError::new(
            line,
            format!("{description} path is empty"),
        ));
    }
    if path.as_bytes().contains(&0) {
        return Err(ExpansionError::new(
            line,
            format!("{description} path contains NUL"),
        ));
    }
    if path.ends_with('/') && !allows_trailing_slash {
        return Err(ExpansionError::new(
            line,
            format!("{description} path must not end with '/'"),
        ));
    }

    // Inspect the original spelling rather than Path::components(), which
    // normalizes repeated separators and '.' before policy checks can reject
    // ambiguous aliases. A single leading root and an allowed trailing
    // Maildir marker are syntax, not empty path components.
    let mut components = path;
    if let Some(relative) = components.strip_prefix('/') {
        components = relative;
    }
    if allows_trailing_slash && let Some(without_marker) = components.strip_suffix('/') {
        components = without_marker;
    }
    if components.is_empty() {
        return Err(ExpansionError::new(
            line,
            format!("{description} path does not name a filesystem entry"),
        ));
    }
    for component in components.split('/') {
        let message = match component {
            "" => Some("contains an empty component"),
            "." => Some("must not contain '.'"),
            ".." => Some("must not contain '..'"),
            _ => None,
        };
        if let Some(message) = message {
            return Err(ExpansionError::new(
                line,
                format!("{description} path {message}"),
            ));
        }
    }
    Ok(())
}

fn resolve_relative_path(
    path: &str,
    base: Option<&str>,
    line: usize,
) -> Result<String, ExpansionError> {
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return Ok(path.to_owned());
    };
    if path.is_empty() || Path::new(path).is_absolute() {
        return Ok(path.to_owned());
    }

    // Join through the same bounded builder used for expansion. PathBuf::join
    // would allocate the complete result before we could reject an oversized
    // base and relative path supplied by the rc file.
    let mut output = Vec::with_capacity(MAX_PATH_EXPRESSION_LEN.min(base.len()));
    push_bounded(&mut output, base.as_bytes(), MAX_PATH_EXPRESSION_LEN, line)?;
    if !base.ends_with('/') {
        push_bounded(&mut output, b"/", MAX_PATH_EXPRESSION_LEN, line)?;
    }
    push_bounded(&mut output, path.as_bytes(), MAX_PATH_EXPRESSION_LEN, line)?;
    String::from_utf8(output)
        .map_err(|_| ExpansionError::new(line, "resolved path is not valid UTF-8"))
}

fn expand_text(
    input: &str,
    line: usize,
    limit: usize,
    variables: &BTreeMap<String, ExpandedValue>,
) -> Result<ExpandedValue, ExpansionError> {
    let expression = parse_expression(input, line)?;
    evaluate_config_expression(&expression, line, limit, variables, 0)
}

fn evaluate_config_expression(
    expression: &ExpansionExpression,
    line: usize,
    limit: usize,
    variables: &BTreeMap<String, ExpandedValue>,
    nesting: usize,
) -> Result<ExpandedValue, ExpansionError> {
    check_expansion_depth(nesting, line)?;
    let mut output = Vec::new();
    let mut depth = 0usize;
    for part in &expression.parts {
        match part {
            ExpansionPart::Literal(text) => {
                push_bounded(&mut output, text.as_bytes(), limit, line)?
            }
            ExpansionPart::Variable { name, default } => {
                let selected = variables.get(name).filter(|value| !value.text.is_empty());
                let value = if let Some(value) = selected {
                    value.clone()
                } else if let Some(default) = default {
                    evaluate_config_expression(default, line, limit, variables, nesting + 1)?
                } else if let Some(value) = variables.get(name) {
                    value.clone()
                } else {
                    return Err(match variable_policy(name) {
                        VariablePolicy::RuntimeOnly => ExpansionError::new(
                            line,
                            format!("runtime variable {name} is not available in this context"),
                        ),
                        _ => ExpansionError::new(line, format!("variable {name} is not defined")),
                    });
                };
                let candidate_depth = value.depth.checked_add(1).ok_or_else(|| {
                    ExpansionError::new(line, "variable expansion depth overflows")
                })?;
                check_expansion_depth(candidate_depth, line)?;
                depth = depth.max(candidate_depth);
                push_bounded(&mut output, value.text.as_bytes(), limit, line)?;
            }
        }
    }
    let text = String::from_utf8(output)
        .map_err(|_| ExpansionError::new(line, "expanded value is not valid UTF-8"))?;
    Ok(ExpandedValue { text, depth })
}

fn validate_path_references(
    expression: &ExpansionExpression,
    line: usize,
    variables: &BTreeMap<String, ExpandedValue>,
) -> Result<(), ExpansionError> {
    for part in &expression.parts {
        if let ExpansionPart::Variable { name, default } = part {
            let present = variables
                .get(name)
                .is_some_and(|value| !value.text.is_empty());
            if present || variable_policy(name) == VariablePolicy::RuntimeOnly {
                continue;
            }
            if let Some(default) = default {
                validate_path_references(default, line, variables)?;
            } else if !variables.contains_key(name) {
                return Err(ExpansionError::new(
                    line,
                    format!("variable {name} is not defined"),
                ));
            }
        }
    }
    Ok(())
}

fn expression_needs_runtime(
    expression: &ExpansionExpression,
    variables: &BTreeMap<String, ExpandedValue>,
) -> bool {
    expression.parts.iter().any(|part| match part {
        ExpansionPart::Literal(_) => false,
        ExpansionPart::Variable { name, default } => {
            if variable_policy(name) == VariablePolicy::RuntimeOnly {
                true
            } else if variables
                .get(name)
                .is_some_and(|value| !value.text.is_empty())
            {
                false
            } else {
                default
                    .as_ref()
                    .is_some_and(|value| expression_needs_runtime(value, variables))
            }
        }
    })
}

fn expression_references_any(expression: &ExpansionExpression, names: &BTreeSet<String>) -> bool {
    expression.parts.iter().any(|part| match part {
        ExpansionPart::Literal(_) => false,
        ExpansionPart::Variable { name, default } => {
            names.contains(name)
                || default
                    .as_ref()
                    .is_some_and(|value| expression_references_any(value, names))
        }
    })
}

fn bind_static_expression(
    expression: &ExpansionExpression,
    line: usize,
    lookup: &mut impl FnMut(&str) -> Option<String>,
    nesting: usize,
) -> Result<ExpansionExpression, ExpansionError> {
    // Bind ordinary values without flattening the whole expression. Runtime
    // references and their defaults must remain structured so a value
    // produced by an earlier delivery can choose the branch later.
    check_expansion_depth(nesting, line)?;
    let mut parts = Vec::new();
    for part in &expression.parts {
        match part {
            ExpansionPart::Literal(text) => push_literal_part(&mut parts, text),
            ExpansionPart::Variable { name, default }
                if variable_policy(name) == VariablePolicy::RuntimeOnly =>
            {
                let default = default
                    .as_ref()
                    .map(|value| bind_static_expression(value, line, lookup, nesting + 1))
                    .transpose()?;
                parts.push(ExpansionPart::Variable {
                    name: name.clone(),
                    default,
                });
            }
            ExpansionPart::Variable { name, default } => match (lookup(name), default) {
                (Some(value), _) if !value.is_empty() => push_literal_part(&mut parts, &value),
                (_, Some(default)) => {
                    let bound = bind_static_expression(default, line, lookup, nesting + 1)?;
                    for part in bound.parts {
                        match part {
                            ExpansionPart::Literal(text) => push_literal_part(&mut parts, &text),
                            other => parts.push(other),
                        }
                    }
                }
                (Some(_), None) => {}
                (None, None) => {
                    return Err(ExpansionError::new(
                        line,
                        format!("variable {name} is not set"),
                    ));
                }
            },
        }
    }
    Ok(ExpansionExpression { parts })
}

fn evaluate_expression(
    expression: &ExpansionExpression,
    line: usize,
    limit: usize,
    lookup: &mut impl FnMut(&str) -> Option<String>,
    nesting: usize,
) -> Result<ExpandedValue, ExpansionError> {
    // Append every selected part through the bounded writer. Evaluating into
    // an unrestricted temporary string first would let a hostile variable
    // exceed the path limit before the caller could reject it.
    check_expansion_depth(nesting, line)?;
    let mut output = Vec::new();
    let mut depth = nesting;
    for part in &expression.parts {
        match part {
            ExpansionPart::Literal(text) => {
                push_bounded(&mut output, text.as_bytes(), limit, line)?
            }
            ExpansionPart::Variable { name, default } => match (lookup(name), default) {
                (Some(value), _) if !value.is_empty() => {
                    push_bounded(&mut output, value.as_bytes(), limit, line)?;
                    depth = depth.max(nesting + 1);
                }
                (_, Some(default)) => {
                    let value = evaluate_expression(default, line, limit, lookup, nesting + 1)?;
                    push_bounded(&mut output, value.text.as_bytes(), limit, line)?;
                    depth = depth.max(value.depth);
                }
                (Some(_), None) => {}
                (None, None) => {
                    return Err(ExpansionError::new(
                        line,
                        format!("runtime variable {name} is not set"),
                    ));
                }
            },
        }
    }
    let text = String::from_utf8(output)
        .map_err(|_| ExpansionError::new(line, "expanded value is not valid UTF-8"))?;
    Ok(ExpandedValue { text, depth })
}

fn expression_has_runtime(expression: &ExpansionExpression) -> bool {
    expression.parts.iter().any(|part| match part {
        ExpansionPart::Literal(_) => false,
        ExpansionPart::Variable { name, default } => {
            variable_policy(name) == VariablePolicy::RuntimeOnly
                || default.as_ref().is_some_and(expression_has_runtime)
        }
    })
}

fn push_literal_part(parts: &mut Vec<ExpansionPart>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(ExpansionPart::Literal(previous)) = parts.last_mut() {
        previous.push_str(text);
    } else {
        parts.push(ExpansionPart::Literal(text.to_owned()));
    }
}

fn check_expansion_depth(depth: usize, line: usize) -> Result<(), ExpansionError> {
    if depth > MAX_EXPANSION_DEPTH {
        return Err(ExpansionError::new(
            line,
            format!("variable expansion exceeds the hard depth limit of {MAX_EXPANSION_DEPTH}"),
        ));
    }
    Ok(())
}

fn parse_expression(input: &str, line: usize) -> Result<ExpansionExpression, ExpansionError> {
    let mut index = 0;
    let expression = parse_expression_until(input, &mut index, line, 0, false)?;
    debug_assert_eq!(index, input.len());
    Ok(expression)
}

fn parse_expression_until(
    input: &str,
    index: &mut usize,
    line: usize,
    nesting: usize,
    stop_at_brace: bool,
) -> Result<ExpansionExpression, ExpansionError> {
    // Build owned parts once so later delivery phases never reinterpret
    // bytes obtained from a variable as expression syntax. The explicit
    // depth check also stops hostile nested defaults before recursion grows.
    check_expansion_depth(nesting, line)?;
    let bytes = input.as_bytes();
    let mut parts = Vec::new();
    let mut literal_start = *index;
    while *index < bytes.len() {
        if stop_at_brace && bytes[*index] == b'}' {
            break;
        }
        if bytes[*index] != b'$' {
            *index += 1;
            continue;
        }
        if literal_start < *index {
            parts.push(ExpansionPart::Literal(
                input[literal_start..*index].to_owned(),
            ));
        }
        *index += 1;
        let Some(first) = bytes.get(*index).copied() else {
            return Err(ExpansionError::new(
                line,
                "'$' must be followed by NAME or {NAME}",
            ));
        };
        let (name, default) = if first == b'{' {
            *index += 1;
            let name_start = *index;
            while *index < bytes.len() && is_name_continue(bytes[*index]) {
                *index += 1;
            }
            let name = &input[name_start..*index];
            validate_reference_name(name, line)?;
            match bytes.get(*index..*index + 2) {
                Some(b":-") => {
                    *index += 2;
                    let default = parse_expression_until(input, index, line, nesting + 1, true)?;
                    if bytes.get(*index) != Some(&b'}') {
                        return Err(ExpansionError::new(
                            line,
                            "variable reference is missing '}'",
                        ));
                    }
                    *index += 1;
                    (name.to_owned(), Some(default))
                }
                _ if bytes.get(*index) == Some(&b'}') => {
                    *index += 1;
                    (name.to_owned(), None)
                }
                _ => {
                    return Err(ExpansionError::new(
                        line,
                        "unsupported parameter expansion; use ${NAME} or ${NAME:-expression}",
                    ));
                }
            }
        } else {
            if !is_name_start(first) {
                return Err(ExpansionError::new(
                    line,
                    "unsupported '$' expansion; use $NAME or ${NAME}",
                ));
            }
            let name_start = *index;
            *index += 1;
            while *index < bytes.len() && is_name_continue(bytes[*index]) {
                *index += 1;
            }
            (input[name_start..*index].to_owned(), None)
        };
        if variable_policy(&name) == VariablePolicy::Unsupported {
            return Err(ExpansionError::new(
                line,
                format!("procmail variable {name} is not supported"),
            ));
        }
        parts.push(ExpansionPart::Variable { name, default });
        literal_start = *index;
    }
    if literal_start < *index {
        parts.push(ExpansionPart::Literal(
            input[literal_start..*index].to_owned(),
        ));
    }
    if stop_at_brace && *index == bytes.len() {
        return Err(ExpansionError::new(
            line,
            "variable reference is missing '}'",
        ));
    }
    Ok(ExpansionExpression { parts })
}

fn validate_reference_name(name: &str, line: usize) -> Result<(), ExpansionError> {
    let mut bytes = name.bytes();
    let valid = bytes.next().is_some_and(is_name_start) && bytes.all(is_name_continue);
    if !valid {
        return Err(ExpansionError::new(
            line,
            "variable reference contains an invalid name",
        ));
    }
    Ok(())
}

fn is_name_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_name_continue(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn push_bounded(
    output: &mut Vec<u8>,
    value: &[u8],
    limit: usize,
    line: usize,
) -> Result<(), ExpansionError> {
    let new_len = output
        .len()
        .checked_add(value.len())
        .ok_or_else(|| ExpansionError::new(line, "expanded value length overflows"))?;
    if new_len > limit {
        return Err(ExpansionError::new(
            line,
            format!("expanded value exceeds the hard limit of {limit} bytes"),
        ));
    }
    output.extend_from_slice(value);
    Ok(())
}

#[cfg(test)]
mod tests;
