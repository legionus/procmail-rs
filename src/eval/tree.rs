// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::condition::{CompiledCondition, compile_conditions};
use super::explanation::{
    ActionKindExplanation, ConditionExplanation, HeaderOperationExplanation, RecipeExplanation,
};
use super::runtime_rc::{CompiledInclude, CompiledSwitch};
use super::{InputRequirements, PlanProperties};
use crate::config::{
    ActionInput, Assignment, AssignmentTarget, CommandAssignment, ContinuationMode, ControlFlow,
    Destination, DestinationKind, HeaderAction, OutputEnding, PipeAction, Recipe, RecipeAction,
    RecipeOptions, Statement,
};
use crate::trace::VariableSource as TraceVariableSource;

#[derive(Debug)]
pub(super) struct CompiledSequence {
    pub(super) recipes: Vec<CompiledNode>,
    pub(super) trailing_statements: Vec<CompiledStatement>,
    properties: PlanProperties,
}

#[derive(Debug)]
pub(super) struct CompiledNode {
    pub(super) line: usize,
    pub(super) preceding_statements: Vec<CompiledStatement>,
    pub(super) lock: Option<crate::config::PathExpression>,
    pub(super) control: ControlFlow,
    pub(super) conditions: Vec<CompiledCondition>,
    pub(super) action: CompiledAction,
    properties: PlanProperties,
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

fn statement_properties(statement: &CompiledStatement) -> PlanProperties {
    let mut properties = PlanProperties::default();
    match statement {
        CompiledStatement::CommandAssignment(_) => {
            properties.requirements = InputRequirements {
                needs_headers: true,
                needs_body_contents: true,
                needs_end_of_message: true,
            };
            properties.requires_ordered_delivery = true;
            properties.requires_preemptive_ordered_delivery = true;
            properties.has_external_commands = true;
        }
        CompiledStatement::Assignment(assignment) => {
            properties.requires_ordered_delivery = assignment.assignment.target
                == AssignmentTarget::LockFile
                || assignment.assignment.target == AssignmentTarget::Trap
                    && !assignment.assignment.value.is_empty();
            properties.requires_preemptive_ordered_delivery = properties.requires_ordered_delivery;
            properties.has_external_commands =
                assignment.assignment.target == AssignmentTarget::Trap;
        }
        CompiledStatement::Host(_)
        | CompiledStatement::Include(_)
        | CompiledStatement::Switch(_) => {}
    }
    properties
}

fn statements_properties(statements: &[CompiledStatement]) -> PlanProperties {
    statements
        .iter()
        .fold(PlanProperties::default(), |properties, statement| {
            properties.union(statement_properties(statement))
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
        let mut compiled = Self {
            recipes,
            trailing_statements: std::mem::take(preceding),
            properties: PlanProperties::default(),
        };
        let mut properties = compiled
            .recipes
            .iter()
            .fold(PlanProperties::default(), |properties, recipe| {
                properties.union(recipe.properties)
            })
            .union(statements_properties(&compiled.trailing_statements));
        properties.requires_ordered_delivery |=
            compiled.recipes.iter().enumerate().any(|(index, recipe)| {
                index != 0
                    && matches!(
                        recipe.control,
                        ControlFlow::AfterPreviousSuccess | ControlFlow::AfterPreviousError
                    )
            });

        // Header edits can run while only the bounded header section is
        // available. Keep them out of this early deferral decision so a
        // header-only configuration does not read or stage the body merely
        // because a later action must observe the edited bytes.
        properties.requires_preemptive_ordered_delivery |=
            compiled.recipes.windows(2).any(|pair| {
                matches!(
                    pair[1].control,
                    ControlFlow::AfterPreviousSuccess | ControlFlow::AfterPreviousError
                ) && !pair[0].header_action_result_is_known()
            });
        compiled.properties = properties;
        compiled
    }

    pub(super) fn properties(&self) -> PlanProperties {
        self.properties
    }

    pub(super) fn requirements(&self) -> InputRequirements {
        self.properties.requirements
    }

    pub(super) fn requires_ordered_delivery(&self) -> bool {
        self.properties.requires_ordered_delivery
    }

    pub(super) fn requires_preemptive_ordered_delivery(&self) -> bool {
        self.properties.requires_preemptive_ordered_delivery
    }

    pub(super) fn requirements_from(&self, start: usize) -> InputRequirements {
        let recipes = &self.recipes[start..];
        let requirements = recipes
            .iter()
            .fold(InputRequirements::default(), |requirements, recipe| {
                requirements.union(recipe.properties.requirements)
            });
        let requirements =
            requirements.union(statements_properties(&self.trailing_statements).requirements);
        if recipes
            .iter()
            .any(|recipe| recipe.properties.requires_ordered_delivery)
        {
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
                    let action = match destination.kind() {
                        DestinationKind::Maildir => ActionKindExplanation::Maildir,
                        DestinationKind::Mbox => ActionKindExplanation::Mbox,
                        DestinationKind::File => ActionKindExplanation::File,
                        DestinationKind::Discard => ActionKindExplanation::Discard,
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
                            crate::config::HeaderOperation::Rename { .. } => {
                                operations.rename += 1;
                            }
                            crate::config::HeaderOperation::Extract { .. } => {
                                operations.extract += 1;
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
        let mut properties = action_properties(&action);
        properties = conditions.iter().fold(properties, |properties, condition| {
            properties.union(condition.properties())
        });
        properties = properties.union(statements_properties(&preceding_statements));
        if recipe.lock.is_some() {
            properties.requires_ordered_delivery = true;
            properties.requires_preemptive_ordered_delivery = true;
        }
        Self {
            line: recipe.line,
            preceding_statements,
            lock: recipe.lock.clone(),
            control: recipe.options.control,
            conditions,
            action,
            properties,
        }
    }
}

fn action_properties(action: &CompiledAction) -> PlanProperties {
    match action {
        CompiledAction::Pipe { action, .. } => PlanProperties {
            requirements: InputRequirements {
                needs_headers: true,
                needs_body_contents: true,
                needs_end_of_message: true,
            },
            requires_ordered_delivery: true,
            requires_preemptive_ordered_delivery: true,
            needs_message_contents: true,
            has_external_commands: !action.command.is_empty(),
        },
        // A capture action only observes the area selected by h/b. A
        // header capture can finish at the header separator and must not
        // force an otherwise streamable body into staging.
        CompiledAction::Capture { options, .. } => match options.action_input {
            ActionInput::Headers => PlanProperties {
                requirements: InputRequirements {
                    needs_headers: true,
                    ..InputRequirements::default()
                },
                requires_ordered_delivery: true,
                has_external_commands: true,
                ..PlanProperties::default()
            },
            ActionInput::Body | ActionInput::Message => PlanProperties {
                requirements: InputRequirements {
                    needs_headers: true,
                    needs_body_contents: true,
                    needs_end_of_message: true,
                },
                requires_ordered_delivery: true,
                requires_preemptive_ordered_delivery: true,
                needs_message_contents: true,
                has_external_commands: true,
            },
        },
        CompiledAction::Deliver { destination, .. } => {
            let command = destination.command_expression().is_some();
            let ordered =
                destination.needs_runtime_variables() || destination.requires_ordered_delivery();
            PlanProperties {
                requirements: if command {
                    InputRequirements {
                        needs_headers: true,
                        needs_body_contents: true,
                        needs_end_of_message: true,
                    }
                } else {
                    InputRequirements::default()
                },
                requires_ordered_delivery: ordered,
                requires_preemptive_ordered_delivery: ordered,
                needs_message_contents: command,
                has_external_commands: command,
            }
        }
        CompiledAction::Block(sequence) => sequence.properties(),
        CompiledAction::Headers(_) => PlanProperties {
            requirements: InputRequirements {
                needs_headers: true,
                ..InputRequirements::default()
            },
            requires_ordered_delivery: true,
            ..PlanProperties::default()
        },
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
