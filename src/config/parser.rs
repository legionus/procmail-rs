// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use regex::bytes::RegexBuilder;
use regex_syntax::ast;
use regex_syntax::ast::parse::ParserBuilder as AstParserBuilder;

use crate::bounded_bytes::{BoundedBytes, BoundedBytesError};

use super::{
    ActionInput, ActionMode, Assignment, AssignmentTarget, CaptureAction, CaseMode,
    ChildStatusMode, CommandAssignment, Condition, ConditionInput, ConditionKind, Config,
    ContinuationMode, ControlFlow, Destination, HeaderAction, HeaderOperation, HeaderValue,
    MAX_ASSIGNMENT_NAME_LEN, MAX_ASSIGNMENT_VALUE_LEN, MAX_HEADER_OPERATIONS_PER_ACTION,
    MAX_PATH_EXPRESSION_LEN, MAX_PIPE_COMMAND_LEN, MAX_RC_SIZE, MAX_REGEX_AST_NESTING,
    MAX_REGEX_CAPTURES, MAX_REGEX_COMPILED_SIZE, MAX_REGEX_MATCH_MARKERS, MAX_REGEX_PATTERN_LEN,
    OutputEnding, ParseBudget, ParseError, PathExpression, PipeAction, RcFileExpression, RcLimits,
    RcParseCounts, Recipe, RecipeAction, RecipeOptions, RegexCondition, ShellExpression, Statement,
    VariablePolicy, VariableSource, WriteErrorMode, variable_policy,
};

#[cfg(test)]
use super::{
    DEFAULT_LINEBUF, HARD_MAX_CONDITIONS_PER_RECIPE, HARD_MAX_RC_ASSIGNMENTS,
    HARD_MAX_RC_CONDITIONS, HARD_MAX_RC_RECIPES, HARD_MAX_RC_REGEXES, HARD_MAX_RC_STATEMENTS,
    HARD_MAX_RECIPE_NESTING_DEPTH, MAX_CONDITIONS_PER_RECIPE, MAX_LINEBUF, MAX_RC_CONDITIONS,
    MAX_RC_RECIPES, MAX_RC_REGEXES, MAX_RC_STATEMENTS, MAX_RECIPE_NESTING_DEPTH, MIN_LINEBUF,
};

pub fn parse(input: &str) -> Result<Config, ParseError> {
    let mut state = ParseBudget::default();
    parse_with_state(input, &mut state)
}

pub(crate) fn parse_with_state(input: &str, state: &mut ParseBudget) -> Result<Config, ParseError> {
    if input.len() > MAX_RC_SIZE {
        return Err(ParseError::limit(
            1,
            format!("rc file exceeds the hard limit of {MAX_RC_SIZE} bytes"),
        ));
    }

    let lines: Vec<&str> = input.lines().collect();
    let initial = state.snapshot();
    let initial_linebuf = state.linebuf();
    let (statements, _) = parse_statements(&lines, 0, 0, state)?;

    Ok(Config {
        statements,
        initial_variables: Vec::new(),
        parse_counts: state.counts_since(initial)?,
        initial_linebuf,
    })
}

