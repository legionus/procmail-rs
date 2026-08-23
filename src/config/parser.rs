// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use regex::bytes::RegexBuilder;

use super::{
    ActionInput, ActionMode, Assignment, AssignmentTarget, CaseMode, ChildStatusMode, Condition,
    ConditionInput, ConditionKind, Config, ContinuationMode, ControlFlow, Destination,
    MAX_ASSIGNMENT_NAME_LEN, MAX_ASSIGNMENT_VALUE_LEN, MAX_PATH_EXPRESSION_LEN,
    MAX_PIPE_COMMAND_LEN, MAX_RC_SIZE, MAX_REGEX_CAPTURES, MAX_REGEX_COMPILED_SIZE,
    MAX_REGEX_PATTERN_LEN, OutputEnding, ParseError, PathExpression, PipeAction, RcFileExpression,
    RcLimits, RcParseCounts, RcParseState, Recipe, RecipeAction, RecipeOptions, RegexCondition,
    Statement, VariablePolicy, VariableSource, WriteErrorMode, variable_policy,
};

#[cfg(test)]
use super::{
    DEFAULT_LINEBUF, HARD_MAX_CONDITIONS_PER_RECIPE, HARD_MAX_RC_ASSIGNMENTS,
    HARD_MAX_RC_CONDITIONS, HARD_MAX_RC_RECIPES, HARD_MAX_RC_REGEXES, HARD_MAX_RC_STATEMENTS,
    HARD_MAX_RECIPE_NESTING_DEPTH, MAX_CONDITIONS_PER_RECIPE, MAX_LINEBUF, MAX_RC_CONDITIONS,
    MAX_RC_RECIPES, MAX_RC_REGEXES, MAX_RC_STATEMENTS, MAX_RECIPE_NESTING_DEPTH, MIN_LINEBUF,
};

pub fn parse(input: &str) -> Result<Config, ParseError> {
    let mut state = RcParseState::default();
    parse_with_state(input, &mut state)
}

