// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use crate::bounded_bytes::{BoundedBytes, BoundedBytesError};

use super::shell_eval::{self, EvaluationContext, EvaluationDepth, UnsupportedPart, VariableValue};
use super::{
    Assignment, AssignmentPath, AssignmentTarget, Config, Destination, HeaderAction,
    HeaderOperation, HeaderValue, MAX_ASSIGNMENT_VALUE_LEN, MAX_EXPANSION_DEPTH,
    MAX_PATH_EXPRESSION_LEN, PathExpression, RcFileExpression, Recipe, RecipeAction,
    ShellExpandedCondition, ShellExpression, ShellPart, Statement, SuppliedVariable,
    VariablePolicy, VariableSource, variable_policy,
};

#[derive(Debug, Clone)]
struct ExpandedValue {
    text: String,
    depth: usize,
}

fn expansion_depth_error(line: usize) -> ExpansionError {
    ExpansionError::new(
        line,
        format!("variable expansion exceeds the hard depth limit of {MAX_EXPANSION_DEPTH}"),
    )
}

fn expansion_length_error(
    line: usize,
    error: BoundedBytesError,
    _current: usize,
    limit: usize,
) -> ExpansionError {
    match error {
        BoundedBytesError::LengthOverflow => {
            ExpansionError::new(line, "expanded value length overflows")
        }
        BoundedBytesError::LimitExceeded { .. } => ExpansionError::new(
            line,
            format!("expanded value exceeds the hard limit of {limit} bytes"),
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparationPhase {
    Eager,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathPurpose {
    Maildir,
    Mbox,
    File,
    Discard,
    Lockfile,
    Logfile,
    RcFile,
}

#[derive(Debug, Clone, Copy)]
struct PathResolver<'a> {
    purpose: PathPurpose,
    description: &'static str,
    line: usize,
    base: Option<&'a str>,
    typed_destination: bool,
}

impl<'a> PathResolver<'a> {
    fn new(
        purpose: PathPurpose,
        description: &'static str,
        line: usize,
        base: Option<&'a str>,
    ) -> Self {
        Self {
            purpose,
            description,
            line,
            base,
            typed_destination: true,
        }
    }

    fn destination(
        purpose: PathPurpose,
        line: usize,
        base: Option<&'a str>,
        typed_destination: bool,
    ) -> Self {
        let description = match purpose {
            PathPurpose::Maildir => "Maildir destination",
            PathPurpose::Mbox => "mbox destination",
            PathPurpose::File => "file destination",
            PathPurpose::Discard => "discard destination",
            _ => "destination",
        };
        Self {
            purpose,
            description,
            line,
            base,
            typed_destination,
        }
    }

    fn evaluate(
        self,
        expression: &ShellExpression,
        lookup: &mut impl FnMut(&str) -> Option<String>,
    ) -> Result<String, ExpansionError> {
        let source =
            evaluate_with_linebuf(expression, self.line, MAX_PATH_EXPRESSION_LEN, lookup)?.text;
        self.resolve(&source)
    }

    fn resolve(self, source: &str) -> Result<String, ExpansionError> {
        if source.is_empty() && self.allows_empty() {
            return Ok(String::new());
        }
        if self.is_destination()
            && !self.typed_destination
            && source.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(ExpansionError::new(
                self.line,
                "multiple unmarked mailbox destinations are not supported",
            ));
        }

        // Validate only after the bounded join. Checking the relative spelling
        // alone would miss unsafe components supplied by MAILDIR, while an
        // ordinary path join could allocate beyond the path ceiling first.
        let path = resolve_relative_path(source, self.base, self.line)?;
        validate_filesystem_path(
            &path,
            self.line,
            self.description,
            self.purpose == PathPurpose::Maildir,
        )?;
        if self.purpose == PathPurpose::Discard && path != "/dev/null" {
            return Err(ExpansionError::new(
                self.line,
                "discard destination must resolve exactly to /dev/null",
            ));
        }
        Ok(path)
    }

    fn allows_empty(self) -> bool {
        matches!(
            self.purpose,
            PathPurpose::Lockfile | PathPurpose::Logfile | PathPurpose::RcFile
        )
    }

    fn is_destination(self) -> bool {
        matches!(
            self.purpose,
            PathPurpose::Maildir | PathPurpose::Mbox | PathPurpose::File | PathPurpose::Discard
        )
    }
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
                    self.target.value_limit(),
                    &mut lookup,
                )?
                .text
            }
            None => self.value.clone(),
        };
        self.target
            .validate_resolved_value(&value)
            .map_err(|message| ExpansionError::new(self.line, message))?;
        let Some(path) = self.target.path() else {
            return Ok(value);
        };
        let (purpose, description) = assignment_path(path);
        let base = lookup("MAILDIR");
        PathResolver::new(purpose, description, self.line, base.as_deref()).resolve(&value)
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
        // procmail treats MAILDIR as its current directory. Resolve against
        // its value at the moment the statement executes; when it is unset,
        // leave the path relative so the loader uses the process directory.
        let base = lookup("MAILDIR");
        PathResolver::new(PathPurpose::RcFile, "rc file", self.line, base.as_deref())
            .evaluate(expression, &mut lookup)
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
    // Keep access to the shared path state and reconstruction of its enum
    // variant together. Resolution may turn an unmarked file into mbox or
    // discard delivery, so duplicating these matches at call sites could
    // preserve stale preparation state or choose a different backend.
    fn expression(&self) -> &PathExpression {
        match self {
            Self::Maildir(expression)
            | Self::Mbox(expression)
            | Self::File(expression)
            | Self::Discard(expression) => expression,
        }
    }

    fn expression_mut(&mut self) -> &mut PathExpression {
        match self {
            Self::Maildir(expression)
            | Self::Mbox(expression)
            | Self::File(expression)
            | Self::Discard(expression) => expression,
        }
    }

    fn purpose(&self) -> PathPurpose {
        match self.kind() {
            super::DestinationKind::Maildir => PathPurpose::Maildir,
            super::DestinationKind::Mbox => PathPurpose::Mbox,
            super::DestinationKind::File => PathPurpose::File,
            super::DestinationKind::Discard => PathPurpose::Discard,
        }
    }

    fn rebuild(&self, expression: PathExpression, classify_file: bool) -> Self {
        match self {
            Self::Maildir(_) => Self::Maildir(expression),
            Self::Mbox(_) => Self::Mbox(expression),
            Self::File(_) if classify_file && expression.source == "/dev/null" => {
                Self::Discard(expression)
            }
            Self::File(_) if classify_file => Self::Mbox(expression),
            Self::File(_) => Self::File(expression),
            Self::Discard(_) => Self::Discard(expression),
        }
    }

    fn resolved_expression(&self, source: String) -> PathExpression {
        let expression = self.expression();
        PathExpression {
            source,
            base: None,
            line: expression.line,
            runtime_dependent: false,
            runtime_base: false,
            typed_destination: expression.typed_destination,
            expansion: None,
        }
    }

    fn adopt_static_discard_classification(&mut self, resolved: &Self) {
        if self.kind() == super::DestinationKind::File
            && resolved.kind() == super::DestinationKind::Discard
        {
            let expression = self.expression().clone();
            *self = Self::Discard(expression);
        }
    }

    fn resolver<'a>(&self, base: Option<&'a str>) -> PathResolver<'a> {
        let expression = self.expression();
        PathResolver::destination(
            self.purpose(),
            expression.line,
            base,
            expression.typed_destination,
        )
    }

    pub(crate) fn command_expression(&self) -> Option<&ShellExpression> {
        self.expression()
            .expansion
            .as_ref()
            .filter(|expression| expression.has_commands())
    }

    pub fn line(&self) -> usize {
        self.expression().line
    }

    pub(crate) fn resolve_command_output(
        &self,
        source: String,
        runtime_maildir: Option<&str>,
    ) -> Result<Self, ExpansionError> {
        let expression = self.expression();
        if !expression
            .expansion
            .as_ref()
            .is_some_and(ShellExpression::has_commands)
        {
            return Err(ExpansionError::new(
                expression.line,
                "destination has no command substitution",
            ));
        }
        let base = if expression.runtime_base {
            runtime_maildir
        } else {
            expression.base.as_deref()
        };
        let path = self.resolver(base).resolve(&source)?;
        let resolved = self.resolved_expression(path);
        Ok(self.rebuild(resolved, true))
    }

    pub fn bind_with(
        &self,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ExpansionError> {
        let expression = self.expression();
        if expression
            .expansion
            .as_ref()
            .is_some_and(ShellExpression::has_commands)
        {
            return Err(ExpansionError::new(
                expression.line,
                "destination command substitution has not executed",
            ));
        }
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
            typed_destination: expression.typed_destination,
            expansion: Some(expansion),
        };
        Ok(self.rebuild(bound, false))
    }

    pub fn resolve_with(
        &self,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ExpansionError> {
        let expression = self.expression();
        if expression
            .expansion
            .as_ref()
            .is_some_and(ShellExpression::has_commands)
        {
            return Err(ExpansionError::new(
                expression.line,
                "destination command substitution has not executed",
            ));
        }
        let parsed;
        let compiled = if let Some(compiled) = expression.expansion.as_ref() {
            compiled
        } else {
            parsed = parse_expression(&expression.source, expression.line)?;
            &parsed
        };
        let runtime_base = expression.runtime_base.then(|| lookup("MAILDIR")).flatten();
        let base = runtime_base.as_deref().or(expression.base.as_deref());
        // Procmail splits an unmarked mailbox action into directory targets,
        // so whitespace introduced by a runtime value cannot safely mean one
        // filename here. Explicit backend syntax supplies that missing
        // distinction and may therefore retain whitespace as path data.
        let path = self.resolver(base).evaluate(compiled, &mut lookup)?;
        let resolved = self.resolved_expression(path);
        Ok(self.rebuild(resolved, true))
    }

    pub fn path(&self) -> &str {
        self.expression().source()
    }

    pub fn needs_runtime_variables(&self) -> bool {
        let expression = self.expression();
        expression.runtime_dependent || expression.runtime_base
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
        let runtime_base = self.runtime_base.then(|| lookup("MAILDIR")).flatten();
        let base = runtime_base.as_deref().or(self.base.as_deref());
        PathResolver::new(PathPurpose::Lockfile, "lockfile", self.line, base)
            .evaluate(compiled, &mut lookup)
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
    // A check has no message values, but it still needs to reject undefined
    // ordinary variables and malformed path expressions throughout a loaded
    // file. Prepare every statement for later symbolic evaluation instead of
    // demanding MATCH or LASTFOLDER before stdin exists.
    let mut preparer = ConfigPreparer::new(
        known,
        maildir.map(str::to_owned),
        config.initial_linebuf,
        PreparationPhase::Deferred,
    );
    preparer.prepare_statements(&mut config.statements)?;
    Ok(config)
}

fn expand_config(
    mut config: Config,
    mut variables: BTreeMap<String, ExpandedValue>,
    initial_variables: Vec<(String, String, VariableSource)>,
    maildir: Option<String>,
) -> Result<Config, ExpansionError> {
    config.initial_variables = initial_variables;
    variables
        .entry("LINEBUF".to_owned())
        .or_insert(ExpandedValue {
            text: config.initial_linebuf.to_string(),
            depth: 0,
        });
    variables
        .entry("LOCKEXT".to_owned())
        .or_insert(ExpandedValue {
            text: super::DEFAULT_LOCK_EXT.to_owned(),
            depth: 0,
        });
    let mut preparer = ConfigPreparer::new(
        variables,
        maildir,
        config.initial_linebuf,
        PreparationPhase::Eager,
    );
    preparer.prepare_statements(&mut config.statements)?;

    Ok(config)
}

fn active_linebuf(lookup: &mut impl FnMut(&str) -> Option<String>) -> usize {
    lookup("LINEBUF")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(super::DEFAULT_LINEBUF)
}

fn evaluate_with_linebuf(
    expression: &ShellExpression,
    line: usize,
    hard_limit: usize,
    lookup: &mut impl FnMut(&str) -> Option<String>,
) -> Result<ExpandedValue, ExpansionError> {
    let linebuf = active_linebuf(lookup);
    let limit = linebuf.min(hard_limit);
    evaluate_expression(expression, line, limit, lookup)
        .map_err(|error| relabel_linebuf_error(error, linebuf, hard_limit))
}

pub(crate) fn expand_runtime_bytes<'a>(
    input: &str,
    line: usize,
    limit: usize,
    mut lookup: impl FnMut(&str) -> Option<&'a [u8]>,
) -> Result<Vec<u8>, ExpansionError> {
    let expression = parse_expression(input, line)?;
    let mut owned_lookup = |name: &str| lookup(name).map(<[u8]>::to_vec);
    let mut context = RuntimeBytesEvaluation {
        line,
        lookup: &mut owned_lookup,
    };
    shell_eval::evaluate(&expression, limit, &mut context).map(|value| value.bytes)
}