impl RcParseCounts {
    fn subtract(self, earlier: Self) -> Result<Self, ParseError> {
        Ok(Self {
            assignments: self
                .assignments
                .checked_sub(earlier.assignments)
                .ok_or_else(|| ParseError::new(1, "rc assignment count moved backwards"))?,
            statements: self
                .statements
                .checked_sub(earlier.statements)
                .ok_or_else(|| ParseError::new(1, "rc statement count moved backwards"))?,
            recipes: self
                .recipes
                .checked_sub(earlier.recipes)
                .ok_or_else(|| ParseError::new(1, "rc recipe count moved backwards"))?,
            conditions: self
                .conditions
                .checked_sub(earlier.conditions)
                .ok_or_else(|| ParseError::new(1, "rc condition count moved backwards"))?,
            regexes: self
                .regexes
                .checked_sub(earlier.regexes)
                .ok_or_else(|| ParseError::new(1, "rc regex count moved backwards"))?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum AssignmentUse {
    Statement,
    CaptureAction,
}

impl ParseBudget {
    // Charge syntax before allocating its parsed representation, but apply a
    // limit-changing assignment only after that assignment has consumed the
    // preceding assignment budget. Keeping both steps here preserves source
    // order and prevents nested parsing from temporarily hiding its usage.
    fn snapshot(&self) -> RcParseCounts {
        self.counts
    }

    fn counts_since(&self, earlier: RcParseCounts) -> Result<RcParseCounts, ParseError> {
        self.counts.subtract(earlier)
    }

    fn linebuf(&self) -> usize {
        self.limits.linebuf
    }

    fn check_line(&self, line: &str, line_number: usize) -> Result<(), ParseError> {
        check_linebuf(line, line_number, self.linebuf())
    }

    fn check_statement(&self, line: usize) -> Result<(), ParseError> {
        check_count_limit(
            self.counts.statements,
            self.limits.statements,
            line,
            "statement",
        )
    }

    fn charge_recipe(&mut self, line: usize) -> Result<(), ParseError> {
        check_count_limit(self.counts.recipes, self.limits.recipes, line, "recipe")?;
        self.counts.recipes = self
            .counts
            .recipes
            .checked_add(1)
            .ok_or_else(|| ParseError::new(line, "rc recipe count overflows"))?;
        self.counts.statements = self
            .counts
            .statements
            .checked_add(1)
            .ok_or_else(|| ParseError::new(line, "rc statement count overflows"))?;
        Ok(())
    }

    fn charge_assignment(&mut self, line: usize, usage: AssignmentUse) -> Result<(), ParseError> {
        check_count_limit(
            self.counts.assignments,
            self.limits.assignments,
            line,
            "assignment",
        )?;
        self.counts.assignments = self
            .counts
            .assignments
            .checked_add(1)
            .ok_or_else(|| ParseError::new(line, "rc assignment count overflows"))?;
        if matches!(usage, AssignmentUse::Statement) {
            self.counts.statements = self
                .counts
                .statements
                .checked_add(1)
                .ok_or_else(|| ParseError::new(line, "rc statement count overflows"))?;
        }
        Ok(())
    }

    fn check_condition(&self, recipe_count: usize, line: usize) -> Result<(), ParseError> {
        if recipe_count >= self.limits.conditions_per_recipe {
            return Err(ParseError::limit(
                line,
                format!(
                    "recipe condition count exceeds the active limit of {}",
                    self.limits.conditions_per_recipe
                ),
            ));
        }
        let total = self
            .counts
            .conditions
            .checked_add(recipe_count)
            .ok_or_else(|| ParseError::new(line, "rc condition count overflows"))?;
        check_count_limit(total, self.limits.conditions, line, "condition")
    }

    fn check_regex(&self, recipe_count: usize, line: usize) -> Result<(), ParseError> {
        let total = self
            .counts
            .regexes
            .checked_add(recipe_count)
            .ok_or_else(|| ParseError::new(line, "rc regex count overflows"))?;
        check_count_limit(total, self.limits.regexes, line, "regex")
    }

    fn record_recipe_contents(
        &mut self,
        conditions: usize,
        regexes: usize,
        line: usize,
    ) -> Result<(), ParseError> {
        self.counts.conditions = self
            .counts
            .conditions
            .checked_add(conditions)
            .ok_or_else(|| ParseError::new(line, "rc condition count overflows"))?;
        self.counts.regexes = self
            .counts
            .regexes
            .checked_add(regexes)
            .ok_or_else(|| ParseError::new(line, "rc regex count overflows"))?;
        Ok(())
    }

    fn check_nesting(&self, depth: usize, line: usize) -> Result<usize, ParseError> {
        let next = depth
            .checked_add(1)
            .ok_or_else(|| ParseError::new(line, "recipe nesting depth overflows"))?;
        if next > self.limits.nesting_depth {
            return Err(ParseError::limit(
                line,
                format!(
                    "recipe nesting depth {next} exceeds the active limit of {}",
                    self.limits.nesting_depth
                ),
            ));
        }
        Ok(next)
    }

    fn apply_assignment(&mut self, assignment: &Assignment) -> Result<(), ParseError> {
        apply_rc_limit(assignment, &mut self.limits)?;
        apply_linebuf(assignment, &mut self.limits)
    }
}

fn parse_statements(
    lines: &[&str],
    mut index: usize,
    depth: usize,
    state: &mut ParseBudget,
) -> Result<(Vec<Statement>, usize), ParseError> {
    let mut statements = Vec::new();
    while index < lines.len() {
        let line_number = index + 1;
        state.check_line(lines[index], line_number)?;
        let line = lines[index].trim();

        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }

        if line == "}" {
            if depth == 0 {
                return Err(ParseError::new(
                    line_number,
                    "closing recipe block has no matching opening block",
                ));
            }
            return Ok((statements, index + 1));
        }

        state.check_statement(line_number)?;

        if line.starts_with(':') {
            state.charge_recipe(line_number)?;
            let (recipe, next) = parse_recipe(lines, index, depth, state)?;
            statements.push(Statement::Recipe(recipe));
            index = next;
            continue;
        }

        if let Some(assignment) = parse_assignment(lines[index].trim_start(), line_number)? {
            state.charge_assignment(line_number, AssignmentUse::Statement)?;
            if depth != 0 && assignment.target.controls_rc_parsing() {
                return Err(ParseError::new(
                    line_number,
                    format!(
                        "variable {} cannot be assigned inside a recipe block",
                        assignment.name
                    ),
                ));
            }
            let statement = if assignment
                .expansion
                .as_ref()
                .is_some_and(ShellExpression::has_commands)
            {
                let mut assignment = assignment;
                let expression = assignment.expansion.take().ok_or_else(|| {
                    ParseError::new(assignment.line, "parsed assignment expression is missing")
                })?;
                if assignment.target.controls_rc_parsing() {
                    return Err(ParseError::new(
                        assignment.line,
                        format!(
                            "variable {} cannot be set from command output because it controls rc parsing",
                            assignment.name
                        ),
                    ));
                }
                Statement::CommandAssignment(CommandAssignment {
                    line: assignment.line,
                    name: assignment.name,
                    source: assignment.value,
                    target: assignment.target,
                    expression,
                })
            } else {
                match assignment.name.as_str() {
                    "INCLUDERC" => Statement::Include(RcFileExpression {
                        line: assignment.line,
                        value: assignment.value,
                        expansion: assignment.expansion,
                    }),
                    "SWITCHRC" => Statement::Switch(RcFileExpression {
                        line: assignment.line,
                        value: assignment.value,
                        expansion: assignment.expansion,
                    }),
                    _ => Statement::Assignment(assignment),
                }
            };
            if let Statement::Assignment(assignment) = &statement {
                state.apply_assignment(assignment)?;
            }
            statements.push(statement);
            index += 1;
            continue;
        }

        return Err(ParseError::new(
            line_number,
            "expected an assignment or a recipe beginning with ':0'",
        ));
    }

    if depth > 0 {
        return Err(ParseError::new(
            lines.len().max(1),
            "recipe block has no closing brace",
        ));
    }
    Ok((statements, index))
}

fn check_linebuf(line: &str, line_number: usize, limit: usize) -> Result<(), ParseError> {
    if line.len() > limit {
        return Err(ParseError::limit(
            line_number,
            format!("rc line exceeds the active LINEBUF limit of {limit} bytes"),
        ));
    }
    Ok(())
}

fn apply_linebuf(assignment: &Assignment, limits: &mut RcLimits) -> Result<(), ParseError> {
    if assignment.target != AssignmentTarget::LineBuf {
        return Ok(());
    }
    let value = assignment.value.parse::<usize>().map_err(|_| {
        ParseError::new(
            assignment.line,
            "LINEBUF must be an unsigned decimal integer",
        )
    })?;
    if !(super::MIN_LINEBUF..=super::MAX_LINEBUF).contains(&value) {
        return Err(ParseError::limit(
            assignment.line,
            format!(
                "LINEBUF must be from {} through {} bytes",
                super::MIN_LINEBUF,
                super::MAX_LINEBUF
            ),
        ));
    }
    limits.linebuf = value;
    Ok(())
}

fn check_count_limit(
    count: usize,
    limit: usize,
    line: usize,
    name: &str,
) -> Result<(), ParseError> {
    if count >= limit {
        return Err(ParseError::limit(
            line,
            format!("rc {name} count exceeds the active limit of {limit}"),
        ));
    }
    Ok(())
}

fn apply_rc_limit(assignment: &Assignment, limits: &mut RcLimits) -> Result<(), ParseError> {
    let AssignmentTarget::RcLimit(kind) = assignment.target else {
        return Ok(());
    };
    let value = assignment.value.parse::<usize>().map_err(|_| {
        ParseError::new(
            assignment.line,
            format!("{} must be an unsigned decimal integer", assignment.name),
        )
    })?;
    if let Err(hard_limit) = limits.set(kind, value) {
        return Err(ParseError::limit(
            assignment.line,
            format!("{} exceeds the hard limit of {hard_limit}", assignment.name),
        ));
    }
    Ok(())
}

fn parse_assignment(line: &str, line_number: usize) -> Result<Option<Assignment>, ParseError> {
    let (name, value) = match line.split_once('=') {
        Some(parts) => parts,
        None if line == "HOST" => ("HOST", ""),
        None => return Ok(None),
    };
    let name = name.trim();
    if name.len() > MAX_ASSIGNMENT_NAME_LEN {
        return Err(ParseError::new(
            line_number,
            format!("assignment name exceeds the hard limit of {MAX_ASSIGNMENT_NAME_LEN} bytes"),
        ));
    }
    if name.is_empty()
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphanumeric() && (index > 0 || !byte.is_ascii_digit())
        })
    {
        return Ok(None);
    }
    let parsed = super::expand::parse_assignment_word(value.trim_start(), line_number)
        .map_err(|error| ParseError::new(error.line, error.message))?;
    let mut value = parsed.source;
    if value.len() > MAX_ASSIGNMENT_VALUE_LEN {
        return Err(ParseError::new(
            line_number,
            format!("assignment value exceeds the hard limit of {MAX_ASSIGNMENT_VALUE_LEN} bytes"),
        ));
    }
    let policy = variable_policy(name);
    if policy == VariablePolicy::Unsupported {
        return Err(ParseError::new(
            line_number,
            format!("procmail variable {name} is not supported"),
        ));
    }
    let target = policy
        .assignment_target(VariableSource::RcFile)
        .ok_or_else(|| {
            ParseError::new(
                line_number,
                format!("variable {name} cannot be assigned in an rc file"),
            )
        })?;
    if target.controls_rc_parsing()
        && let Some(literal) = parsed.expression.literal_text()
    {
        value = literal;
    }
    let limit = target.value_limit();
    if value.len() > limit {
        let kind = if target.uses_path_error_label() {
            "path"
        } else {
            "value"
        };
        return Err(ParseError::new(
            line_number,
            format!("{name} {kind} exceeds the hard limit of {limit} bytes"),
        ));
    }

    Ok(Some(Assignment {
        line: line_number,
        name: name.to_owned(),
        value,
        target,
        expansion: Some(parsed.expression),
    }))
}

fn parse_recipe(
    lines: &[&str],
    start: usize,
    depth: usize,
    state: &mut ParseBudget,
) -> Result<(Recipe, usize), ParseError> {
    let header = lines[start].trim();
    let rest = header
        .strip_prefix(":0")
        .ok_or_else(|| ParseError::new(start + 1, "only ':0' recipes are supported"))?;
    let (mut options, lock, recipe_flags) = parse_recipe_header(rest, start + 1)?;
    let mut conditions = Vec::new();
    let mut regex_count = 0usize;
    let mut index = start + 1;

    while index < lines.len() {
        state.check_line(lines[index], index + 1)?;
        let line = lines[index].trim();
        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }
        if line.starts_with('*') {
            // Reject excess conditions before parsing can allocate their text
            // or compile a regular expression. The local and file-wide
            // budgets are separate because either shape can make later plan
            // construction disproportionately expensive.
            state.check_condition(conditions.len(), index + 1)?;
            let (source, next) = parse_condition_continuation(lines, index, state.linebuf())?;
            let (condition, is_regex) = parse_condition(
                &source,
                index + 1,
                options.case_mode == CaseMode::Sensitive,
                regex_count,
                state,
            )?;
            conditions.push(condition);
            regex_count = regex_count
                .checked_add(usize::from(is_regex))
                .ok_or_else(|| ParseError::new(index + 1, "recipe regex count overflows"))?;
            index = next;
            continue;
        }
        break;
    }

