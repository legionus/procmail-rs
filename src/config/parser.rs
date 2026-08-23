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
    Statement, VariableSource, WriteErrorMode, variable_policy,
};

#[cfg(test)]
use super::{
    HARD_MAX_CONDITIONS_PER_RECIPE, HARD_MAX_RC_ASSIGNMENTS, HARD_MAX_RC_CONDITIONS,
    HARD_MAX_RC_RECIPES, HARD_MAX_RC_REGEXES, HARD_MAX_RC_STATEMENTS,
    HARD_MAX_RECIPE_NESTING_DEPTH, MAX_CONDITIONS_PER_RECIPE, MAX_RC_CONDITIONS, MAX_RC_RECIPES,
    MAX_RC_REGEXES, MAX_RC_STATEMENTS, MAX_RECIPE_NESTING_DEPTH,
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
    let (statements, _) = parse_statements(&lines, 0, 0, state)?;

    Ok(Config {
        statements,
        initial_variables: Vec::new(),
        parse_counts: state.counts.subtract(initial)?,
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
            if depth != 0 && matches!(assignment.target, AssignmentTarget::RcLimit(_)) {
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
    let target = variable_policy(name)
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
        .map(|line| line.trim())
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
    if !is_pipe
        && (options.action_input != ActionInput::Message
            || options.action_mode != ActionMode::Deliver
            || (!has_program_condition
                && (options.child_status != ChildStatusMode::Ignore
                    || options.write_errors != WriteErrorMode::Fail))
            || options.output_ending != OutputEnding::Normalize)
    {
        return Err(ParseError::new(
            start + 1,
            "flags h, b, f, and r require a pipe action; flags w and W require a pipe action or program condition",
        ));
    }

    let (action, next) = if is_pipe {
        let (command, next) = parse_pipe_command(lines, index)?;
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
        lock,
        conditions,
        action,
    };
    Ok((recipe, next))
}

fn parse_pipe_command(lines: &[&str], start: usize) -> Result<(String, usize), ParseError> {
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
        let added = physical
            .len()
            .checked_add(usize::from(physical.ends_with('\\')))
            .ok_or_else(|| ParseError::new(start + 1, "pipe command size overflows"))?;
        let new_len = command
            .len()
            .checked_add(added)
            .ok_or_else(|| ParseError::new(start + 1, "pipe command size overflows"))?;
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
        let (compiled_pattern, marker_name) = prepare_capture_pattern(pattern, line)?;
        let compiled = build_regex(&compiled_pattern, case_sensitive).map_err(|error| {
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
mod tests {
    use super::*;
    use crate::config::MAX_SHELL_SETTING_LEN;

    #[test]
    fn parses_assignment_and_recipe() {
        let config =
            parse("MAILDIR=/srv/mail\n\n:0 Bc:\n* ! ^Subject: spam\nmaildir:inbox\n").unwrap();

        assert_eq!(config.statements.len(), 2);
        assert_eq!(
            config.statements[0],
            Statement::Assignment(Assignment {
                line: 1,
                name: "MAILDIR".into(),
                value: "/srv/mail".into(),
                target: crate::config::AssignmentTarget::Maildir,
                expansion: None,
            })
        );
        assert_eq!(
            config.statements[1],
            Statement::Recipe(Recipe {
                line: 3,
                action_line: 5,
                options: RecipeOptions {
                    condition_input: ConditionInput::Body,
                    case_mode: CaseMode::Insensitive,
                    control: ControlFlow::Independent,
                    action_input: ActionInput::Message,
                    action_mode: ActionMode::Deliver,
                    continuation: ContinuationMode::Continue,
                    child_status: ChildStatusMode::Ignore,
                    write_errors: WriteErrorMode::Fail,
                    output_ending: OutputEnding::Normalize,
                },
                lock: Some(String::new()),
                conditions: vec![Condition {
                    line: 4,
                    negated: true,
                    kind: ConditionKind::Regex(RegexCondition {
                        pattern: "^Subject: spam".into(),
                        compiled: build_regex("^Subject: spam", false).unwrap(),
                        match_capture: None,
                        capture_indexes: Vec::new(),
                    }),
                }],
                action: RecipeAction::Deliver(Destination::Maildir("inbox".into())),
            })
        );
    }

    #[test]
    fn trailing_slash_selects_maildir() {
        let config = parse(":0\ninbox/\n").unwrap();
        let Statement::Recipe(recipe) = &config.statements[0] else {
            panic!("expected recipe");
        };

        assert_eq!(
            recipe.action,
            RecipeAction::Deliver(Destination::Maildir("inbox/".into()))
        );
    }

    #[test]
    fn rejects_destination_without_a_stable_type() {
        let error = parse(":0\ninbox\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            "destination type is ambiguous; use an explicit maildir: or mbox: prefix, or a trailing '/' for Maildir"
        );
    }

    #[test]
    fn classifies_user_assignments_for_future_expansion() {
        let config = parse("USER_VALUE=text\n").unwrap();
        let Statement::Assignment(assignment) = &config.statements[0] else {
            panic!("expected assignment");
        };

        assert_eq!(assignment.target, crate::config::AssignmentTarget::User);
    }

    #[test]
    fn rc_limits_change_only_following_syntax() {
        let raised = format!(
            "LIMIT_RC_REGEXES={}\n{}",
            MAX_RC_REGEXES + 1,
            config_with_regexes(MAX_RC_REGEXES + 1)
        );
        assert!(parse(&raised).is_ok());

        let lowered = "LIMIT_RC_RECIPES=0\n:0\ninbox/\n";
        let error = parse(lowered).unwrap_err();
        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            "rc recipe count exceeds the active limit of 0"
        );

        let late_raise = format!(
            "{}LIMIT_RC_REGEXES={}\n",
            config_with_regexes(MAX_RC_REGEXES + 1),
            MAX_RC_REGEXES + 1
        );
        assert_eq!(
            parse(&late_raise).unwrap_err().message,
            format!("rc regex count exceeds the active limit of {MAX_RC_REGEXES}")
        );
    }

    #[test]
    fn zero_assignment_limit_prevents_further_limit_changes() {
        let error = parse("LIMIT_MAX_ASSIGNMENTS=0\nLIMIT_RC_RECIPES=10\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            "rc assignment count exceeds the active limit of 0"
        );
    }

    #[test]
    fn every_structural_limit_rejects_the_next_matching_item() {
        let cases = [
            (
                "LIMIT_RC_STATEMENTS=1\nA=1\n",
                2,
                "rc statement count exceeds the active limit of 1",
            ),
            (
                "LIMIT_RC_CONDITIONS=0\n:0\n* < 1\ninbox/\n",
                3,
                "rc condition count exceeds the active limit of 0",
            ),
            (
                "LIMIT_RC_REGEXES=0\n:0\n* pattern\ninbox/\n",
                3,
                "rc regex count exceeds the active limit of 0",
            ),
            (
                "LIMIT_RECIPE_CONDITIONS=0\n:0\n* < 1\ninbox/\n",
                3,
                "recipe condition count exceeds the active limit of 0",
            ),
            (
                "LIMIT_RECIPE_NESTING=0\n:0\n{\n}\n",
                3,
                "recipe nesting depth 1 exceeds the active limit of 0",
            ),
        ];

        for (source, line, message) in cases {
            let error = parse(source).unwrap_err();
            assert_eq!(error.line, line, "source: {source:?}");
            assert_eq!(error.message, message, "source: {source:?}");
        }
    }

    #[test]
    fn lowering_below_the_used_count_fails_on_the_next_item() {
        let error = parse(":0 c\ninbox/\nLIMIT_RC_RECIPES=0\n:0\ninbox/\n").unwrap_err();

        assert_eq!(error.line, 4);
        assert_eq!(
            error.message,
            "rc recipe count exceeds the active limit of 0"
        );
    }

    #[test]
    fn rejects_rc_limit_assignments_inside_recipe_blocks() {
        let error = parse(":0\n{\nLIMIT_RC_REGEXES=100\n}\n").unwrap_err();

        assert_eq!(error.line, 3);
        assert_eq!(
            error.message,
            "variable LIMIT_RC_REGEXES cannot be assigned inside a recipe block"
        );
    }

    #[test]
    fn rejects_invalid_and_excessive_rc_limits() {
        let invalid = parse("LIMIT_RC_RECIPES=1k\n").unwrap_err();
        assert_eq!(invalid.line, 1);
        assert_eq!(
            invalid.message,
            "LIMIT_RC_RECIPES must be an unsigned decimal integer"
        );

        let cases = [
            ("LIMIT_MAX_ASSIGNMENTS", HARD_MAX_RC_ASSIGNMENTS),
            ("LIMIT_RC_STATEMENTS", HARD_MAX_RC_STATEMENTS),
            ("LIMIT_RC_RECIPES", HARD_MAX_RC_RECIPES),
            ("LIMIT_RC_CONDITIONS", HARD_MAX_RC_CONDITIONS),
            ("LIMIT_RC_REGEXES", HARD_MAX_RC_REGEXES),
            ("LIMIT_RECIPE_CONDITIONS", HARD_MAX_CONDITIONS_PER_RECIPE),
            ("LIMIT_RECIPE_NESTING", HARD_MAX_RECIPE_NESTING_DEPTH),
        ];
        for (name, hard_limit) in cases {
            let error = parse(&format!("{name}={}\n", hard_limit + 1)).unwrap_err();
            assert_eq!(error.line, 1, "limit: {name}");
            assert_eq!(
                error.message,
                format!("{name} exceeds the hard limit of {hard_limit}"),
                "limit: {name}"
            );
        }
    }

    #[test]
    fn parses_bare_host_as_an_empty_assignment() {
        let config = parse("HOST\n").unwrap();
        let Statement::Assignment(assignment) = &config.statements[0] else {
            panic!("expected assignment");
        };

        assert_eq!(assignment.name, "HOST");
        assert_eq!(assignment.value, "");
        assert_eq!(assignment.target, crate::config::AssignmentTarget::Host);
    }

    #[test]
    fn rejects_non_empty_host_until_hostname_matching_is_supported() {
        let error = parse("HOST=elsewhere\n").unwrap_err();

        assert_eq!(error.line, 1);
        assert_eq!(
            error.message,
            "non-empty HOST assignments are not supported yet"
        );
    }

    #[test]
    fn rejects_runtime_variable_assignment() {
        let error = parse("LASTFOLDER=forged\n").unwrap_err();

        assert_eq!(error.line, 1);
        assert_eq!(
            error.message,
            "variable LASTFOLDER cannot be assigned in an rc file"
        );
    }

    #[test]
    fn parses_pipe_flags_and_preserves_continued_shell_text() {
        let config = parse(
            ":0 HBcfhwir\n| FOO=1 formail \\\n    -I X-Spam: \\\n    -I X-Virus:\n:0\nmaildir:final\n",
        )
        .unwrap();
        let Statement::Recipe(recipe) = &config.statements[0] else {
            panic!("expected pipe recipe");
        };
        let RecipeAction::Pipe(action) = &recipe.action else {
            panic!("expected pipe action");
        };

        assert_eq!(recipe.options.condition_input, ConditionInput::Message);
        assert_eq!(recipe.options.action_input, ActionInput::Headers);
        assert_eq!(recipe.options.action_mode, ActionMode::Filter);
        assert_eq!(recipe.options.continuation, ContinuationMode::Continue);
        assert_eq!(recipe.options.child_status, ChildStatusMode::Wait);
        assert_eq!(recipe.options.write_errors, WriteErrorMode::Ignore);
        assert_eq!(recipe.options.output_ending, OutputEnding::Preserve);
        assert_eq!(
            action.command,
            "FOO=1 formail \\\n    -I X-Spam: \\\n    -I X-Virus:"
        );
        assert!(config.has_pipe_actions());
        assert_eq!(config.statements.len(), 2);
    }

    #[test]
    fn rejects_invalid_pipe_flag_combinations_and_uses() {
        let error = parse(":0 wW\n| command\n").unwrap_err();
        assert_eq!(error.line, 1);
        assert_eq!(error.message, "recipe flags 'w' and 'W' cannot be combined");

        let error = parse(":0 f\nmaildir:target\n").unwrap_err();
        assert_eq!(error.line, 1);
        assert_eq!(
            error.message,
            "flags h, b, f, and r require a pipe action; flags w and W require a pipe action or program condition"
        );

        for destination in ["mbox:target", "maildir:target"] {
            let error = parse(&format!(":0 i\n{destination}\n")).unwrap_err();
            assert_eq!(error.line, 1);
            assert_eq!(
                error.message,
                "recipe flag 'i' is not supported for filesystem delivery because it may publish an incomplete message"
            );
        }

        let error = parse(":0 i\n{\n:0\nmaildir:target\n}\n").unwrap_err();
        assert_eq!(error.line, 1);
        assert_eq!(
            error.message,
            "recipe flag 'i' is not supported on blocks; original procmail ignores it"
        );
    }

    #[test]
    fn documents_filesystem_ignore_write_error_rejection() {
        let compatibility = include_str!("../../Documentation/Compatibility.md");

        assert!(compatibility.contains("`i` on mbox or Maildir"));
        assert!(compatibility.contains("publish a truncated Maildir file"));
        assert!(compatibility.contains("Rejected before message input"));
    }

    #[test]
    fn bounds_and_validates_pipe_command_text() {
        let accepted = format!(":0\n| {}\n", "x".repeat(MAX_PIPE_COMMAND_LEN));
        assert!(parse(&accepted).is_ok());

        let rejected = format!(":0\n| {}\n", "x".repeat(MAX_PIPE_COMMAND_LEN + 1));
        let error = parse(&rejected).unwrap_err();
        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            format!("pipe command exceeds the hard limit of {MAX_PIPE_COMMAND_LEN} bytes")
        );

        for (source, message) in [
            (":0\n|\n", "pipe command is empty"),
            (
                ":0\n| command \\\n",
                "pipe command continuation is incomplete",
            ),
            (":0\n| command\0arg\n", "pipe command contains NUL"),
        ] {
            let error = parse(source).unwrap_err();
            assert_eq!(error.line, 2);
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn rejects_recipe_without_action() {
        let error = parse(":0\n* ^Subject:\n").unwrap_err();

        assert_eq!(error.line, 1);
        assert_eq!(error.message, "recipe has no action");
    }

    #[test]
    fn rejects_unsupported_flag() {
        let error = parse(":0 q\ninbox/\n").unwrap_err();

        assert_eq!(error.line, 1);
        assert_eq!(error.message, "recipe flag 'q' is not supported yet");
    }

    #[test]
    fn accepts_chain_flags_but_rejects_their_combination() {
        assert!(parse(":0 A\nmaildir:after-match\n").is_ok());
        assert!(parse(":0 a\nmaildir:after-success\n").is_ok());
        assert!(parse(":0 E\nmaildir:otherwise\n").is_ok());
        assert!(parse(":0 e\nmaildir:on-error\n").is_ok());

        let error = parse(":0 Aa\nmaildir:ambiguous\n").unwrap_err();
        assert_eq!(error.line, 1);
        assert_eq!(
            error.message,
            "recipe control flags 'A' and 'a' cannot be combined"
        );

        let error = parse(":0 Ee\nmaildir:ambiguous\n").unwrap_err();
        assert_eq!(
            error.message,
            "recipe control flags 'E' and 'e' cannot be combined"
        );
    }

    #[test]
    fn accepts_error_handler_after_a_block() {
        assert!(parse(":0\n{\n:0 c\nmaildir:child\n}\n:0 e\nmaildir:fallback\n").is_ok());
    }

    #[test]
    fn enforces_recipe_nesting_depth_at_the_boundary() {
        fn nested(depth: usize) -> String {
            let mut source = ":0\n{\n".repeat(depth);
            source.push_str(":0\ninbox/\n");
            source.push_str(&"}\n".repeat(depth));
            source
        }

        assert_eq!(MAX_RECIPE_NESTING_DEPTH, 64);
        for depth in [
            MAX_RECIPE_NESTING_DEPTH - 1,
            MAX_RECIPE_NESTING_DEPTH,
            MAX_RECIPE_NESTING_DEPTH + 1,
        ] {
            let result = parse(&nested(depth));
            if depth <= MAX_RECIPE_NESTING_DEPTH {
                assert!(result.is_ok(), "depth {depth} must be accepted");
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, MAX_RECIPE_NESTING_DEPTH * 2 + 2);
                assert_eq!(
                    error.message,
                    format!(
                        "recipe nesting depth {depth} exceeds the active limit of {MAX_RECIPE_NESTING_DEPTH}"
                    )
                );
            }
        }
    }

    #[test]
    fn rejects_unclosed_recipe_block() {
        let error = parse(":0\n{\n:0\ninbox/\n").unwrap_err();

        assert_eq!(error.line, 4);
        assert_eq!(error.message, "recipe block has no closing brace");
    }

    #[test]
    fn parses_assignment_inside_recipe_block() {
        let config = parse(":0\n{\nBOX=inbox\n}\n").unwrap();
        let Statement::Recipe(recipe) = &config.statements[0] else {
            panic!("expected recipe");
        };
        let RecipeAction::Block(statements) = &recipe.action else {
            panic!("expected block");
        };
        let Statement::Assignment(assignment) = &statements[0] else {
            panic!("expected assignment");
        };

        assert_eq!(assignment.name, "BOX");
        assert_eq!(assignment.value, "inbox");
    }

    #[test]
    fn parses_program_condition_and_quoted_block_assignment() {
        let config =
            parse(":0 W\n* ? test ! -e $LISTDIR\n{\n    LISTDIR=\"$UNKNOWN_FOLDER\"\n}\n").unwrap();
        let Statement::Recipe(recipe) = &config.statements[0] else {
            panic!("expected recipe");
        };
        let ConditionKind::Program(command) = &recipe.conditions[0].kind else {
            panic!("expected program condition");
        };
        assert_eq!(command, "test ! -e $LISTDIR");
        let RecipeAction::Block(statements) = &recipe.action else {
            panic!("expected block");
        };
        let Statement::Assignment(assignment) = &statements[0] else {
            panic!("expected assignment");
        };
        assert_eq!(assignment.value, "$UNKNOWN_FOLDER");
    }

    #[test]
    fn rejects_unterminated_quoted_assignment() {
        let error = parse("NAME=\"value\n").unwrap_err();

        assert_eq!(error.line, 1);
        assert_eq!(error.message, "unterminated double-quoted assignment value");
    }

    #[test]
    fn bounds_and_validates_program_condition_text() {
        for length in [MAX_PIPE_COMMAND_LEN - 1, MAX_PIPE_COMMAND_LEN] {
            let source = format!(":0\n* ? {}\nmaildir:selected\n", "x".repeat(length));
            assert!(parse(&source).is_ok(), "length {length} must be accepted");
        }

        let source = format!(
            ":0\n* ? {}\nmaildir:selected\n",
            "x".repeat(MAX_PIPE_COMMAND_LEN + 1)
        );
        let error = parse(&source).unwrap_err();
        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            format!(
                "program condition command exceeds the hard limit of {MAX_PIPE_COMMAND_LEN} bytes"
            )
        );

        let error = parse(":0\n* ?\nmaildir:selected\n").unwrap_err();
        assert_eq!(error.line, 2);
        assert_eq!(error.message, "program condition command is empty");
    }

    #[test]
    fn parses_include_and_switch_as_ordered_statements() {
        let config = parse("INCLUDERC=common.rc\n:0\n{\nSWITCHRC=$MATCH\n}\n").unwrap();
        let Statement::Include(include) = &config.statements[0] else {
            panic!("expected include");
        };
        let Statement::Recipe(recipe) = &config.statements[1] else {
            panic!("expected recipe");
        };
        let RecipeAction::Block(statements) = &recipe.action else {
            panic!("expected block");
        };
        let Statement::Switch(switch) = &statements[0] else {
            panic!("expected switch");
        };

        assert_eq!(include.value, "common.rc");
        assert_eq!(switch.value, "$MATCH");
    }

    #[test]
    fn parses_typed_nested_block_actions() {
        let config = parse(":0\n* ^List-Id:\n{\n:0\nmaildir:list\n}\n").unwrap();
        let [Statement::Recipe(parent)] = config.statements.as_slice() else {
            panic!("expected one parent recipe");
        };
        let RecipeAction::Block(children) = &parent.action else {
            panic!("expected block action");
        };
        let [Statement::Recipe(child)] = children.as_slice() else {
            panic!("expected one child recipe");
        };
        assert_eq!(
            child.action,
            RecipeAction::Deliver(Destination::Maildir("list".into()))
        );
    }

    #[test]
    fn rejects_unmatched_closing_recipe_block() {
        let error = parse(":0\n}\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            "closing recipe block has no matching opening block"
        );
    }

    #[test]
    fn rejects_invalid_regex_at_condition_line() {
        let error = parse(":0\n* [unterminated\ninbox/\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert!(error.message.starts_with("invalid regular expression:"));
    }

    #[test]
    fn parses_variable_regex_condition() {
        let config = parse(":0\n* CATEGORY ?? ^alerts$\nmaildir:matched\n").unwrap();
        let [Statement::Recipe(recipe)] = config.statements.as_slice() else {
            panic!("expected one recipe");
        };
        let ConditionKind::VariableRegex { name, regex } = &recipe.conditions[0].kind else {
            panic!("expected a variable regex condition");
        };

        assert_eq!(name, "CATEGORY");
        assert_eq!(regex.pattern(), "^alerts$");
    }

    #[test]
    fn parses_match_marker_without_exposing_its_helper_group() {
        let config = parse(":0\n* ^Subject: ([a-z]+)-\\/([a-z]+)$\nmaildir:matched\n").unwrap();
        let Statement::Recipe(recipe) = &config.statements[0] else {
            panic!("expected recipe");
        };
        let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
            panic!("expected regex");
        };

        assert!(regex.match_capture().is_some());
        assert_eq!(regex.capture_indexes().len(), 2);
    }

    #[test]
    fn bounds_and_validates_capture_syntax() {
        let accepted = format!(
            ":0\n* {}\nmaildir:matched\n",
            "()".repeat(MAX_REGEX_CAPTURES)
        );
        assert!(parse(&accepted).is_ok());

        let rejected = format!(
            ":0\n* {}\nmaildir:matched\n",
            "()".repeat(MAX_REGEX_CAPTURES + 1)
        );
        assert!(
            parse(&rejected)
                .unwrap_err()
                .message
                .contains("capture count exceeds")
        );

        let duplicate = parse(":0\n* left\\/middle\\/right\nmaildir:matched\n").unwrap_err();
        assert!(duplicate.message.contains("more than one '\\/'"));

        let named = parse(":0\n* (?P<name>value)\nmaildir:matched\n").unwrap_err();
        assert_eq!(
            named.message,
            "named regular expression groups are not supported"
        );
    }

    #[test]
    fn translates_supported_procmail_regex_extensions() {
        let start = parse(":0\n* ^^start\nmaildir:matched\n").unwrap();
        let Statement::Recipe(recipe) = &start.statements[0] else {
            panic!("expected recipe");
        };
        let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
            panic!("expected regex");
        };
        assert_eq!(regex.compiled().as_str(), "\\Astart");

        let end = parse(":0\n* end^^\nmaildir:matched\n").unwrap();
        let Statement::Recipe(recipe) = &end.statements[0] else {
            panic!("expected recipe");
        };
        let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
            panic!("expected regex");
        };
        assert_eq!(regex.compiled().as_str(), "end\\z");

        let lines = parse(":0\n* ^line$\nmaildir:matched\n").unwrap();
        let Statement::Recipe(recipe) = &lines.statements[0] else {
            panic!("expected recipe");
        };
        let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
            panic!("expected regex");
        };
        assert_eq!(regex.compiled().as_str(), "(?:\\A|\\n)line(?:\\n|\\z)");

        let words = parse(":0\n* \\<word\\>\nmaildir:matched\n").unwrap();
        let Statement::Recipe(recipe) = &words.statements[0] else {
            panic!("expected recipe");
        };
        let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
            panic!("expected regex");
        };
        assert_eq!(regex.compiled().as_str(), "[^a-zA-Z0-9_]word[^a-zA-Z0-9_]");

        let middle = parse(":0\n* left^^right\nmaildir:matched\n").unwrap_err();
        assert_eq!(
            middle.message,
            "'^^' is supported only at the start or end of a regular expression"
        );
    }

    #[test]
    fn parses_special_procmail_condition_areas() {
        for (name, expected) in [
            ("H", ConditionInput::Headers),
            ("B", ConditionInput::Body),
            ("HB", ConditionInput::Message),
            ("BH", ConditionInput::Message),
        ] {
            let config = parse(&format!(":0\n* {name} ?? pattern\nmaildir:matched\n")).unwrap();
            let Statement::Recipe(recipe) = &config.statements[0] else {
                panic!("expected recipe");
            };
            let ConditionKind::AreaRegex { area, regex } = &recipe.conditions[0].kind else {
                panic!("expected an area regex condition");
            };
            assert_eq!(*area, expected);
            assert_eq!(regex.pattern(), "pattern");
        }
    }

    #[test]
    fn enforces_regex_pattern_length_at_the_boundary() {
        for length in [
            MAX_REGEX_PATTERN_LEN - 1,
            MAX_REGEX_PATTERN_LEN,
            MAX_REGEX_PATTERN_LEN + 1,
        ] {
            let source = format!(":0\n* {}\ninbox/\n", "a".repeat(length));
            let result = parse(&source);

            if length <= MAX_REGEX_PATTERN_LEN {
                let config = result.unwrap();
                let Statement::Recipe(recipe) = &config.statements[0] else {
                    panic!("expected recipe");
                };
                let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
                    panic!("expected regular expression");
                };
                assert_eq!(regex.pattern().len(), length);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 2);
                assert_eq!(
                    error.message,
                    format!(
                        "regular expression exceeds the hard limit of {MAX_REGEX_PATTERN_LEN} bytes"
                    )
                );
            }
        }
    }

    #[test]
    fn size_conditions_do_not_use_regex_pattern_limit() {
        let value = "9".repeat(MAX_REGEX_PATTERN_LEN + 1);
        let error = parse(&format!(":0\n* < {value}\ninbox/\n")).unwrap_err();

        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            "size condition requires a non-negative integer"
        );
    }

    #[test]
    fn rejects_regex_above_compiled_size_limit() {
        let error = parse(":0\n* (?:a.){65535}\ninbox/\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert!(error.message.starts_with("invalid regular expression:"));
        assert!(error.message.contains("Compiled regex exceeds size limit"));
    }

    #[test]
    fn enforces_regex_count_at_the_boundary() {
        for count in [MAX_RC_REGEXES - 1, MAX_RC_REGEXES, MAX_RC_REGEXES + 1] {
            let source = config_with_regexes(count);
            let result = parse(&source);

            if count <= MAX_RC_REGEXES {
                assert!(result.is_ok());
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    error.message,
                    format!("rc regex count exceeds the active limit of {MAX_RC_REGEXES}")
                );
            }
        }
    }

    fn config_with_regexes(mut count: usize) -> String {
        let mut source = String::new();
        while count > 0 {
            let recipe_regexes = count.min(MAX_CONDITIONS_PER_RECIPE);
            source.push_str(":0 c\n");
            source.push_str(&"* pattern\n".repeat(recipe_regexes));
            source.push_str("inbox/\n");
            count -= recipe_regexes;
        }
        source
    }

    #[test]
    fn size_conditions_do_not_count_as_regexes() {
        let mut source = format!(":0\n{}", "* pattern\n".repeat(MAX_RC_REGEXES));
        source.push_str(&"* < 100\n".repeat(MAX_CONDITIONS_PER_RECIPE - MAX_RC_REGEXES));
        source.push_str("inbox/\n");

        assert!(parse(&source).is_ok());
    }

    #[test]
    fn accumulates_regex_count_across_recipes() {
        let mut source = ":0 c\n* pattern\ninbox/\n".repeat(MAX_RC_REGEXES);
        source.push_str(":0\n* excess\ninbox/\n");

        let error = parse(&source).unwrap_err();

        assert_eq!(error.line, MAX_RC_REGEXES * 3 + 2);
        assert_eq!(
            error.message,
            format!("rc regex count exceeds the active limit of {MAX_RC_REGEXES}")
        );
    }

    #[test]
    fn rejects_oversized_source_before_splitting_lines() {
        for length in [MAX_RC_SIZE - 1, MAX_RC_SIZE, MAX_RC_SIZE + 1] {
            let source = format!("#{}", "x".repeat(length - 1));
            let result = parse(&source);

            if length <= MAX_RC_SIZE {
                assert!(result.is_ok());
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 1);
                assert_eq!(
                    error.message,
                    format!("rc file exceeds the hard limit of {MAX_RC_SIZE} bytes")
                );
            }
        }
    }

    #[test]
    fn enforces_assignment_name_length_at_the_boundary() {
        for length in [
            MAX_ASSIGNMENT_NAME_LEN - 1,
            MAX_ASSIGNMENT_NAME_LEN,
            MAX_ASSIGNMENT_NAME_LEN + 1,
        ] {
            let source = format!("{}=value\n", "A".repeat(length));
            let result = parse(&source);

            if length <= MAX_ASSIGNMENT_NAME_LEN {
                let config = result.unwrap();
                let Statement::Assignment(assignment) = &config.statements[0] else {
                    panic!("expected assignment");
                };
                assert_eq!(assignment.name.len(), length);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 1);
                assert_eq!(
                    error.message,
                    format!(
                        "assignment name exceeds the hard limit of {MAX_ASSIGNMENT_NAME_LEN} bytes"
                    )
                );
            }
        }
    }

    #[test]
    fn enforces_assignment_value_length_at_the_boundary() {
        for length in [
            MAX_ASSIGNMENT_VALUE_LEN - 1,
            MAX_ASSIGNMENT_VALUE_LEN,
            MAX_ASSIGNMENT_VALUE_LEN + 1,
        ] {
            let source = format!("VALUE={}\n", "x".repeat(length));
            let result = parse(&source);

            if length <= MAX_ASSIGNMENT_VALUE_LEN {
                let config = result.unwrap();
                let Statement::Assignment(assignment) = &config.statements[0] else {
                    panic!("expected assignment");
                };
                assert_eq!(assignment.value.len(), length);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 1);
                assert_eq!(
                    error.message,
                    format!(
                        "assignment value exceeds the hard limit of {MAX_ASSIGNMENT_VALUE_LEN} bytes"
                    )
                );
            }
        }
    }

    #[test]
    fn enforces_shell_setting_length_at_the_boundary() {
        for name in ["SHELL", "SHELLFLAGS", "PATH"] {
            let accepted = format!("{name}={}\n", "x".repeat(MAX_SHELL_SETTING_LEN));
            assert!(parse(&accepted).is_ok(), "{name}");

            let rejected = format!("{name}={}\n", "x".repeat(MAX_SHELL_SETTING_LEN + 1));
            let error = parse(&rejected).unwrap_err();
            assert_eq!(
                error.message,
                format!(
                    "{name} value exceeds the hard limit of {} bytes",
                    MAX_SHELL_SETTING_LEN
                )
            );
        }
    }

    #[test]
    fn excludes_assignment_formatting_from_value_length() {
        let source = format!(
            "VALUE=  {}  # ignored\n",
            "x".repeat(MAX_ASSIGNMENT_VALUE_LEN)
        );
        let config = parse(&source).unwrap();
        let Statement::Assignment(assignment) = &config.statements[0] else {
            panic!("expected assignment");
        };

        assert_eq!(assignment.value.len(), MAX_ASSIGNMENT_VALUE_LEN);
    }

    #[test]
    fn enforces_destination_path_length_at_the_boundary() {
        for length in [
            MAX_PATH_EXPRESSION_LEN - 1,
            MAX_PATH_EXPRESSION_LEN,
            MAX_PATH_EXPRESSION_LEN + 1,
        ] {
            let source = format!(":0\nmaildir:{}\n", "x".repeat(length));
            let result = parse(&source);

            if length <= MAX_PATH_EXPRESSION_LEN {
                let config = result.unwrap();
                let Statement::Recipe(recipe) = &config.statements[0] else {
                    panic!("expected recipe");
                };
                let RecipeAction::Deliver(Destination::Maildir(path)) = &recipe.action else {
                    panic!("expected Maildir destination");
                };
                assert_eq!(path.source().len(), length);
            } else {
                assert_path_limit_error(result.unwrap_err(), 2, "destination path");
            }
        }
    }

    #[test]
    fn applies_path_limit_to_every_destination_form() {
        let oversized = "x".repeat(MAX_PATH_EXPRESSION_LEN + 1);
        for action in [
            format!("mbox:{oversized}"),
            oversized.clone(),
            format!("{oversized}/"),
        ] {
            let error = parse(&format!(":0\n{action}\n")).unwrap_err();
            assert_path_limit_error(error, 2, "destination path");
        }
    }

    #[test]
    fn enforces_lockfile_path_length_at_the_boundary() {
        for length in [
            MAX_PATH_EXPRESSION_LEN - 1,
            MAX_PATH_EXPRESSION_LEN,
            MAX_PATH_EXPRESSION_LEN + 1,
        ] {
            let source = format!(":0 :{}\ninbox/\n", "x".repeat(length));
            let result = parse(&source);

            if length <= MAX_PATH_EXPRESSION_LEN {
                let config = result.unwrap();
                let Statement::Recipe(recipe) = &config.statements[0] else {
                    panic!("expected recipe");
                };
                assert_eq!(recipe.lock.as_ref().unwrap().len(), length);
            } else {
                assert_path_limit_error(result.unwrap_err(), 1, "lockfile path");
            }
        }
    }

    #[test]
    fn applies_path_limit_to_maildir_assignment() {
        let source = format!("MAILDIR={}\n", "x".repeat(MAX_PATH_EXPRESSION_LEN + 1));
        let error = parse(&source).unwrap_err();

        assert_path_limit_error(error, 1, "MAILDIR path");
    }

    fn assert_path_limit_error(error: ParseError, line: usize, description: &str) {
        assert_eq!(error.line, line);
        assert_eq!(
            error.message,
            format!("{description} exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes")
        );
    }

    #[test]
    fn enforces_statement_count_at_the_boundary() {
        for count in [
            MAX_RC_STATEMENTS - 1,
            MAX_RC_STATEMENTS,
            MAX_RC_STATEMENTS + 1,
        ] {
            let source = "A=\n".repeat(count);
            let result = parse(&source);

            if count <= MAX_RC_STATEMENTS {
                assert_eq!(result.unwrap().statements.len(), count);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, MAX_RC_STATEMENTS + 1);
                assert_eq!(
                    error.message,
                    format!("rc statement count exceeds the active limit of {MAX_RC_STATEMENTS}")
                );
            }
        }
    }

    #[test]
    fn comments_and_blank_lines_do_not_count_as_statements() {
        let mut source = "A=\n".repeat(MAX_RC_STATEMENTS);
        source.push_str("# comment\n\n");

        assert_eq!(parse(&source).unwrap().statements.len(), MAX_RC_STATEMENTS);
    }

    #[test]
    fn enforces_recipe_count_at_the_boundary() {
        for count in [MAX_RC_RECIPES - 1, MAX_RC_RECIPES, MAX_RC_RECIPES + 1] {
            let source = ":0\ninbox/\n".repeat(count);
            let result = parse(&source);

            if count <= MAX_RC_RECIPES {
                assert_eq!(result.unwrap().statements.len(), count);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 2 * MAX_RC_RECIPES + 1);
                assert_eq!(
                    error.message,
                    format!("rc recipe count exceeds the active limit of {MAX_RC_RECIPES}")
                );
            }
        }
    }

    #[test]
    fn assignments_do_not_count_as_recipes() {
        let mut source = ":0\ninbox/\n".repeat(MAX_RC_RECIPES);
        source.push_str("MAILDIR=mail\n");

        assert_eq!(parse(&source).unwrap().statements.len(), MAX_RC_RECIPES + 1);
    }

    #[test]
    fn enforces_conditions_per_recipe_at_the_boundary() {
        for count in [
            MAX_CONDITIONS_PER_RECIPE - 1,
            MAX_CONDITIONS_PER_RECIPE,
            MAX_CONDITIONS_PER_RECIPE + 1,
        ] {
            let source = format!(":0\n{}inbox/\n", "* < 1\n".repeat(count));
            let result = parse(&source);

            if count <= MAX_CONDITIONS_PER_RECIPE {
                let config = result.unwrap();
                let Statement::Recipe(recipe) = &config.statements[0] else {
                    panic!("expected recipe");
                };
                assert_eq!(recipe.conditions.len(), count);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, MAX_CONDITIONS_PER_RECIPE + 2);
                assert_eq!(
                    error.message,
                    format!(
                        "recipe condition count exceeds the active limit of {MAX_CONDITIONS_PER_RECIPE}"
                    )
                );
            }
        }
    }

    #[test]
    fn enforces_total_condition_count_at_the_boundary() {
        for count in [
            MAX_RC_CONDITIONS - 1,
            MAX_RC_CONDITIONS,
            MAX_RC_CONDITIONS + 1,
        ] {
            let source = config_with_conditions(count);
            let result = parse(&source);

            if count <= MAX_RC_CONDITIONS {
                assert!(result.is_ok());
            } else {
                let error = result.unwrap_err();
                let full_recipes = MAX_RC_CONDITIONS / MAX_CONDITIONS_PER_RECIPE;
                let expected_line = full_recipes * (MAX_CONDITIONS_PER_RECIPE + 2) + 2;
                assert_eq!(error.line, expected_line);
                assert_eq!(
                    error.message,
                    format!("rc condition count exceeds the active limit of {MAX_RC_CONDITIONS}")
                );
            }
        }
    }

    fn config_with_conditions(mut count: usize) -> String {
        let mut source = String::new();
        while count > 0 {
            let recipe_conditions = count.min(MAX_CONDITIONS_PER_RECIPE);
            source.push_str(":0\n");
            source.push_str(&"* < 1\n".repeat(recipe_conditions));
            source.push_str("inbox/\n");
            count -= recipe_conditions;
        }
        source
    }
}
