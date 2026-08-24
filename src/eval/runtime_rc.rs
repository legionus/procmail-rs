// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::cell::{Cell, Ref, RefCell};

use super::{CompiledSequence, EvalError};
use crate::config::RcFileExpression;
use crate::rc_file::{MAX_RC_TRANSITIONS, RcFileLoader};
use crate::runtime::RuntimeVariables;

const MAX_RC_DIAGNOSTIC_LEN: usize = 1024;
pub const MAX_RUNTIME_RC_WARNINGS: usize = 128;

#[derive(Debug)]
pub(super) struct RuntimeRcState {
    loader: RefCell<Option<RcFileLoader>>,
    transitions: Cell<usize>,
    dynamic_ordered_delivery: Cell<bool>,
    dynamic_message_contents: Cell<bool>,
    diagnostics: RefCell<Vec<String>>,
    warning_count: Cell<usize>,
    warnings_omitted: Cell<bool>,
}

impl RuntimeRcState {
    pub(super) fn new(loader: Option<RcFileLoader>) -> Self {
        Self {
            loader: RefCell::new(loader),
            transitions: Cell::new(0),
            dynamic_ordered_delivery: Cell::new(false),
            dynamic_message_contents: Cell::new(false),
            diagnostics: RefCell::new(Vec::new()),
            warning_count: Cell::new(0),
            warnings_omitted: Cell::new(false),
        }
    }

    pub(super) fn context(&self) -> RcExecutionContext<'_> {
        RcExecutionContext {
            state: self,
            depth: 0,
        }
    }

    pub(super) fn take_diagnostics(&self) -> Vec<String> {
        let mut diagnostics = std::mem::take(&mut *self.diagnostics.borrow_mut());
        if self.warnings_omitted.replace(false) {
            diagnostics.push("warning: additional runtime rc warnings were omitted".to_owned());
        }
        self.warning_count.set(0);
        diagnostics
    }

    pub(super) fn requires_ordered_delivery(&self) -> bool {
        self.dynamic_ordered_delivery.get()
    }

    pub(super) fn needs_message_contents(&self) -> bool {
        self.dynamic_message_contents.get()
    }

    pub(super) fn reset_transitions(&self) {
        self.transitions.set(0);
    }
}

#[derive(Debug)]
pub(super) struct CompiledInclude {
    expression: RcFileExpression,
    loaded: RefCell<LoadedRuntimeRc>,
}

impl CompiledInclude {
    pub(super) fn new(expression: RcFileExpression) -> Self {
        Self {
            expression,
            loaded: RefCell::new(LoadedRuntimeRc::Unloaded),
        }
    }

    pub(super) fn line(&self) -> usize {
        self.expression.line
    }

    pub(super) fn loaded(&self) -> Ref<'_, LoadedRuntimeRc> {
        self.loaded.borrow()
    }

    pub(super) fn ensure_loaded(
        &self,
        runtime: &RuntimeVariables,
        context: RcExecutionContext<'_>,
    ) -> Result<(), EvalError> {
        load_runtime_rc(
            &self.expression,
            &self.loaded,
            "INCLUDERC",
            runtime,
            context,
        )
    }
}

#[derive(Debug)]
pub(super) struct CompiledSwitch {
    expression: RcFileExpression,
    loaded: RefCell<LoadedRuntimeRc>,
}

impl CompiledSwitch {
    pub(super) fn new(expression: RcFileExpression) -> Self {
        Self {
            expression,
            loaded: RefCell::new(LoadedRuntimeRc::Unloaded),
        }
    }

    pub(super) fn line(&self) -> usize {
        self.expression.line
    }

    pub(super) fn loaded(&self) -> Ref<'_, LoadedRuntimeRc> {
        self.loaded.borrow()
    }

    pub(super) fn ensure_loaded(
        &self,
        runtime: &RuntimeVariables,
        context: RcExecutionContext<'_>,
    ) -> Result<(), EvalError> {
        load_runtime_rc(&self.expression, &self.loaded, "SWITCHRC", runtime, context)
    }
}