    let action = lines
        .get(index)
        .map(|line| {
            state.check_line(line, index + 1)?;
            Ok(line.trim())
        })
        .transpose()?
        .ok_or_else(|| ParseError::new(start + 1, "recipe has no action"))?;

    // Charge the parent recipe before descending into a block so nested
    // parsing cannot temporarily hide conditions or regexes from file-wide
    // limits.
    state.record_recipe_contents(conditions.len(), regex_count, start + 1)?;

    if action.starts_with('!') {
        return Err(ParseError::new(
            index + 1,
            "forward actions are not supported",
        ));
    }
    if action == "}" {
        return Err(ParseError::new(
            index + 1,
            "closing recipe block has no matching opening block",
        ));
    }
    if action.starts_with(':') {
        return Err(ParseError::new(start + 1, "recipe has no action"));
    }
    if action.is_empty() {
        return Err(ParseError::new(index + 1, "recipe action is empty"));
    }

    let capture = parse_capture_action_prefix(action, index + 1)?;
    let is_pipe = action.starts_with('|');
    let is_command_action = is_pipe || capture.is_some();
    let is_headers = action == "headers {";
    if !is_command_action && !is_headers && action != "{" {
        validate_destination_syntax(action, index + 1)?;
    }
    if is_headers {
        // Header edits are internal transformations rather than deliveries or
        // child processes. Reject options whose meaning depends on either,
        // and encode their mandatory fall-through here so every evaluator
        // receives the same behavior without consulting the original text.
        validate_header_action_recipe(&recipe_flags, lock.as_deref(), start + 1)?;
        options.continuation = ContinuationMode::Continue;
    }
    let has_program_condition = conditions.iter().any(|condition| {
        matches!(
            condition.kind,
            ConditionKind::Program(_) | ConditionKind::ShellExpanded(_)
        )
    });
    if !is_command_action
        && !is_headers
        && action != "{"
        && options.write_errors == WriteErrorMode::Ignore
    {
        return Err(ParseError::new(
            start + 1,
            "recipe flag 'i' is not supported for filesystem delivery because it may publish an incomplete message",
        ));
    }
    if !is_command_action
        && !is_headers
        && (options.action_input != ActionInput::Message
            || options.action_mode != ActionMode::Deliver
            || (!has_program_condition
                && (options.child_status != ChildStatusMode::Ignore
                    || (action != "{" && options.write_errors != WriteErrorMode::Fail))))
    {
        return Err(ParseError::new(
            start + 1,
            "flags h, b, and f require a pipe action; flags w and W require a pipe action or program condition",
        ));
    }

    let (action, next) = if let Some((name, target)) = capture {
        state.charge_assignment(index + 1, AssignmentUse::CaptureAction)?;
        let command_text = action
            .split_once("=|")
            .map(|(_, command)| command)
            .ok_or_else(|| ParseError::new(index + 1, "capture action is malformed"))?
            .trim_start();
        let (command, next) = parse_command_continuation(
            lines,
            index,
            command_text,
            state.linebuf(),
            "capture action command",
        )?;
        (
            RecipeAction::Capture(CaptureAction {
                line: index + 1,
                name,
                target,
                command,
            }),
            next,
        )
    } else if is_pipe {
        let (command, next) = parse_pipe_command(lines, index, state.linebuf())?;
        if command.is_empty() && options.action_mode == ActionMode::Filter {
            return Err(ParseError::new(
                index + 1,
                "recipe flag 'f' requires a pipe command",
            ));
        }
        (RecipeAction::Pipe(PipeAction { command }), next)
    } else if is_headers {
        let (action, next) = parse_header_action(lines, index, state.linebuf())?;
        (RecipeAction::Headers(action), next)
    } else if action == "{" {
        let next_depth = state.check_nesting(depth, index + 1)?;
        if lock.as_deref() == Some("") {
            return Err(ParseError::new(
                start + 1,
                "an implicit local lockfile cannot be derived for a recipe block",
            ));
        }
        if options.continuation == ContinuationMode::Continue {
            return Err(ParseError::new(
                start + 1,
                "copy flag 'c' on recipe blocks is not supported yet",
            ));
        }
        let (statements, next) = parse_statements(lines, index + 1, next_depth, state)?;
        (RecipeAction::Block(statements), next)
    } else if action.starts_with('{') {
        return Err(ParseError::new(
            index + 1,
            "opening recipe block must be a standalone '{' action",
        ));
    } else if let Some(path) = action.strip_prefix("mbox:") {
        (
            RecipeAction::Deliver(Destination::Mbox(destination_path_expression(
                required_path(path, index + 1, "destination path")?,
                index + 1,
                true,
            )?)),
            index + 1,
        )
    } else if let Some(path) = action.strip_prefix("maildir:") {
        (
            RecipeAction::Deliver(Destination::Maildir(destination_path_expression(
                required_path(path, index + 1, "destination path")?,
                index + 1,
                true,
            )?)),
            index + 1,
        )
    } else if action.ends_with('/') {
        (
            RecipeAction::Deliver(Destination::Maildir(destination_path_expression(
                required_path(action, index + 1, "destination path")?,
                index + 1,
                false,
            )?)),
            index + 1,
        )
    } else {
        (
            RecipeAction::Deliver(Destination::File(destination_path_expression(
                required_path(action, index + 1, "destination path")?,
                index + 1,
                false,
            )?)),
            index + 1,
        )
    };

