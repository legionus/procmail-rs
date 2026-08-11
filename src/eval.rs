use std::fmt;

use crate::config::{
    Condition, ConditionKind, Config, Destination, Recipe, Statement, build_regex,
};
use crate::message::Message;

pub trait Delivery {
    fn deliver(&mut self, destination: &Destination, message: &Message) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Delivered { deliveries: usize },
    Undelivered { copies: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    InvalidRegex {
        pattern: String,
        message: String,
    },
    Delivery {
        destination: String,
        message: String,
    },
}

impl fmt::Display for EvalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRegex { pattern, message } => {
                write!(
                    formatter,
                    "invalid regular expression '{pattern}': {message}"
                )
            }
            Self::Delivery {
                destination,
                message,
            } => write!(formatter, "cannot deliver to {destination}: {message}"),
        }
    }
}

impl std::error::Error for EvalError {}

pub fn evaluate(
    config: &Config,
    message: &Message,
    delivery: &mut impl Delivery,
) -> Result<Outcome, EvalError> {
    let mut deliveries = 0;

    for statement in &config.statements {
        let Statement::Recipe(recipe) = statement else {
            continue;
        };
        if !recipe_matches(recipe, message)? {
            continue;
        }

        delivery
            .deliver(&recipe.destination, message)
            .map_err(|error| EvalError::Delivery {
                destination: destination_name(&recipe.destination).to_owned(),
                message: error,
            })?;
        deliveries += 1;

        if !recipe.has_flag('c') {
            return Ok(Outcome::Delivered { deliveries });
        }
    }

    Ok(Outcome::Undelivered { copies: deliveries })
}

fn recipe_matches(recipe: &Recipe, message: &Message) -> Result<bool, EvalError> {
    recipe
        .conditions
        .iter()
        .try_fold(true, |matched, condition| {
            if !matched {
                return Ok(false);
            }
            condition_matches(condition, recipe, message)
        })
}

fn condition_matches(
    condition: &Condition,
    recipe: &Recipe,
    message: &Message,
) -> Result<bool, EvalError> {
    let matched = match &condition.kind {
        ConditionKind::SmallerThan(size) => message.len() < *size,
        ConditionKind::LargerThan(size) => message.len() > *size,
        ConditionKind::Regex(pattern) => {
            let regex = build_regex(pattern, recipe.has_flag('D')).map_err(|error| {
                EvalError::InvalidRegex {
                    pattern: pattern.clone(),
                    message: error.to_string(),
                }
            })?;
            regex.is_match(search_area(recipe, message))
        }
    };

    Ok(matched ^ condition.negated)
}

fn search_area<'a>(recipe: &Recipe, message: &'a Message) -> &'a [u8] {
    match (recipe.has_flag('H'), recipe.has_flag('B')) {
        (false, true) => message.body(),
        (true, true) => message.as_bytes(),
        _ => message.header(),
    }
}

fn destination_name(destination: &Destination) -> &str {
    match destination {
        Destination::Mbox(path) | Destination::Maildir(path) | Destination::Auto(path) => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    #[derive(Default)]
    struct Recorder {
        destinations: Vec<Destination>,
    }

    impl Delivery for Recorder {
        fn deliver(&mut self, destination: &Destination, _: &Message) -> Result<(), String> {
            self.destinations.push(destination.clone());
            Ok(())
        }
    }

    fn evaluate_config(source: &str, raw: &[u8]) -> (Outcome, Recorder) {
        let config = config::parse(source).unwrap();
        let message = Message::from_bytes(raw.to_vec());
        let mut recorder = Recorder::default();
        let outcome = evaluate(&config, &message, &mut recorder).unwrap();
        (outcome, recorder)
    }

    #[test]
    fn delivers_first_matching_recipe() {
        let (outcome, recorder) = evaluate_config(
            ":0\n* ^Subject: wanted$\nmaildir:wanted\n\n:0\nmaildir:fallback\n",
            b"Subject: wanted\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
        assert_eq!(
            recorder.destinations,
            [Destination::Maildir("wanted".into())]
        );
    }

    #[test]
    fn defaults_to_case_insensitive_header_matching() {
        let (outcome, _) = evaluate_config(
            ":0\n* ^subject: WANTED$\nmaildir:wanted\n",
            b"Subject: wanted\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn body_flag_limits_regex_to_body() {
        let (outcome, _) = evaluate_config(
            ":0 B\n* ^needle$\nmaildir:wanted\n",
            b"Subject: no\n\nneedle\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn combines_conditions_with_and_and_supports_negation() {
        let (outcome, _) = evaluate_config(
            ":0\n* ^Subject: wanted$\n* ! ^From: blocked@\nmaildir:wanted\n",
            b"From: allowed@example.org\nSubject: wanted\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn supports_size_conditions() {
        let (outcome, _) = evaluate_config(
            ":0\n* > 10\n* < 100\nmaildir:wanted\n",
            b"Subject: test\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 1 });
    }

    #[test]
    fn copy_recipe_continues_to_final_delivery() {
        let (outcome, recorder) = evaluate_config(
            ":0 c\nmaildir:copy\n\n:0\nmbox:final\n",
            b"Subject: test\n\nbody\n",
        );

        assert_eq!(outcome, Outcome::Delivered { deliveries: 2 });
        assert_eq!(
            recorder.destinations,
            [
                Destination::Maildir("copy".into()),
                Destination::Mbox("final".into())
            ]
        );
    }

    #[test]
    fn reports_copy_only_as_undelivered_original() {
        let (outcome, _) = evaluate_config(":0 c\nmaildir:copy\n", b"Subject: test\n\nbody\n");

        assert_eq!(outcome, Outcome::Undelivered { copies: 1 });
    }
}
