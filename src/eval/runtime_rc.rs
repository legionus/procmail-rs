// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{CompiledSequence, EvalError};
use crate::config::RcFileExpression;
use crate::rc_file::{MAX_RC_TRANSITIONS, RcFileLoader};
use crate::runtime::RuntimeVariables;

const MAX_RC_DIAGNOSTIC_LEN: usize = 1024;
pub const MAX_RUNTIME_RC_WARNINGS: usize = 128;

#[derive(Debug)]
pub(super) struct RuntimeRcState {
    loader: Mutex<Option<RcFileLoader>>,
    transitions: AtomicUsize,
    dynamic_ordered_delivery: AtomicBool,
    dynamic_message_contents: AtomicBool,
    diagnostics: Mutex<Vec<String>>,
    warning_count: AtomicUsize,
    warnings_omitted: AtomicBool,
}

impl RuntimeRcState {
    pub(super) fn new(loader: Option<RcFileLoader>) -> Self {
        Self {
            loader: Mutex::new(loader),
            transitions: AtomicUsize::new(0),
            dynamic_ordered_delivery: AtomicBool::new(false),
            dynamic_message_contents: AtomicBool::new(false),
            diagnostics: Mutex::new(Vec::new()),
            warning_count: AtomicUsize::new(0),
            warnings_omitted: AtomicBool::new(false),
        }
    }

    pub(super) fn context(&self) -> RcExecutionContext<'_> {
        RcExecutionContext {
            state: self,
            depth: 0,
        }
    }

    pub(super) fn take_diagnostics(&self) -> Vec<String> {
        let mut diagnostics = std::mem::take(
            &mut *self
                .diagnostics
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        if self.warnings_omitted.swap(false, Ordering::Relaxed) {
            diagnostics.push("warning: additional runtime rc warnings were omitted".to_owned());
        }
        self.warning_count.store(0, Ordering::Relaxed);
        diagnostics
    }

    pub(super) fn requires_ordered_delivery(&self) -> bool {
        self.dynamic_ordered_delivery.load(Ordering::Relaxed)
    }

    pub(super) fn needs_message_contents(&self) -> bool {
        self.dynamic_message_contents.load(Ordering::Relaxed)
    }

    pub(super) fn reset_transitions(&self) {
        self.transitions.store(0, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub(super) struct CompiledInclude {
    expression: RcFileExpression,
    loaded: Mutex<HashMap<String, Arc<LoadedRuntimeRc>>>,
}

impl CompiledInclude {
    pub(super) fn new(expression: RcFileExpression) -> Self {
        Self {
            expression,
            loaded: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn line(&self) -> usize {
        self.expression.line
    }

    pub(super) fn enter<'state>(
        &self,
        runtime: &RuntimeVariables,
        context: RcExecutionContext<'state>,
    ) -> Result<EnteredRuntimeRc<'state>, EvalError> {
        enter_runtime_rc(
            &self.expression,
            &self.loaded,
            RuntimeRcStatement::Include,
            runtime,
            context,
        )
    }
}

#[derive(Debug)]
pub(super) struct CompiledSwitch {
    expression: RcFileExpression,
    loaded: Mutex<HashMap<String, Arc<LoadedRuntimeRc>>>,
}

impl CompiledSwitch {
    pub(super) fn new(expression: RcFileExpression) -> Self {
        Self {
            expression,
            loaded: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn line(&self) -> usize {
        self.expression.line
    }

    pub(super) fn enter<'state>(
        &self,
        runtime: &RuntimeVariables,
        context: RcExecutionContext<'state>,
    ) -> Result<EnteredRuntimeRc<'state>, EvalError> {
        enter_runtime_rc(
            &self.expression,
            &self.loaded,
            RuntimeRcStatement::Switch,
            runtime,
            context,
        )
    }
}

pub(super) struct EnteredRuntimeRc<'state> {
    loaded: Arc<LoadedRuntimeRc>,
    child_context: Option<RcExecutionContext<'state>>,
}

impl<'state> EnteredRuntimeRc<'state> {
    pub(super) fn is_empty(&self) -> bool {
        matches!(self.loaded.as_ref(), LoadedRuntimeRc::Empty)
    }

    pub(super) fn sequence(
        &self,
    ) -> Result<Option<(&CompiledSequence, RcExecutionContext<'state>)>, EvalError> {
        match (self.loaded.as_ref(), self.child_context) {
            (LoadedRuntimeRc::Sequence(sequence), Some(context)) => Ok(Some((sequence, context))),
            (LoadedRuntimeRc::Sequence(_), None) => Err(EvalError::RuntimeRc(
                "loaded runtime rc sequence has no child context".to_owned(),
            )),
            _ => Ok(None),
        }
    }
}

#[derive(Debug, Default)]
pub(super) enum LoadedRuntimeRc {
    #[default]
    Empty,
    Failed,
    Sequence(Box<CompiledSequence>),
}

#[derive(Clone, Copy)]
enum RuntimeRcStatement {
    Include,
    Switch,
}

impl RuntimeRcStatement {
    fn name(self) -> &'static str {
        match self {
            Self::Include => "INCLUDERC",
            Self::Switch => "SWITCHRC",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct RcExecutionContext<'a> {
    state: &'a RuntimeRcState,
    pub(super) depth: usize,
}

impl RcExecutionContext<'_> {
    fn push_warning(self, diagnostic: String) {
        let admitted = self
            .state
            .warning_count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < MAX_RUNTIME_RC_WARNINGS).then_some(count + 1)
            })
            .is_ok();
        if admitted {
            self.state
                .diagnostics
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(diagnostic);
        } else {
            self.state.warnings_omitted.store(true, Ordering::Relaxed);
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
        self.state
            .transitions
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count
                    .checked_add(1)
                    .filter(|next| *next <= MAX_RC_TRANSITIONS)
            })
            .map(|_| ())
            .map_err(|_| {
                EvalError::RuntimeRc(format!(
                    "rc transitions exceed the hard limit of {MAX_RC_TRANSITIONS}"
                ))
            })
    }
}

