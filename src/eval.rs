use std::fmt;

use regex::bytes::Regex;

use crate::config::{ConditionKind, Config, Destination, Recipe, Statement};
use crate::message::{Message, MessageHead, StreamedMessage};

pub trait Delivery {
    fn deliver(&mut self, destination: &Destination, message: &Message) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputRequirements {
    pub needs_headers: bool,
    pub needs_body_contents: bool,
    pub needs_end_of_message: bool,
}

impl InputRequirements {
    fn union(self, other: Self) -> Self {
        Self {
            needs_headers: self.needs_headers || other.needs_headers,
            needs_body_contents: self.needs_body_contents || other.needs_body_contents,
            needs_end_of_message: self.needs_end_of_message || other.needs_end_of_message,
        }
    }
}

#[derive(Debug)]
pub struct ExecutionPlan {
    recipes: Vec<CompiledRecipe>,
    suffix_requirements: Vec<InputRequirements>,
    requirements: InputRequirements,
}

#[derive(Debug)]
struct CompiledRecipe {
    conditions: Vec<CompiledCondition>,
    destination: Destination,
    copy: bool,
}

#[derive(Debug)]
struct CompiledCondition {
    negated: bool,
    kind: CompiledConditionKind,
}

#[derive(Debug)]
enum CompiledConditionKind {
    HeaderRegex(Regex),
    BodyRegex(Regex),
    MessageRegex(Regex),
    SmallerThan(usize),
    LargerThan(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPlan {
    destinations: Vec<Destination>,
    original_delivered: bool,
}

impl DeliveryPlan {
    pub fn destinations(&self) -> &[Destination] {
        &self.destinations
    }

    pub fn original_delivered(&self) -> bool {
        self.original_delivered
    }

    pub fn copies(&self) -> usize {
        self.destinations
            .len()
            .saturating_sub(usize::from(self.original_delivered))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderEvaluation {
    Decided(DeliveryPlan),
    NeedsMessage(Continuation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continuation {
    recipe_index: usize,
    destinations: Vec<Destination>,
    requirements: InputRequirements,
}

impl Continuation {
    pub fn requirements(&self) -> InputRequirements {
        self.requirements
    }

    pub fn pending_destinations(&self) -> &[Destination] {
        &self.destinations
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Delivered { deliveries: usize },
    Undelivered { copies: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    BodyWasNotBuffered,
    Delivery {
        destination: String,
        message: String,
    },
}

impl fmt::Display for EvalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyWasNotBuffered => {
                formatter.write_str("execution plan requires body contents that were not buffered")
            }
            Self::Delivery {
                destination,
                message,
            } => write!(formatter, "cannot deliver to {destination}: {message}"),
        }
    }
}

impl std::error::Error for EvalError {}

impl ExecutionPlan {
    pub fn compile(config: &Config) -> Self {
        let mut recipes = Vec::new();
        for statement in &config.statements {
            let Statement::Recipe(recipe) = statement else {
                continue;
            };
            recipes.push(CompiledRecipe::compile(recipe));
        }

        let mut suffix_requirements = vec![InputRequirements::default(); recipes.len() + 1];
        for index in (0..recipes.len()).rev() {
            suffix_requirements[index] = recipes[index]
                .requirements()
                .union(suffix_requirements[index + 1]);
        }
        let requirements = suffix_requirements[0];

        Self {
            recipes,
            suffix_requirements,
            requirements,
        }
    }

    pub fn requirements(&self) -> InputRequirements {
        self.requirements
    }

    pub fn evaluate_headers(&self, head: &MessageHead) -> HeaderEvaluation {
        let mut destinations = Vec::new();

        for (index, recipe) in self.recipes.iter().enumerate() {
            match recipe.matches_headers(head) {
                PartialMatch::False => continue,
                PartialMatch::Deferred => {
                    return HeaderEvaluation::NeedsMessage(Continuation {
                        recipe_index: index,
                        destinations,
                        requirements: self.suffix_requirements[index],
                    });
                }
                PartialMatch::True => {
                    destinations.push(recipe.destination.clone());
                    if !recipe.copy {
                        return HeaderEvaluation::Decided(DeliveryPlan {
                            destinations,
                            original_delivered: true,
                        });
                    }
                }
            }
        }

        HeaderEvaluation::Decided(DeliveryPlan {
            destinations,
            original_delivered: false,
        })
    }

    pub fn resume_buffered(
        &self,
        continuation: Continuation,
        message: &Message,
    ) -> Result<DeliveryPlan, EvalError> {
        self.resume(continuation, CompleteMessage::Buffered(message))
    }

    pub fn resume_streamed(
        &self,
        continuation: Continuation,
        message: &StreamedMessage,
    ) -> Result<DeliveryPlan, EvalError> {
        if continuation.requirements.needs_body_contents {
            return Err(EvalError::BodyWasNotBuffered);
        }
        self.resume(continuation, CompleteMessage::Streamed(message))
    }

    pub fn resume_mapped(
        &self,
        continuation: Continuation,
        raw: &[u8],
        header_len: usize,
    ) -> Result<DeliveryPlan, EvalError> {
        if header_len > raw.len() {
            return Err(EvalError::BodyWasNotBuffered);
        }
        self.resume(continuation, CompleteMessage::Mapped { raw, header_len })
    }

    pub fn evaluate_full(&self, message: &Message) -> Result<DeliveryPlan, EvalError> {
        self.resume(
            Continuation {
                recipe_index: 0,
                destinations: Vec::new(),
                requirements: self.requirements,
            },
            CompleteMessage::Buffered(message),
        )
    }

    fn resume(
        &self,
        continuation: Continuation,
        message: CompleteMessage<'_>,
    ) -> Result<DeliveryPlan, EvalError> {
        let mut destinations = continuation.destinations;

        for recipe in &self.recipes[continuation.recipe_index..] {
            if !recipe.matches_complete(message)? {
                continue;
            }
            destinations.push(recipe.destination.clone());
            if !recipe.copy {
                return Ok(DeliveryPlan {
                    destinations,
                    original_delivered: true,
                });
            }
        }

        Ok(DeliveryPlan {
            destinations,
            original_delivered: false,
        })
    }
}

impl CompiledRecipe {
    fn compile(recipe: &Recipe) -> Self {
        let area = match (recipe.has_flag('H'), recipe.has_flag('B')) {
            (false, true) => RegexArea::Body,
            (true, true) => RegexArea::Message,
            _ => RegexArea::Headers,
        };
        let mut conditions = Vec::with_capacity(recipe.conditions.len());

        for condition in &recipe.conditions {
            let kind = match &condition.kind {
                ConditionKind::SmallerThan(size) => CompiledConditionKind::SmallerThan(*size),
                ConditionKind::LargerThan(size) => CompiledConditionKind::LargerThan(*size),
                ConditionKind::Regex(regex) => {
                    // Parsing already validated and compiled this expression.
                    // Cloning Regex shares its read-only compiled program, so
                    // execution planning cannot repeat attacker-controlled
                    // compilation work after configuration validation.
                    let regex = regex.compiled().clone();
                    match area {
                        RegexArea::Headers => CompiledConditionKind::HeaderRegex(regex),
                        RegexArea::Body => CompiledConditionKind::BodyRegex(regex),
                        RegexArea::Message => CompiledConditionKind::MessageRegex(regex),
                    }
                }
            };
            conditions.push(CompiledCondition {
                negated: condition.negated,
                kind,
            });
        }

        Self {
            conditions,
            destination: recipe.destination.clone(),
            copy: recipe.has_flag('c'),
        }
    }

    fn requirements(&self) -> InputRequirements {
        self.conditions
            .iter()
            .fold(InputRequirements::default(), |requirements, condition| {
                requirements.union(condition.requirements())
            })
    }

    fn matches_headers(&self, head: &MessageHead) -> PartialMatch {
        let mut deferred = false;
        for condition in &self.conditions {
            match condition.matches_headers(head) {
                PartialMatch::False => return PartialMatch::False,
                PartialMatch::Deferred => deferred = true,
                PartialMatch::True => {}
            }
        }
        if deferred {
            PartialMatch::Deferred
        } else {
            PartialMatch::True
        }
    }

    fn matches_complete(&self, message: CompleteMessage<'_>) -> Result<bool, EvalError> {
        for condition in &self.conditions {
            if !condition.matches_complete(message)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl CompiledCondition {
    fn requirements(&self) -> InputRequirements {
        match self.kind {
            CompiledConditionKind::HeaderRegex(_) => InputRequirements {
                needs_headers: true,
                ..InputRequirements::default()
            },
            CompiledConditionKind::BodyRegex(_) | CompiledConditionKind::MessageRegex(_) => {
                InputRequirements {
                    needs_headers: true,
                    needs_body_contents: true,
                    needs_end_of_message: true,
                }
            }
            CompiledConditionKind::SmallerThan(_) | CompiledConditionKind::LargerThan(_) => {
                InputRequirements {
                    needs_end_of_message: true,
                    ..InputRequirements::default()
                }
            }
        }
    }

    fn matches_headers(&self, head: &MessageHead) -> PartialMatch {
        let matched = match &self.kind {
            CompiledConditionKind::HeaderRegex(regex) => {
                return PartialMatch::from_bool(regex.is_match(head.as_bytes()) ^ self.negated);
            }
            CompiledConditionKind::BodyRegex(_) | CompiledConditionKind::MessageRegex(_) => {
                return PartialMatch::Deferred;
            }
            CompiledConditionKind::SmallerThan(size) => {
                if head.len() >= *size {
                    false
                } else {
                    return PartialMatch::Deferred;
                }
            }
            CompiledConditionKind::LargerThan(size) => {
                if head.len() > *size {
                    true
                } else {
                    return PartialMatch::Deferred;
                }
            }
        };
        PartialMatch::from_bool(matched ^ self.negated)
    }

    fn matches_complete(&self, message: CompleteMessage<'_>) -> Result<bool, EvalError> {
        let matched = match &self.kind {
            CompiledConditionKind::HeaderRegex(regex) => regex.is_match(message.header_bytes()),
            CompiledConditionKind::BodyRegex(regex) => {
                regex.is_match(message.body().ok_or(EvalError::BodyWasNotBuffered)?)
            }
            CompiledConditionKind::MessageRegex(regex) => {
                regex.is_match(message.full().ok_or(EvalError::BodyWasNotBuffered)?)
            }
            CompiledConditionKind::SmallerThan(size) => message.len() < *size,
            CompiledConditionKind::LargerThan(size) => message.len() > *size,
        };
        Ok(matched ^ self.negated)
    }
}

#[derive(Debug, Clone, Copy)]
enum CompleteMessage<'a> {
    Buffered(&'a Message),
    Streamed(&'a StreamedMessage),
    Mapped { raw: &'a [u8], header_len: usize },
}

impl<'a> CompleteMessage<'a> {
    fn header_bytes(self) -> &'a [u8] {
        match self {
            Self::Buffered(message) => message.header(),
            Self::Streamed(message) => message.header(),
            Self::Mapped { raw, header_len } => &raw[..header_len],
        }
    }

    fn body(self) -> Option<&'a [u8]> {
        match self {
            Self::Buffered(message) => Some(message.body()),
            Self::Streamed(_) => None,
            Self::Mapped { raw, header_len } => Some(&raw[header_len..]),
        }
    }

    fn full(self) -> Option<&'a [u8]> {
        match self {
            Self::Buffered(message) => Some(message.as_bytes()),
            Self::Streamed(_) => None,
            Self::Mapped { raw, .. } => Some(raw),
        }
    }

    fn len(self) -> usize {
        match self {
            Self::Buffered(message) => message.len(),
            Self::Streamed(message) => message.len(),
            Self::Mapped { raw, .. } => raw.len(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartialMatch {
    True,
    False,
    Deferred,
}

impl PartialMatch {
    fn from_bool(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }
}

#[derive(Debug, Clone, Copy)]
enum RegexArea {
    Headers,
    Body,
    Message,
}

pub fn evaluate(
    config: &Config,
    message: &Message,
    delivery: &mut impl Delivery,
) -> Result<Outcome, EvalError> {
    let plan = ExecutionPlan::compile(config);
    let delivery_plan = plan.evaluate_full(message)?;
    execute_deliveries(&delivery_plan, message, delivery)
}

fn execute_deliveries(
    plan: &DeliveryPlan,
    message: &Message,
    delivery: &mut impl Delivery,
) -> Result<Outcome, EvalError> {
    for destination in &plan.destinations {
        delivery
            .deliver(destination, message)
            .map_err(|error| EvalError::Delivery {
                destination: destination_name(destination).to_owned(),
                message: error,
            })?;
    }

    if plan.original_delivered {
        Ok(Outcome::Delivered {
            deliveries: plan.destinations.len(),
        })
    } else {
        Ok(Outcome::Undelivered {
            copies: plan.destinations.len(),
        })
    }
}

fn destination_name(destination: &Destination) -> &str {
    match destination {
        Destination::Mbox(path) | Destination::Maildir(path) => path,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::config;
    use crate::limits::MessageLimits;

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

    fn compile(source: &str) -> ExecutionPlan {
        ExecutionPlan::compile(&config::parse(source).unwrap())
    }

    fn evaluate_config(source: &str, raw: &[u8]) -> (Outcome, Recorder) {
        let config = config::parse(source).unwrap();
        let message = Message::from_bytes(raw.to_vec());
        let mut recorder = Recorder::default();
        let outcome = evaluate(&config, &message, &mut recorder).unwrap();
        (outcome, recorder)
    }

    fn head(raw: &[u8]) -> MessageHead {
        Message::read_headers(&mut Cursor::new(raw), MessageLimits::default()).unwrap()
    }

    #[test]
    fn computes_static_input_requirements() {
        let header_only = compile(":0\n* ^Subject:\ninbox/\n");
        assert_eq!(
            header_only.requirements(),
            InputRequirements {
                needs_headers: true,
                needs_body_contents: false,
                needs_end_of_message: false,
            }
        );

        let body = compile(":0 B\n* needle\ninbox/\n");
        assert!(body.requirements().needs_body_contents);
        assert!(body.requirements().needs_end_of_message);

        let size = compile(":0\n* < 100\ninbox/\n");
        assert!(!size.requirements().needs_body_contents);
        assert!(size.requirements().needs_end_of_message);
    }

    #[test]
    fn header_match_decides_before_body() {
        let plan =
            compile(":0\n* ^Subject: wanted$\nmaildir:wanted\n\n:0 B\n* needle\nmaildir:body\n");
        let result = plan.evaluate_headers(&head(b"Subject: wanted\n\nbody"));

        let HeaderEvaluation::Decided(delivery) = result else {
            panic!("expected a header decision");
        };
        assert_eq!(
            delivery.destinations(),
            [Destination::Maildir("wanted".into())]
        );
    }

    #[test]
    fn unconditional_recipe_makes_later_body_rule_unreachable() {
        let plan = compile(":0\nmaildir:all\n\n:0 B\n* needle\nmaildir:body\n");
        let result = plan.evaluate_headers(&head(b"Subject: test\n\nbody"));

        let HeaderEvaluation::Decided(delivery) = result else {
            panic!("expected an unconditional decision");
        };
        assert_eq!(
            delivery.destinations(),
            [Destination::Maildir("all".into())]
        );
    }

    #[test]
    fn failed_header_match_defers_to_reachable_body_recipe() {
        let plan =
            compile(":0\n* ^Subject: wanted$\nmaildir:wanted\n\n:0 B\n* needle\nmaildir:body\n");
        let result = plan.evaluate_headers(&head(b"Subject: other\n\nbody"));

        let HeaderEvaluation::NeedsMessage(continuation) = result else {
            panic!("expected deferred evaluation");
        };
        assert!(continuation.requirements().needs_body_contents);
    }

    #[test]
    fn preserves_header_selected_copies_across_continuation() {
        let plan = compile(":0 c\n* ^List-Id:\nmaildir:copy\n\n:0 B\n* needle\nmaildir:body\n");
        let raw = b"List-Id: users.example\n\nneedle\n";
        let result = plan.evaluate_headers(&head(raw));
        let HeaderEvaluation::NeedsMessage(continuation) = result else {
            panic!("expected deferred evaluation");
        };
        assert_eq!(
            continuation.pending_destinations(),
            [Destination::Maildir("copy".into())]
        );

        let delivery = plan
            .resume_buffered(continuation, &Message::from_bytes(raw.to_vec()))
            .unwrap();
        assert_eq!(
            delivery.destinations(),
            [
                Destination::Maildir("copy".into()),
                Destination::Maildir("body".into())
            ]
        );
        assert!(delivery.original_delivered());
    }

    #[test]
    fn size_only_continuation_can_resume_without_buffered_body() {
        let plan = compile(":0\n* < 100\nmaildir:small\n");
        let raw = b"Subject: test\n\nbody";
        let HeaderEvaluation::NeedsMessage(continuation) = plan.evaluate_headers(&head(raw)) else {
            panic!("expected deferred evaluation");
        };
        assert!(!continuation.requirements().needs_body_contents);

        let mut reader = Cursor::new(raw);
        let head = Message::read_headers(&mut reader, MessageLimits::default()).unwrap();
        let streamed = head.stream_to(&mut reader, &mut Vec::new()).unwrap();
        let delivery = plan.resume_streamed(continuation, &streamed).unwrap();
        assert_eq!(
            delivery.destinations(),
            [Destination::Maildir("small".into())]
        );
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