struct RuntimeBytesEvaluation<'a, L> {
    line: usize,
    lookup: &'a mut L,
}

impl<L> EvaluationContext for RuntimeBytesEvaluation<'_, L>
where
    L: FnMut(&str) -> Option<Vec<u8>>,
{
    type Error = ExpansionError;

    fn depth_mode(&self) -> EvaluationDepth {
        EvaluationDepth::None
    }

    fn variable(&mut self, name: &str) -> Result<Option<VariableValue>, Self::Error> {
        Ok((self.lookup)(name).map(|bytes| VariableValue { bytes, depth: 0 }))
    }

    fn command(&mut self, _: &str, _: usize) -> Result<Vec<u8>, Self::Error> {
        Err(self.unsupported_part(UnsupportedPart::Command))
    }

    fn regex_quoted(&mut self, _: &str, _: usize) -> Result<Vec<u8>, Self::Error> {
        Err(self.unsupported_part(UnsupportedPart::RegexQuotedVariable))
    }

    fn missing_variable(&self, name: &str) -> Self::Error {
        ExpansionError::new(self.line, format!("variable {name} is not defined"))
    }

    fn unsupported_part(&self, _: UnsupportedPart) -> Self::Error {
        ExpansionError::new(self.line, "expression is not valid in this context")
    }

    fn depth_exceeded(&self) -> Self::Error {
        expansion_depth_error(self.line)
    }

    fn depth_overflow(&self) -> Self::Error {
        ExpansionError::new(self.line, "variable expansion depth overflows")
    }

    fn length_error(&self, error: BoundedBytesError, current: usize, limit: usize) -> Self::Error {
        expansion_length_error(self.line, error, current, limit)
    }
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

struct ConfigPreparer {
    known: BTreeMap<String, ExpandedValue>,
    dynamic: BTreeSet<String>,
    maildir: Option<String>,
    linebuf: usize,
    phase: PreparationPhase,
}

impl ConfigPreparer {
    fn new(
        known: BTreeMap<String, ExpandedValue>,
        maildir: Option<String>,
        linebuf: usize,
        phase: PreparationPhase,
    ) -> Self {
        Self {
            known,
            dynamic: BTreeSet::new(),
            maildir,
            linebuf,
            phase,
        }
    }

    fn prepare_statements(&mut self, statements: &mut [Statement]) -> Result<(), ExpansionError> {
        for statement in statements {
            self.prepare_statement(statement)?;
        }
        Ok(())
    }

    fn prepare_statement(&mut self, statement: &mut Statement) -> Result<(), ExpansionError> {
        match statement {
            Statement::Assignment(assignment) => self.prepare_assignment(assignment),
            Statement::CommandAssignment(assignment) => {
                prepare_command_assignment(assignment, &self.known, &self.dynamic)?;
                self.dynamic.insert(assignment.name.clone());
                Ok(())
            }
            Statement::Recipe(recipe) => {
                let phase = if self.phase == PreparationPhase::Eager && self.dynamic.is_empty() {
                    PreparationPhase::Eager
                } else {
                    PreparationPhase::Deferred
                };
                self.prepare_recipe(recipe, phase)?;
                record_recipe_dynamic_names(recipe, &mut self.dynamic);
                Ok(())
            }
            Statement::Include(expression) | Statement::Switch(expression) => {
                let parsed = parse_expression(&expression.value, expression.line)?;
                validate_runtime_references(&parsed, expression.line, &self.known, &self.dynamic)?;
                expression.expansion = Some(parsed);
                Ok(())
            }
        }
    }

    fn prepare_assignment(&mut self, assignment: &mut Assignment) -> Result<(), ExpansionError> {
        if self.phase == PreparationPhase::Deferred {
            return self.prepare_deferred_assignment(assignment);
        }

        let parsed = parse_assignment_expression(
            &assignment.value,
            assignment.line,
            assignment.double_quoted,
        )?;
        let analysis = ExpressionAnalysis::new(&parsed, &self.known, &self.dynamic);
        if analysis.references_dynamic {
            analysis.validate_runtime_references(assignment.line)?;
            assignment.expansion = Some(parsed);
            self.dynamic.insert(assignment.name.clone());
            return Ok(());
        }

        let hard_limit = assignment.target.value_limit();
        let limit = hard_limit.min(self.linebuf);
        let expanded = evaluate_config_expression(&parsed, assignment.line, limit, &self.known)
            .map_err(|error| relabel_linebuf_error(error, self.linebuf, hard_limit))?;
        assignment.value = expanded.text;
        self.validate_static_assignment(assignment)?;
        self.resolve_static_assignment_path(assignment)?;
        self.known.insert(
            assignment.name.clone(),
            ExpandedValue {
                text: assignment.value.clone(),
                depth: expanded.depth,
            },
        );
        Ok(())
    }

    fn prepare_deferred_assignment(
        &mut self,
        assignment: &mut Assignment,
    ) -> Result<(), ExpansionError> {
        if !assignment.target.supports_conditional_assignment() {
            return Err(ExpansionError::new(
                assignment.line,
                format!(
                    "variable {} cannot be assigned conditionally yet",
                    assignment.name
                ),
            ));
        }
        let expression = parse_assignment_expression(
            &assignment.value,
            assignment.line,
            assignment.double_quoted,
        )?;
        let analysis = ExpressionAnalysis::new(&expression, &self.known, &self.dynamic);
        analysis.validate_runtime_references(assignment.line)?;
        self.validate_known_deferred_assignment(assignment, &expression, &analysis)?;
        assignment.expansion = Some(expression);

        // Conditional assignments exist only on a selected execution path.
        // Record their names without changing the known values so following
        // expressions are validated against the value available at runtime.
        self.dynamic.insert(assignment.name.clone());
        Ok(())
    }

    fn validate_static_assignment(
        &mut self,
        assignment: &Assignment,
    ) -> Result<(), ExpansionError> {
        assignment
            .target
            .validate_known_value(&assignment.value)
            .map_err(|message| ExpansionError::new(assignment.line, message))?;
        if assignment.target == AssignmentTarget::LineBuf {
            self.linebuf = parse_linebuf(&assignment.value, assignment.line)?;
        }
        Ok(())
    }

    fn resolve_static_assignment_path(
        &mut self,
        assignment: &mut Assignment,
    ) -> Result<(), ExpansionError> {
        let Some(path) = assignment.target.path() else {
            return Ok(());
        };
        let (purpose, description) = assignment_path(path);
        assignment.value = PathResolver::new(
            purpose,
            description,
            assignment.line,
            self.maildir.as_deref(),
        )
        .resolve(&assignment.value)?;
        if assignment.target == AssignmentTarget::Maildir {
            self.maildir = Some(assignment.value.clone());
        }
        Ok(())
    }

    fn validate_known_deferred_assignment(
        &self,
        assignment: &Assignment,
        expression: &ShellExpression,
        analysis: &ExpressionAnalysis<'_>,
    ) -> Result<(), ExpansionError> {
        if analysis.needs_runtime || analysis.references_dynamic {
            return Ok(());
        }
        let value = evaluate_config_expression(
            expression,
            assignment.line,
            assignment.target.value_limit(),
            &self.known,
        )?;
        assignment
            .target
            .validate_known_value(&value.text)
            .map_err(|message| ExpansionError::new(assignment.line, message))
    }

    fn prepare_recipe(
        &self,
        recipe: &mut Recipe,
        phase: PreparationPhase,
    ) -> Result<(), ExpansionError> {
        // All recipes pass through the same preparation path. Only filesystem
        // destinations distinguish values fixed before input from values that
        // must remain structured until their execution path is selected.
        prepare_shell_conditions(recipe, &self.known, &self.dynamic)?;
        if let Some(expression) = &mut recipe.lock {
            prepare_lock_expression(
                expression,
                recipe.line,
                &self.known,
                &self.dynamic,
                self.maildir.as_deref(),
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
                self.prepare_destination(destination, recipe.action_line, phase)
            }
            RecipeAction::Headers(action) => {
                prepare_header_action(action, &self.known, &self.dynamic)
            }
            RecipeAction::Block(statements) => {
                let dynamic = if phase == PreparationPhase::Eager {
                    BTreeSet::new()
                } else {
                    self.dynamic.clone()
                };
                let mut child = Self {
                    known: self.known.clone(),
                    dynamic,
                    maildir: self.maildir.clone(),
                    linebuf: self.linebuf,
                    phase: PreparationPhase::Deferred,
                };
                child.prepare_statements(statements)
            }
            RecipeAction::Pipe(_) | RecipeAction::Capture(_) => Ok(()),
        }
    }

    fn prepare_destination(
        &self,
        destination: &mut Destination,
        line: usize,
        phase: PreparationPhase,
    ) -> Result<(), ExpansionError> {
        let expression = destination.expression_mut();
        expression.base = self.maildir.clone();
        expression.line = line;
        if let Some(command_expression) = expression
            .expansion
            .as_ref()
            .filter(|expression| expression.has_commands())
        {
            validate_shell_expression(command_expression, line, &self.known, &self.dynamic)?;
            expression.runtime_dependent = true;
            expression.runtime_base = self.dynamic.contains("MAILDIR");
            return Ok(());
        }
        let parsed = parse_expression(&expression.source, line)?;
        let analysis = ExpressionAnalysis::new(&parsed, &self.known, &self.dynamic);
        if phase == PreparationPhase::Eager {
            analysis.validate_path_references(line)?;
        } else {
            analysis.validate_runtime_references(line)?;
        }
        let has_runtime_reference = analysis.references_dynamic || analysis.needs_runtime;
        expression.runtime_dependent = has_runtime_reference;
        expression.runtime_base = self.dynamic.contains("MAILDIR");
        expression.expansion = Some(parsed);
        if phase == PreparationPhase::Eager && !has_runtime_reference {
            let resolved = destination
                .resolve_with(|name| self.known.get(name).map(|value| value.text.clone()))?;
            destination.adopt_static_discard_classification(&resolved);
        }
        Ok(())
    }
}

fn assignment_path(path: AssignmentPath) -> (PathPurpose, &'static str) {
    match path {
        AssignmentPath::Maildir => (PathPurpose::Maildir, "MAILDIR"),
        AssignmentPath::LogFile => (PathPurpose::Logfile, "LOGFILE"),
        AssignmentPath::LockFile => (PathPurpose::Lockfile, "LOCKFILE"),
    }
}

fn prepare_command_assignment(
    assignment: &super::CommandAssignment,
    known: &BTreeMap<String, ExpandedValue>,
    dynamic: &BTreeSet<String>,
) -> Result<(), ExpansionError> {
    validate_shell_expression(&assignment.expression, assignment.line, known, dynamic)
}

fn validate_shell_expression(
    expression: &ShellExpression,
    line: usize,
    known: &BTreeMap<String, ExpandedValue>,
    dynamic: &BTreeSet<String>,
) -> Result<(), ExpansionError> {
    validate_runtime_references(expression, line, known, dynamic)
}

fn record_recipe_dynamic_names(recipe: &Recipe, dynamic: &mut BTreeSet<String>) {
    match &recipe.action {
        RecipeAction::Capture(action) => {
            dynamic.insert(action.name.clone());
        }
        RecipeAction::Block(statements) => {
            for statement in statements {
                match statement {
                    Statement::Assignment(assignment) => {
                        dynamic.insert(assignment.name.clone());
                    }
                    Statement::CommandAssignment(assignment) => {
                        dynamic.insert(assignment.name.clone());
                    }
                    Statement::Recipe(child) => record_recipe_dynamic_names(child, dynamic),
                    Statement::Include(_) | Statement::Switch(_) => {}
                }
            }
        }
        RecipeAction::Deliver(_) | RecipeAction::Pipe(_) | RecipeAction::Headers(_) => {}
    }
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
        let analysis = ExpressionAnalysis::new(&expression, known, dynamic);
        analysis.validate_runtime_references(line)?;
        let needs_runtime = analysis.needs_runtime || analysis.references_dynamic;

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
    let analysis = ExpressionAnalysis::new(&parsed, known, dynamic);
    analysis.validate_runtime_references(line)?;
    expression.base = maildir.map(str::to_owned);
    expression.line = line;
    expression.runtime_dependent = analysis.needs_runtime || analysis.references_dynamic;
    expression.runtime_base = expression.runtime_dependent;
    expression.expansion = Some(parsed);
    if !expression.runtime_dependent && !expression.source.is_empty() {
        expression.resolve_with(|name| known.get(name).map(|value| value.text.clone()))?;
    }
    Ok(())
}

fn validate_runtime_references(
    expression: &ShellExpression,
    line: usize,
    known: &BTreeMap<String, ExpandedValue>,
    dynamic: &BTreeSet<String>,
) -> Result<(), ExpansionError> {
    ExpressionAnalysis::new(expression, known, dynamic).validate_runtime_references(line)
}

fn prepare_shell_conditions(
    recipe: &mut Recipe,
    known: &BTreeMap<String, ExpandedValue>,
    dynamic: &BTreeSet<String>,
) -> Result<(), ExpansionError> {
    for condition in &mut recipe.conditions {
        let mut runtime = crate::runtime::RuntimeVariables::default();
        for (name, value) in known {
            runtime.set(name.clone(), value.text.clone());
        }

        // A known value may supply another complete `$` condition. Resolve the
        // entire static chain now so malformed configuration still fails before
        // stdin is consumed, while the pass limit bounds adversarial recursion.
        for pass in 0..=MAX_EXPANSION_DEPTH {
            let super::ConditionKind::ShellExpanded(shell) = &condition.kind else {
                break;
            };
            let expression = parse_shell_condition_expression(&shell.source, condition.line)?;
            let analysis = ExpressionAnalysis::new(&expression, known, dynamic);
            analysis.validate_shell_condition_references(condition.line)?;
            if !analysis.shell_condition_static {
                if let super::ConditionKind::ShellExpanded(shell) = &mut condition.kind {
                    shell.expansion = Some(expression);
                }
                break;
            }
            if pass == MAX_EXPANSION_DEPTH {
                return Err(ExpansionError::new(
                    condition.line,
                    format!(
                        "condition expansion exceeds the hard depth limit of {MAX_EXPANSION_DEPTH}"
                    ),
                ));
            }
            let prepared = ShellExpandedCondition {
                source: shell.source.clone(),
                expansion: Some(expression),
            };
            let expanded = expand_shell_condition(&prepared, condition.line, &runtime)?;
            let parsed = super::parse_reparsed_condition(
                expanded.trim_start(),
                condition.line,
                recipe.options.case_mode == super::CaseMode::Sensitive,
            )
            .map_err(|error| ExpansionError::new(error.line, error.message))?;
            condition.negated ^= parsed.negated;
            condition.kind = parsed.kind;
        }
    }
    Ok(())
}

pub(crate) fn expand_shell_condition(
    condition: &ShellExpandedCondition,
    line: usize,
    runtime: &crate::runtime::RuntimeVariables,
) -> Result<String, ExpansionError> {
    let parsed;
    let expression = if let Some(expression) = condition.expansion.as_ref() {
        expression
    } else {
        parsed = parse_shell_condition_expression(&condition.source, line)?;
        &parsed
    };
    let linebuf = crate::runtime::RuntimeSettings::at_line(runtime, line)
        .linebuf()
        .map_err(|error| ExpansionError::new(line, error.message().to_owned()))?;
    let mut context = ShellConditionEvaluation { line, runtime };
    let bytes = shell_eval::evaluate(expression, linebuf, &mut context)
        .map(|value| value.bytes)
        .map_err(|error| relabel_linebuf_error(error, linebuf, usize::MAX))?;
    String::from_utf8(bytes).map_err(|_| {
        ExpansionError::new(
            line,
            "shell-expanded condition contains non-UTF-8 variable data",
        )
    })
}

struct ShellConditionEvaluation<'a> {
    line: usize,
    runtime: &'a crate::runtime::RuntimeVariables,
}