pub(crate) fn parse_with_state(
    input: &str,
    state: &mut RcParseState,
) -> Result<Config, ParseError> {
    if input.len() > MAX_RC_SIZE {
        return Err(ParseError::limit(
            1,
            format!("rc file exceeds the hard limit of {MAX_RC_SIZE} bytes"),
        ));
    }

    let lines: Vec<&str> = input.lines().collect();
    let initial = state.counts;
    let initial_linebuf = state.limits.linebuf;
    let (statements, _) = parse_statements(&lines, 0, 0, state)?;

    Ok(Config {
        statements,
        initial_variables: Vec::new(),
        parse_counts: state.counts.subtract(initial)?,
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

fn parse_statements(
    lines: &[&str],
    mut index: usize,
    depth: usize,
    state: &mut RcParseState,
) -> Result<(Vec<Statement>, usize), ParseError> {
    let mut statements = Vec::new();
    while index < lines.len() {
        let line_number = index + 1;
        check_linebuf(lines[index], line_number, state.limits.linebuf)?;
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

        check_count_limit(
            state.counts.statements,
            state.limits.statements,
            line_number,
            "statement",
        )?;

        if line.starts_with(':') {
            check_count_limit(
                state.counts.recipes,
                state.limits.recipes,
                line_number,
                "recipe",
            )?;
            state.counts.recipes = state
                .counts
                .recipes
                .checked_add(1)
                .ok_or_else(|| ParseError::new(line_number, "rc recipe count overflows"))?;
            state.counts.statements = state
                .counts
                .statements
                .checked_add(1)
                .ok_or_else(|| ParseError::new(line_number, "rc statement count overflows"))?;
            let (recipe, next) = parse_recipe(lines, index, depth, state)?;
            statements.push(Statement::Recipe(recipe));
            index = next;
            continue;
        }

        if let Some(assignment) = parse_assignment(line, line_number)? {
            check_count_limit(
                state.counts.assignments,
                state.limits.assignments,
                line_number,
                "assignment",
            )?;
            if depth != 0
                && matches!(
                    assignment.target,
                    AssignmentTarget::RcLimit(_) | AssignmentTarget::LineBuf
                )
            {
                return Err(ParseError::new(
                    line_number,
                    format!(
                        "variable {} cannot be assigned inside a recipe block",
                        assignment.name
                    ),
                ));
            }
            let statement = match assignment.name.as_str() {
                "INCLUDERC" => Statement::Include(RcFileExpression {
                    line: assignment.line,
                    value: assignment.value,
                    expansion: None,
                }),
                "SWITCHRC" => Statement::Switch(RcFileExpression {
                    line: assignment.line,
                    value: assignment.value,
                    expansion: None,
                }),
                _ => Statement::Assignment(assignment),
            };
            state.counts.assignments = state
                .counts
                .assignments
                .checked_add(1)
                .ok_or_else(|| ParseError::new(line_number, "rc assignment count overflows"))?;
            state.counts.statements = state
                .counts
                .statements
                .checked_add(1)
                .ok_or_else(|| ParseError::new(line_number, "rc statement count overflows"))?;
            if let Statement::Assignment(assignment) = &statement {
                apply_rc_limit(assignment, &mut state.limits)?;
                apply_linebuf(assignment, &mut state.limits)?;
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
    let value = parse_assignment_value(value.trim(), line_number)?;
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
    if target == AssignmentTarget::Host && !value.is_empty() {
        return Err(ParseError::new(
            line_number,
            "non-empty HOST assignments are not supported yet",
        ));
    }
    let limit = super::assignment_value_limit(target);
    if value.len() > limit {
        let kind = if matches!(
            target,
            AssignmentTarget::Maildir | AssignmentTarget::LogFile
        ) {
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
        expansion: None,
    }))
}

fn parse_recipe(
    lines: &[&str],
    start: usize,
    depth: usize,
    state: &mut RcParseState,
) -> Result<(Recipe, usize), ParseError> {
    let header = lines[start].trim();
    let rest = header
        .strip_prefix(":0")
        .ok_or_else(|| ParseError::new(start + 1, "only ':0' recipes are supported"))?;
    let (options, lock) = parse_recipe_header(rest, start + 1)?;
    let mut conditions = Vec::new();
    let mut regex_count = 0usize;
    let mut index = start + 1;

    while index < lines.len() {
        check_linebuf(lines[index], index + 1, state.limits.linebuf)?;
        let line = lines[index].trim();
        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }
        if let Some(condition) = line.strip_prefix('*') {
            // Reject excess conditions before parsing can allocate their text
            // or compile a regular expression. The local and file-wide
            // budgets are separate because either shape can make later plan
            // construction disproportionately expensive.
            check_condition_limits(conditions.len(), state, index + 1)?;
            let (condition, is_regex) = parse_condition(
                condition,
                index + 1,
                options.case_mode == CaseMode::Sensitive,
                state.counts.regexes,
                regex_count,
                state.limits.regexes,
            )?;
            conditions.push(condition);
            regex_count = regex_count
                .checked_add(usize::from(is_regex))
                .ok_or_else(|| ParseError::new(index + 1, "recipe regex count overflows"))?;
            index += 1;
            continue;
        }
        break;
    }

    let action = lines
        .get(index)
        .map(|line| {
            check_linebuf(line, index + 1, state.limits.linebuf)?;
            Ok(line.trim())
        })
        .transpose()?
        .ok_or_else(|| ParseError::new(start + 1, "recipe has no action"))?;

    // Charge the parent recipe before descending into a block so nested
    // parsing cannot temporarily hide conditions or regexes from file-wide
    // limits.
    state.counts.conditions = state
        .counts
        .conditions
        .checked_add(conditions.len())
        .ok_or_else(|| ParseError::new(start + 1, "rc condition count overflows"))?;
    state.counts.regexes = state
        .counts
        .regexes
        .checked_add(regex_count)
        .ok_or_else(|| ParseError::new(start + 1, "rc regex count overflows"))?;

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

    let is_pipe = action.starts_with('|');
    let has_program_condition = conditions
        .iter()
        .any(|condition| matches!(condition.kind, ConditionKind::Program(_)));
    if !is_pipe && options.write_errors == WriteErrorMode::Ignore {
        let message = if action == "{" {
            "recipe flag 'i' is not supported on blocks; original procmail ignores it"
        } else {
            "recipe flag 'i' is not supported for filesystem delivery because it may publish an incomplete message"
        };
        return Err(ParseError::new(start + 1, message));
    }
    if action == "{" && options.output_ending == OutputEnding::Preserve {
        return Err(ParseError::new(
            start + 1,
            "recipe flag 'r' is not supported on blocks; original procmail ignores it",
        ));
    }
    if !is_pipe
        && (options.action_input != ActionInput::Message
            || options.action_mode != ActionMode::Deliver
            || (!has_program_condition
                && (options.child_status != ChildStatusMode::Ignore
                    || options.write_errors != WriteErrorMode::Fail)))
    {
        return Err(ParseError::new(
            start + 1,
            "flags h, b, and f require a pipe action; flags w and W require a pipe action or program condition",
        ));
    }

    let (action, next) = if is_pipe {
        let (command, next) = parse_pipe_command(lines, index, state.limits.linebuf)?;
        (RecipeAction::Pipe(PipeAction { command }), next)
    } else if action == "{" {
        let next_depth = depth
            .checked_add(1)
            .ok_or_else(|| ParseError::new(index + 1, "recipe nesting depth overflows"))?;
        if next_depth > state.limits.nesting_depth {
            return Err(ParseError::limit(
                index + 1,
                format!(
                    "recipe nesting depth {next_depth} exceeds the active limit of {}",
                    state.limits.nesting_depth
                ),
            ));
        }
        if lock.is_some() {
            return Err(ParseError::new(
                start + 1,
                "local lockfiles on recipe blocks are not supported",
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
            RecipeAction::Deliver(Destination::Mbox(PathExpression {
                source: required_path(path, index + 1, "destination path")?,
                base: None,
                line: index + 1,
                runtime_dependent: false,
                runtime_base: false,
                expansion: None,
            })),
            index + 1,
        )
    } else if let Some(path) = action.strip_prefix("maildir:") {
        (
            RecipeAction::Deliver(Destination::Maildir(PathExpression {
                source: required_path(path, index + 1, "destination path")?,
                base: None,
                line: index + 1,
                runtime_dependent: false,
                runtime_base: false,
                expansion: None,
            })),
            index + 1,
        )
    } else if action.ends_with('/') {
        (
            RecipeAction::Deliver(Destination::Maildir(PathExpression {
                source: required_path(action, index + 1, "destination path")?,
                base: None,
                line: index + 1,
                runtime_dependent: false,
                runtime_base: false,
                expansion: None,
            })),
            index + 1,
        )
    } else {
        check_path_length(action, index + 1, "destination path")?;
        return Err(ParseError::new(
            index + 1,
            "destination type is ambiguous; use an explicit maildir: or mbox: prefix, or a trailing '/' for Maildir",
        ));
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

fn parse_pipe_command(
    lines: &[&str],
    start: usize,
    linebuf: usize,
) -> Result<(String, usize), ParseError> {
    let first = lines[start].trim_start();
    let mut physical = first
        .strip_prefix('|')
        .expect("pipe command starts with '|'")
        .trim_start();
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
            .ok_or_else(|| ParseError::new(start + 1, "pipe command size overflows"))?;
        let new_len = command
            .len()
            .checked_add(added)
            .ok_or_else(|| ParseError::new(start + 1, "pipe command size overflows"))?;
        if new_len > linebuf {
            return Err(ParseError::limit(
                start + 1,
                format!("expanded rc line exceeds the active LINEBUF limit of {linebuf} bytes"),
            ));
        }
        if new_len > MAX_PIPE_COMMAND_LEN {
            return Err(ParseError::limit(
                start + 1,
                format!("pipe command exceeds the hard limit of {MAX_PIPE_COMMAND_LEN} bytes"),
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
        physical = lines
            .get(index)
            .copied()
            .ok_or_else(|| ParseError::new(start + 1, "pipe command continuation is incomplete"))?;
    }
    if command.is_empty() {
        return Err(ParseError::new(start + 1, "pipe command is empty"));
    }
    if command.as_bytes().contains(&0) {
        return Err(ParseError::new(start + 1, "pipe command contains NUL"));
    }
    Ok((command, index + 1))
}

fn check_condition_limits(
    recipe_count: usize,
    state: &RcParseState,
    line: usize,
) -> Result<(), ParseError> {
    if recipe_count >= state.limits.conditions_per_recipe {
        return Err(ParseError::limit(
            line,
            format!(
                "recipe condition count exceeds the active limit of {}",
                state.limits.conditions_per_recipe
            ),
        ));
    }
    let total = state
        .counts
        .conditions
        .checked_add(recipe_count)
        .ok_or_else(|| ParseError::new(line, "rc condition count overflows"))?;
    if total >= state.limits.conditions {
        return Err(ParseError::limit(
            line,
            format!(
                "rc condition count exceeds the active limit of {}",
                state.limits.conditions
            ),
        ));
    }
    Ok(())
}

fn parse_recipe_header(
    rest: &str,
    line: usize,
) -> Result<(RecipeOptions, Option<String>), ParseError> {
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
    ))
}

fn parse_condition(
    input: &str,
    line: usize,
    case_sensitive: bool,
    prior_regexes: usize,
    recipe_regexes: usize,
    regex_limit: usize,
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

    let (kind, is_regex) = if let Some(value) = input.strip_prefix('<') {
        (ConditionKind::SmallerThan(parse_size(value, line)?), false)
    } else if let Some(value) = input.strip_prefix('>') {
        (ConditionKind::LargerThan(parse_size(value, line)?), false)
    } else if let Some(command) = input.strip_prefix('?') {
        let command = command.trim_start();
        validate_program_condition(command, line)?;
        (ConditionKind::Program(command.to_owned()), false)
    } else {
        let total_regexes = prior_regexes
            .checked_add(recipe_regexes)
            .ok_or_else(|| ParseError::new(line, "rc regex count overflows"))?;
        if total_regexes >= regex_limit {
            return Err(ParseError::limit(
                line,
                format!("rc regex count exceeds the active limit of {regex_limit}"),
            ));
        }
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
        let (expanded_pattern, force_case_insensitive) =
            expand_reserved_procmail_regex_forms(pattern, line)?;
        let (compiled_pattern, marker_name) = prepare_capture_pattern(&expanded_pattern, line)?;
        let compiled = build_regex(&compiled_pattern, case_sensitive && !force_case_insensitive)
            .map_err(|error| {
                ParseError::new(line, format!("invalid regular expression: {error}"))
            })?;
        let match_capture = marker_name.as_deref().and_then(|wanted| {
            compiled
                .capture_names()
                .enumerate()
                .find_map(|(index, name)| (name == Some(wanted)).then_some(index))
        });
        if compiled
            .capture_names()
            .flatten()
            .any(|name| Some(name) != marker_name.as_deref())
        {
            return Err(ParseError::new(
                line,
                "named regular expression groups are not supported",
            ));
        }
        let capture_indexes = (1..compiled.captures_len())
            .filter(|index| Some(*index) != match_capture)
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
            match_capture,
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

fn expand_reserved_procmail_regex_forms(
    pattern: &str,
    line: usize,
) -> Result<(String, bool), ParseError> {
    const TO_ADDRESS: &str = "(?:^(?:(?:Original-)?(?:Resent-)?(?:To|Cc|Bcc)|(?:X-Envelope|Apparently(?:-Resent)?)-To):(?:.*[^-a-zA-Z0-9_.])?)";
    const TO_WORD: &str = "(?:^(?:(?:Original-)?(?:Resent-)?(?:To|Cc|Bcc)|(?:X-Envelope|Apparently(?:-Resent)?)-To):(?:.*[^a-zA-Z])?)";
    const FROM_DAEMON: &str = r"(?:^(?:Mailing-List:|Precedence:.*(?:junk|bulk|list)|To: Multiple recipients of |(?:(?:(?:Resent-)?(?:From|Sender)|X-Envelope-From):|>?From )(?:[^>]*[^(.%@a-z0-9])?(?:Post(?:ma?(?:st(?:e?r)?|n)|office)|(?:send)?Mail(?:er)?|daemon|m(?:mdf|ajordomo)|n?uucp|LIST(?:SERV|proc)|NETSERV|o(?:wner|ps)|r(?:e(?:quest|sponse)|oot)|b(?:ounce|bs\.smtp)|echo|mirror|s(?:erv(?:ices?|er)|mtp(?:error)?|ystem)|A(?:dmin(?:istrator)?|MMGR|utoanswer))(?:(?:[^).!:a-z0-9][-_a-z0-9]*)?[%@>	 ][^<)]*(?:\(.*\).*)?)?$(?:[^>]|$)))";
    const FROM_MAILER: &str = r"(?:^(?:(?:(?:Resent-)?(?:From|Sender)|X-Envelope-From):|>?From )(?:[^>]*[^(.%@a-z0-9])?(?:Post(?:ma(?:st(?:er)?|n)|office)|(?:send)?Mail(?:er)?|daemon|mmdf|n?uucp|ops|r(?:esponse|oot)|(?:bbs\.)?smtp(?:error)?|s(?:erv(?:ices?|er)|ystem)|A(?:dmin(?:istrator)?|MMGR))(?:(?:[^).!:a-z0-9][-_a-z0-9]*)?[%@>	 ][^<)]*(?:\(.*\).*)?)?$(?:[^>]|$))";
    const FORMS: [(&str, &str, bool); 4] = [
        ("^FROM_DAEMON", FROM_DAEMON, true),
        ("^TO_", TO_ADDRESS, false),
        ("^TO", TO_WORD, false),
        ("^FROM_MAILER", FROM_MAILER, false),
    ];
    let bytes = pattern.as_bytes();
    let mut expanded = Vec::with_capacity(pattern.len());
    let mut force_case_insensitive = false;
    let mut index = 0usize;

    // Expand the reference implementation's fixed byte patterns before the
    // regular procmail-to-Rust translation. Bound every append because many
    // short keywords can otherwise amplify an accepted source pattern beyond
    // the compiled-regex resource policy.
    while index < bytes.len() {
        let replacement = (index == 0 || bytes[index - 1] != b'\\')
            .then(|| {
                FORMS
                    .iter()
                    .find(|(name, _, _)| pattern[index..].starts_with(name))
            })
            .flatten();
        if let Some((name, value, insensitive)) = replacement {
            push_regex_bytes(&mut expanded, value.as_bytes(), line)?;
            force_case_insensitive |= *insensitive;
            index += name.len();
        } else {
            push_regex_bytes(&mut expanded, &bytes[index..index + 1], line)?;
            index += 1;
        }
    }
    let expanded = String::from_utf8(expanded)
        .map_err(|_| ParseError::new(line, "expanded regular expression is not valid UTF-8"))?;
    Ok((expanded, force_case_insensitive))
}

fn push_regex_bytes(output: &mut Vec<u8>, value: &[u8], line: usize) -> Result<(), ParseError> {
    let length = output
        .len()
        .checked_add(value.len())
        .ok_or_else(|| ParseError::new(line, "expanded regular expression length overflows"))?;
    if length > MAX_REGEX_PATTERN_LEN {
        return Err(ParseError::new(
            line,
            format!(
                "expanded regular expression exceeds the hard limit of {MAX_REGEX_PATTERN_LEN} bytes"
            ),
        ));
    }
    output.extend_from_slice(value);
    Ok(())
}

fn prepare_capture_pattern(
    pattern: &str,
    line: usize,
) -> Result<(String, Option<String>), ParseError> {
    const MARKER: &str = "__procmail_rs_match";
    let bytes = pattern.as_bytes();
    let mut marker_output = None;
    let mut terminal_line_end_output = None;
    let mut in_class = false;
    let mut translated = String::with_capacity(pattern.len() + MARKER.len() + 8);
    let mut literal_start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'[' && !escaped(bytes, index) {
            in_class = true;
        } else if byte == b']' && !escaped(bytes, index) {
            in_class = false;
        } else if !in_class
            && byte == b'^'
            && bytes.get(index + 1) == Some(&b'^')
            && !escaped(bytes, index)
        {
            translated.push_str(&pattern[literal_start..index]);
            if index == 0 {
                translated.push_str("\\A");
            } else if index + 2 == bytes.len() {
                translated.push_str("\\z");
            } else {
                return Err(ParseError::new(
                    line,
                    "'^^' is supported only at the start or end of a regular expression",
                ));
            }
            index += 2;
            literal_start = index;
            continue;
        } else if !in_class && matches!(byte, b'^' | b'$') && !escaped(bytes, index) {
            // Procmail's single line anchors consume the separating newline
            // during a multiline match, while still matching the outer edge
            // of the selected area. Express both cases explicitly so HB
            // patterns can advance from headers into the body.
            translated.push_str(&pattern[literal_start..index]);
            if byte == b'^' {
                translated.push_str("(?:\\A|\\n)");
            } else {
                if index + 1 == bytes.len() {
                    terminal_line_end_output = Some(translated.len());
                }
                translated.push_str("(?:\\n|\\z)");
            }
            index += 1;
            literal_start = index;
            continue;
        } else if !in_class && matches!(byte, b'/' | b'<' | b'>') && escaped(bytes, index) {
            let escape = index - 1;
            translated.push_str(&pattern[literal_start..escape]);
            if byte == b'/' {
                if marker_output.replace(translated.len()).is_some() {
                    return Err(ParseError::new(
                        line,
                        "regular expression contains more than one '\\/' capture marker",
                    ));
                }
            } else {
                translated.push_str("[^a-zA-Z0-9_]");
            }
            index += 1;
            literal_start = index;
            continue;
        }
        index += 1;
    }
    translated.push_str(&pattern[literal_start..]);

    let Some(index) = marker_output else {
        return Ok((translated, None));
    };
    let capture_start = format!("(?P<{MARKER}>");
    translated.insert_str(index, &capture_start);
    if let Some(capture_end) = terminal_line_end_output {
        // `$` consumes a newline in procmail, but that separator is not part
        // of MATCH. Close the helper capture before the translated terminal
        // anchor while leaving it inside the condition's complete match.
        translated.insert(capture_end + capture_start.len(), ')');
    } else {
        translated.push(')');
    }
    Ok((translated, Some(MARKER.to_owned())))
}

fn escaped(bytes: &[u8], index: usize) -> bool {
    bytes[..index]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
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

fn parse_assignment_value(value: &str, line: usize) -> Result<String, ParseError> {
    if !value.starts_with('"') {
        return Ok(strip_comment(value).trim().to_owned());
    }

    // An outer double-quoted value is a single rc value, so a '#' within it
    // is data rather than a comment. Quote escapes and trailing shell syntax
    // stay rejected until their exact procmail behavior is implemented.
    let quoted = &value[1..];
    let Some(closing) = quoted.find('"') else {
        return Err(ParseError::new(
            line,
            "unterminated double-quoted assignment value",
        ));
    };
    let inner = &quoted[..closing];
    let trailing = quoted[closing + 1..].trim();
    if !trailing.is_empty() && !trailing.starts_with('#') {
        return Err(ParseError::new(
            line,
            "syntax after a double-quoted assignment value is not supported",
        ));
    }
    Ok(inner.to_owned())
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
mod tests;
