// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::InputRequirements;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanExplanation {
    pub(super) requirements: InputRequirements,
    pub(super) requires_ordered_delivery: bool,
    pub(super) recipes: Vec<RecipeExplanation>,
}

impl PlanExplanation {
    pub fn requirements(&self) -> InputRequirements {
        self.requirements
    }

    pub fn requires_ordered_delivery(&self) -> bool {
        self.requires_ordered_delivery
    }

    pub fn recipes(&self) -> &[RecipeExplanation] {
        &self.recipes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeExplanation {
    pub(super) line: usize,
    pub(super) assignment_count: usize,
    pub(super) conditions: Vec<ConditionExplanation>,
    pub(super) action: ActionKindExplanation,
    pub(super) header_operations: Option<HeaderOperationExplanation>,
    pub(super) copy: bool,
    pub(super) defers_destination: bool,
}

impl RecipeExplanation {
    pub fn line(&self) -> usize {
        self.line
    }

    pub fn assignment_count(&self) -> usize {
        self.assignment_count
    }

    pub fn conditions(&self) -> &[ConditionExplanation] {
        &self.conditions
    }

    pub fn action(&self) -> ActionKindExplanation {
        self.action
    }

    pub fn header_operations(&self) -> Option<HeaderOperationExplanation> {
        self.header_operations
    }

    pub fn is_copy(&self) -> bool {
        self.copy
    }

    pub fn defers_destination(&self) -> bool {
        self.defers_destination
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConditionExplanation {
    pub(super) negated: bool,
    pub(super) kind: ConditionKindExplanation,
}

impl ConditionExplanation {
    pub fn is_negated(self) -> bool {
        self.negated
    }

    pub fn kind(self) -> ConditionKindExplanation {
        self.kind
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionKindExplanation {
    HeaderRegex,
    BodyRegex,
    MessageRegex,
    VariableRegex,
    Program,
    SmallerThan,
    LargerThan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKindExplanation {
    Maildir,
    Mbox,
    ExternalProgram,
    Headers,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeaderOperationExplanation {
    pub(super) remove: usize,
    pub(super) set: usize,
    pub(super) add: usize,
    pub(super) prepend: usize,
}

impl HeaderOperationExplanation {
    pub fn remove_count(self) -> usize {
        self.remove
    }

    pub fn set_count(self) -> usize {
        self.set
    }

    pub fn add_count(self) -> usize {
        self.add
    }

    pub fn prepend_count(self) -> usize {
        self.prepend
    }
}