impl EvaluationContext for ShellConditionEvaluation<'_> {
    type Error = ExpansionError;

    fn depth_mode(&self) -> EvaluationDepth {
        EvaluationDepth::None
    }

    fn variable(&mut self, name: &str) -> Result<Option<VariableValue>, Self::Error> {
        Ok(self.runtime.get_bytes(name).map(|bytes| VariableValue {
            bytes: bytes.to_vec(),
            depth: 0,
        }))
    }

    fn command(&mut self, _: &str, _: usize) -> Result<Vec<u8>, Self::Error> {
        Err(self.unsupported_part(UnsupportedPart::Command))
    }

    fn regex_quoted(&mut self, name: &str, limit: usize) -> Result<Vec<u8>, Self::Error> {
        let value = self
            .runtime
            .get_bytes(name)
            .ok_or_else(|| self.missing_variable(name))?;
        let mut output = Vec::new();
        push_regex_escaped(&mut output, value, limit, self.line)?;
        Ok(output)
    }

    fn missing_variable(&self, name: &str) -> Self::Error {
        ExpansionError::new(self.line, format!("variable {name} is not defined"))
    }

    fn unsupported_part(&self, part: UnsupportedPart) -> Self::Error {
        let message = match part {
            UnsupportedPart::Command => "command substitution requires ordered evaluation",
            UnsupportedPart::RegexQuotedVariable => {
                "regex-quoted variable is not valid in this context"
            }
        };
        ExpansionError::new(self.line, message)
    }

    fn depth_exceeded(&self) -> Self::Error {
        expansion_depth_error(self.line)
    }

    fn depth_overflow(&self) -> Self::Error {
        ExpansionError::new(self.line, "variable expansion depth overflows")
    }

    fn length_error(&self, error: BoundedBytesError, current: usize, limit: usize) -> Self::Error {
        expansion_length_error(self.line, error, current, limit)
    }
}

