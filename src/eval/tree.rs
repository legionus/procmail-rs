// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::InputRequirements;
use super::condition::{CompiledCondition, compile_conditions};
use super::explanation::{
    ActionKindExplanation, ConditionExplanation, HeaderOperationExplanation, RecipeExplanation,
};
use super::runtime_rc::{CompiledInclude, CompiledSwitch};
use crate::config::{
    ActionInput, Assignment, AssignmentTarget, CommandAssignment, ContinuationMode, ControlFlow,
    Destination, HeaderAction, OutputEnding, PipeAction, Recipe, RecipeAction, RecipeOptions,
    Statement,
};
use crate::trace::VariableSource as TraceVariableSource;

#[derive(Debug)]
pub(super) struct CompiledSequence {
    pub(super) recipes: Vec<CompiledNode>,
    pub(super) trailing_statements: Vec<CompiledStatement>,
}

#[derive(Debug)]
pub(super) struct CompiledNode {
    pub(super) line: usize,
    pub(super) preceding_statements: Vec<CompiledStatement>,
    pub(super) lock: Option<crate::config::PathExpression>,
    pub(super) control: ControlFlow,
    pub(super) conditions: Vec<CompiledCondition>,
    pub(super) action: CompiledAction,
}

#[derive(Debug)]
pub(super) enum CompiledAction {
    Deliver {
        destination: Destination,
        continuation: ContinuationMode,
        output_ending: OutputEnding,
    },
    Pipe {
        action: PipeAction,
        options: RecipeOptions,
    },
    Capture {
        action: crate::config::CaptureAction,
        options: RecipeOptions,
    },
    Block(CompiledSequence),
    Headers(HeaderAction),
}

#[derive(Debug, Clone)]
pub(super) struct CompiledAssignment {
    pub(super) assignment: Assignment,
    pub(super) line: Option<usize>,
    pub(super) source: TraceVariableSource,
}

