// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::convert::Infallible;

use super::header::ResumeInput;
use super::*;

impl ExecutionPlan {
    pub fn evaluate_headers(&self, head: &MessageHead) -> HeaderEvaluation {
        self.evaluate_headers_with_trace(head, &mut RuntimeVariables::default(), &mut NoTrace)
    }

    pub fn evaluate_headers_with_trace(
        &self,
        head: &MessageHead,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> HeaderEvaluation {
        let mut head = head.clone();
        self.evaluate_headers_editing_with_trace(&mut head, runtime, trace)
    }

    pub fn evaluate_headers_editing_with_trace(
        &self,
        head: &mut MessageHead,
        runtime: &mut RuntimeVariables,
        trace: &mut impl TraceSink,
    ) -> HeaderEvaluation {
        match self.evaluate_headers_editing_inner::<Infallible, _>(head, runtime, trace, None) {
            Ok(evaluation) => evaluation,
            Err(OrderedExecutionError::Evaluation(error)) => HeaderEvaluation::Error(error),
            Err(OrderedExecutionError::Delivery(error)) => match error {},
        }
    }

    pub fn resume_buffered(
        &self,
        continuation: Continuation,
        message: &Message,
    ) -> Result<DeliveryPlan, EvalError> {
        self.resume_input(
            continuation,
            ResumeInput::Buffered(message),
            &mut RuntimeVariables::default(),
            &mut NoTrace,
        )
    }

    pub fn resume_streamed(
        &self,
        continuation: Continuation,
        message: &StreamedMessage,
    ) -> Result<DeliveryPlan, EvalError> {
        self.resume_input(
            continuation,
            ResumeInput::Streamed(message),
            &mut RuntimeVariables::default(),
            &mut NoTrace,
        )
    }

    pub fn evaluate_full(&self, message: &Message) -> Result<DeliveryPlan, EvalError> {
        let continuation = Continuation {
            frames: Vec::new(),
            execution: FanoutPlanState::default(),
            runtime: RuntimeVariables::default(),
            requirements: self.requirements(),
            restart: true,
        };
        self.resume_input(
            continuation,
            ResumeInput::Buffered(message),
            &mut RuntimeVariables::default(),
            &mut NoTrace,
        )
    }
}
