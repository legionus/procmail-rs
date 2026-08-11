use super::{Assignment, Config, Destination, ParseError, Recipe, Statement};

pub fn parse(input: &str) -> Result<Config, ParseError> {
    let lines: Vec<&str> = input.lines().collect();
    let mut statements = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line_number = index + 1;
        let line = lines[index].trim();

        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }

        if line.starts_with(':') {
            let (recipe, next) = parse_recipe(&lines, index)?;
            statements.push(Statement::Recipe(recipe));
            index = next;
            continue;
        }

        if let Some(assignment) = parse_assignment(line) {
            statements.push(Statement::Assignment(assignment));
            index += 1;
            continue;
        }

        return Err(ParseError::new(
            line_number,
            "expected an assignment or a recipe beginning with ':0'",
        ));
    }

    Ok(Config { statements })
}

fn parse_assignment(line: &str) -> Option<Assignment> {
    let (name, value) = line.split_once('=')?;
    let name = name.trim();
    if name.is_empty()
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphanumeric() && (index > 0 || !byte.is_ascii_digit())
        })
    {
        return None;
    }

    Some(Assignment {
        name: name.to_owned(),
        value: strip_comment(value.trim()).trim().to_owned(),
    })
}

fn parse_recipe(lines: &[&str], start: usize) -> Result<(Recipe, usize), ParseError> {
    let header = lines[start].trim();
    let rest = header
        .strip_prefix(":0")
        .ok_or_else(|| ParseError::new(start + 1, "only ':0' recipes are supported"))?;
    let (flags, lock) = parse_recipe_header(rest, start + 1)?;
    let mut conditions = Vec::new();
    let mut index = start + 1;

    while index < lines.len() {
        let line = lines[index].trim();
        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }
        if let Some(condition) = line.strip_prefix('*') {
            conditions.push(condition.trim().to_owned());
            index += 1;
            continue;
        }
        break;
    }

    let action = lines
        .get(index)
        .map(|line| line.trim())
        .ok_or_else(|| ParseError::new(start + 1, "recipe has no action"))?;

    if action.starts_with('|') {
        return Err(ParseError::new(index + 1, "pipe actions are not supported"));
    }
    if action.starts_with('!') {
        return Err(ParseError::new(
            index + 1,
            "forward actions are not supported",
        ));
    }
    if action.starts_with('{') || action == "}" {
        return Err(ParseError::new(
            index + 1,
            "recipe blocks are not supported yet",
        ));
    }
    if action.starts_with(':') {
        return Err(ParseError::new(start + 1, "recipe has no action"));
    }
    if action.is_empty() {
        return Err(ParseError::new(index + 1, "recipe action is empty"));
    }

    let destination = if let Some(path) = action.strip_prefix("mbox:") {
        Destination::Mbox(required_path(path, index + 1)?)
    } else if let Some(path) = action.strip_prefix("maildir:") {
        Destination::Maildir(required_path(path, index + 1)?)
    } else if action.ends_with('/') {
        Destination::Maildir(action.to_owned())
    } else {
        Destination::Auto(action.to_owned())
    };

    Ok((
        Recipe {
            flags,
            lock,
            conditions,
            destination,
        },
        index + 1,
    ))
}

fn parse_recipe_header(rest: &str, line: usize) -> Result<(String, Option<String>), ParseError> {
    let rest = strip_comment(rest).trim();
    let (flag_text, lock) = match rest.split_once(':') {
        Some((flags, lock)) => {
            let lock = lock.trim();
            (flags.trim(), Some(lock.to_owned()))
        }
        None => (rest, None),
    };

    if !flag_text.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(ParseError::new(line, "invalid recipe flags"));
    }

    Ok((flag_text.to_owned(), lock))
}

fn required_path(path: &str, line: usize) -> Result<String, ParseError> {
    let path = path.trim();
    if path.is_empty() {
        Err(ParseError::new(line, "destination path is empty"))
    } else {
        Ok(path.to_owned())
    }
}

fn strip_comment(value: &str) -> &str {
    value.split_once('#').map_or(value, |(value, _)| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_assignment_and_recipe() {
        let config =
            parse("MAILDIR=/srv/mail\n\n:0 Bc:\n* ! ^Subject: spam\nmaildir:inbox\n").unwrap();

        assert_eq!(config.statements.len(), 2);
        assert_eq!(
            config.statements[0],
            Statement::Assignment(Assignment {
                name: "MAILDIR".into(),
                value: "/srv/mail".into(),
            })
        );
        assert_eq!(
            config.statements[1],
            Statement::Recipe(Recipe {
                flags: "Bc".into(),
                lock: Some(String::new()),
                conditions: vec!["! ^Subject: spam".into()],
                destination: Destination::Maildir("inbox".into()),
            })
        );
    }

    #[test]
    fn trailing_slash_selects_maildir() {
        let config = parse(":0\ninbox/\n").unwrap();
        let Statement::Recipe(recipe) = &config.statements[0] else {
            panic!("expected recipe");
        };

        assert_eq!(recipe.destination, Destination::Maildir("inbox/".into()));
    }

    #[test]
    fn rejects_pipe_action_explicitly() {
        let error = parse(":0\n| command\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert_eq!(error.message, "pipe actions are not supported");
    }

    #[test]
    fn rejects_recipe_without_action() {
        let error = parse(":0\n* ^Subject:\n").unwrap_err();

        assert_eq!(error.line, 1);
        assert_eq!(error.message, "recipe has no action");
    }
}
