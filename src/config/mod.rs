// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

mod expand;
mod parser;
mod variables;

use std::fmt;

use regex::bytes::Regex;

pub use expand::ExpansionError;
pub use parser::parse;
pub use variables::{
    AssignmentTarget, MAX_COMMAND_LINE_VARIABLES, MessageLimitVariable, SuppliedVariable,
    SuppliedVariableError, VariablePolicy, VariableSource, variable_policy,
};

pub const MAX_ASSIGNMENT_NAME_LEN: usize = 128;
pub const MAX_ASSIGNMENT_VALUE_LEN: usize = 64 * 1024;
pub const MAX_CONDITIONS_PER_RECIPE: usize = 256;
pub const MAX_EXPANSION_DEPTH: usize = 32;
pub const MAX_PATH_EXPRESSION_LEN: usize = 4096;
pub const MAX_RECIPE_NESTING_DEPTH: usize = 64;
pub const MAX_REGEX_COMPILED_SIZE: usize = 8 * 1024 * 1024;
pub const MAX_REGEX_PATTERN_LEN: usize = 64 * 1024;
pub const MAX_RC_REGEXES: usize = 32;
pub const MAX_RC_SIZE: usize = 1024 * 1024;
pub const MAX_RC_CONDITIONS: usize = 4096;
pub const MAX_RC_RECIPES: usize = 1024;
pub const MAX_RC_STATEMENTS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub statements: Vec<Statement>,
    pub(crate) initial_variables: Vec<(String, String)>,
}

impl Config {
    pub fn expand(self) -> Result<Self, ExpansionError> {
        expand::expand(self, &[])
    }

    pub fn expand_with(self, supplied: &[SuppliedVariable]) -> Result<Self, ExpansionError> {
        expand::expand(self, supplied)
    }

    pub fn maildir(&self) -> Option<&str> {
        self.statements.iter().rev().find_map(|statement| {
            let Statement::Assignment(assignment) = statement else {
                return None;
            };
            (assignment.target == AssignmentTarget::Maildir).then_some(assignment.value.as_str())
        })
    }

    pub(crate) fn initial_variables(&self) -> &[(String, String)] {
        &self.initial_variables
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Assignment(Assignment),
    Recipe(Recipe),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub line: usize,
    pub name: String,
    pub value: String,
    pub target: AssignmentTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub line: usize,
    pub action_line: usize,
    pub flags: String,
    pub lock: Option<String>,
    pub conditions: Vec<Condition>,
    pub action: RecipeAction,
}

impl Recipe {
    pub fn has_flag(&self, flag: char) -> bool {
        self.flags.contains(flag)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeAction {
    Deliver(Destination),
    Block(Vec<Statement>),
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
    SmallerThan(usize),
    LargerThan(usize),
}

#[derive(Debug, Clone)]
pub struct RegexCondition {
    pattern: String,
    compiled: Regex,
}

impl RegexCondition {
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub(crate) fn compiled(&self) -> &Regex {
        &self.compiled
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
}

impl ParseError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
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