#[derive(Debug, Default)]
pub(super) enum LoadedRuntimeRc {
    #[default]
    Unloaded,
    Empty,
    Failed,
    Sequence(Box<CompiledSequence>),
}

#[derive(Clone, Copy)]
pub(super) struct RcExecutionContext<'a> {
    state: &'a RuntimeRcState,
    pub(super) depth: usize,
}

impl RcExecutionContext<'_> {
    fn push_warning(self, diagnostic: String) {
        let count = self.state.warning_count.get();
        if count < MAX_RUNTIME_RC_WARNINGS {
            self.state.diagnostics.borrow_mut().push(diagnostic);
            self.state.warning_count.set(count + 1);
        } else {
            self.state.warnings_omitted.set(true);
        }
    }

    pub(super) fn descend(self) -> Result<Self, EvalError> {
        let depth = self
            .depth
            .checked_add(1)
            .ok_or_else(|| EvalError::RuntimeRc("rc include depth overflows".to_owned()))?;
        Ok(Self { depth, ..self })
    }

    fn record_transition(self) -> Result<(), EvalError> {
        let transitions = self
            .state
            .transitions
            .get()
            .checked_add(1)
            .ok_or_else(|| EvalError::RuntimeRc("rc transition count overflows".to_owned()))?;
        if transitions > MAX_RC_TRANSITIONS {
            return Err(EvalError::RuntimeRc(format!(
                "rc transitions exceed the hard limit of {MAX_RC_TRANSITIONS}"
            )));
        }
        self.state.transitions.set(transitions);
        Ok(())
    }
}

fn load_runtime_rc(
    expression: &RcFileExpression,
    loaded_state: &RefCell<LoadedRuntimeRc>,
    statement: &'static str,
    runtime: &RuntimeVariables,
    context: RcExecutionContext<'_>,
) -> Result<(), EvalError> {
    context.record_transition()?;
    if !matches!(*loaded_state.borrow(), LoadedRuntimeRc::Unloaded) {
        return Ok(());
    }
    let child_context = context.descend()?;
    let loaded = context
        .state
        .loader
        .borrow_mut()
        .as_mut()
        .ok_or(EvalError::RuntimeRcLoaderUnavailable {
            line: expression.line,
            statement,
        })?
        .load_config(expression, runtime, child_context.depth);
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(error) if error.is_resource_limit() => {
            return Err(EvalError::RuntimeRc(format!(
                "line {}: {statement} resource limit: {}",
                expression.line,
                error.safe_message()
            )));
        }
        Err(error) => {
            let mut diagnostic = format!(
                "line {}: {statement} failed: {}",
                expression.line,
                error.safe_message()
            );
            truncate_utf8(&mut diagnostic, MAX_RC_DIAGNOSTIC_LEN);
            context.state.diagnostics.borrow_mut().push(diagnostic);
            *loaded_state.borrow_mut() = LoadedRuntimeRc::Failed;
            return Ok(());
        }
    };
    let Some(loaded) = loaded else {
        *loaded_state.borrow_mut() = LoadedRuntimeRc::Empty;
        return Ok(());
    };
    loaded
        .config()
        .for_each_compatibility_warning(|line, flag| {
            let mut diagnostic = format!(
                "warning: {}:{line}: recipe flag '{flag}' has no effect on a block",
                loaded.path().display()
            );
            truncate_utf8(&mut diagnostic, MAX_RC_DIAGNOSTIC_LEN);
            context.push_warning(diagnostic);
        });
    let mut preceding = Vec::new();
    let sequence = CompiledSequence::compile(&loaded.into_config().statements, &mut preceding);
    let requirements = sequence.requirements();
    if requirements.needs_body_contents {
        context.state.dynamic_message_contents.set(true);
    }
    if sequence.requires_ordered_delivery() {
        context.state.dynamic_ordered_delivery.set(true);
    }
    *loaded_state.borrow_mut() = LoadedRuntimeRc::Sequence(Box::new(sequence));
    Ok(())
}

fn truncate_utf8(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}