pub(crate) fn push_regex_escaped(
    output: &mut Vec<u8>,
    value: &[u8],
    limit: usize,
    line: usize,
) -> Result<(), ExpansionError> {
    push_bounded(output, b"(?:)", limit, line)?;
    if let Ok(value) = std::str::from_utf8(value) {
        for character in value.chars() {
            if matches!(
                character,
                '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
            ) {
                push_bounded(output, b"\\", limit, line)?;
            }
            let mut encoded = [0; 4];
            push_bounded(
                output,
                character.encode_utf8(&mut encoded).as_bytes(),
                limit,
                line,
            )?;
        }
        return Ok(());
    }

    push_bounded(output, b"(?-u:", limit, line)?;
    for byte in value {
        let escaped = format!("\\x{byte:02x}");
        push_bounded(output, escaped.as_bytes(), limit, line)?;
    }
    push_bounded(output, b")", limit, line)
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
    evaluate_config_expression(&expression, line, limit, variables)
}

fn evaluate_config_expression(
    expression: &ShellExpression,
    line: usize,
    limit: usize,
    variables: &BTreeMap<String, ExpandedValue>,
) -> Result<ExpandedValue, ExpansionError> {
    let mut context = ConfigEvaluation { line, variables };
    let evaluated = shell_eval::evaluate(expression, limit, &mut context)?;
    let text = String::from_utf8(evaluated.bytes)
        .map_err(|_| ExpansionError::new(line, "expanded value is not valid UTF-8"))?;
    Ok(ExpandedValue {
        text,
        depth: evaluated.depth,
    })
}