    let recipe = Recipe {
        line: start + 1,
        action_line: index + 1,
        options,
        lock: lock.map(PathExpression::from),
        conditions,
        action,
    };
    Ok((recipe, next))
}

fn validate_destination_syntax(action: &str, line: usize) -> Result<(), ParseError> {
    if !action.starts_with("mbox:")
        && !action.starts_with("maildir:")
        && destination_has_literal_whitespace(action)
    {
        return Err(ParseError::new(
            line,
            "multiple unmarked mailbox destinations are not supported",
        ));
    }
    Ok(())
}

fn destination_has_literal_whitespace(action: &str) -> bool {
    let mut in_command = false;
    let mut escaped = false;
    for byte in action.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
        } else if byte == b'`' {
            in_command = !in_command;
        } else if !in_command && byte.is_ascii_whitespace() {
            return true;
        }
    }
    false
}

fn destination_path_expression(
    source: String,
    line: usize,
    typed_destination: bool,
) -> Result<PathExpression, ParseError> {
    let expansion = super::expand::parse_command_expression(&source, line)
        .map_err(|error| ParseError::new(error.line, error.message))?;
    Ok(PathExpression {
        source,
        base: None,
        line,
        runtime_dependent: expansion.is_some(),
        runtime_base: false,
        typed_destination,
        expansion,
    })
}

fn parse_capture_action_prefix(
    action: &str,
    line: usize,
) -> Result<Option<(String, AssignmentTarget)>, ParseError> {
    let Some((name, _)) = action.split_once("=|") else {
        return Ok(None);
    };
    let name = name.trim();
    if name.is_empty()
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphanumeric() && (index > 0 || !byte.is_ascii_digit())
        })
    {
        return Ok(None);
    }
    if name.len() > MAX_ASSIGNMENT_NAME_LEN {
        return Err(ParseError::new(
            line,
            format!("assignment name exceeds the hard limit of {MAX_ASSIGNMENT_NAME_LEN} bytes"),
        ));
    }
    let policy = variable_policy(name);
    if policy == VariablePolicy::Unsupported {
        return Err(ParseError::new(
            line,
            format!("procmail variable {name} is not supported"),
        ));
    }
    let target = policy
        .assignment_target(VariableSource::RcFile)
        .ok_or_else(|| ParseError::new(line, format!("variable {name} cannot be assigned")))?;
    Ok(Some((name.to_owned(), target)))
}

fn parse_header_action(
    lines: &[&str],
    opening: usize,
    linebuf: usize,
) -> Result<(HeaderAction, usize), ParseError> {
    let mut operations = Vec::new();
    let mut index = opening + 1;

    // Bound the operation list independently of the rc byte ceiling because
    // even very short operations create typed nodes and later editing work.
    // Stop before parsing the excess line so it cannot allocate another node.
    while let Some(raw) = lines.get(index) {
        check_linebuf(raw, index + 1, linebuf)?;
        let text = raw.trim();
        if text.is_empty() || text.starts_with('#') {
            index += 1;
            continue;
        }
        if text == "}" {
            return Ok((HeaderAction { operations }, index + 1));
        }
        if operations.len() == MAX_HEADER_OPERATIONS_PER_ACTION {
            return Err(ParseError::limit(
                index + 1,
                format!(
                    "header operation count exceeds the hard limit of {MAX_HEADER_OPERATIONS_PER_ACTION}"
                ),
            ));
        }

        operations.push(parse_header_operation(text, index + 1)?);
        index += 1;
    }

    Err(ParseError::new(
        opening + 1,
        "headers action has no closing '}'",
    ))
}

fn validate_header_action_recipe(
    flags: &str,
    lock: Option<&str>,
    line: usize,
) -> Result<(), ParseError> {
    if lock.is_some() {
        return Err(ParseError::new(
            line,
            "headers actions do not support local lockfiles",
        ));
    }
    if let Some(flag) = flags
        .chars()
        .find(|flag| matches!(flag, 'h' | 'b' | 'f' | 'w' | 'W' | 'i' | 'r'))
    {
        return Err(ParseError::new(
            line,
            format!("recipe flag '{flag}' is not supported for headers actions"),
        ));
    }
    Ok(())
}

fn parse_header_operation(text: &str, line: usize) -> Result<HeaderOperation, ParseError> {
    let (operation, arguments) = text.split_once(char::is_whitespace).ok_or_else(|| {
        ParseError::new(line, format!("header operation '{text}' has no arguments"))
    })?;
    let arguments = arguments.trim_start();

    if operation == "remove" {
        validate_header_name(arguments, line)?;
        return Ok(HeaderOperation::Remove {
            line,
            name: arguments.to_owned(),
        });
    }

    if !matches!(operation, "set" | "add" | "prepend") {
        return Err(ParseError::new(
            line,
            format!("unknown headers operation '{operation}'"),
        ));
    }

    let (name, value) = arguments.split_once(':').ok_or_else(|| {
        ParseError::new(
            line,
            format!("header operation '{operation}' requires NAME: VALUE"),
        )
    })?;
    let name = name.trim();
    validate_header_name(name, line)?;
    let value = value.trim_start();
    if value.ends_with('\\') {
        return Err(ParseError::new(
            line,
            "folded header values are not supported",
        ));
    }
    if value.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0)) {
        return Err(ParseError::new(
            line,
            "header value contains a forbidden byte",
        ));
    }
    let fields = (
        line,
        name.to_owned(),
        HeaderValue {
            source: value.to_owned(),
            expansion: None,
        },
    );
    match operation {
        "set" => Ok(HeaderOperation::Set {
            line: fields.0,
            name: fields.1,
            value: fields.2,
        }),
        "add" => Ok(HeaderOperation::Add {
            line: fields.0,
            name: fields.1,
            value: fields.2,
        }),
        "prepend" => Ok(HeaderOperation::Prepend {
            line: fields.0,
            name: fields.1,
            value: fields.2,
        }),
        _ => Err(ParseError::new(
            line,
            format!("unknown headers operation '{operation}'"),
        )),
    }
}