#[derive(Debug)]
pub(super) enum CompiledStatement {
    Assignment(CompiledAssignment),
    CommandAssignment(CommandAssignment),
    Host(CompiledAssignment),
    Include(CompiledInclude),
    Switch(CompiledSwitch),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SequenceState {
    pub(super) previous: Option<RecipeExecution>,
    pub(super) chain_base_matched: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RecipeExecution {
    pub(super) conditions_matched: bool,
    pub(super) else_handled: bool,
    pub(super) action: ActionExecution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActionExecution {
    NotAttempted,
    Succeeded,
    Failed,
}

fn statement_requires_ordered_message(statement: &CompiledStatement) -> bool {
    match statement {
        CompiledStatement::CommandAssignment(_) => true,
        CompiledStatement::Assignment(assignment) => {
            matches!(assignment.assignment.target, AssignmentTarget::LockFile)
                || assignment.assignment.target == AssignmentTarget::Trap
                    && !assignment.assignment.value.is_empty()
        }
        CompiledStatement::Host(_)
        | CompiledStatement::Include(_)
        | CompiledStatement::Switch(_) => false,
    }
}

fn statement_requirements(statement: &CompiledStatement) -> InputRequirements {
    // Procmail gives every backquoted command the complete current message,
    // regardless of where the substitution appears in its assignment. Mark
    // all three needs here so top-level, nested, and trailing statements use
    // the same conservative staging decision.
    if matches!(statement, CompiledStatement::CommandAssignment(_)) {
        InputRequirements {
            needs_headers: true,
            needs_body_contents: true,
            needs_end_of_message: true,
        }
    } else {
        InputRequirements::default()
    }
}

fn statements_requirements(statements: &[CompiledStatement]) -> InputRequirements {
    statements
        .iter()
        .fold(InputRequirements::default(), |requirements, statement| {
            requirements.union(statement_requirements(statement))
        })
}

impl CompiledSequence {
    pub(super) fn compile(
        statements: &[Statement],
        preceding: &mut Vec<CompiledStatement>,
    ) -> Self {
        let mut recipes = Vec::new();
        for statement in statements {
            match statement {
                Statement::Assignment(assignment) => {
                    let compiled = CompiledAssignment {
                        assignment: assignment.clone(),
                        line: Some(assignment.line),
                        source: TraceVariableSource::RcFile,
                    };
                    if assignment.target == AssignmentTarget::Host {
                        preceding.push(CompiledStatement::Host(compiled));
                    } else {
                        preceding.push(CompiledStatement::Assignment(compiled));
                    }
                }
                Statement::CommandAssignment(assignment) => {
                    preceding.push(CompiledStatement::CommandAssignment(assignment.clone()))
                }
                Statement::Recipe(recipe) => {
                    recipes.push(CompiledNode::compile(recipe, std::mem::take(preceding)));
                }
                Statement::Include(expression) => preceding.push(CompiledStatement::Include(
                    CompiledInclude::new(expression.clone()),
                )),
                Statement::Switch(expression) => preceding.push(CompiledStatement::Switch(
                    CompiledSwitch::new(expression.clone()),
                )),
            }
        }

        // Statements after the final recipe must remain executable because
        // include and switch operations may end an rc file without another
        // recipe to which the parser could attach them.
        Self {
            recipes,
            trailing_statements: std::mem::take(preceding),
        }
    }

    pub(super) fn requirements(&self) -> InputRequirements {
        let requirements = self
            .recipes
            .iter()
            .fold(InputRequirements::default(), |requirements, recipe| {
                requirements.union(recipe.requirements())
            });
        requirements.union(statements_requirements(&self.trailing_statements))
    }

    pub(super) fn requires_ordered_delivery(&self) -> bool {
        self.trailing_statements
            .iter()
            .any(statement_requires_ordered_message)
            || self.recipes.iter().enumerate().any(|(index, recipe)| {
                recipe.requires_ordered_delivery()
                    || recipe
                        .preceding_statements
                        .iter()
                        .any(statement_requires_ordered_message)
                    || (index != 0
                        && matches!(
                            recipe.control,
                            ControlFlow::AfterPreviousSuccess | ControlFlow::AfterPreviousError
                        ))
            })
    }

    pub(super) fn requires_preemptive_ordered_delivery(&self) -> bool {
        // Header edits can run while only the bounded header section is
        // available. Keep them out of this early deferral decision so a
        // header-only configuration does not read or stage the body merely
        // because a later action must observe the edited bytes.
        self.trailing_statements
            .iter()
            .any(statement_requires_ordered_message)
            || self.recipes.iter().any(|recipe| {
                recipe.requires_preemptive_ordered_delivery()
                    || recipe
                        .preceding_statements
                        .iter()
                        .any(statement_requires_ordered_message)
            })
            || self.recipes.windows(2).any(|pair| {
                matches!(
                    pair[1].control,
                    ControlFlow::AfterPreviousSuccess | ControlFlow::AfterPreviousError
                ) && !pair[0].header_action_result_is_known()
            })
    }

    pub(super) fn needs_message_contents(&self) -> bool {
        self.recipes
            .iter()
            .any(CompiledNode::needs_message_contents)
    }

    pub(super) fn requirements_from(&self, start: usize) -> InputRequirements {
        let recipes = &self.recipes[start..];
        let requirements = recipes
            .iter()
            .fold(InputRequirements::default(), |requirements, recipe| {
                requirements.union(recipe.requirements())
            });
        let requirements = requirements.union(statements_requirements(&self.trailing_statements));
        if recipes.iter().any(CompiledNode::requires_ordered_delivery) {
            requirements.union(InputRequirements {
                needs_end_of_message: true,
                ..InputRequirements::default()
            })
        } else {
            requirements
        }
    }

    pub(super) fn has_error_handler(&self, index: usize) -> bool {
        self.recipes
            .get(index + 1)
            .is_some_and(|next| next.control == ControlFlow::AfterPreviousError)
    }

    pub(super) fn collect_explanations(
        &self,
        inherited_conditions: &[ConditionExplanation],
        inherited_assignments: usize,
        explanations: &mut Vec<RecipeExplanation>,
    ) {
        for recipe in &self.recipes {
            let mut conditions = inherited_conditions.to_vec();
            conditions.extend(recipe.conditions.iter().map(CompiledCondition::explain));
            let assignment_count = inherited_assignments + recipe.preceding_statements.len();
            match &recipe.action {
                CompiledAction::Pipe { .. } | CompiledAction::Capture { .. } => {
                    explanations.push(RecipeExplanation {
                        line: recipe.line,
                        assignment_count,
                        conditions,
                        action: ActionKindExplanation::ExternalProgram,
                        header_operations: None,
                        copy: false,
                        defers_destination: true,
                    })
                }
                CompiledAction::Deliver {
                    destination,
                    continuation,
                    ..
                } => {
                    let action = match destination {
                        Destination::Maildir(_) => ActionKindExplanation::Maildir,
                        Destination::Mbox(_) => ActionKindExplanation::Mbox,
                        Destination::File(_) => ActionKindExplanation::File,
                        Destination::Discard(_) => ActionKindExplanation::Discard,
                    };
                    explanations.push(RecipeExplanation {
                        line: recipe.line,
                        assignment_count,
                        conditions,
                        action,
                        header_operations: None,
                        copy: *continuation == ContinuationMode::Continue,
                        defers_destination: destination.needs_runtime_variables(),
                    });
                }
                CompiledAction::Block(children) => {
                    children.collect_explanations(&conditions, assignment_count, explanations);
                }
                CompiledAction::Headers(action) => {
                    let mut operations = HeaderOperationExplanation::default();
                    for operation in &action.operations {
                        match operation {
                            crate::config::HeaderOperation::Remove { .. } => {
                                operations.remove += 1;
                            }
                            crate::config::HeaderOperation::Set { .. } => operations.set += 1,
                            crate::config::HeaderOperation::Add { .. } => operations.add += 1,
                            crate::config::HeaderOperation::Prepend { .. } => {
                                operations.prepend += 1;
                            }
                        }
                    }
                    explanations.push(RecipeExplanation {
                        line: recipe.line,
                        assignment_count,
                        conditions,
                        action: ActionKindExplanation::Headers,
                        header_operations: Some(operations),
                        copy: false,
                        defers_destination: false,
                    });
                }
            }
        }
    }
}

impl CompiledNode {
    fn header_action_result_is_known(&self) -> bool {
        matches!(
            &self.action,
            CompiledAction::Capture { options, .. }
                if options.action_input == ActionInput::Headers
        ) || matches!(&self.action, CompiledAction::Headers(_))
    }

    fn compile(recipe: &Recipe, preceding_statements: Vec<CompiledStatement>) -> Self {
        let conditions = compile_conditions(recipe);
        let action = match &recipe.action {
            RecipeAction::Pipe(action) => CompiledAction::Pipe {
                action: action.clone(),
                options: recipe.options,
            },
            RecipeAction::Capture(action) => CompiledAction::Capture {
                action: action.clone(),
                options: recipe.options,
            },
            RecipeAction::Deliver(destination) => CompiledAction::Deliver {
                destination: destination.clone(),
                continuation: recipe.options.continuation,
                output_ending: recipe.options.output_ending,
            },
            RecipeAction::Block(statements) => {
                CompiledAction::Block(CompiledSequence::compile(statements, &mut Vec::new()))
            }
            RecipeAction::Headers(action) => CompiledAction::Headers(action.clone()),
        };
        Self {
            line: recipe.line,
            preceding_statements,
            lock: recipe.lock.clone(),
            control: recipe.options.control,
            conditions,
            action,
        }
    }

    fn requirements(&self) -> InputRequirements {
        let action = match &self.action {
            CompiledAction::Pipe { .. } => InputRequirements {
                needs_headers: true,
                needs_body_contents: true,
                needs_end_of_message: true,
            },
            // A capture action only observes the area selected by h/b. A
            // header capture can finish at the header separator and must not
            // force an otherwise streamable body into staging.
            CompiledAction::Capture { options, .. } => match options.action_input {
                ActionInput::Headers => InputRequirements {
                    needs_headers: true,
                    ..InputRequirements::default()
                },
                ActionInput::Body | ActionInput::Message => InputRequirements {
                    needs_headers: true,
                    needs_body_contents: true,
                    needs_end_of_message: true,
                },
            },
            CompiledAction::Deliver { destination, .. }
                if destination.command_parts().is_some() =>
            {
                InputRequirements {
                    needs_headers: true,
                    needs_body_contents: true,
                    needs_end_of_message: true,
                }
            }
            CompiledAction::Deliver { .. } => InputRequirements::default(),
            CompiledAction::Block(sequence) => sequence.requirements(),
            CompiledAction::Headers(_) => InputRequirements {
                needs_headers: true,
                ..InputRequirements::default()
            },
        };
        let requirements = self
            .conditions
            .iter()
            .fold(action, |requirements, condition| {
                requirements.union(condition.requirements())
            });
        requirements.union(statements_requirements(&self.preceding_statements))
    }

    fn requires_ordered_delivery(&self) -> bool {
        self.lock.is_some()
            || self
                .conditions
                .iter()
                .any(CompiledCondition::requires_ordered_execution)
            || match &self.action {
                CompiledAction::Pipe { .. } => true,
                CompiledAction::Capture { .. } => true,
                CompiledAction::Deliver { destination, .. } => {
                    destination.needs_runtime_variables()
                        || matches!(destination, Destination::Mbox(_) | Destination::File(_))
                }
                CompiledAction::Block(sequence) => sequence.requires_ordered_delivery(),
                CompiledAction::Headers(_) => true,
            }
    }

    fn requires_preemptive_ordered_delivery(&self) -> bool {
        self.lock.is_some()
            || self
                .conditions
                .iter()
                .any(CompiledCondition::requires_ordered_execution)
            || match &self.action {
                CompiledAction::Pipe { .. } => true,
                CompiledAction::Capture { options, .. } => {
                    options.action_input != ActionInput::Headers
                }
                CompiledAction::Deliver { destination, .. } => {
                    destination.needs_runtime_variables()
                        || matches!(destination, Destination::Mbox(_) | Destination::File(_))
                }
                CompiledAction::Block(sequence) => sequence.requires_preemptive_ordered_delivery(),
                CompiledAction::Headers(_) => false,
            }
    }

    pub(super) fn needs_message_contents(&self) -> bool {
        self.conditions
            .iter()
            .any(CompiledCondition::needs_message_contents)
            || match &self.action {
                CompiledAction::Pipe { .. } => true,
                CompiledAction::Capture { options, .. } => {
                    options.action_input != ActionInput::Headers
                }
                CompiledAction::Deliver { destination, .. } => {
                    destination.command_parts().is_some()
                }
                CompiledAction::Block(sequence) => sequence.needs_message_contents(),
                CompiledAction::Headers(_) => false,
            }
    }
}

impl SequenceState {
    pub(super) fn record(
        &mut self,
        control: ControlFlow,
        conditions_matched: bool,
        action: ActionExecution,
        else_handled: bool,
    ) {
        self.previous = Some(RecipeExecution {
            conditions_matched,
            else_handled,
            action,
        });
        if !matches!(
            control,
            ControlFlow::AfterChainMatch | ControlFlow::AfterPreviousSuccess
        ) {
            self.chain_base_matched = Some(conditions_matched);
        }
    }
}
