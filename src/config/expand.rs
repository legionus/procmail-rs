// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use super::{
    Assignment, AssignmentTarget, Config, Destination, ExpansionExpression, ExpansionPart,
    MAX_ASSIGNMENT_VALUE_LEN, MAX_EXPANSION_DEPTH, MAX_PATH_EXPRESSION_LEN, PathExpression,
    RcFileExpression, Recipe, RecipeAction, Statement, SuppliedVariable, VariablePolicy,
    VariableSource, assignment_value_limit, variable_policy,
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
        let Some(expression) = self.expansion.as_ref() else {
            return Ok(self.value.clone());
        };
        let limit = assignment_value_limit(self.target);
        let value = evaluate_expression(expression, self.line, limit, &mut lookup, 0)?.text;
        if self.target != AssignmentTarget::Maildir {
            return Ok(value);
        }
        let base = lookup("MAILDIR");
        let value = resolve_relative_path(&value, base.as_deref(), self.line)?;
        validate_filesystem_path(&value, self.line, "MAILDIR", true)?;
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
        let value = evaluate_expression(
            expression,
            self.line,
            MAX_PATH_EXPRESSION_LEN,
            &mut lookup,
            0,
        )?
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
        let source = evaluate_expression(
            compiled,
            expression.line,
            MAX_PATH_EXPRESSION_LEN,
            &mut lookup,
            0,
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

pub(super) fn expand(
    config: Config,
    supplied: &[SuppliedVariable],
) -> Result<Config, ExpansionError> {
    let mut variables = BTreeMap::<String, ExpandedValue>::new();
    let mut initial_variables = Vec::with_capacity(supplied.len());
    for variable in supplied {
        let value = if variable.source() == VariableSource::Environment {
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
    config.initial_variables = initial_variables;

    for statement in &mut config.statements {
        match statement {
            Statement::Assignment(assignment) => {
                let limit = assignment_value_limit(assignment.target);
                let expanded = expand_text(&assignment.value, assignment.line, limit, &variables)?;
                assignment.value = expanded.text;
                if assignment.target == AssignmentTarget::Maildir {
                    assignment.value = resolve_relative_path(
                        &assignment.value,
                        maildir.as_deref(),
                        assignment.line,
                    )?;
                    validate_filesystem_path(&assignment.value, assignment.line, "MAILDIR", true)?;
                    maildir = Some(assignment.value.clone());
                } else if assignment.target == AssignmentTarget::LogFile
                    && !assignment.value.is_empty()
                {
                    assignment.value = resolve_relative_path(
                        &assignment.value,
                        maildir.as_deref(),
                        assignment.line,
                    )?;
                    validate_filesystem_path(&assignment.value, assignment.line, "LOGFILE", false)?;
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

fn expand_recipe(
    recipe: &mut Recipe,
    variables: &BTreeMap<String, ExpandedValue>,
    maildir: Option<&str>,
) -> Result<(), ExpansionError> {
    if let Some(lock) = &mut recipe.lock {
        *lock = expand_text(lock, recipe.line, MAX_PATH_EXPRESSION_LEN, variables)?.text;
        if !lock.is_empty() {
            *lock = resolve_relative_path(lock, maildir, recipe.line)?;
            validate_filesystem_path(lock, recipe.line, "lockfile", false)?;
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
    if let Some(lock) = &recipe.lock {
        let expression = parse_expression(lock, recipe.line)?;
        validate_runtime_references(&expression, recipe.line, known, dynamic)?;
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
        RecipeAction::Block(children) => {
            let mut child_dynamic = dynamic.clone();
            prepare_runtime_statements(children, known, &mut child_dynamic, maildir)?;
        }
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
mod tests {
    use super::*;
    use crate::config::{ConditionKind, MAX_SHELL_SETTING_LEN, parse};

    fn resolved_destination(config: &Config, statement_index: usize) -> Destination {
        let mut variables = config
            .initial_variables()
            .iter()
            .map(|(name, value, _)| (name.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        for (index, statement) in config.statements[..=statement_index].iter().enumerate() {
            match statement {
                Statement::Assignment(assignment) => {
                    variables.insert(assignment.name.clone(), assignment.value.clone());
                }
                Statement::Recipe(recipe) if index == statement_index => {
                    let RecipeAction::Deliver(destination) = &recipe.action else {
                        panic!("expected delivery recipe");
                    };
                    return destination
                        .resolve_with(|name| variables.get(name).cloned())
                        .unwrap();
                }
                Statement::Recipe(_) => {}
                Statement::Include(_) | Statement::Switch(_) => {}
            }
        }
        panic!("statement is not a recipe");
    }

    #[test]
    fn expands_both_variable_reference_forms_sequentially() {
        let config = parse(
            "ROOT=mail\nBOX=${ROOT}/inbox\nMAILDIR=/srv/$ROOT\n:0 :lock-$BOX\nmaildir:$BOX\n",
        )
        .unwrap()
        .expand()
        .unwrap();

        let Statement::Assignment(box_assignment) = &config.statements[1] else {
            panic!("expected assignment");
        };
        assert_eq!(box_assignment.value, "mail/inbox");
        assert_eq!(config.maildir(), Some("/srv/mail"));
        let Statement::Recipe(recipe) = &config.statements[3] else {
            panic!("expected recipe");
        };
        assert_eq!(recipe.lock.as_deref(), Some("/srv/mail/lock-mail/inbox"));
        assert_eq!(
            resolved_destination(&config, 3),
            Destination::Maildir("/srv/mail/mail/inbox".into())
        );
    }

    #[test]
    fn expands_supplied_variables_before_rc_assignments() {
        let supplied = [
            SuppliedVariable::parse("ROOT=old".into()).unwrap(),
            SuppliedVariable::parse("ROOT=cli".into()).unwrap(),
            SuppliedVariable::parse("BOX=$ROOT".into()).unwrap(),
        ];
        let config = parse("FIRST=$BOX\nBOX=rc\nSECOND=$BOX\n:0\nmaildir:$FIRST-$SECOND\n")
            .unwrap()
            .expand_with(&supplied)
            .unwrap();

        let Statement::Recipe(_) = &config.statements[3] else {
            panic!("expected recipe");
        };
        assert_eq!(
            resolved_destination(&config, 3),
            Destination::Maildir("cli-rc".into())
        );
    }

    #[test]
    fn inserts_passwd_values_without_rescanning_their_text() {
        let supplied = [
            SuppliedVariable::from_environment("HOME", "/home/$literal".into()).unwrap(),
            SuppliedVariable::from_environment("LOGNAME", "user".into()).unwrap(),
        ];
        let config = parse("VALUE=$HOME\n")
            .unwrap()
            .expand_with(&supplied)
            .unwrap();
        let Statement::Assignment(assignment) = &config.statements[0] else {
            panic!("expected assignment");
        };

        assert_eq!(assignment.value, "/home/$literal");
    }

    #[test]
    fn rejects_self_references_and_cycles_without_recursive_scanning() {
        for source in ["A=$A\n", "A=$B\nB=$A\n"] {
            let error = parse(source).unwrap().expand().unwrap_err();
            assert_eq!(error.line, 1);
            assert!(error.message.contains("is not defined"));
        }

        let supplied = [SuppliedVariable::parse("A=$A".into()).unwrap()];
        let error = parse("").unwrap().expand_with(&supplied).unwrap_err();
        assert_eq!(error.line, 0);
        assert_eq!(error.to_string(), "command line: variable A is not defined");
    }

    #[test]
    fn enforces_expansion_depth_at_the_boundary() {
        let mut source = String::from("V0=value\n");
        for depth in 1..=MAX_EXPANSION_DEPTH {
            source.push_str(&format!("V{depth}=$V{}\n", depth - 1));
        }
        assert!(parse(&source).unwrap().expand().is_ok());

        source.push_str(&format!(
            "V{}=$V{}\n",
            MAX_EXPANSION_DEPTH + 1,
            MAX_EXPANSION_DEPTH
        ));
        let error = parse(&source).unwrap().expand().unwrap_err();
        assert_eq!(error.line, MAX_EXPANSION_DEPTH + 2);
        assert_eq!(
            error.message,
            format!("variable expansion exceeds the hard depth limit of {MAX_EXPANSION_DEPTH}")
        );
    }

    #[test]
    fn resolves_paths_against_maildir_active_at_each_recipe() {
        let config = parse("MAILDIR=/srv/first\n:0 c\none/\nMAILDIR=second\n:0\nmaildir:two\n")
            .unwrap()
            .expand()
            .unwrap();

        let Statement::Recipe(_) = &config.statements[1] else {
            panic!("expected first recipe");
        };
        let Statement::Recipe(_) = &config.statements[3] else {
            panic!("expected second recipe");
        };
        assert_eq!(
            resolved_destination(&config, 1),
            Destination::Maildir("/srv/first/one/".into())
        );
        assert_eq!(
            resolved_destination(&config, 3),
            Destination::Maildir("/srv/first/second/two".into())
        );
        assert_eq!(config.maildir(), Some("/srv/first/second"));
    }

    #[test]
    fn rejects_undefined_forward_references() {
        let error = parse("A=$B\nB=value\n").unwrap().expand().unwrap_err();
        assert_eq!(error.message, "variable B is not defined");
    }

    #[test]
    fn resolves_runtime_path_only_when_it_is_used() {
        let config = parse("MAILDIR=/mail\n:0\nmaildir:${LASTFOLDER}-related\n")
            .unwrap()
            .expand()
            .unwrap();
        let Statement::Recipe(recipe) = &config.statements[1] else {
            panic!("expected recipe");
        };
        let RecipeAction::Deliver(destination) = &recipe.action else {
            panic!("expected delivery recipe");
        };
        assert!(destination.needs_runtime_variables());

        let error = destination.resolve_with(|_| None).unwrap_err();
        assert_eq!(error.message, "runtime variable LASTFOLDER is not set");
        let resolved = destination
            .resolve_with(|name| (name == "LASTFOLDER").then(|| "archive/item".to_owned()))
            .unwrap();
        assert_eq!(resolved.path(), "/mail/archive/item-related");
    }

    #[test]
    fn expands_destinations_inside_recipe_blocks() {
        let config = parse("MAILDIR=/mail\nBOX=lists\n:0\n{\n:0\nmaildir:$BOX/inbox\n}\n")
            .unwrap()
            .expand()
            .unwrap();
        let Statement::Recipe(parent) = &config.statements[2] else {
            panic!("expected parent recipe");
        };
        let RecipeAction::Block(children) = &parent.action else {
            panic!("expected block action");
        };
        let Statement::Recipe(child) = &children[0] else {
            panic!("expected child recipe");
        };
        let RecipeAction::Deliver(destination) = &child.action else {
            panic!("expected delivery action");
        };

        let resolved = destination
            .resolve_with(|name| (name == "BOX").then(|| "lists".to_owned()))
            .unwrap();
        assert_eq!(resolved.path(), "/mail/lists/inbox");
    }

    #[test]
    fn rejects_unsupported_and_malformed_references() {
        for source in ["A=$$\n", "A=${NAME:=value}\n", "A=${NAME\n", "A=$\n"] {
            assert!(parse(source).unwrap().expand().is_err(), "{source:?}");
        }
    }

    #[test]
    fn follows_shell_like_name_boundaries() {
        let config = parse("NAME=mail\nNAMEsuffix=archive\nA=$NAMEsuffix\nB=${NAME}suffix\n")
            .unwrap()
            .expand()
            .unwrap();

        let Statement::Assignment(a) = &config.statements[2] else {
            panic!("expected assignment");
        };
        let Statement::Assignment(b) = &config.statements[3] else {
            panic!("expected assignment");
        };
        assert_eq!(a.value, "archive");
        assert_eq!(b.value, "mailsuffix");
    }

    #[test]
    fn expands_shell_like_defaults_lazily() {
        let config = parse(
            "EMPTY=\nROOT=/mail\nA=${MISSING:-$ROOT/inbox}\nB=${EMPTY:-${MISSING:-fallback}}\nC=${ROOT:-$UNDEFINED}\n",
        )
        .unwrap()
        .expand()
        .unwrap();

        for (index, expected) in [(2, "/mail/inbox"), (3, "fallback"), (4, "/mail")] {
            let Statement::Assignment(assignment) = &config.statements[index] else {
                panic!("expected assignment");
            };
            assert_eq!(assignment.value, expected);
        }
    }

    #[test]
    fn bounds_nested_default_syntax_depth() {
        let mut within_limit = String::new();
        for index in 0..MAX_EXPANSION_DEPTH {
            within_limit.push_str(&format!("${{MISSING{index}:-"));
        }
        within_limit.push_str("value");
        within_limit.push_str(&"}".repeat(MAX_EXPANSION_DEPTH));
        let source = format!("A={within_limit}\n");
        assert!(parse(&source).unwrap().expand().is_ok());

        let beyond_limit = format!("${{OUTER:-{within_limit}}}");
        let source = format!("A={beyond_limit}\n");
        let error = parse(&source).unwrap().expand().unwrap_err();
        assert_eq!(
            error.message,
            format!("variable expansion exceeds the hard depth limit of {MAX_EXPANSION_DEPTH}")
        );
    }

    #[test]
    fn resolves_runtime_defaults_without_rescanning_values() {
        let config = parse("MAILDIR=/mail\n:0\nmaildir:${LASTFOLDER:-$MAILDIR}/next\n")
            .unwrap()
            .expand()
            .unwrap();
        let Statement::Recipe(recipe) = &config.statements[1] else {
            panic!("expected recipe");
        };
        let RecipeAction::Deliver(destination) = &recipe.action else {
            panic!("expected delivery recipe");
        };
        let bound = destination
            .bind_with(|name| (name == "MAILDIR").then(|| "/mail".to_owned()))
            .unwrap();

        let fallback = bound.resolve_with(|_| Some(String::new())).unwrap();
        assert_eq!(fallback.path(), "/mail/next");
        let literal = bound
            .resolve_with(|_| Some("archive/$MAILDIR".to_owned()))
            .unwrap();
        assert_eq!(literal.path(), "/mail/archive/$MAILDIR/next");
    }

    #[test]
    fn bounds_expanded_paths_before_allocation_growth() {
        let source = format!(
            "A={}\nB={}\n:0\nmaildir:$A$B\n",
            "a".repeat(MAX_PATH_EXPRESSION_LEN),
            "b"
        );
        let error = parse(&source).unwrap().expand().unwrap_err();

        assert_eq!(error.line, 4);
        assert_eq!(
            error.message,
            format!("expanded value exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes")
        );
    }

    #[test]
    fn bounds_expanded_assignment_values_at_the_boundary() {
        let prefix = "a".repeat(MAX_ASSIGNMENT_VALUE_LEN / 2);
        for length in [
            MAX_ASSIGNMENT_VALUE_LEN - 1,
            MAX_ASSIGNMENT_VALUE_LEN,
            MAX_ASSIGNMENT_VALUE_LEN + 1,
        ] {
            let suffix = "b".repeat(length - prefix.len());
            let source = format!("PREFIX={prefix}\nVALUE=${{PREFIX}}{suffix}\n");
            let result = parse(&source).unwrap().expand();

            if length <= MAX_ASSIGNMENT_VALUE_LEN {
                let config = result.unwrap();
                let Statement::Assignment(value) = &config.statements[1] else {
                    panic!("expected assignment");
                };
                assert_eq!(value.value.len(), length);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 2);
                assert_eq!(
                    error.message,
                    format!(
                        "expanded value exceeds the hard limit of {MAX_ASSIGNMENT_VALUE_LEN} bytes"
                    )
                );
            }
        }
    }

    #[test]
    fn bounds_expanded_shell_settings() {
        let prefix = "x".repeat(MAX_SHELL_SETTING_LEN / 2 + 1);
        let source = format!("PREFIX={prefix}\nSHELL=$PREFIX$PREFIX\n");
        let error = parse(&source).unwrap().expand().unwrap_err();

        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            format!("expanded value exceeds the hard limit of {MAX_SHELL_SETTING_LEN} bytes")
        );
    }

    #[test]
    fn bounds_expanded_destination_and_lock_paths_at_the_boundary() {
        let prefix = "a".repeat(MAX_PATH_EXPRESSION_LEN / 2);
        for length in [
            MAX_PATH_EXPRESSION_LEN - 1,
            MAX_PATH_EXPRESSION_LEN,
            MAX_PATH_EXPRESSION_LEN + 1,
        ] {
            let suffix = "b".repeat(length - prefix.len());
            for source in [
                format!("PREFIX={prefix}\n:0\nmaildir:${{PREFIX}}{suffix}\n"),
                format!("PREFIX={prefix}\n:0 :${{PREFIX}}{suffix}\nmaildir:target\n"),
            ] {
                let result = parse(&source).unwrap().expand();
                if length <= MAX_PATH_EXPRESSION_LEN {
                    let config = result.unwrap();
                    let Statement::Recipe(recipe) = &config.statements[1] else {
                        panic!("expected recipe");
                    };
                    let resolved = resolved_destination(&config, 1);
                    let actual = recipe.lock.as_deref().unwrap_or_else(|| resolved.path());
                    assert_eq!(actual.len(), length);
                } else {
                    let error = result.unwrap_err();
                    assert!(matches!(error.line, 2 | 3));
                    assert_eq!(
                        error.message,
                        format!(
                            "expanded value exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes"
                        )
                    );
                }
            }
        }
    }

    #[test]
    fn bounds_maildir_path_join_before_allocation_growth() {
        let source = format!(
            "MAILDIR=/{}\n:0\nmaildir:child\n",
            "a".repeat(MAX_PATH_EXPRESSION_LEN - 1)
        );
        let error = parse(&source).unwrap().expand().unwrap_err();

        assert_eq!(error.line, 3);
        assert_eq!(
            error.message,
            format!("expanded value exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes")
        );
    }

    #[test]
    fn bounds_maildir_path_join_at_the_boundary() {
        for length in [
            MAX_PATH_EXPRESSION_LEN - 1,
            MAX_PATH_EXPRESSION_LEN,
            MAX_PATH_EXPRESSION_LEN + 1,
        ] {
            let base_len = length - 2;
            let source = format!("MAILDIR=/{}\n:0\nmaildir:x\n", "a".repeat(base_len - 1));
            let result = parse(&source).unwrap().expand();

            if length <= MAX_PATH_EXPRESSION_LEN {
                let config = result.unwrap();
                let Statement::Recipe(_) = &config.statements[1] else {
                    panic!("expected recipe");
                };
                let resolved = resolved_destination(&config, 1);
                let Destination::Maildir(path) = &resolved else {
                    panic!("expected Maildir destination");
                };
                assert_eq!(path.source().len(), length);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 3);
                assert_eq!(
                    error.message,
                    format!(
                        "expanded value exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes"
                    )
                );
            }
        }
    }

    #[test]
    fn accepts_only_unambiguous_filesystem_path_components() {
        for (path, allows_trailing_slash) in [
            ("relative/mail", false),
            ("/absolute/mail", false),
            ("relative/mail/", true),
            ("/absolute/mail/", true),
        ] {
            validate_filesystem_path(path, 1, "test", allows_trailing_slash).unwrap();
        }

        for (path, allows_trailing_slash, expected) in [
            ("", true, "path is empty"),
            ("/", true, "does not name a filesystem entry"),
            ("a//b", true, "contains an empty component"),
            ("//a", true, "contains an empty component"),
            ("a/./b", true, "must not contain '.'"),
            ("a/../b", true, "must not contain '..'"),
            ("a/", false, "must not end with '/'"),
            ("a\0b", true, "contains NUL"),
        ] {
            let error =
                validate_filesystem_path(path, 7, "test", allows_trailing_slash).unwrap_err();
            assert_eq!(error.line, 7);
            assert!(error.message.contains(expected), "{path:?}: {error}");
        }
    }

    #[test]
    fn validates_paths_after_variable_expansion() {
        for source in [
            "MAILDIR=\n",
            "EMPTY=\n:0\nmaildir:$EMPTY\n",
            "BAD=../escape\n:0\nmaildir:$BAD\n",
            "BAD=one//two\n:0\nmaildir:$BAD\n",
            "BAD=one/./two\n:0 :$BAD\nmaildir:target\n",
            "BAD=box/\n:0\nmbox:$BAD\n",
        ] {
            assert!(parse(source).unwrap().expand().is_err(), "{source:?}");
        }
    }

    #[test]
    fn leaves_regex_patterns_unchanged() {
        let config = parse("NAME=value\n:0\n* ^Subject: $NAME$\ninbox/\n")
            .unwrap()
            .expand()
            .unwrap();
        let Statement::Recipe(recipe) = &config.statements[1] else {
            panic!("expected recipe");
        };
        let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
            panic!("expected regex");
        };

        assert_eq!(regex.pattern(), "^Subject: $NAME$");
    }
}
