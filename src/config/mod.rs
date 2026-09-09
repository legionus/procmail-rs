// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

pub(crate) mod expand;
mod parser;
pub(crate) mod shell_eval;
pub(crate) mod shell_pattern;
mod variables;

use std::fmt;

use regex::bytes::Regex;

pub use expand::ExpansionError;
pub use parser::parse;
pub(crate) use parser::{parse_reparsed_condition, parse_with_state};
pub(crate) use variables::AssignmentPath;
pub use variables::{
    AssignmentTarget, DEFAULT_LOCK_EXT, DEFAULT_UMASK, MAX_COMMAND_LINE_VARIABLES,
    MAX_LOCK_SLEEP_SECONDS, MAX_LOCK_TIMEOUT_SECONDS, MAX_PROCESS_TIMEOUT_SECONDS,
    MessageLimitVariable, RcLimitVariable, SuppliedVariable, SuppliedVariableError,
    UNSUPPORTED_PROCMAIL_VARIABLES, VariablePolicy, VariableSource, parse_lock_sleep_seconds,
    parse_lock_timeout_seconds, parse_process_timeout_seconds, parse_umask, validate_lock_ext,
    validate_lock_method, validate_log_abstract, validate_trap_command, variable_policy,
};

pub const MAX_ASSIGNMENT_NAME_LEN: usize = 128;
pub const MAX_ASSIGNMENT_VALUE_LEN: usize = 64 * 1024;

