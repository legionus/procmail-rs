// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

mod expand;
mod parser;
mod variables;

use std::fmt;

use regex::bytes::Regex;

pub use expand::ExpansionError;
pub use parser::parse;
pub(crate) use parser::parse_with_state;
pub use variables::{
    AssignmentTarget, MAX_COMMAND_LINE_VARIABLES, MAX_LOCK_TIMEOUT_SECONDS,
    MAX_PROCESS_TIMEOUT_SECONDS, MessageLimitVariable, RcLimitVariable, SuppliedVariable,
    SuppliedVariableError, VariablePolicy, VariableSource, assignment_value_limit,
    parse_lock_timeout_seconds, parse_process_timeout_seconds, parse_umask, validate_lock_method,
    variable_policy,
};

pub const MAX_ASSIGNMENT_NAME_LEN: usize = 128;
pub const MAX_ASSIGNMENT_VALUE_LEN: usize = 64 * 1024;

pub fn umask_from_config(config: &Config) -> Result<u32, String> {
    let mut mask = parse_umask("077")?;
    for statement in &config.statements {
        let Statement::Assignment(assignment) = statement else {
            continue;
        };
        if assignment.target != AssignmentTarget::Umask {
            continue;
        }
        mask = parse_umask(&assignment.value)
            .map_err(|message| format!("line {}: {message}", assignment.line))?;
    }
    Ok(mask)
}
pub const MAX_SHELL_SETTING_LEN: usize = 4096;
pub const MAX_CONDITIONS_PER_RECIPE: usize = 256;
pub const MAX_EXPANSION_DEPTH: usize = 32;
pub const MAX_PATH_EXPRESSION_LEN: usize = 4096;
pub const MAX_PIPE_COMMAND_LEN: usize = 64 * 1024;
pub const MAX_RECIPE_NESTING_DEPTH: usize = 64;
pub const MAX_REGEX_COMPILED_SIZE: usize = 8 * 1024 * 1024;
pub const MAX_REGEX_PATTERN_LEN: usize = 64 * 1024;
pub const MAX_REGEX_CAPTURES: usize = 64;
pub const MAX_MATCH_BYTES: usize = MAX_ASSIGNMENT_VALUE_LEN;
pub const MAX_RC_REGEXES: usize = 256;
pub const MAX_RC_SIZE: usize = 1024 * 1024;
pub const DEFAULT_LINEBUF: usize = 2048;
pub const MIN_LINEBUF: usize = 128;
pub const MAX_LINEBUF: usize = MAX_RC_SIZE;
pub const MAX_RC_CONDITIONS: usize = 4096;
pub const MAX_RC_RECIPES: usize = 1024;
pub const MAX_RC_STATEMENTS: usize = 4096;
pub const MAX_RC_ASSIGNMENTS: usize = 4096;

