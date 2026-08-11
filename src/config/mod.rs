mod parser;

use std::fmt;

pub(crate) use parser::build_regex;
pub use parser::parse;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub statements: Vec<Statement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Assignment(Assignment),
    Recipe(Recipe),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
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
    Regex(String),
    SmallerThan(usize),
    LargerThan(usize),
}

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