pub fn umask_from_config(config: &Config) -> Result<u32, String> {
    let mut mask = DEFAULT_UMASK;
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
pub const MAX_HEADER_OPERATIONS_PER_ACTION: usize = 256;
pub const MAX_RECIPE_NESTING_DEPTH: usize = 64;
pub const MAX_REGEX_COMPILED_SIZE: usize = 8 * 1024 * 1024;
pub const MAX_REGEX_PATTERN_LEN: usize = 64 * 1024;
pub const MAX_REGEX_CAPTURES: usize = 64;
pub const MAX_REGEX_MATCH_MARKERS: usize = 64;
pub const MAX_REGEX_AST_NESTING: u32 = 256;
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
pub(crate) struct ParseBudget {
    counts: RcParseCounts,
    limits: RcLimits,
}

impl ParseBudget {
    pub(crate) fn reset(&mut self, counts: RcParseCounts) {
        self.counts = counts;
        self.limits = RcLimits::default();
    }

    pub(crate) fn replace_limits(&mut self, limits: RcLimits) {
        self.limits = limits;
    }

    #[cfg(test)]
    pub(crate) fn set_linebuf(&mut self, linebuf: usize) {
        self.limits.linebuf = linebuf;
    }
}

impl Config {
    pub fn expand(self, supplied: &[SuppliedVariable]) -> Result<Self, ExpansionError> {
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

    pub fn for_each_compatibility_warning(&self, mut report: impl FnMut(usize, char)) {
        fn visit(statements: &[Statement], report: &mut impl FnMut(usize, char)) {
            for statement in statements {
                let Statement::Recipe(recipe) = statement else {
                    continue;
                };
                let RecipeAction::Block(children) = &recipe.action else {
                    continue;
                };

                // Original procmail accepts these action-specific flags on a
                // block but cannot apply their pipe behavior there. Report
                // that compatibility choice before visiting nested recipes so
                // diagnostics retain source order without another collection.
                if recipe.options.write_errors == WriteErrorMode::Ignore {
                    report(recipe.line, 'i');
                }
                if recipe.options.output_ending == OutputEnding::Preserve {
                    report(recipe.line, 'r');
                }
                visit(children, report);
            }
        }

        visit(&self.statements, &mut report);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Assignment(Assignment),
    CommandAssignment(CommandAssignment),
    Include(RcFileExpression),
    Switch(RcFileExpression),
    Recipe(Recipe),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandAssignment {
    pub line: usize,
    pub name: String,
    pub source: String,
    pub target: AssignmentTarget,
    pub(crate) expression: ShellExpression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcFileExpression {
    pub line: usize,
    pub value: String,
    pub(crate) expansion: Option<ShellExpression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub line: usize,
    pub name: String,
    pub value: String,
    pub target: AssignmentTarget,
    pub(crate) expansion: Option<ShellExpression>,
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
    Capture(CaptureAction),
    Block(Vec<Statement>),
    Headers(HeaderAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureAction {
    pub line: usize,
    pub name: String,
    pub target: AssignmentTarget,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderAction {
    pub operations: Vec<HeaderOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderOperation {
    Remove {
        line: usize,
        name: String,
    },
    Set {
        line: usize,
        name: String,
        value: HeaderValue,
    },
    Add {
        line: usize,
        name: String,
        value: HeaderValue,
    },
    Prepend {
        line: usize,
        name: String,
        value: HeaderValue,
    },
    Rename {
        line: usize,
        from: String,
        to: String,
    },
    Extract {
        line: usize,
        name: String,
        target: String,
        mode: HeaderExtractionMode,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderExtractionMode {
    Raw,
    Unfolded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderValue {
    pub source: String,
    pub(crate) expansion: Option<ShellExpression>,
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
    ShellExpanded(ShellExpandedCondition),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellExpandedCondition {
    pub(crate) source: String,
    pub(crate) expansion: Option<ShellExpression>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellExpression {
    pub(crate) parts: Vec<ShellPart>,
}

impl ShellExpression {
    pub(crate) fn has_commands(&self) -> bool {
        self.parts.iter().any(|part| match part {
            ShellPart::Command(_) => true,
            ShellPart::PatternQuote(expression) => expression.has_commands(),
            ShellPart::Variable { operation, .. } => {
                operation.word().is_some_and(ShellExpression::has_commands)
            }
            ShellPart::Literal(_) | ShellPart::RegexQuotedVariable(_) => false,
        })
    }

    pub(crate) fn has_assignments(&self) -> bool {
        self.parts.iter().any(|part| match part {
            ShellPart::Variable { operation, .. } => operation.has_assignments(),
            ShellPart::PatternQuote(expression) => expression.has_assignments(),
            ShellPart::Literal(_) | ShellPart::RegexQuotedVariable(_) | ShellPart::Command(_) => {
                false
            }
        })
    }

    pub(crate) fn requires_ordered_evaluation(&self) -> bool {
        self.has_commands() || self.has_assignments()
    }

    pub(crate) fn for_each_assignment(&self, visit: &mut impl FnMut(&str)) {
        for part in &self.parts {
            let ShellPart::Variable { name, operation } = part else {
                if let ShellPart::PatternQuote(expression) = part {
                    expression.for_each_assignment(visit);
                }
                continue;
            };
            if matches!(operation, ParameterOperation::AssignIfUnsetOrEmpty(_)) {
                visit(name);
            }
            if let Some(word) = operation.word() {
                word.for_each_assignment(visit);
            }
        }
    }

    pub(crate) fn literal_text(&self) -> Option<String> {
        let mut value = String::new();
        for part in &self.parts {
            let ShellPart::Literal(text) = part else {
                return None;
            };
            value.push_str(text);
        }
        Some(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellPart {
    Literal(String),
    Variable {
        name: String,
        operation: ParameterOperation,
    },
    RegexQuotedVariable(String),
    Command(String),
    PatternQuote(Box<ShellExpression>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParameterOperation {
    Value,
    DefaultIfUnset(ShellExpression),
    DefaultIfUnsetOrEmpty(ShellExpression),
    AlternateIfSet(ShellExpression),
    AlternateIfSetAndNotEmpty(ShellExpression),
    AssignIfUnsetOrEmpty(ShellExpression),
    ErrorIfUnsetOrEmpty(ShellExpression),
    Length,
    RemovePrefix {
        pattern: ShellExpression,
        longest: bool,
    },
    RemoveSuffix {
        pattern: ShellExpression,
        longest: bool,
    },
    ChangeCase {
        pattern: ShellExpression,
        direction: CaseDirection,
        all: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaseDirection {
    Upper,
    Lower,
}

impl ParameterOperation {
    pub(crate) fn word(&self) -> Option<&ShellExpression> {
        match self {
            Self::Value | Self::Length => None,
            Self::DefaultIfUnset(word)
            | Self::DefaultIfUnsetOrEmpty(word)
            | Self::AlternateIfSet(word)
            | Self::AlternateIfSetAndNotEmpty(word)
            | Self::AssignIfUnsetOrEmpty(word) => Some(word),
            Self::ErrorIfUnsetOrEmpty(_) => None,
            Self::RemovePrefix { pattern, .. }
            | Self::RemoveSuffix { pattern, .. }
            | Self::ChangeCase { pattern, .. } => Some(pattern),
        }
    }

    pub(crate) fn selects_word(&self, is_set: bool, is_empty: bool) -> bool {
        self.selected_word(is_set, is_empty).is_some()
    }

    pub(crate) fn selected_word(&self, is_set: bool, is_empty: bool) -> Option<&ShellExpression> {
        match self {
            Self::DefaultIfUnset(word) if !is_set => Some(word),
            Self::DefaultIfUnsetOrEmpty(word) if !is_set || is_empty => Some(word),
            Self::AlternateIfSet(word) if is_set => Some(word),
            Self::AlternateIfSetAndNotEmpty(word) if is_set && !is_empty => Some(word),
            Self::AssignIfUnsetOrEmpty(word) if !is_set || is_empty => Some(word),
            Self::Value
            | Self::DefaultIfUnset(_)
            | Self::DefaultIfUnsetOrEmpty(_)
            | Self::AlternateIfSet(_)
            | Self::AlternateIfSetAndNotEmpty(_)
            | Self::AssignIfUnsetOrEmpty(_)
            | Self::ErrorIfUnsetOrEmpty(_)
            | Self::Length
            | Self::RemovePrefix { .. }
            | Self::RemoveSuffix { .. }
            | Self::ChangeCase { .. } => None,
        }
    }

    pub(crate) fn requires_value(&self) -> bool {
        matches!(
            self,
            Self::Value
                | Self::Length
                | Self::RemovePrefix { .. }
                | Self::RemoveSuffix { .. }
                | Self::ChangeCase { .. }
        )
    }

    pub(crate) fn is_value_transform(&self) -> bool {
        matches!(
            self,
            Self::Length
                | Self::RemovePrefix { .. }
                | Self::RemoveSuffix { .. }
                | Self::ChangeCase { .. }
        )
    }

    pub(crate) fn evaluates_word(&self, is_set: bool, is_empty: bool) -> bool {
        match self {
            Self::RemovePrefix { .. } | Self::RemoveSuffix { .. } | Self::ChangeCase { .. } => {
                is_set
            }
            _ => self.selects_word(is_set, is_empty),
        }
    }

    pub(crate) fn uses_value_when_word_is_not_selected(&self) -> bool {
        matches!(
            self,
            Self::Value
                | Self::DefaultIfUnset(_)
                | Self::DefaultIfUnsetOrEmpty(_)
                | Self::ErrorIfUnsetOrEmpty(_)
                | Self::Length
                | Self::RemovePrefix { .. }
                | Self::RemoveSuffix { .. }
                | Self::ChangeCase { .. }
        )
    }

    fn has_assignments(&self) -> bool {
        matches!(self, Self::AssignIfUnsetOrEmpty(_))
            || self.word().is_some_and(ShellExpression::has_assignments)
    }
}

#[derive(Debug, Clone)]
pub struct RegexCondition {
    pattern: String,
    compiled: Regex,
    match_captures: Vec<usize>,
    capture_indexes: Vec<usize>,
}

impl RegexCondition {
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub(crate) fn compiled(&self) -> &Regex {
        &self.compiled
    }

    pub(crate) fn match_captures(&self) -> &[usize] {
        &self.match_captures
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
    File(PathExpression),
    Discard(PathExpression),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationKind {
    Maildir,
    Mbox,
    File,
    Discard,
}

impl Destination {
    pub fn kind(&self) -> DestinationKind {
        match self {
            Self::Maildir(_) => DestinationKind::Maildir,
            Self::Mbox(_) => DestinationKind::Mbox,
            Self::File(_) => DestinationKind::File,
            Self::Discard(_) => DestinationKind::Discard,
        }
    }

    pub fn requires_ordered_delivery(&self) -> bool {
        matches!(self.kind(), DestinationKind::Mbox | DestinationKind::File)
    }

    pub fn supports_fanout_delivery(&self) -> bool {
        matches!(
            self.kind(),
            DestinationKind::Maildir | DestinationKind::Discard
        )
    }
}

#[derive(Debug, Clone)]
pub struct PathExpression {
    pub(crate) source: String,
    pub(crate) base: Option<String>,
    pub(crate) line: usize,
    pub(crate) runtime_dependent: bool,
    pub(crate) runtime_base: bool,
    pub(crate) typed_destination: bool,
    pub(crate) expansion: Option<ShellExpression>,
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
            typed_destination: false,
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
            typed_destination: false,
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
#[path = "../tests/config/mod.rs"]
mod tests;