// These ceilings allow operational tuning without permitting an rc file to
// turn a count setting into an effectively unbounded allocation request.
pub const HARD_MAX_CONDITIONS_PER_RECIPE: usize = 4096;
pub const HARD_MAX_RECIPE_NESTING_DEPTH: usize = 256;
pub const HARD_MAX_RC_ASSIGNMENTS: usize = 65_536;
pub const HARD_MAX_RC_CONDITIONS: usize = 65_536;
pub const HARD_MAX_RC_RECIPES: usize = 16_384;
pub const HARD_MAX_RC_REGEXES: usize = 1024;
pub const HARD_MAX_RC_STATEMENTS: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub statements: Vec<Statement>,
    pub(crate) initial_variables: Vec<(String, String, VariableSource)>,
    pub(crate) parse_counts: RcParseCounts,
    pub(crate) initial_linebuf: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RcParseCounts {
    pub(crate) assignments: usize,
    pub(crate) statements: usize,
    pub(crate) recipes: usize,
    pub(crate) conditions: usize,
    pub(crate) regexes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RcLimits {
    pub(crate) assignments: usize,
    pub(crate) statements: usize,
    pub(crate) recipes: usize,
    pub(crate) conditions: usize,
    pub(crate) regexes: usize,
    pub(crate) conditions_per_recipe: usize,
    pub(crate) nesting_depth: usize,
    pub(crate) linebuf: usize,
}

impl Default for RcLimits {
    fn default() -> Self {
        Self {
            assignments: MAX_RC_ASSIGNMENTS,
            statements: MAX_RC_STATEMENTS,
            recipes: MAX_RC_RECIPES,
            conditions: MAX_RC_CONDITIONS,
            regexes: MAX_RC_REGEXES,
            conditions_per_recipe: MAX_CONDITIONS_PER_RECIPE,
            nesting_depth: MAX_RECIPE_NESTING_DEPTH,
            linebuf: DEFAULT_LINEBUF,
        }
    }
}

impl RcLimits {
    pub(crate) fn set(&mut self, kind: RcLimitVariable, value: usize) -> Result<(), usize> {
        let (slot, hard_limit) = match kind {
            RcLimitVariable::Assignments => (&mut self.assignments, HARD_MAX_RC_ASSIGNMENTS),
            RcLimitVariable::Statements => (&mut self.statements, HARD_MAX_RC_STATEMENTS),
            RcLimitVariable::Recipes => (&mut self.recipes, HARD_MAX_RC_RECIPES),
            RcLimitVariable::Conditions => (&mut self.conditions, HARD_MAX_RC_CONDITIONS),
            RcLimitVariable::Regexes => (&mut self.regexes, HARD_MAX_RC_REGEXES),
            RcLimitVariable::ConditionsPerRecipe => (
                &mut self.conditions_per_recipe,
                HARD_MAX_CONDITIONS_PER_RECIPE,
            ),
            RcLimitVariable::NestingDepth => {
                (&mut self.nesting_depth, HARD_MAX_RECIPE_NESTING_DEPTH)
            }
        };
        if value > hard_limit {
            return Err(hard_limit);
        }
        *slot = value;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RcParseState {
    pub(crate) counts: RcParseCounts,
    pub(crate) limits: RcLimits,
}

impl Config {
    pub fn expand(self) -> Result<Self, ExpansionError> {
        expand::expand(self, &[])
    }

    pub fn expand_with(self, supplied: &[SuppliedVariable]) -> Result<Self, ExpansionError> {
        expand::expand(self, supplied)
    }

    pub(crate) fn expand_with_runtime_values<'a>(
        self,
        values: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, ExpansionError> {
        expand::expand_with_runtime_values(self, values)
    }

    pub(crate) fn prepare_for_check<'a>(
        self,
        values: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, ExpansionError> {
        expand::prepare_for_check(self, values)
    }

    pub fn maildir(&self) -> Option<&str> {
        self.statements
            .iter()
            .rev()
            .find_map(|statement| match statement {
                Statement::Assignment(assignment)
                    if assignment.target == AssignmentTarget::Maildir =>
                {
                    Some(assignment.value.as_str())
                }
                _ => None,
            })
    }

    pub(crate) fn initial_variables(&self) -> &[(String, String, VariableSource)] {
        &self.initial_variables
    }

    pub(crate) fn parse_counts(&self) -> RcParseCounts {
        self.parse_counts
    }

    pub fn has_pipe_actions(&self) -> bool {
        statements_have_pipe_actions(&self.statements)
    }

    pub fn has_external_commands(&self) -> bool {
        statements_have_external_commands(&self.statements)
    }
}

fn statements_have_pipe_actions(statements: &[Statement]) -> bool {
    statements.iter().any(|statement| match statement {
        Statement::Recipe(recipe) => match &recipe.action {
            RecipeAction::Pipe(_) => true,
            RecipeAction::Block(children) => statements_have_pipe_actions(children),
            RecipeAction::Deliver(_) => false,
        },
        Statement::Assignment(_) | Statement::Include(_) | Statement::Switch(_) => false,
    })
}

fn statements_have_external_commands(statements: &[Statement]) -> bool {
    statements.iter().any(|statement| match statement {
        Statement::Recipe(recipe) => {
            recipe
                .conditions
                .iter()
                .any(|condition| matches!(condition.kind, ConditionKind::Program(_)))
                || match &recipe.action {
                    RecipeAction::Pipe(_) => true,
                    RecipeAction::Block(children) => statements_have_external_commands(children),
                    RecipeAction::Deliver(_) => false,
                }
        }
        Statement::Assignment(_) | Statement::Include(_) | Statement::Switch(_) => false,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Assignment(Assignment),
    Include(RcFileExpression),
    Switch(RcFileExpression),
    Recipe(Recipe),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcFileExpression {
    pub line: usize,
    pub value: String,
    pub(crate) expansion: Option<ExpansionExpression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub line: usize,
    pub name: String,
    pub value: String,
    pub target: AssignmentTarget,
    pub(crate) expansion: Option<ExpansionExpression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub line: usize,
    pub action_line: usize,
    pub options: RecipeOptions,
    pub lock: Option<PathExpression>,
    pub conditions: Vec<Condition>,
    pub action: RecipeAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecipeOptions {
    pub condition_input: ConditionInput,
    pub case_mode: CaseMode,
    pub control: ControlFlow,
    pub action_input: ActionInput,
    pub action_mode: ActionMode,
    pub continuation: ContinuationMode,
    pub child_status: ChildStatusMode,
    pub write_errors: WriteErrorMode,
    pub output_ending: OutputEnding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionInput {
    Headers,
    Body,
    Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseMode {
    Insensitive,
    Sensitive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFlow {
    Independent,
    AfterChainMatch,
    AfterPreviousSuccess,
    Else,
    AfterPreviousError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionInput {
    Message,
    Headers,
    Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionMode {
    Deliver,
    Filter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationMode {
    Stop,
    Continue,
    BranchBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStatusMode {
    Ignore,
    Wait,
    WaitQuietly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteErrorMode {
    Fail,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputEnding {
    Normalize,
    Preserve,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeAction {
    Deliver(Destination),
    Pipe(PipeAction),
    Block(Vec<Statement>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeAction {
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub line: usize,
    pub negated: bool,
    pub kind: ConditionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionKind {
    Regex(RegexCondition),
    AreaRegex {
        area: ConditionInput,
        regex: RegexCondition,
    },
    VariableRegex {
        name: String,
        regex: RegexCondition,
    },
    Program(String),
    SmallerThan(usize),
    LargerThan(usize),
}

#[derive(Debug, Clone)]
pub struct RegexCondition {
    pattern: String,
    compiled: Regex,
    match_capture: Option<usize>,
    capture_indexes: Vec<usize>,
}

impl RegexCondition {
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub(crate) fn compiled(&self) -> &Regex {
        &self.compiled
    }

    pub(crate) fn match_capture(&self) -> Option<usize> {
        self.match_capture
    }

    pub(crate) fn capture_indexes(&self) -> &[usize] {
        &self.capture_indexes
    }
}

impl PartialEq for RegexCondition {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
    }
}

impl Eq for RegexCondition {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    Mbox(PathExpression),
    Maildir(PathExpression),
}

#[derive(Debug, Clone)]
pub struct PathExpression {
    pub(crate) source: String,
    pub(crate) base: Option<String>,
    pub(crate) line: usize,
    pub(crate) runtime_dependent: bool,
    pub(crate) runtime_base: bool,
    pub(crate) expansion: Option<ExpansionExpression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpansionExpression {
    pub(crate) parts: Vec<ExpansionPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExpansionPart {
    Literal(String),
    Variable {
        name: String,
        default: Option<ExpansionExpression>,
    },
}

impl PartialEq for PathExpression {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.base == other.base
    }
}

impl Eq for PathExpression {}

impl From<&str> for PathExpression {
    fn from(source: &str) -> Self {
        Self {
            source: source.to_owned(),
            base: None,
            line: 0,
            runtime_dependent: false,
            runtime_base: false,
            expansion: None,
        }
    }
}

impl From<String> for PathExpression {
    fn from(source: String) -> Self {
        Self {
            source,
            base: None,
            line: 0,
            runtime_dependent: false,
            runtime_base: false,
            expansion: None,
        }
    }
}

impl PathExpression {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn line(&self) -> usize {
        self.line
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
    resource_limit: bool,
}

impl ParseError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
            resource_limit: false,
        }
    }

    fn limit(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
            resource_limit: true,
        }
    }

    pub(crate) fn is_resource_limit(&self) -> bool {
        self.resource_limit
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_maildir_assignment_selects_staging_base() {
        let config = parse("MAILDIR=old\nMAILDIR=/srv/mail\n").unwrap();

        assert_eq!(config.maildir(), Some("/srv/mail"));
    }
}