fn validate_header_name(name: &str, line: usize) -> Result<(), ParseError> {
    if name.is_empty() || !name.bytes().all(|byte| matches!(byte, 33..=57 | 59..=126)) {
        return Err(ParseError::new(
            line,
            "header name must contain only printable ASCII except ':'",
        ));
    }
    Ok(())
}

fn parse_pipe_command(
    lines: &[&str],
    start: usize,
    linebuf: usize,
) -> Result<(String, usize), ParseError> {
    let first = lines[start].trim_start();
    let physical = first
        .strip_prefix('|')
        .expect("pipe command starts with '|'")
        .trim_start();
    if physical.is_empty() {
        return Ok((String::new(), start + 1));
    }
    parse_command_continuation(lines, start, physical, linebuf, "pipe command")
}

fn parse_command_continuation(
    lines: &[&str],
    start: usize,
    first: &str,
    linebuf: usize,
    description: &str,
) -> Result<(String, usize), ParseError> {
    let mut physical = first;
    let mut command = String::new();
    let mut index = start;

    // Keep backslash-newline pairs for the real shell. The parser only finds
    // the physical extent of the action and enforces its own allocation
    // limit; it does not attempt to interpret shell quoting or substitutions.
    loop {
        check_linebuf(lines[index], index + 1, linebuf)?;
        let added = physical
            .len()
            .checked_add(usize::from(physical.ends_with('\\')))
            .ok_or_else(|| ParseError::new(start + 1, format!("{description} size overflows")))?;
        let new_len = command
            .len()
            .checked_add(added)
            .ok_or_else(|| ParseError::new(start + 1, format!("{description} size overflows")))?;
        if new_len > linebuf {
            return Err(ParseError::limit(
                start + 1,
                format!("expanded rc line exceeds the active LINEBUF limit of {linebuf} bytes"),
            ));
        }
        if new_len > MAX_PIPE_COMMAND_LEN {
            return Err(ParseError::limit(
                start + 1,
                format!("{description} exceeds the hard limit of {MAX_PIPE_COMMAND_LEN} bytes"),
            ));
        }
        command.push_str(physical);
        if !physical.ends_with('\\') {
            break;
        }
        command.push('\n');
        index = index
            .checked_add(1)
            .ok_or_else(|| ParseError::new(start + 1, "rc line index overflows"))?;
        physical = lines.get(index).copied().ok_or_else(|| {
            ParseError::new(
                start + 1,
                format!("{description} continuation is incomplete"),
            )
        })?;
    }
    if command.is_empty() {
        return Err(ParseError::new(
            start + 1,
            format!("{description} is empty"),
        ));
    }
    if command.as_bytes().contains(&0) {
        return Err(ParseError::new(start + 1, "pipe command contains NUL"));
    }
    Ok((command, index + 1))
}

fn parse_condition_continuation(
    lines: &[&str],
    start: usize,
    linebuf: usize,
) -> Result<(String, usize), ParseError> {
    let first = lines[start]
        .trim_start()
        .strip_prefix('*')
        .ok_or_else(|| ParseError::new(start + 1, "recipe condition does not begin with '*'"))?;
    let preserve_leading_whitespace = condition_is_shell_expanded(first);
    let mut physical = first;
    let mut condition = String::new();
    let mut index = start;

    // A continued condition is one logical expression, so bound the joined
    // text before allocating each addition. Ordinary regex continuations drop
    // indentation for readable rc files, while `$` conditions retain it for
    // the later shell-like expansion pass as documented by procmail.
    loop {
        check_linebuf(lines[index], index + 1, linebuf)?;
        let continued = physical.ends_with('\\');
        let fragment = physical.strip_suffix('\\').unwrap_or(physical);
        let new_len = condition
            .len()
            .checked_add(fragment.len())
            .ok_or_else(|| ParseError::new(start + 1, "recipe condition size overflows"))?;
        if new_len > linebuf {
            return Err(ParseError::limit(
                start + 1,
                format!(
                    "continued recipe condition exceeds the active LINEBUF limit of {linebuf} bytes"
                ),
            ));
        }
        condition.push_str(fragment);
        if !continued {
            break;
        }
        index = index
            .checked_add(1)
            .ok_or_else(|| ParseError::new(start + 1, "rc line index overflows"))?;
        physical = lines.get(index).copied().ok_or_else(|| {
            ParseError::new(start + 1, "recipe condition continuation is incomplete")
        })?;
        if !preserve_leading_whitespace {
            physical = physical.trim_start();
        }
    }
    Ok((condition, index + 1))
}

fn condition_is_shell_expanded(mut input: &str) -> bool {
    input = input.trim_start();
    while let Some(rest) = input.strip_prefix('!') {
        input = rest.trim_start();
    }
    input.starts_with('$')
}

fn parse_recipe_header(
    rest: &str,
    line: usize,
) -> Result<(RecipeOptions, Option<String>, String), ParseError> {
    let rest = strip_comment(rest).trim();
    let (flag_text, lock) = match rest.split_once(':') {
        Some((flags, lock)) => {
            let lock = lock.trim();
            check_path_length(lock, line, "lockfile path")?;
            (flags.trim(), Some(lock.to_owned()))
        }
        None => (rest, None),
    };

    if !flag_text.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(ParseError::new(line, "invalid recipe flags"));
    }
    if let Some(flag) = flag_text.chars().find(|flag| {
        !matches!(
            flag,
            'H' | 'B' | 'D' | 'c' | 'A' | 'a' | 'E' | 'e' | 'h' | 'b' | 'f' | 'w' | 'W' | 'i' | 'r'
        )
    }) {
        return Err(ParseError::new(
            line,
            format!("recipe flag '{flag}' is not supported yet"),
        ));
    }
    let control_flags = ['A', 'a', 'E', 'e']
        .into_iter()
        .filter(|flag| flag_text.contains(*flag))
        .collect::<Vec<_>>();
    if control_flags.len() > 1 {
        return Err(ParseError::new(
            line,
            format!(
                "recipe control flags '{}' and '{}' cannot be combined",
                control_flags[0], control_flags[1]
            ),
        ));
    }
    if flag_text.contains('w') && flag_text.contains('W') {
        return Err(ParseError::new(
            line,
            "recipe flags 'w' and 'W' cannot be combined",
        ));
    }

    let condition_input = match (flag_text.contains('H'), flag_text.contains('B')) {
        (false, true) => ConditionInput::Body,
        (true, true) => ConditionInput::Message,
        _ => ConditionInput::Headers,
    };
    let control = if flag_text.contains('A') {
        ControlFlow::AfterChainMatch
    } else if flag_text.contains('a') {
        ControlFlow::AfterPreviousSuccess
    } else if flag_text.contains('E') {
        ControlFlow::Else
    } else if flag_text.contains('e') {
        ControlFlow::AfterPreviousError
    } else {
        ControlFlow::Independent
    };
    let continuation = if flag_text.contains('c') {
        ContinuationMode::Continue
    } else {
        ContinuationMode::Stop
    };
    Ok((
        RecipeOptions {
            condition_input,
            case_mode: if flag_text.contains('D') {
                CaseMode::Sensitive
            } else {
                CaseMode::Insensitive
            },
            control,
            action_input: match (flag_text.contains('h'), flag_text.contains('b')) {
                (true, false) => ActionInput::Headers,
                (false, true) => ActionInput::Body,
                _ => ActionInput::Message,
            },
            action_mode: if flag_text.contains('f') {
                ActionMode::Filter
            } else {
                ActionMode::Deliver
            },
            continuation,
            child_status: if flag_text.contains('w') {
                ChildStatusMode::Wait
            } else if flag_text.contains('W') {
                ChildStatusMode::WaitQuietly
            } else {
                ChildStatusMode::Ignore
            },
            write_errors: if flag_text.contains('i') {
                WriteErrorMode::Ignore
            } else {
                WriteErrorMode::Fail
            },
            output_ending: if flag_text.contains('r') {
                OutputEnding::Preserve
            } else {
                OutputEnding::Normalize
            },
        },
        lock,
        flag_text.to_owned(),
    ))
}