struct ConfigEvaluation<'a> {
    line: usize,
    variables: &'a BTreeMap<String, ExpandedValue>,
}

impl EvaluationContext for ConfigEvaluation<'_> {
    type Error = ExpansionError;

    fn depth_mode(&self) -> EvaluationDepth {
        EvaluationDepth::ExpansionChain
    }

    fn variable(&mut self, name: &str) -> Result<Option<VariableValue>, Self::Error> {
        Ok(self.variables.get(name).map(|value| VariableValue {
            bytes: value.text.as_bytes().to_vec(),
            depth: value.depth,
        }))
    }

    fn command(&mut self, _: &str, _: usize) -> Result<Vec<u8>, Self::Error> {
        Err(self.unsupported_part(UnsupportedPart::Command))
    }

    fn regex_quoted(&mut self, _: &str, _: usize) -> Result<Vec<u8>, Self::Error> {
        Err(self.unsupported_part(UnsupportedPart::RegexQuotedVariable))
    }

    fn missing_variable(&self, name: &str) -> Self::Error {
        match variable_policy(name) {
            VariablePolicy::RuntimeOnly => ExpansionError::new(
                self.line,
                format!("runtime variable {name} is not available in this context"),
            ),
            _ => ExpansionError::new(self.line, format!("variable {name} is not defined")),
        }
    }

    fn unsupported_part(&self, _: UnsupportedPart) -> Self::Error {
        ExpansionError::new(self.line, "expression is not valid in this context")
    }

    fn depth_exceeded(&self) -> Self::Error {
        expansion_depth_error(self.line)
    }

    fn depth_overflow(&self) -> Self::Error {
        ExpansionError::new(self.line, "variable expansion depth overflows")
    }

    fn length_error(&self, error: BoundedBytesError, current: usize, limit: usize) -> Self::Error {
        expansion_length_error(self.line, error, current, limit)
    }
}