fn load_runtime_rc(
    expression: &RcFileExpression,
    loaded_states: &Mutex<HashMap<String, Arc<LoadedRuntimeRc>>>,
    statement: RuntimeRcStatement,
    runtime: &RuntimeVariables,
    context: RcExecutionContext<'_>,
) -> Result<Arc<LoadedRuntimeRc>, EvalError> {
    context.record_transition()?;
    let path = expression
        .resolve_with(|name| runtime.get(name).map(str::to_owned))
        .map_err(EvalError::Expansion)?;
    let mut loaded_states = loaded_states
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(loaded) = loaded_states.get(&path) {
        return Ok(Arc::clone(loaded));
    }
    loaded_states
        .try_reserve(1)
        .map_err(|_| EvalError::RuntimeRc("cannot reserve runtime rc path cache".to_owned()))?;
    let statement_name = statement.name();

    // Original procmail uses SWITCHRC=/dev/null as a successful request to
    // stop the current rc file. Account it as a transition before recognizing
    // the exact resolved path, but never open the device or weaken the regular
    // file checks used by INCLUDERC and other switch targets.
    if matches!(statement, RuntimeRcStatement::Switch) && path == "/dev/null" {
        let loaded = Arc::new(LoadedRuntimeRc::Empty);
        loaded_states.insert(path, Arc::clone(&loaded));
        return Ok(loaded);
    }
    let child_context = context.descend()?;
    let loaded = context
        .state
        .loader
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
        .ok_or(EvalError::RuntimeRcLoaderUnavailable {
            line: expression.line,
            statement: statement_name,
        })?
        .load_config(expression, runtime, child_context.depth);
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(error) if error.is_resource_limit() => {
            return Err(EvalError::RuntimeRc(format!(
                "line {}: {statement_name} resource limit: {}",
                expression.line,
                error.safe_message()
            )));
        }
        Err(error) => {
            let mut diagnostic = format!(
                "line {}: {statement_name} failed: {}",
                expression.line,
                error.safe_message()
            );
            truncate_utf8(&mut diagnostic, MAX_RC_DIAGNOSTIC_LEN);
            context
                .state
                .diagnostics
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(diagnostic);
            let loaded = Arc::new(LoadedRuntimeRc::Failed);
            loaded_states.insert(path, Arc::clone(&loaded));
            return Ok(loaded);
        }
    };
    let Some(loaded) = loaded else {
        let loaded = Arc::new(LoadedRuntimeRc::Empty);
        loaded_states.insert(path, Arc::clone(&loaded));
        return Ok(loaded);
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
        context
            .state
            .dynamic_message_contents
            .store(true, Ordering::Relaxed);
    }
    if sequence.requires_ordered_delivery() {
        context
            .state
            .dynamic_ordered_delivery
            .store(true, Ordering::Relaxed);
    }
    let loaded = Arc::new(LoadedRuntimeRc::Sequence(Box::new(sequence)));
    loaded_states.insert(path, Arc::clone(&loaded));
    Ok(loaded)
}

fn enter_runtime_rc<'state>(
    expression: &RcFileExpression,
    loaded_states: &Mutex<HashMap<String, Arc<LoadedRuntimeRc>>>,
    statement: RuntimeRcStatement,
    runtime: &RuntimeVariables,
    context: RcExecutionContext<'state>,
) -> Result<EnteredRuntimeRc<'state>, EvalError> {
    let loaded = load_runtime_rc(expression, loaded_states, statement, runtime, context)?;

    // Compute the child context at the same boundary that owns the loaded
    // tree. This keeps depth checking identical in every evaluation mode and
    // prevents a caller from accidentally evaluating a child with its
    // parent's rc-file depth.
    let child_context = if matches!(&*loaded, LoadedRuntimeRc::Sequence(_)) {
        Some(context.descend()?)
    } else {
        None
    };
    Ok(EnteredRuntimeRc {
        loaded,
        child_context,
    })
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