fn parse_condition(
    input: &str,
    line: usize,
    case_sensitive: bool,
    recipe_regexes: usize,
    budget: &ParseBudget,
) -> Result<(Condition, bool), ParseError> {
    let mut input = input.trim();
    let mut negated = false;
    while let Some(rest) = input.strip_prefix('!') {
        negated = !negated;
        input = rest.trim_start();
    }

    if input.is_empty() {
        return Err(ParseError::new(line, "condition is empty"));
    }
    if has_scoring_prefix(input) {
        return Err(ParseError::new(
            line,
            "weighted recipe conditions are not supported",
        ));
    }

    let (kind, is_regex) = if let Some(source) = input.strip_prefix('$') {
        (
            ConditionKind::ShellExpanded(super::ShellExpandedCondition {
                source: source.to_owned(),
                expansion: None,
            }),
            true,
        )
    } else if let Some(value) = input.strip_prefix('<') {
        (ConditionKind::SmallerThan(parse_size(value, line)?), false)
    } else if let Some(value) = input.strip_prefix('>') {
        (ConditionKind::LargerThan(parse_size(value, line)?), false)
    } else if let Some(command) = input.strip_prefix('?') {
        let command = command.trim_start();
        validate_program_condition(command, line)?;
        (ConditionKind::Program(command.to_owned()), false)
    } else {
        budget.check_regex(recipe_regexes, line)?;
        let (target, pattern) = condition_regex_target(input, line)?;
        if pattern.len() > MAX_REGEX_PATTERN_LEN {
            return Err(ParseError::new(
                line,
                format!(
                    "regular expression exceeds the hard limit of {MAX_REGEX_PATTERN_LEN} bytes"
                ),
            ));
        }

        // LINEBUF has already bounded the rc source line. Macro text belongs
        // to this implementation rather than to the user, so constrain its
        // generated size only with the regex expansion and compilation limits.
        let (compiled_pattern, marker_count, force_case_insensitive) =
            prepare_condition_regex(pattern, line)?;
        let compiled = build_regex(&compiled_pattern, case_sensitive && !force_case_insensitive)
            .map_err(|error| {
                ParseError::new(line, format!("invalid regular expression: {error}"))
            })?;
        let match_captures = (0..marker_count)
            .map(|marker| {
                let wanted = match_marker_name(marker);
                compiled
                    .capture_names()
                    .enumerate()
                    .find_map(|(index, name)| (name == Some(wanted.as_str())).then_some(index))
                    .ok_or_else(|| {
                        ParseError::new(line, "internal MATCH marker capture is missing")
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if compiled
            .capture_names()
            .enumerate()
            .any(|(index, name)| name.is_some() && !match_captures.contains(&index))
        {
            return Err(ParseError::new(
                line,
                "named regular expression groups are not supported",
            ));
        }
        let capture_indexes = (1..compiled.captures_len())
            .filter(|index| !match_captures.contains(index))
            .collect::<Vec<_>>();
        if capture_indexes.len() > MAX_REGEX_CAPTURES {
            return Err(ParseError::new(
                line,
                format!(
                    "regular expression capture count exceeds the hard limit of {MAX_REGEX_CAPTURES}"
                ),
            ));
        }
        let regex = RegexCondition {
            pattern: pattern.to_owned(),
            compiled,
            match_captures,
            capture_indexes,
        };
        match target {
            Some(ConditionRegexTarget::Variable(name)) => {
                (ConditionKind::VariableRegex { name, regex }, true)
            }
            Some(ConditionRegexTarget::Area(area)) => {
                (ConditionKind::AreaRegex { area, regex }, true)
            }
            None => (ConditionKind::Regex(regex), true),
        }
    };

    Ok((
        Condition {
            line,
            negated,
            kind,
        },
        is_regex,
    ))
}

pub(crate) fn parse_reparsed_condition(
    input: &str,
    line: usize,
    case_sensitive: bool,
) -> Result<Condition, ParseError> {
    let budget = ParseBudget::default();
    parse_condition(input, line, case_sensitive, 0, &budget).map(|(condition, _)| condition)
}

fn has_scoring_prefix(input: &str) -> bool {
    let Some((weight, rest)) = input.split_once('^') else {
        return false;
    };
    let exponent = rest.split_ascii_whitespace().next().unwrap_or_default();
    is_scoring_number(weight.trim_end()) && is_scoring_number(exponent)
}

fn is_scoring_number(input: &str) -> bool {
    let input = input
        .strip_prefix('+')
        .or_else(|| input.strip_prefix('-'))
        .unwrap_or(input);
    let mut has_digit = false;
    let mut has_point = false;
    for byte in input.bytes() {
        match byte {
            b'0'..=b'9' => has_digit = true,
            b'.' if !has_point => has_point = true,
            _ => return false,
        }
    }
    has_digit
}

fn prepare_condition_regex(
    pattern: &str,
    line: usize,
) -> Result<(String, usize, bool), ParseError> {
    const TO_ADDRESS: &str = "(?:^(?:(?:Original-)?(?:Resent-)?(?:To|Cc|Bcc)|(?:X-Envelope|Apparently(?:-Resent)?)-To):(?:.*[^-a-zA-Z0-9_.])?)";
    const TO_WORD: &str = "(?:^(?:(?:Original-)?(?:Resent-)?(?:To|Cc|Bcc)|(?:X-Envelope|Apparently(?:-Resent)?)-To):(?:.*[^a-zA-Z])?)";
    const FROM_DAEMON: &str = r"(?:^(?:Mailing-List:|Precedence:.*(?:junk|bulk|list)|To: Multiple recipients of |(?:(?:(?:Resent-)?(?:From|Sender)|X-Envelope-From):|>?From )(?:[^>]*[^(.%@a-z0-9])?(?:Post(?:ma?(?:st(?:e?r)?|n)|office)|(?:send)?Mail(?:er)?|daemon|m(?:mdf|ajordomo)|n?uucp|LIST(?:SERV|proc)|NETSERV|o(?:wner|ps)|r(?:e(?:quest|sponse)|oot)|b(?:ounce|bs\.smtp)|echo|mirror|s(?:erv(?:ices?|er)|mtp(?:error)?|ystem)|A(?:dmin(?:istrator)?|MMGR|utoanswer))(?:(?:[^).!:a-z0-9][-_a-z0-9]*)?[%@>	 ][^<)]*(?:\(.*\).*)?)?$(?:[^>]|$)))";
    const FROM_MAILER: &str = r"(?:^(?:(?:(?:Resent-)?(?:From|Sender)|X-Envelope-From):|>?From )(?:[^>]*[^(.%@a-z0-9])?(?:Post(?:ma(?:st(?:er)?|n)|office)|(?:send)?Mail(?:er)?|daemon|mmdf|n?uucp|ops|r(?:esponse|oot)|(?:bbs\.)?smtp(?:error)?|s(?:erv(?:ices?|er)|ystem)|A(?:dmin(?:istrator)?|MMGR))(?:(?:[^).!:a-z0-9][-_a-z0-9]*)?[%@>	 ][^<)]*(?:\(.*\).*)?)?$(?:[^>]|$))";
    let ast = parse_regex_ast(pattern, line)?;
    let collector =
        RegexEditCollector::new(pattern, line, TO_ADDRESS, TO_WORD, FROM_DAEMON, FROM_MAILER);
    let edits = ast::visit(&ast, collector)?;
    let output = edits.render(pattern, line)?;
    validate_regex_ast(&output, line)?;
    Ok((
        output,
        edits.match_marker_count,
        edits.force_case_insensitive,
    ))
}

const MATCH_MARKER_PREFIX: &str = "__procmail_rs_match_";

fn match_marker_name(index: usize) -> String {
    format!("{MATCH_MARKER_PREFIX}{index}")
}

fn parse_regex_ast(pattern: &str, line: usize) -> Result<ast::Ast, ParseError> {
    AstParserBuilder::new()
        .nest_limit(MAX_REGEX_AST_NESTING)
        .octal(false)
        .ignore_whitespace(false)
        .build()
        .parse(pattern)
        .map_err(|error| ParseError::new(line, format!("invalid regular expression: {error}")))
}

fn validate_regex_ast(pattern: &str, line: usize) -> Result<(), ParseError> {
    parse_regex_ast(pattern, line).map(drop)
}

#[derive(Clone, Copy)]
struct RegexEdit {
    start: usize,
    end: usize,
    replacement: RegexReplacement,
}

#[derive(Clone, Copy)]
enum RegexReplacement {
    Static(&'static str),
    MatchMarker(usize),
}

struct RegexEdits {
    edits: Vec<RegexEdit>,
    match_marker_count: usize,
    force_case_insensitive: bool,
}

impl RegexEdits {
    fn render(&self, pattern: &str, line: usize) -> Result<String, ParseError> {
        let mut output = Vec::with_capacity(pattern.len());
        let mut cursor = 0usize;

        // Edits come from a parser-owned tree and are applied in source order.
        // Checking their ranges here keeps a future AST transformation from
        // silently duplicating or dropping hostile source text.
        for edit in &self.edits {
            if edit.start < cursor || edit.end < edit.start || edit.end > pattern.len() {
                return Err(ParseError::new(
                    line,
                    "internal regular expression edit ranges overlap",
                ));
            }
            let source = pattern.get(cursor..edit.start).ok_or_else(|| {
                ParseError::new(line, "internal regular expression edit splits UTF-8 text")
            })?;
            push_regex_bytes(&mut output, source.as_bytes(), line)?;
            match edit.replacement {
                RegexReplacement::Static(replacement) => {
                    push_regex_bytes(&mut output, replacement.as_bytes(), line)?;
                }
                RegexReplacement::MatchMarker(index) => {
                    let marker = match_marker_name(index);
                    push_regex_bytes(&mut output, format!("(?P<{marker}>)").as_bytes(), line)?;
                }
            }
            cursor = edit.end;
        }
        let source = pattern.get(cursor..).ok_or_else(|| {
            ParseError::new(line, "internal regular expression edit splits UTF-8 text")
        })?;
        push_regex_bytes(&mut output, source.as_bytes(), line)?;
        String::from_utf8(output)
            .map_err(|_| ParseError::new(line, "translated regular expression is not valid UTF-8"))
    }
}

struct RegexEditCollector<'a> {
    pattern: &'a str,
    line: usize,
    to_address: &'static str,
    to_word: &'static str,
    from_daemon: &'static str,
    from_mailer: &'static str,
    edits: RegexEdits,
}

impl<'a> RegexEditCollector<'a> {
    fn new(
        pattern: &'a str,
        line: usize,
        to_address: &'static str,
        to_word: &'static str,
        from_daemon: &'static str,
        from_mailer: &'static str,
    ) -> Self {
        Self {
            pattern,
            line,
            to_address,
            to_word,
            from_daemon,
            from_mailer,
            edits: RegexEdits {
                edits: Vec::new(),
                match_marker_count: 0,
                force_case_insensitive: false,
            },
        }
    }

    fn source(&self, span: &ast::Span) -> Option<&str> {
        self.pattern.get(span.start.offset..span.end.offset)
    }

    fn covered(&self, offset: usize) -> bool {
        self.edits
            .edits
            .last()
            .is_some_and(|edit| offset < edit.end)
    }

    fn push(
        &mut self,
        start: usize,
        end: usize,
        replacement: RegexReplacement,
    ) -> Result<(), ParseError> {
        if self.edits.edits.len() == MAX_REGEX_PATTERN_LEN {
            return Err(ParseError::new(
                self.line,
                format!(
                    "regular expression edits exceed the hard limit of {MAX_REGEX_PATTERN_LEN}"
                ),
            ));
        }
        self.edits.edits.push(RegexEdit {
            start,
            end,
            replacement,
        });
        Ok(())
    }

    fn visit_assertion(&mut self, assertion: &ast::Assertion) -> Result<(), ParseError> {
        use ast::AssertionKind;

        let start = assertion.span.start.offset;
        if self.covered(start) {
            return Ok(());
        }
        match assertion.kind {
            AssertionKind::StartLine => {
                let tail = self.pattern.as_bytes().get(start..).ok_or_else(|| {
                    ParseError::new(self.line, "internal regular expression span is invalid")
                })?;
                if tail.starts_with(b"^^") {
                    let end = start + 2;
                    let replacement = if start == 0 {
                        r"\A"
                    } else if end == self.pattern.len() {
                        r"\z"
                    } else {
                        return Err(ParseError::new(
                            self.line,
                            "'^^' is supported only at the start or end of a regular expression",
                        ));
                    };
                    self.push(start, end, RegexReplacement::Static(replacement))?;
                } else if tail.starts_with(b"^FROM_DAEMON") {
                    self.push(
                        start,
                        start + 12,
                        RegexReplacement::Static(self.from_daemon),
                    )?;
                    self.edits.force_case_insensitive = true;
                } else if tail.starts_with(b"^FROM_MAILER") {
                    self.push(
                        start,
                        start + 12,
                        RegexReplacement::Static(self.from_mailer),
                    )?;
                } else if tail.starts_with(b"^TO_") {
                    self.push(start, start + 4, RegexReplacement::Static(self.to_address))?;
                } else if tail.starts_with(b"^TO") {
                    self.push(start, start + 3, RegexReplacement::Static(self.to_word))?;
                } else {
                    self.push(
                        start,
                        assertion.span.end.offset,
                        RegexReplacement::Static(r"(?:\A|\n)"),
                    )?;
                }
            }
            AssertionKind::EndLine => {
                self.push(
                    start,
                    assertion.span.end.offset,
                    RegexReplacement::Static(r"(?:\n|\z)"),
                )?;
            }
            AssertionKind::WordBoundaryStartAngle | AssertionKind::WordBoundaryEndAngle => {
                self.push(
                    start,
                    assertion.span.end.offset,
                    RegexReplacement::Static("[^a-zA-Z0-9_]"),
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    fn visit_literal(&mut self, literal: &ast::Literal) -> Result<(), ParseError> {
        if literal.kind != ast::LiteralKind::Superfluous
            || literal.c != '/'
            || self.source(&literal.span) != Some(r"\/")
        {
            return Ok(());
        }
        if self.edits.match_marker_count == MAX_REGEX_MATCH_MARKERS {
            return Err(ParseError::new(
                self.line,
                format!(
                    "regular expression MATCH marker count exceeds the hard limit of {MAX_REGEX_MATCH_MARKERS}"
                ),
            ));
        }
        let index = self.edits.match_marker_count;
        self.edits.match_marker_count += 1;
        self.push(
            literal.span.start.offset,
            literal.span.end.offset,
            RegexReplacement::MatchMarker(index),
        )
    }
}

impl ast::Visitor for RegexEditCollector<'_> {
    type Output = RegexEdits;
    type Err = ParseError;

    fn finish(self) -> Result<Self::Output, Self::Err> {
        let mut edits = self.edits;
        edits.edits.sort_by_key(|edit| edit.start);
        Ok(edits)
    }

    fn visit_pre(&mut self, ast: &ast::Ast) -> Result<(), Self::Err> {
        match ast {
            ast::Ast::Assertion(assertion) => self.visit_assertion(assertion),
            ast::Ast::Literal(literal) => self.visit_literal(literal),
            _ => Ok(()),
        }
    }
}

fn push_regex_bytes(output: &mut Vec<u8>, value: &[u8], line: usize) -> Result<(), ParseError> {
    BoundedBytes::try_extend_vec(output, MAX_REGEX_PATTERN_LEN, value).map_err(|error| match error {
        BoundedBytesError::LengthOverflow => {
            ParseError::new(line, "expanded regular expression length overflows")
        }
        BoundedBytesError::LimitExceeded { .. } => ParseError::new(
            line,
            format!(
                "expanded regular expression exceeds the hard limit of {MAX_REGEX_PATTERN_LEN} bytes"
            ),
        ),
    })
}

enum ConditionRegexTarget {
    Variable(String),
    Area(ConditionInput),
}

fn condition_regex_target(
    input: &str,
    line: usize,
) -> Result<(Option<ConditionRegexTarget>, &str), ParseError> {
    let name_len = input
        .bytes()
        .take_while(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
        .count();
    let name = &input[..name_len];
    let rest = input[name_len..].trim_start();
    let Some(pattern) = rest.strip_prefix("??") else {
        return Ok((None, input));
    };
    if name.is_empty()
        || !name
            .bytes()
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
    {
        return Err(ParseError::new(
            line,
            "variable condition has an invalid name",
        ));
    }
    if name.len() > MAX_ASSIGNMENT_NAME_LEN {
        return Err(ParseError::new(
            line,
            format!("variable name exceeds the hard limit of {MAX_ASSIGNMENT_NAME_LEN} bytes"),
        ));
    }
    let target = match name {
        "H" => ConditionRegexTarget::Area(ConditionInput::Headers),
        "B" => ConditionRegexTarget::Area(ConditionInput::Body),
        "HB" | "BH" => ConditionRegexTarget::Area(ConditionInput::Message),
        _ => ConditionRegexTarget::Variable(name.to_owned()),
    };
    Ok((Some(target), pattern.trim_start()))
}

fn parse_size(input: &str, line: usize) -> Result<usize, ParseError> {
    input
        .trim()
        .parse()
        .map_err(|_| ParseError::new(line, "size condition requires a non-negative integer"))
}

pub(crate) fn build_regex(
    pattern: &str,
    case_sensitive: bool,
) -> Result<regex::bytes::Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .multi_line(true)
        .unicode(false)
        .nest_limit(MAX_REGEX_AST_NESTING)
        .size_limit(MAX_REGEX_COMPILED_SIZE)
        .build()
}

fn required_path(path: &str, line: usize, description: &str) -> Result<String, ParseError> {
    let path = path.trim();
    if path.is_empty() {
        Err(ParseError::new(line, "destination path is empty"))
    } else {
        check_path_length(path, line, description)?;
        Ok(path.to_owned())
    }
}

fn check_path_length(path: &str, line: usize, description: &str) -> Result<(), ParseError> {
    if path.len() > MAX_PATH_EXPRESSION_LEN {
        return Err(ParseError::new(
            line,
            format!("{description} exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes"),
        ));
    }
    Ok(())
}

fn strip_comment(value: &str) -> &str {
    value.split_once('#').map_or(value, |(value, _)| value)
}

fn validate_program_condition(command: &str, line: usize) -> Result<(), ParseError> {
    if command.is_empty() {
        return Err(ParseError::new(line, "program condition command is empty"));
    }
    if command.len() > MAX_PIPE_COMMAND_LEN {
        return Err(ParseError::limit(
            line,
            format!(
                "program condition command exceeds the hard limit of {MAX_PIPE_COMMAND_LEN} bytes"
            ),
        ));
    }
    if command.as_bytes().contains(&0) {
        return Err(ParseError::new(
            line,
            "program condition command contains NUL",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/config/parser.rs"]
mod tests;