#[derive(Debug, Default)]
struct ExpressionAnalysis<'a> {
    missing_runtime: Option<&'a str>,
    missing_shell_condition: Option<&'a str>,
    missing_path: Option<&'a str>,
    needs_runtime: bool,
    references_dynamic: bool,
    has_runtime_variable: bool,
    has_command: bool,
    has_regex_quoted_variable: bool,
    shell_condition_static: bool,
}

impl<'a> ExpressionAnalysis<'a> {
    fn new(
        expression: &'a ShellExpression,
        known: &BTreeMap<String, ExpandedValue>,
        dynamic: &BTreeSet<String>,
    ) -> Self {
        let mut analysis = Self {
            shell_condition_static: true,
            ..Self::default()
        };

        // Analyze every expression node here so additions to ShellPart cannot
        // silently escape one of the preparation checks. Default branches need
        // their own summaries because some consumers inspect all references,
        // while value-dependent checks inspect only the branch that can run.
        for part in &expression.parts {
            match part {
                ShellPart::Literal(_) => {}
                ShellPart::Variable { name, default } => {
                    let child = default
                        .as_ref()
                        .map(|value| Self::new(value, known, dynamic));
                    let policy = variable_policy(name);
                    let known_value = known.get(name);
                    let known_nonempty = known_value.is_some_and(|value| !value.text.is_empty());
                    let available_at_runtime = known_value.is_some()
                        || dynamic.contains(name)
                        || policy == VariablePolicy::RuntimeOnly;

                    if !available_at_runtime {
                        merge_first_missing(
                            &mut analysis.missing_runtime,
                            child.as_ref().and_then(|value| value.missing_runtime),
                            default.is_none().then_some(name),
                        );
                        merge_first_missing(
                            &mut analysis.missing_shell_condition,
                            child
                                .as_ref()
                                .and_then(|value| value.missing_shell_condition),
                            default.is_none().then_some(name),
                        );
                    }

                    if !known_nonempty && policy != VariablePolicy::RuntimeOnly {
                        merge_first_missing(
                            &mut analysis.missing_path,
                            child.as_ref().and_then(|value| value.missing_path),
                            (default.is_none() && known_value.is_none()).then_some(name),
                        );
                    }

                    analysis.references_dynamic |= dynamic.contains(name)
                        || child.as_ref().is_some_and(|value| value.references_dynamic);
                    analysis.has_runtime_variable |= policy == VariablePolicy::RuntimeOnly
                        || child
                            .as_ref()
                            .is_some_and(|value| value.has_runtime_variable);
                    analysis.has_command |= child.as_ref().is_some_and(|value| value.has_command);
                    analysis.has_regex_quoted_variable |= child
                        .as_ref()
                        .is_some_and(|value| value.has_regex_quoted_variable);
                    analysis.needs_runtime |= if policy == VariablePolicy::RuntimeOnly {
                        true
                    } else if known_nonempty {
                        false
                    } else {
                        child.as_ref().is_some_and(|value| value.needs_runtime)
                    };
                    analysis.shell_condition_static &=
                        if dynamic.contains(name) || policy == VariablePolicy::RuntimeOnly {
                            false
                        } else if known_nonempty {
                            true
                        } else if known_value.is_some() {
                            child
                                .as_ref()
                                .is_none_or(|value| value.shell_condition_static)
                        } else {
                            child
                                .as_ref()
                                .is_some_and(|value| value.shell_condition_static)
                        };
                }
                ShellPart::RegexQuotedVariable(name) => {
                    let available_at_runtime = known.contains_key(name)
                        || dynamic.contains(name)
                        || variable_policy(name) == VariablePolicy::RuntimeOnly;
                    if !available_at_runtime && analysis.missing_shell_condition.is_none() {
                        analysis.missing_shell_condition = Some(name);
                    }
                    analysis.references_dynamic |= dynamic.contains(name);
                    analysis.has_regex_quoted_variable = true;
                    analysis.needs_runtime = true;
                    analysis.shell_condition_static &= known.contains_key(name)
                        && !dynamic.contains(name)
                        && variable_policy(name) != VariablePolicy::RuntimeOnly;
                }
                ShellPart::Command(_) => {
                    analysis.has_command = true;
                    analysis.needs_runtime = true;
                    analysis.shell_condition_static = false;
                }
            }
        }
        analysis
    }

    fn validate_runtime_references(&self, line: usize) -> Result<(), ExpansionError> {
        self.validate_missing(self.missing_runtime, line)
    }

    fn validate_shell_condition_references(&self, line: usize) -> Result<(), ExpansionError> {
        self.validate_missing(self.missing_shell_condition, line)
    }

    fn validate_path_references(&self, line: usize) -> Result<(), ExpansionError> {
        self.validate_missing(self.missing_path, line)
    }

    fn validate_missing(&self, missing: Option<&str>, line: usize) -> Result<(), ExpansionError> {
        match missing {
            Some(name) => Err(ExpansionError::new(
                line,
                format!("variable {name} is not defined"),
            )),
            None => Ok(()),
        }
    }

    fn has_runtime_part(&self) -> bool {
        self.has_runtime_variable || self.has_command || self.has_regex_quoted_variable
    }
}

fn merge_first_missing<'a>(
    target: &mut Option<&'a str>,
    nested: Option<&'a str>,
    current: Option<&'a String>,
) {
    if target.is_none() {
        *target = nested.or(current.map(String::as_str));
    }
}

