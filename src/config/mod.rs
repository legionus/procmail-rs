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
pub const MAX_PATH_EXPRESSION_LEN: usize = 4096;
pub const MAX_RECIPE_NESTING_DEPTH: usize = 0;
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
    pub destination: Destination,
}

impl Recipe {
    pub fn has_flag(&self, flag: char) -> bool {
        self.flags.contains(flag)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
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
    Mbox(String),
    Maildir(String),
    Auto(String),
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