fn bind_static_expression(
    expression: &ShellExpression,
    line: usize,
    lookup: &mut impl FnMut(&str) -> Option<String>,
    nesting: usize,
) -> Result<ShellExpression, ExpansionError> {
    // Bind ordinary values without flattening the whole expression. Runtime
    // references and their defaults must remain structured so a value
    // produced by an earlier delivery can choose the branch later.
    check_expansion_depth(nesting, line)?;
    let mut parts = Vec::new();
    for part in &expression.parts {
        match part {
            ShellPart::Literal(text) => push_literal_part(&mut parts, text),
            ShellPart::Variable { name, default }
                if variable_policy(name) == VariablePolicy::RuntimeOnly =>
            {
                let default = default
                    .as_ref()
                    .map(|value| bind_static_expression(value, line, lookup, nesting + 1))
                    .transpose()?;
                parts.push(ShellPart::Variable {
                    name: name.clone(),
                    default,
                });
            }
            ShellPart::Variable { name, default } => match (lookup(name), default) {
                (Some(value), _) if !value.is_empty() => push_literal_part(&mut parts, &value),
                (_, Some(default)) => {
                    let bound = bind_static_expression(default, line, lookup, nesting + 1)?;
                    for part in bound.parts {
                        match part {
                            ShellPart::Literal(text) => push_literal_part(&mut parts, &text),
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
            ShellPart::RegexQuotedVariable(_) | ShellPart::Command(_) => {
                return Err(ExpansionError::new(
                    line,
                    "expression cannot be statically bound",
                ));
            }
        }
    }
    Ok(ShellExpression { parts })
}

fn evaluate_expression(
    expression: &ShellExpression,
    line: usize,
    limit: usize,
    lookup: &mut impl FnMut(&str) -> Option<String>,
) -> Result<ExpandedValue, ExpansionError> {
    let mut context = RuntimeStringEvaluation { line, lookup };
    let evaluated = shell_eval::evaluate(expression, limit, &mut context)?;
    let text = String::from_utf8(evaluated.bytes)
        .map_err(|_| ExpansionError::new(line, "expanded value is not valid UTF-8"))?;
    Ok(ExpandedValue {
        text,
        depth: evaluated.depth,
    })
}

struct RuntimeStringEvaluation<'a, L> {
    line: usize,
    lookup: &'a mut L,
}

impl<L> EvaluationContext for RuntimeStringEvaluation<'_, L>
where
    L: FnMut(&str) -> Option<String>,
{
    type Error = ExpansionError;

    fn depth_mode(&self) -> EvaluationDepth {
        EvaluationDepth::SyntaxNesting
    }

    fn variable(&mut self, name: &str) -> Result<Option<VariableValue>, Self::Error> {
        Ok((self.lookup)(name).map(|value| VariableValue {
            bytes: value.into_bytes(),
            depth: 0,
        }))
    }

    fn command(&mut self, _: &str, _: usize) -> Result<Vec<u8>, Self::Error> {
        Err(self.unsupported_part(UnsupportedPart::Command))
    }

    fn regex_quoted(&mut self, _: &str, _: usize) -> Result<Vec<u8>, Self::Error> {
        Err(self.unsupported_part(UnsupportedPart::RegexQuotedVariable))
    }

    fn missing_variable(&self, name: &str) -> Self::Error {
        ExpansionError::new(self.line, format!("runtime variable {name} is not set"))
    }

    fn unsupported_part(&self, _: UnsupportedPart) -> Self::Error {
        ExpansionError::new(self.line, "expression cannot be evaluated here")
    }

    fn depth_exceeded(&self) -> Self::Error {
        expansion_depth_error(self.line)
    }

    fn depth_overflow(&self) -> Self::Error {
        ExpansionError::new(self.line, "variable expansion depth overflows")
    }

    fn length_error(&self, error: BoundedBytesError, current: usize, limit: usize) -> Self::Error {
        expansion_length_error(self.line, error, current, limit)
    }
}

fn expression_has_runtime(expression: &ShellExpression) -> bool {
    ExpressionAnalysis::new(expression, &BTreeMap::new(), &BTreeSet::new()).has_runtime_part()
}

fn push_literal_part(parts: &mut Vec<ShellPart>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(ShellPart::Literal(previous)) = parts.last_mut() {
        previous.push_str(text);
    } else {
        parts.push(ShellPart::Literal(text.to_owned()));
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

pub(crate) fn parse_shell_condition_expression(
    input: &str,
    line: usize,
) -> Result<ShellExpression, ExpansionError> {
    ExpressionParser::new(input, line, ExpressionSyntax::ShellCondition).parse()
}

fn parse_expression(input: &str, line: usize) -> Result<ShellExpression, ExpansionError> {
    parse_assignment_expression(input, line, false)
}

fn parse_assignment_expression(
    input: &str,
    line: usize,
    double_quoted: bool,
) -> Result<ShellExpression, ExpansionError> {
    ExpressionParser::new(
        input,
        line,
        ExpressionSyntax::Ordinary {
            allow_commands: false,
            double_quoted,
        },
    )
    .parse()
}

pub(crate) fn parse_command_expression(
    input: &str,
    line: usize,
    double_quoted: bool,
) -> Result<Option<ShellExpression>, ExpansionError> {
    let syntax = ExpressionSyntax::Ordinary {
        allow_commands: true,
        double_quoted,
    };
    if !syntax.contains_command(input) {
        return Ok(None);
    }
    let expression = ExpressionParser::new(input, line, syntax).parse()?;
    Ok(expression.has_commands().then_some(expression))
}

#[derive(Debug, Clone, Copy)]
enum ExpressionSyntax {
    Ordinary {
        allow_commands: bool,
        double_quoted: bool,
    },
    ShellCondition,
}

impl ExpressionSyntax {
    fn allows_commands(self) -> bool {
        match self {
            Self::Ordinary { allow_commands, .. } => allow_commands,
            Self::ShellCondition => true,
        }
    }

    fn escapes(self, byte: u8) -> bool {
        match self {
            Self::Ordinary {
                double_quoted: false,
                ..
            } => true,
            Self::Ordinary {
                double_quoted: true,
                ..
            } => matches!(byte, b'$' | b'`' | b'"' | b'\\' | b'\n'),
            Self::ShellCondition => matches!(byte, b'$' | b'`' | b'"' | b'\\'),
        }
    }

    fn invalid_utf8_message(self) -> &'static str {
        match self {
            Self::Ordinary { .. } => "expression contains invalid UTF-8",
            Self::ShellCondition => "shell-expanded condition contains invalid UTF-8",
        }
    }

    fn unterminated_command_message(self) -> &'static str {
        match self {
            Self::Ordinary { .. } => "unterminated backquoted command in expression",
            Self::ShellCondition => "unterminated backquoted command in shell-expanded condition",
        }
    }

    fn contains_command(self, input: &str) -> bool {
        let bytes = input.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() {
            if bytes[index] == b'\\' && bytes.get(index + 1).is_some_and(|next| self.escapes(*next))
            {
                index += 2;
                continue;
            }
            if self.allows_commands() && bytes[index] == b'`' {
                return true;
            }
            index += 1;
        }
        false
    }
}

struct ExpressionParser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    index: usize,
    line: usize,
    syntax: ExpressionSyntax,
}

impl<'a> ExpressionParser<'a> {
    fn new(input: &'a str, line: usize, syntax: ExpressionSyntax) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            index: 0,
            line,
            syntax,
        }
    }

    fn parse(mut self) -> Result<ShellExpression, ExpansionError> {
        let expression = self.parse_until(0, false)?;
        debug_assert_eq!(self.index, self.bytes.len());
        Ok(expression)
    }

    fn parse_until(
        &mut self,
        nesting: usize,
        stop_at_brace: bool,
    ) -> Result<ShellExpression, ExpansionError> {
        // Build owned parts once so later delivery phases never reinterpret
        // bytes obtained from a variable as expression syntax. The explicit
        // depth check also stops hostile nested defaults before recursion grows.
        check_expansion_depth(nesting, self.line)?;
        let mut parts = Vec::new();
        let mut literal = String::new();
        while self.index < self.bytes.len() {
            if stop_at_brace && self.bytes[self.index] == b'}' {
                break;
            }
            if self.bytes[self.index] == b'\\' {
                let Some(next) = self.bytes.get(self.index + 1).copied() else {
                    literal.push('\\');
                    self.index += 1;
                    continue;
                };
                if !self.syntax.escapes(next) {
                    literal.push('\\');
                    self.index += 1;
                    continue;
                }
                let character = self.input[self.index + 1..].chars().next().ok_or_else(|| {
                    ExpansionError::new(self.line, self.syntax.invalid_utf8_message())
                })?;
                literal.push(character);
                self.index += 1 + character.len_utf8();
                continue;
            }
            if self.syntax.allows_commands() && self.bytes[self.index] == b'`' {
                if !literal.is_empty() {
                    push_literal_part(&mut parts, &std::mem::take(&mut literal));
                }
                parts.push(self.parse_command()?);
                continue;
            }
            if self.bytes[self.index] != b'$' {
                let character = self.input[self.index..].chars().next().ok_or_else(|| {
                    ExpansionError::new(self.line, self.syntax.invalid_utf8_message())
                })?;
                literal.push(character);
                self.index += character.len_utf8();
                continue;
            }
            if !literal.is_empty() {
                push_literal_part(&mut parts, &std::mem::take(&mut literal));
            }
            if let Some(part) = self.parse_variable(nesting, &mut literal)? {
                parts.push(part);
            }
        }
        if !literal.is_empty() {
            push_literal_part(&mut parts, &literal);
        }
        if stop_at_brace && self.index == self.bytes.len() {
            return Err(ExpansionError::new(
                self.line,
                "variable reference is missing '}'",
            ));
        }
        Ok(ShellExpression { parts })
    }

    fn parse_command(&mut self) -> Result<ShellPart, ExpansionError> {
        let command_start = self.index + 1;
        self.index = command_start;
        let mut escaped = false;
        while self.index < self.bytes.len() {
            match self.bytes[self.index] {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'`' => break,
                _ => {}
            }
            self.index += 1;
        }
        if self.index == self.bytes.len() {
            return Err(ExpansionError::new(
                self.line,
                self.syntax.unterminated_command_message(),
            ));
        }
        let command = self.input[command_start..self.index].to_owned();
        self.index += 1;
        Ok(ShellPart::Command(command))
    }

    fn parse_variable(
        &mut self,
        nesting: usize,
        literal: &mut String,
    ) -> Result<Option<ShellPart>, ExpansionError> {
        self.index += 1;
        let regex_escape = if matches!(self.syntax, ExpressionSyntax::ShellCondition)
            && self.bytes.get(self.index) == Some(&b'\\')
        {
            self.index += 1;
            true
        } else {
            false
        };
        let Some(first) = self.bytes.get(self.index).copied() else {
            if regex_escape {
                return Err(ExpansionError::new(
                    self.line,
                    "regex-escaped condition variable is missing its name",
                ));
            }
            if matches!(self.syntax, ExpressionSyntax::ShellCondition) {
                literal.push('$');
                return Ok(None);
            }
            return Err(ExpansionError::new(
                self.line,
                "'$' must be followed by NAME or {NAME}",
            ));
        };
        let (name, default) = if first == b'{' {
            if regex_escape {
                return Err(ExpansionError::new(
                    self.line,
                    "regex-escaped condition variables use $\\NAME syntax",
                ));
            }
            self.parse_braced_variable(nesting)?
        } else {
            if !is_name_start(first) {
                if matches!(self.syntax, ExpressionSyntax::ShellCondition) {
                    if matches!(first, b'?' | b'#' | b'$' | b'-' | b'=' | b'@')
                        || first.is_ascii_digit()
                    {
                        return Err(ExpansionError::new(
                            self.line,
                            "unsupported special parameter in shell-expanded condition",
                        ));
                    }
                    literal.push('$');
                    return Ok(None);
                }
                return Err(ExpansionError::new(
                    self.line,
                    "unsupported '$' expansion; use $NAME or ${NAME}",
                ));
            }
            (self.parse_name(), None)
        };
        if variable_policy(&name) == VariablePolicy::Unsupported {
            return Err(ExpansionError::new(
                self.line,
                format!("procmail variable {name} is not supported"),
            ));
        }
        if regex_escape {
            Ok(Some(ShellPart::RegexQuotedVariable(name)))
        } else {
            Ok(Some(ShellPart::Variable { name, default }))
        }
    }

    fn parse_braced_variable(
        &mut self,
        nesting: usize,
    ) -> Result<(String, Option<ShellExpression>), ExpansionError> {
        self.index += 1;
        let name = self.parse_name();
        validate_reference_name(&name, self.line)?;
        match self.bytes.get(self.index..self.index + 2) {
            Some(b":-") => {
                self.index += 2;
                let default = self.parse_until(nesting + 1, true)?;
                if self.bytes.get(self.index) != Some(&b'}') {
                    return Err(ExpansionError::new(
                        self.line,
                        "variable reference is missing '}'",
                    ));
                }
                self.index += 1;
                Ok((name, Some(default)))
            }
            _ if self.bytes.get(self.index) == Some(&b'}') => {
                self.index += 1;
                Ok((name, None))
            }
            _ => Err(ExpansionError::new(
                self.line,
                "unsupported parameter expansion; use ${NAME} or ${NAME:-expression}",
            )),
        }
    }

    fn parse_name(&mut self) -> String {
        let name_start = self.index;
        if self
            .bytes
            .get(self.index)
            .is_some_and(|byte| is_name_start(*byte))
        {
            self.index += 1;
            while self.index < self.bytes.len() && is_name_continue(self.bytes[self.index]) {
                self.index += 1;
            }
        }
        self.input[name_start..self.index].to_owned()
    }
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
    BoundedBytes::try_extend_vec(output, limit, value).map_err(|error| match error {
        BoundedBytesError::LengthOverflow => {
            ExpansionError::new(line, "expanded value length overflows")
        }
        BoundedBytesError::LimitExceeded { .. } => ExpansionError::new(
            line,
            format!("expanded value exceeds the hard limit of {limit} bytes"),
        ),
    })
}

#[cfg(test)]
mod tests;
